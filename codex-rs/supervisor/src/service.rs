//! 编排层：五件套语义的真身（P1-4）。
//!
//! MCP 层只做参数搬运；任务的出生、看护、判卷、收尸全在这里：
//!
//! - `spawn_task`：登记 → 会话租约 → worktree 分配 → 发射 → **立即返回句柄**，
//!   完成驱动器在后台看护（#1 异步模型）。
//! - 启动恢复：加载全部记录，把上一世的 Running/Pending 扫成 Failed 并释放
//!   租约（#2：跨进程重启可恢复；rollout 在盘，映射在记录里）。
//! - judge 钩子：任务自报完成 ≠ 完成；注册了判卷命令就以退出码为准（#5）。
//!   判卷命令按 #9 硬化（pipefail + 硬超时 + 进程组）。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;

use crate::executor;
use crate::executor::ExecError;
use crate::executor::ExecutorConfig;
use crate::executor::ExecutorEvent;
use crate::executor::LaunchPlan;
use crate::models::ModelRegistry;
use crate::models::RegistryError;
use crate::observe;
use crate::paths::LockError;
use crate::paths::StoreError;
use crate::paths::SupervisorHome;
use crate::paths::SupervisorLock;
use crate::registry::JudgeOutcome;
use crate::registry::JudgeSpec;
use crate::registry::SessionTarget;
use crate::registry::TaskId;
use crate::registry::TaskRecord;
use crate::registry::TaskSpec;
use crate::registry::TaskState;
use crate::registry::TransitionError;
use crate::registry::WorktreePolicy;
use crate::session::ApprovalPolicy;
use crate::session::ModelAffinity;
use crate::session::OwnershipError;
use crate::session::SandboxMode;
use crate::session::SessionId;
use crate::session::SessionRecord;
use crate::worktree;
use crate::worktree::ReclaimMode;
use crate::worktree::WorktreeHandle;

/// 判卷命令硬超时（#9）。
const JUDGE_TIMEOUT: Duration = Duration::from_secs(600);
/// 任务缺省硬超时（#9：无超时 = 沙箱禁网吊死的实测形态）。
const DEFAULT_TASK_TIMEOUT_SECS: u64 = 900;
/// last_agent_message 存档截断（全文在 events 原始流）。
const MESSAGE_ARCHIVE_CHARS: usize = 2000;

#[derive(Debug, Clone)]
pub struct SupervisorConfig {
    pub codex_home: PathBuf,
    pub codex_bin: PathBuf,
}

/// spawn/reply 的入参（MCP 参数已翻译成类型化形态）。
#[derive(Debug, Clone)]
pub struct SpawnRequest {
    pub prompt: String,
    pub model_key: String,
    pub cwd: PathBuf,
    pub sandbox: SandboxMode,
    pub judge_command: Option<String>,
    pub timeout_secs: Option<u64>,
    pub worktree: WorktreePolicy,
    pub session: SessionTarget,
    pub diff_first: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SpawnReply {
    pub task_id: String,
    pub session_id: String,
    pub worktree_branch: Option<String>,
}

/// status 视图（#15：紧凑摘要，绝不携带原始流）。
#[derive(Debug, Clone, Serialize)]
pub struct TaskStatusView {
    pub task_id: String,
    pub state: String,
    pub model_key: String,
    pub session_id: Option<String>,
    pub worktree_branch: Option<String>,
    pub prompt_snippet: String,
    pub last_agent_message: Option<String>,
    pub output_tokens: Option<i64>,
    pub created_at: String,
    pub updated_at: String,
}

/// 启动恢复报告。
#[derive(Debug, Default)]
pub struct StartReport {
    pub tasks_loaded: usize,
    pub sessions_loaded: usize,
    /// 上一世死于任上的任务（已扫成 Failed 并释放租约）。
    pub swept_tasks: Vec<TaskId>,
    /// 损坏跳过的记录文件（路径 + 原因）——绝不静默。
    pub skipped: Vec<(PathBuf, String)>,
}

struct State {
    tasks: HashMap<TaskId, TaskRecord>,
    sessions: HashMap<SessionId, SessionRecord>,
    running: HashMap<TaskId, executor::InterruptHandle>,
}

pub struct Supervisor {
    home: SupervisorHome,
    exec: ExecutorConfig,
    registry: ModelRegistry,
    state: Mutex<State>,
    _lock: SupervisorLock,
}

impl Supervisor {
    /// 启动：单例锁 → 载入三张表 → 扫尸 → 就绪。
    pub fn start(config: SupervisorConfig) -> Result<(Arc<Self>, StartReport), ServiceError> {
        let home = SupervisorHome::new(&config.codex_home);
        let lock = home.acquire_singleton_lock()?;
        home.ensure_layout().map_err(|source| ServiceError::Io {
            context: "ensure supervisor layout".to_string(),
            source,
        })?;

        // 登记表缺失 = 空表可跑（P1-6 入库三台助理）；损坏必须响亮失败。
        let registry = match ModelRegistry::load(&home) {
            Ok(registry) => registry,
            Err(StoreError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                tracing::warn!(
                    "supervisor.models.toml 不存在，登记表为空——spawn_task 将拒绝一切 model_key"
                );
                ModelRegistry::new()
            }
            Err(err) => return Err(err.into()),
        };

        let mut report = StartReport::default();
        let task_load = TaskRecord::load_all(&home)?;
        let session_load = SessionRecord::load_all(&home)?;
        report.skipped.extend(task_load.skipped);
        report.skipped.extend(session_load.skipped);

        let mut tasks: HashMap<TaskId, TaskRecord> =
            task_load.records.into_iter().map(|t| (t.id, t)).collect();
        let mut sessions: HashMap<SessionId, SessionRecord> = session_load
            .records
            .into_iter()
            .map(|s| (s.id, s))
            .collect();
        report.tasks_loaded = tasks.len();
        report.sessions_loaded = sessions.len();

        // 扫尸：上一世的 Running/Pending 不可能还活着（子进程随旧 bridge 消亡）。
        for task in tasks.values_mut() {
            let reason = match task.state {
                TaskState::Running { .. } => "bridge 重启时任务仍在执行，进程已随旧 bridge 消亡",
                TaskState::Pending => "bridge 重启于任务起跑前",
                _ => continue,
            };
            task.transition(TaskState::Failed {
                error: reason.to_string(),
            })?;
            task.save(&home)?;
            report.swept_tasks.push(task.id);
            if let Some(session_id) = task.session_id
                && let Some(session) = sessions.get_mut(&session_id)
                && session.release(task.id).is_ok()
            {
                session.save(&home)?;
            }
        }

        let supervisor = Arc::new(Self {
            home,
            exec: ExecutorConfig {
                codex_bin: config.codex_bin,
                codex_home: config.codex_home,
            },
            registry,
            state: Mutex::new(State {
                tasks,
                sessions,
                running: HashMap::new(),
            }),
            _lock: lock,
        });
        Ok((supervisor, report))
    }

    /// spawn_task：立即返回句柄，后台看护到终局（#1）。
    pub async fn spawn_task(self: &Arc<Self>, req: SpawnRequest) -> Result<SpawnReply, ServiceError> {
        let entry = self.registry.resolve(&req.model_key)?.clone();

        let spec = TaskSpec {
            prompt: req.prompt.clone(),
            model_key: req.model_key.clone(),
            cwd: req.cwd.clone(),
            sandbox: req.sandbox,
            approval: ApprovalPolicy::Never, // #13 挂账 Phase 2：v1 = never + 沙箱兜底
            judge: req.judge_command.clone().map(|command| JudgeSpec { command }),
            diff_first: req.diff_first,
            worktree: req.worktree,
            session: req.session,
            timeout_secs: Some(req.timeout_secs.unwrap_or(DEFAULT_TASK_TIMEOUT_SECS)),
        };
        let mut task = TaskRecord::new(spec);
        let task_id = task.id;

        // 会话：新建或续接，一律持租约执行（#4）。
        let session_id = {
            let mut state = self.lock_state();
            match req.session {
                SessionTarget::New => {
                    let mut session = SessionRecord::new(
                        ModelAffinity {
                            model: entry.served_name.clone(),
                            provider: entry.provider.clone(),
                            reasoning: None,
                        },
                        req.sandbox,
                        ApprovalPolicy::Never,
                        req.cwd.clone(),
                    );
                    session.claim(task_id)?;
                    let id = session.id;
                    session.save(&self.home)?;
                    state.sessions.insert(id, session);
                    id
                }
                SessionTarget::Resume { session: id } => {
                    let session = state
                        .sessions
                        .get_mut(&id)
                        .ok_or(ServiceError::SessionNotFound { session: id })?;
                    if session.rollout.conversation_id.is_none() {
                        return Err(ServiceError::SessionNotResumable { session: id });
                    }
                    session.claim(task_id)?;
                    session.save(&self.home)?;
                    id
                }
            }
        };
        task.session_id = Some(session_id);

        // worktree 分配（Isolated）；失败时回滚租约。
        let worktree_handle = if req.worktree == WorktreePolicy::Isolated {
            let home = self.home.clone();
            let cwd = req.cwd.clone();
            let allocated = tokio::task::spawn_blocking(move || {
                worktree::allocate(&home, task_id, &cwd)
            })
            .await
            .map_err(|e| ServiceError::Internal {
                message: format!("worktree allocation task panicked: {e}"),
            })?;
            match allocated {
                Ok(handle) => Some(handle),
                Err(err) => {
                    self.release_session(session_id, task_id);
                    return Err(err.into());
                }
            }
        } else {
            None
        };
        if let Some(handle) = &worktree_handle {
            task.worktree_path = Some(handle.worktree_root.clone());
        }

        // 发射计划：路由显式 model + model_provider（#12）；resume 时一并注入
        // 模型亲和，杜绝 `exec resume` 回落缺省的语义差（#3）。
        let mut overrides = vec![(
            "model_provider".to_string(),
            format!("\"{}\"", entry.provider.id),
        )];
        if let Some(base_url) = &entry.provider.base_url {
            overrides.push((
                format!("model_providers.{}.base_url", entry.provider.id),
                format!("\"{base_url}\""),
            ));
        }
        if let Some(wire_api) = &entry.provider.wire_api {
            overrides.push((
                format!("model_providers.{}.wire_api", entry.provider.id),
                format!("\"{wire_api}\""),
            ));
        }
        let resume_id = {
            let state = self.lock_state();
            match req.session {
                SessionTarget::Resume { session } => state
                    .sessions
                    .get(&session)
                    .and_then(|s| s.rollout.conversation_id.clone()),
                SessionTarget::New => None,
            }
        };
        let plan = LaunchPlan {
            prompt: req.prompt,
            model: Some(entry.served_name.clone()),
            sandbox: req.sandbox,
            cwd: worktree_handle
                .as_ref()
                .map(|h| h.task_cwd.clone())
                .unwrap_or_else(|| req.cwd.clone()),
            overrides,
            resume: resume_id,
            skip_git_repo_check: req.worktree == WorktreePolicy::InPlace,
            timeout: task.spec.timeout_secs.map(Duration::from_secs),
        };

        let events_path = self.home.events_dir().join(format!("{task_id}.jsonl"));
        task.events_path = Some(events_path.clone());

        let mut running = match executor::launch(&self.exec, task_id, &plan, &events_path).await {
            Ok(running) => running,
            Err(err) => {
                self.release_session(session_id, task_id);
                if let Some(handle) = &worktree_handle {
                    reclaim_blocking(handle.clone(), ReclaimMode::DeleteBranch).await;
                }
                return Err(err.into());
            }
        };

        task.transition(TaskState::Running { pid: running.pid })?;
        task.save(&self.home)?;
        let interrupt = running.interrupt.take();
        {
            let mut state = self.lock_state();
            if let Some(handle) = interrupt {
                state.running.insert(task_id, handle);
            }
            state.tasks.insert(task_id, task);
        }

        // 完成驱动器：看护到终局。
        let driver = Arc::clone(self);
        tokio::spawn(async move {
            driver
                .drive_to_completion(task_id, session_id, running, worktree_handle)
                .await;
        });

        Ok(SpawnReply {
            task_id: task_id.to_string(),
            session_id: session_id.to_string(),
            worktree_branch: (req.worktree == WorktreePolicy::Isolated)
                .then(|| format!("rdos/task/{task_id}")),
        })
    }

    /// reply：续接既有会话（T4 语义）。模型亲和取自会话，除非显式换将。
    pub async fn reply(
        self: &Arc<Self>,
        session_id: SessionId,
        prompt: String,
        model_key: Option<String>,
        judge_command: Option<String>,
        timeout_secs: Option<u64>,
    ) -> Result<SpawnReply, ServiceError> {
        let (cwd, sandbox, affinity_key) = {
            let state = self.lock_state();
            let session = state
                .sessions
                .get(&session_id)
                .ok_or(ServiceError::SessionNotFound { session: session_id })?;
            // 会话记录持有 served_name；反查登记键（#12 分派仍走登记表）。
            let affinity_key = model_key.or_else(|| {
                self.registry
                    .models
                    .iter()
                    .find(|(_, entry)| entry.served_name == session.model.model)
                    .map(|(key, _)| key.clone())
            });
            (session.cwd.clone(), session.sandbox, affinity_key)
        };
        let model_key = affinity_key.ok_or(ServiceError::SessionNotResumable { session: session_id })?;
        self.spawn_task(SpawnRequest {
            prompt,
            model_key,
            cwd,
            sandbox,
            judge_command,
            timeout_secs,
            worktree: WorktreePolicy::InPlace, // 续接在原会话语境，不再另辟 worktree
            session: SessionTarget::Resume { session: session_id },
            diff_first: true,
        })
        .await
    }

    pub fn status(&self, task: Option<TaskId>) -> Result<Vec<TaskStatusView>, ServiceError> {
        let state = self.lock_state();
        let mut views: Vec<TaskStatusView> = match task {
            Some(id) => vec![state
                .tasks
                .get(&id)
                .ok_or(ServiceError::TaskNotFound { task: id })?
                .clone()],
            None => state.tasks.values().cloned().collect(),
        }
        .into_iter()
        .map(|record| view_of(&record))
        .collect();
        views.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(views)
    }

    pub fn tail(&self, task: TaskId, lines: usize) -> Result<Vec<String>, ServiceError> {
        let events_path = {
            let state = self.lock_state();
            state
                .tasks
                .get(&task)
                .ok_or(ServiceError::TaskNotFound { task })?
                .events_path
                .clone()
        };
        let Some(events_path) = events_path else {
            return Ok(Vec::new());
        };
        observe::tail_summarized(&events_path, lines).map_err(|source| ServiceError::Io {
            context: format!("read events for {task}"),
            source,
        })
    }

    pub fn interrupt(&self, task: TaskId) -> Result<(), ServiceError> {
        let handle = {
            let mut state = self.lock_state();
            if !state.tasks.contains_key(&task) {
                return Err(ServiceError::TaskNotFound { task });
            }
            state.running.remove(&task)
        };
        match handle {
            Some(handle) => {
                handle.trigger();
                Ok(())
            }
            None => Err(ServiceError::TaskNotRunning { task }),
        }
    }

    /// 完成驱动器：折叠事件（rollout 映射第一时间落盘）→ 终局裁决 → 判卷 →
    /// 记录回写 → 租约释放 → worktree 收尸。
    async fn drive_to_completion(
        self: Arc<Self>,
        task_id: TaskId,
        session_id: SessionId,
        mut running: executor::RunningTask,
        worktree_handle: Option<WorktreeHandle>,
    ) {
        // 事件折叠：thread.started 一到就持久化 session↔rollout 映射（#2——
        // 60s 窗口的教训：映射晚一秒落盘就多一分连坐风险）。
        while let Some(event) = running.events.recv().await {
            if let ExecutorEvent::Parsed(codex_exec::ThreadEvent::ThreadStarted(started)) = &event {
                let mut state = self.lock_state();
                if let Some(session) = state.sessions.get_mut(&session_id) {
                    session.rollout.conversation_id = Some(started.thread_id.clone());
                    session.touch();
                    let _ = session.save(&self.home);
                }
            }
        }

        let exit = match running.join.await {
            Ok(Ok(exit)) => exit,
            Ok(Err(exec_err)) => {
                self.finalize(
                    task_id,
                    session_id,
                    TaskState::Failed {
                        error: format!("executor: {exec_err}"),
                    },
                    None,
                    None,
                    worktree_handle,
                    ReclaimMode::DeleteBranch,
                )
                .await;
                return;
            }
            Err(join_err) => {
                self.finalize(
                    task_id,
                    session_id,
                    TaskState::Failed {
                        error: format!("executor task panicked: {join_err}"),
                    },
                    None,
                    None,
                    worktree_handle,
                    ReclaimMode::DeleteBranch,
                )
                .await;
                return;
            }
        };

        let judge_spec = {
            let state = self.lock_state();
            state.tasks.get(&task_id).and_then(|t| t.spec.judge.clone())
        };
        let judge_cwd = worktree_handle
            .as_ref()
            .map(|h| h.task_cwd.clone())
            .or_else(|| {
                let state = self.lock_state();
                state.tasks.get(&task_id).map(|t| t.spec.cwd.clone())
            });

        let (final_state, reclaim_mode) = if exit.interrupted {
            (TaskState::Interrupted, ReclaimMode::DeleteBranch)
        } else if exit.timed_out {
            (
                TaskState::Failed {
                    error: "任务级硬超时触发，进程组已终止（#9）".to_string(),
                },
                ReclaimMode::KeepBranch,
            )
        } else if let Some(message) = exit.fatal_error.clone().or_else(|| exit.turn_failed.clone())
        {
            (
                TaskState::Failed { error: message },
                ReclaimMode::KeepBranch,
            )
        } else if exit.exit_code != Some(0) {
            (
                TaskState::Failed {
                    error: format!("codex exec 退出码 {:?}", exit.exit_code),
                },
                ReclaimMode::KeepBranch,
            )
        } else {
            // 自报成功 → 判卷定生死（#5）。
            let judge = match (&judge_spec, &judge_cwd) {
                (Some(spec), Some(cwd)) => run_judge(spec, cwd).await,
                _ => JudgeOutcome::NotJudged,
            };
            (TaskState::Completed { judge }, ReclaimMode::KeepBranch)
        };

        // diff_first=false 的任务无需保留审查分支。
        let diff_first = {
            let state = self.lock_state();
            state
                .tasks
                .get(&task_id)
                .map(|t| t.spec.diff_first)
                .unwrap_or(true)
        };
        let reclaim_mode = if reclaim_mode == ReclaimMode::KeepBranch && !diff_first {
            ReclaimMode::DeleteBranch
        } else {
            reclaim_mode
        };

        self.finalize(
            task_id,
            session_id,
            final_state,
            exit.usage.clone(),
            exit.last_agent_message.clone(),
            worktree_handle,
            reclaim_mode,
        )
        .await;
    }

    #[allow(clippy::too_many_arguments)]
    async fn finalize(
        &self,
        task_id: TaskId,
        session_id: SessionId,
        final_state: TaskState,
        usage: Option<codex_exec::Usage>,
        last_agent_message: Option<String>,
        worktree_handle: Option<WorktreeHandle>,
        reclaim_mode: ReclaimMode,
    ) {
        {
            let mut state = self.lock_state();
            state.running.remove(&task_id);
            if let Some(task) = state.tasks.get_mut(&task_id) {
                task.usage = usage;
                task.last_agent_message =
                    last_agent_message.map(|m| truncate_chars(&m, MESSAGE_ARCHIVE_CHARS));
                if let Err(err) = task.transition(final_state) {
                    tracing::error!("task {task_id} final transition rejected: {err}");
                }
                let _ = task.save(&self.home);
            }
        }
        self.release_session(session_id, task_id);
        if let Some(handle) = worktree_handle {
            reclaim_blocking(handle, reclaim_mode).await;
        }
    }

    fn release_session(&self, session_id: SessionId, task_id: TaskId) {
        let mut state = self.lock_state();
        if let Some(session) = state.sessions.get_mut(&session_id) {
            match session.release(task_id) {
                Ok(()) => {
                    let _ = session.save(&self.home);
                }
                Err(err) => tracing::warn!("session release: {err}"),
            }
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, State> {
        match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

fn view_of(record: &TaskRecord) -> TaskStatusView {
    let state = match &record.state {
        TaskState::Pending => "pending".to_string(),
        TaskState::Running { pid } => format!("running (pid {pid:?})"),
        TaskState::Completed { judge } => match judge {
            JudgeOutcome::Passed => "completed · judge passed".to_string(),
            JudgeOutcome::Failed { exit_code } => {
                format!("completed · judge FAILED (exit {exit_code})")
            }
            JudgeOutcome::NotJudged => "completed · not judged".to_string(),
        },
        TaskState::Failed { error } => format!("failed: {}", truncate_chars(error, 160)),
        TaskState::Interrupted => "interrupted".to_string(),
    };
    TaskStatusView {
        task_id: record.id.to_string(),
        state,
        model_key: record.spec.model_key.clone(),
        session_id: record.session_id.map(|s| s.to_string()),
        worktree_branch: record
            .worktree_path
            .as_ref()
            .map(|_| format!("rdos/task/{}", record.id)),
        prompt_snippet: truncate_chars(&record.spec.prompt, 160),
        last_agent_message: record
            .last_agent_message
            .as_ref()
            .map(|m| truncate_chars(m, 200)),
        output_tokens: record.usage.as_ref().map(|u| u.output_tokens),
        created_at: record.created_at.to_rfc3339(),
        updated_at: record.updated_at.to_rfc3339(),
    }
}

/// 判卷：`sh -c` + pipefail + 硬超时 + 进程组（#9 硬化三件套）。
async fn run_judge(spec: &JudgeSpec, cwd: &std::path::Path) -> JudgeOutcome {
    let mut command = tokio::process::Command::new("/bin/sh");
    command
        .arg("-c")
        .arg(format!("set -o pipefail; {}", spec.command))
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => return JudgeOutcome::Failed { exit_code: 127 },
    };
    match tokio::time::timeout(JUDGE_TIMEOUT, child.wait()).await {
        Ok(Ok(status)) if status.success() => JudgeOutcome::Passed,
        Ok(Ok(status)) => JudgeOutcome::Failed {
            exit_code: status.code().unwrap_or(-1),
        },
        Ok(Err(_)) => JudgeOutcome::Failed { exit_code: -1 },
        Err(_) => {
            // 超时：杀进程组，按 timeout(1) 惯例记 124。
            #[cfg(unix)]
            if let Some(pid) = child.id() {
                unsafe {
                    libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
                }
            }
            let _ = child.kill().await;
            JudgeOutcome::Failed { exit_code: 124 }
        }
    }
}

async fn reclaim_blocking(handle: WorktreeHandle, mode: ReclaimMode) {
    let result = tokio::task::spawn_blocking(move || worktree::reclaim(&handle, mode)).await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(err)) => tracing::warn!("worktree reclaim failed: {err}"),
        Err(err) => tracing::warn!("worktree reclaim task panicked: {err}"),
    }
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (count, ch) in text.chars().enumerate() {
        if count >= max_chars {
            out.push('…');
            return out;
        }
        out.push(ch);
    }
    out
}

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error(transparent)]
    Lock(#[from] LockError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Registry(#[from] RegistryError),
    #[error(transparent)]
    Ownership(#[from] OwnershipError),
    #[error(transparent)]
    Transition(#[from] TransitionError),
    #[error(transparent)]
    Worktree(#[from] crate::worktree::WorktreeError),
    #[error(transparent)]
    Exec(#[from] ExecError),
    #[error("session {session} not found")]
    SessionNotFound { session: SessionId },
    #[error("session {session} has no resumable rollout (or model affinity is unresolvable)")]
    SessionNotResumable { session: SessionId },
    #[error("task {task} not found")]
    TaskNotFound { task: TaskId },
    #[error("task {task} is not running")]
    TaskNotRunning { task: TaskId },
    #[error("{context}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{message}")]
    Internal { message: String },
}
