//! 任务注册表与生命周期状态机（#1；P1-2/P1-3）。
//!
//! - `spawn_task` 立即返回 [`TaskId`]；进度经 `status`/`tail` 拉取、结果推送通知。
//!   实证：MCP 阻塞调用 60s 上限四重钉死，同步模型对本地模型的真实任务不可用。
//! - [`TaskRecord`] 逐任务落盘（JSON），跨 bridge 进程重启可恢复。
//! - 状态机由 [`TaskRecord::transition`] 强制：非法迁移是 bug，直接报错。

use std::path::PathBuf;

use chrono::DateTime;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;
use uuid::Uuid;

use crate::paths::LoadReport;
use crate::paths::StoreError;
use crate::paths::SupervisorHome;
use crate::session::ApprovalPolicy;
use crate::session::SandboxMode;
use crate::session::SessionId;

pub const TASK_SCHEMA_VERSION: u32 = 1;

/// 任务句柄：`spawn_task` 的即时返回值，跨进程稳定。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TaskId(Uuid);

impl TaskId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for TaskId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for TaskId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(Uuid::parse_str(s)?))
    }
}

/// 机器判卷结果（#5）：模型自我汇报零可信（假完成 ×4 实测），
/// 完成态必须携带判卷命令的客观结论。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum JudgeOutcome {
    /// 判卷命令 exit 0。
    Passed,
    /// 判卷命令非零退出（退出码随身）。
    Failed { exit_code: i32 },
    /// 任务未注册判卷命令（如只读分析任务）——语义上「无卷可判」，
    /// 供北向如实呈现，绝不折算成 Passed。
    NotJudged,
}

/// 任务生命周期。合法迁移见 [`TaskState::can_transition_to`]：
///
/// ```text
/// Pending ──→ Running ──→ Completed{judge}
///    │           ├──────→ Failed{error}
///    │           └──────→ Interrupted
///    ├──────────────────→ Failed（起跑前失败：preflight/worktree/spawn）
///    └──────────────────→ Interrupted（起跑前取消）
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum TaskState {
    Pending,
    Running {
        /// 子进程 pid（interrupt 杀进程用；进程消亡后仅存档案价值）。
        #[serde(skip_serializing_if = "Option::is_none")]
        pid: Option<u32>,
    },
    Completed {
        judge: JudgeOutcome,
    },
    Failed {
        error: String,
    },
    Interrupted,
}

impl TaskState {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            TaskState::Completed { .. } | TaskState::Failed { .. } | TaskState::Interrupted
        )
    }

    pub fn can_transition_to(&self, next: &TaskState) -> bool {
        match (self, next) {
            (TaskState::Pending, TaskState::Running { .. }) => true,
            (TaskState::Pending, TaskState::Failed { .. }) => true,
            (TaskState::Pending, TaskState::Interrupted) => true,
            (TaskState::Running { .. }, TaskState::Completed { .. }) => true,
            (TaskState::Running { .. }, TaskState::Failed { .. }) => true,
            (TaskState::Running { .. }, TaskState::Interrupted) => true,
            // Running → Running 允许（补记 pid）；其余原地迁移与终态外逸全部非法。
            (TaskState::Running { .. }, TaskState::Running { .. }) => true,
            _ => false,
        }
    }
}

/// worktree 策略（Phase0 §10 文件系统硬约束）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorktreePolicy {
    /// 缺省：spawn 时分配独立 git worktree，结束合并/丢弃。
    Isolated,
    /// 直接在目标 cwd 执行（只读任务/非 git 目录）。
    InPlace,
}

/// 会话路由：新会话 or 续接既有会话（`reply` 语义，T4）。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum SessionTarget {
    New,
    Resume { session: SessionId },
}

/// 判卷规格（#5）：随任务注册的验收命令，以退出码为准。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JudgeSpec {
    /// shell 命令（在任务 worktree 内执行；P1-3 执行时按 #9 硬化）。
    pub command: String,
}

/// 任务规格：spawn_task 的完整输入，落盘后即任务的「出生证明」。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskSpec {
    pub prompt: String,
    /// 模型登记表键（#12 路由在 spawn 时经登记表解析为 model+provider）。
    pub model_key: String,
    /// 项目根目录（Isolated 策略下是 worktree 的母本）。
    pub cwd: PathBuf,
    pub sandbox: SandboxMode,
    pub approval: ApprovalPolicy,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub judge: Option<JudgeSpec>,
    /// #8：写任务默认产出 diff、审后落盘。
    pub diff_first: bool,
    pub worktree: WorktreePolicy,
    pub session: SessionTarget,
    /// 任务级硬超时（#9：沙箱禁网静默丢包实测，无超时=吊死）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// 熔断阈值：连续同签名工具失败 N 次自动中止（#6；缺省见 service 层常量。
    /// 实测依据：apply_patch 328 连败烧 36 分钟无熔断）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub breaker_threshold: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskRecord {
    #[serde(default)]
    pub schema_version: u32,
    pub id: TaskId,
    pub spec: TaskSpec,
    pub state: TaskState,
    /// spawn 后指向承载会话（Resume 时即目标会话）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<PathBuf>,
    /// 原始 JSONL 事件流落盘路径（#15：北向只给摘要，原始流留档可审计）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub events_path: Option<PathBuf>,
    /// 南向自动重试计数（#7）。
    pub retries: u32,
    /// 终局 token 用量（最后一个 `turn.completed`；北向 status 直读此处）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<codex_exec::Usage>,
    /// 最后一条 agent 文本回复（截断存档；全文在 events 原始流里）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_agent_message: Option<String>,
    /// 任务分支的 diff --stat 摘要（#8：diff-first 落盘后由 finalize 记录，
    /// 供 supervisor 审查决策；全量 diff 用 `git diff main...rdos/task/<id>`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff_stat: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl TaskRecord {
    pub fn new(spec: TaskSpec) -> Self {
        let now = Utc::now();
        Self {
            schema_version: TASK_SCHEMA_VERSION,
            id: TaskId::new(),
            spec,
            state: TaskState::Pending,
            session_id: None,
            worktree_path: None,
            events_path: None,
            retries: 0,
            usage: None,
            last_agent_message: None,
            diff_stat: None,
            created_at: now,
            updated_at: now,
        }
    }

    /// 状态迁移（校验合法性 + 刷新 updated_at）。
    pub fn transition(&mut self, next: TaskState) -> Result<(), TransitionError> {
        if !self.state.can_transition_to(&next) {
            return Err(TransitionError {
                task: self.id,
                from: self.state.clone(),
                to: next,
            });
        }
        self.state = next;
        self.updated_at = Utc::now();
        Ok(())
    }

    pub fn save(&self, home: &SupervisorHome) -> Result<(), StoreError> {
        let path = home.tasks_dir().join(format!("{}.json", self.id));
        crate::paths::write_json_atomic(&path, self)
    }

    pub fn load_all(home: &SupervisorHome) -> Result<LoadReport<TaskRecord>, StoreError> {
        crate::paths::load_dir(&home.tasks_dir())
    }
}

#[derive(Debug, thiserror::Error)]
#[error("illegal task state transition for {task}: {from:?} -> {to:?}")]
pub struct TransitionError {
    pub task: TaskId,
    pub from: TaskState,
    pub to: TaskState,
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use tempfile::TempDir;

    fn sample_spec() -> TaskSpec {
        TaskSpec {
            prompt: "修复 testbed 中失败的浅 bug 测试".to_string(),
            model_key: "qwen36-nvfp4".to_string(),
            cwd: PathBuf::from("/Users/kit/Workspace/RDOSCli/testbed"),
            sandbox: SandboxMode::WorkspaceWrite,
            approval: ApprovalPolicy::Never,
            judge: Some(JudgeSpec {
                command: "cargo test".to_string(),
            }),
            diff_first: true,
            worktree: WorktreePolicy::Isolated,
            session: SessionTarget::New,
            timeout_secs: Some(600),
            breaker_threshold: None,
        }
    }

    #[test]
    fn lifecycle_happy_path() {
        let mut task = TaskRecord::new(sample_spec());
        assert_eq!(task.state, TaskState::Pending);
        task.transition(TaskState::Running { pid: None }).expect("start");
        task.transition(TaskState::Running { pid: Some(4242) })
            .expect("record pid");
        task.transition(TaskState::Completed {
            judge: JudgeOutcome::Passed,
        })
        .expect("complete");
        assert!(task.state.is_terminal());
    }

    #[test]
    fn terminal_states_reject_further_transitions() {
        let mut task = TaskRecord::new(sample_spec());
        task.transition(TaskState::Interrupted).expect("cancel pending");
        let err = task
            .transition(TaskState::Running { pid: None })
            .expect_err("terminal is terminal");
        assert_eq!(err.task, task.id);
    }

    #[test]
    fn pending_cannot_complete_without_running() {
        let mut task = TaskRecord::new(sample_spec());
        assert!(
            task.transition(TaskState::Completed {
                judge: JudgeOutcome::NotJudged,
            })
            .is_err(),
            "completion must pass through Running"
        );
    }

    #[test]
    fn task_roundtrip_via_disk() {
        let home = TempDir::new().expect("tempdir");
        let sup = SupervisorHome::new(home.path());
        sup.ensure_layout().expect("layout");

        let mut task = TaskRecord::new(sample_spec());
        task.transition(TaskState::Running { pid: Some(7) }).expect("run");
        task.save(&sup).expect("save");

        let report = TaskRecord::load_all(&sup).expect("load");
        assert!(report.skipped.is_empty());
        assert_eq!(report.records, vec![task]);
    }
}
