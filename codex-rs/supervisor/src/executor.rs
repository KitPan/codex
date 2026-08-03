//! 南向执行器：per-task spawn `codex exec --json` 子进程（#7 #9 #17；P1-3）。
//!
//! - JSONL 事件流以**编译期类型**解析（直接依赖 `codex-exec` 的 [`ThreadEvent`]）：
//!   协议漂移由编译器兜住，这是薄 fork 的核心红利。
//! - 原始事件流逐行落盘（`<events>.jsonl` + `<events>.stderr.log`）——#15 北向
//!   只给摘要，原始流留档可审计。
//! - 生命周期三路裁决：自然退出 / `interrupt`（杀进程）/ 任务级硬超时（#9：
//!   沙箱禁网静默丢包实测，无超时=吊死）。中断与超时是**事实不是错误**，
//!   随 [`TaskExit`] 如实上报，状态裁决归调用方。
//! - 南向重试 + preflight（#7）在 P1-5 接线；本层暴露错误分类作为接缝。

use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use codex_exec::ThreadEvent;
use codex_exec::ThreadItemDetails;
use codex_exec::Usage;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncRead;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::process::Child;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::registry::TaskId;
use crate::session::SandboxMode;

/// 执行器静态配置：spawn 哪个二进制、用哪个 CODEX_HOME。
///
/// CODEX_HOME **始终显式注入**子进程环境——隔离约定不依赖 fork 的默认值。
#[derive(Debug, Clone)]
pub struct ExecutorConfig {
    pub codex_bin: PathBuf,
    pub codex_home: PathBuf,
}

/// 一次 `codex exec` 调用的完整发射计划（由分派层组装，本层只负责机械执行）。
#[derive(Debug, Clone, PartialEq)]
pub struct LaunchPlan {
    pub prompt: String,
    /// `-m` 传值（服务端 model 名）；None = 用 base config 缺省（仅测试场景，
    /// 生产分派一律显式——#12）。
    pub model: Option<String>,
    pub sandbox: SandboxMode,
    /// 子进程工作目录（Isolated 任务 = worktree 内的 task_cwd）。
    pub cwd: PathBuf,
    /// `-c key=value` 覆写序列（model_provider、嵌套 provider 定义、reasoning 档
    /// 等全部经此通道——#12）。
    pub overrides: Vec<(String, String)>,
    /// Some(conversation_id) = `exec resume <id>` 续接（reply 语义，#3 模型亲和
    /// 由调用方一并注入 overrides/model，杜绝 resume 回落缺省的语义差）。
    pub resume: Option<String>,
    /// 非 git 目录的 InPlace 任务需要（worktree 任务天然在 git repo 内）。
    pub skip_git_repo_check: bool,
    /// 任务级硬超时（#9）。
    pub timeout: Option<Duration>,
}

impl LaunchPlan {
    /// 组装 `codex` 的 argv（不含 argv0）。
    pub fn to_args(&self) -> Vec<String> {
        let mut args = vec!["exec".to_string(), "--json".to_string()];
        if self.skip_git_repo_check {
            args.push("--skip-git-repo-check".to_string());
        }
        if let Some(model) = &self.model {
            args.push("-m".to_string());
            args.push(model.clone());
        }
        args.push("-s".to_string());
        args.push(self.sandbox.cli_name().to_string());
        for (key, value) in &self.overrides {
            args.push("-c".to_string());
            args.push(format!("{key}={value}"));
        }
        match &self.resume {
            Some(conversation_id) => {
                args.push("resume".to_string());
                args.push("--".to_string());
                args.push(conversation_id.clone());
                args.push(self.prompt.clone());
            }
            None => {
                args.push("--".to_string());
                args.push(self.prompt.clone());
            }
        }
        args
    }
}

/// 北向内部事件流（P1-4 的 observe/notify 源；接收方掉线不影响落盘）。
#[derive(Debug)]
pub enum ExecutorEvent {
    /// 一行合法 JSONL → 编译期类型。
    Parsed(ThreadEvent),
    /// 无法解析的 stdout 行（假完成/工具调用吐进文本通道的实测形态——
    /// 保留原文，这本身就是诊断信号）。
    ParseError { line: String, error: String },
    /// stderr 行（南向网络故障 "stream disconnected" 等在此现身）。
    StderrLine(String),
}

/// 子进程终局事实（裁决材料，不做裁决）。
#[derive(Debug, Default, Clone, PartialEq)]
pub struct TaskExit {
    pub exit_code: Option<i32>,
    pub interrupted: bool,
    pub timed_out: bool,
    /// `thread.started` 携带的 conversation id（#2 映射写回 SessionRecord）。
    pub thread_id: Option<String>,
    /// 最后一个 `turn.completed` 的用量。
    pub usage: Option<Usage>,
    /// 最后一条 agent 文本回复。
    pub last_agent_message: Option<String>,
    /// `turn.failed` 的错误信息。
    pub turn_failed: Option<String>,
    /// 流级 `error` 事件的错误信息。
    pub fatal_error: Option<String>,
}

/// interrupt 触发器（一次性）。
#[derive(Debug)]
pub struct InterruptHandle {
    tx: oneshot::Sender<()>,
}

impl InterruptHandle {
    pub fn trigger(self) {
        let _ = self.tx.send(());
    }
}

/// 运行中的任务句柄。
#[derive(Debug)]
pub struct RunningTask {
    pub task: TaskId,
    /// 子进程 pid（写进 TaskState::Running 供档案与诊断）。
    pub pid: Option<u32>,
    /// 事件流（bounded；接收方须及时消费，掉线不影响原始流落盘）。
    pub events: mpsc::Receiver<ExecutorEvent>,
    /// interrupt 触发器（take 后归调用方持有）。
    pub interrupt: Option<InterruptHandle>,
    /// 终局 join：TaskExit 事实或执行器错误。
    pub join: JoinHandle<Result<TaskExit, ExecError>>,
}

#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error("failed to spawn {bin}")]
    Spawn {
        bin: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to open events file {path}")]
    EventsFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed waiting for child process")]
    Wait {
        #[source]
        source: std::io::Error,
    },
    #[error("executor internal failure: {message}")]
    Internal { message: String },
}

/// 发射一个任务子进程。
///
/// 立即返回句柄（#1 异步模型的执行面）；事件经 `events` 流出，原始行写入
/// `events_path`（stderr 写同名 `.stderr.log`）。
pub async fn launch(
    config: &ExecutorConfig,
    task: TaskId,
    plan: &LaunchPlan,
    events_path: &Path,
) -> Result<RunningTask, ExecError> {
    if let Some(parent) = events_path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| ExecError::EventsFile {
            path: events_path.to_path_buf(),
            source,
        })?;
    }
    let stdout_file = tokio::fs::File::create(events_path)
        .await
        .map_err(|source| ExecError::EventsFile {
            path: events_path.to_path_buf(),
            source,
        })?;
    let stderr_path = events_path.with_extension("stderr.log");
    let stderr_file = tokio::fs::File::create(&stderr_path)
        .await
        .map_err(|source| ExecError::EventsFile {
            path: stderr_path,
            source,
        })?;

    let mut command = tokio::process::Command::new(&config.codex_bin);
    command
        .args(plan.to_args())
        .env("CODEX_HOME", &config.codex_home)
        .current_dir(&plan.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // 任务自成进程组：interrupt/timeout 必须杀整棵进程树——codex 会 spawn
    // shell/cargo 等子进程，只杀 codex 本体会留下孤儿继续烧机器（并占住
    // stdout 管道写端，使 EOF 永不到来）。
    #[cfg(unix)]
    command.process_group(0);

    let mut child = command.spawn().map_err(|source| ExecError::Spawn {
        bin: config.codex_bin.clone(),
        source,
    })?;
    let pid = child.id();

    let stdout = child.stdout.take().ok_or(ExecError::Internal {
        message: "child stdout pipe missing".to_string(),
    })?;
    let stderr = child.stderr.take().ok_or(ExecError::Internal {
        message: "child stderr pipe missing".to_string(),
    })?;

    let (event_tx, event_rx) = mpsc::channel::<ExecutorEvent>(256);
    let (interrupt_tx, interrupt_rx) = oneshot::channel::<()>();

    // 摘要共享折叠：pump 被宽限超时 abort 时，已折叠的事实（thread_id 等
    // #2 映射材料）不随之丢失。
    let summary = Arc::new(Mutex::new(StreamSummary::default()));
    let mut stdout_pump = tokio::spawn(pump_stdout(
        stdout,
        stdout_file,
        event_tx.clone(),
        Arc::clone(&summary),
    ));
    let mut stderr_pump = tokio::spawn(pump_stderr(stderr, stderr_file, event_tx));
    let timeout = plan.timeout;

    let join = tokio::spawn(async move {
        let (interrupted, timed_out) = supervise_child(&mut child, interrupt_rx, timeout).await?;
        let status = child.wait().await.map_err(|source| ExecError::Wait { source })?;

        // 收尾宽限：正常路径 EOF 即达；若有逃逸出进程组的残余进程占住管道，
        // 宽限到点 abort pump——原始流该落盘的已逐行落盘。
        if tokio::time::timeout(PUMP_GRACE, &mut stdout_pump)
            .await
            .is_err()
        {
            stdout_pump.abort();
        }
        if tokio::time::timeout(PUMP_GRACE, &mut stderr_pump)
            .await
            .is_err()
        {
            stderr_pump.abort();
        }
        let summary = summary
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        Ok(TaskExit {
            exit_code: status.code(),
            interrupted,
            timed_out,
            thread_id: summary.thread_id,
            usage: summary.usage,
            last_agent_message: summary.last_agent_message,
            turn_failed: summary.turn_failed,
            fatal_error: summary.fatal_error,
        })
    });

    Ok(RunningTask {
        task,
        pid,
        events: event_rx,
        interrupt: Some(InterruptHandle { tx: interrupt_tx }),
        join,
    })
}

/// 三路裁决：返回 (interrupted, timed_out)。自然退出时不杀进程。
async fn supervise_child(
    child: &mut Child,
    mut interrupt_rx: oneshot::Receiver<()>,
    timeout: Option<Duration>,
) -> Result<(bool, bool), ExecError> {
    let timeout_fut = async {
        match timeout {
            Some(duration) => tokio::time::sleep(duration).await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::pin!(timeout_fut);

    let mut interrupt_armed = true;
    loop {
        tokio::select! {
            // tokio 的 Child 是 fused：wait() 可重复调用，exit status 不丢。
            status = child.wait() => {
                status.map_err(|source| ExecError::Wait { source })?;
                return Ok((false, false));
            }
            result = &mut interrupt_rx, if interrupt_armed => {
                match result {
                    Ok(()) => {
                        kill_task_tree(child);
                        return Ok((true, false));
                    }
                    // InterruptHandle 被丢弃 ≠ 中断——解除该臂继续等。
                    Err(_) => interrupt_armed = false,
                }
            }
            _ = &mut timeout_fut => {
                kill_task_tree(child);
                return Ok((false, true));
            }
        }
    }
}

/// 杀整个任务进程组（进程树），随后对直接子进程补一发兜底。
fn kill_task_tree(child: &mut Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // 负 pid = 按进程组投递；组由 spawn 时的 process_group(0) 建立。
        unsafe {
            libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
        }
    }
    let _ = child.start_kill();
}

/// stdout pump 折叠出的事实摘要。
#[derive(Debug, Default, Clone)]
struct StreamSummary {
    thread_id: Option<String>,
    usage: Option<Usage>,
    last_agent_message: Option<String>,
    turn_failed: Option<String>,
    fatal_error: Option<String>,
}

/// pump 收尾宽限：child 已死后残余管道持有者的最长等待。
const PUMP_GRACE: Duration = Duration::from_secs(5);

async fn pump_stdout(
    stdout: impl AsyncRead + Unpin,
    mut file: tokio::fs::File,
    tx: mpsc::Sender<ExecutorEvent>,
    summary: Arc<Mutex<StreamSummary>>,
) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        // 原始流先落盘（逐行 flush：tail 的新鲜度即监督的新鲜度）。
        let _ = file.write_all(line.as_bytes()).await;
        let _ = file.write_all(b"\n").await;
        let _ = file.flush().await;

        match serde_json::from_str::<ThreadEvent>(&line) {
            Ok(event) => {
                if let Ok(mut guard) = summary.lock() {
                    fold_event(&mut guard, &event);
                }
                let _ = tx.send(ExecutorEvent::Parsed(event)).await;
            }
            Err(error) => {
                let _ = tx
                    .send(ExecutorEvent::ParseError {
                        line,
                        error: error.to_string(),
                    })
                    .await;
            }
        }
    }
}

fn fold_event(summary: &mut StreamSummary, event: &ThreadEvent) {
    match event {
        ThreadEvent::ThreadStarted(started) => {
            summary.thread_id = Some(started.thread_id.clone());
        }
        ThreadEvent::TurnCompleted(completed) => {
            summary.usage = Some(completed.usage.clone());
        }
        ThreadEvent::TurnFailed(failed) => {
            summary.turn_failed = Some(failed.error.message.clone());
        }
        ThreadEvent::Error(error) => {
            summary.fatal_error = Some(error.message.clone());
        }
        ThreadEvent::ItemCompleted(item) => {
            if let ThreadItemDetails::AgentMessage(message) = &item.item.details {
                summary.last_agent_message = Some(message.text.clone());
            }
        }
        _ => {}
    }
}

async fn pump_stderr(
    stderr: impl AsyncRead + Unpin,
    mut file: tokio::fs::File,
    tx: mpsc::Sender<ExecutorEvent>,
) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let _ = file.write_all(line.as_bytes()).await;
        let _ = file.write_all(b"\n").await;
        let _ = file.flush().await;
        let _ = tx.send(ExecutorEvent::StderrLine(line)).await;
    }
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use crate::session::SandboxMode;
    use pretty_assertions::assert_eq;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    fn plan(prompt: &str, cwd: &Path) -> LaunchPlan {
        LaunchPlan {
            prompt: prompt.to_string(),
            model: None,
            sandbox: SandboxMode::ReadOnly,
            cwd: cwd.to_path_buf(),
            overrides: Vec::new(),
            resume: None,
            skip_git_repo_check: false,
            timeout: None,
        }
    }

    fn fake_codex(dir: &Path, script_body: &str) -> PathBuf {
        let path = dir.join("fake-codex.sh");
        let script = format!("#!/bin/sh\n{script_body}");
        std::fs::write(&path, script).expect("write script");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod");
        path
    }

    fn config(bin: PathBuf, home: &Path) -> ExecutorConfig {
        ExecutorConfig {
            codex_bin: bin,
            codex_home: home.to_path_buf(),
        }
    }

    #[test]
    fn to_args_new_session_shape() {
        let plan = LaunchPlan {
            prompt: "修 bug".to_string(),
            model: Some("qwen".to_string()),
            sandbox: SandboxMode::WorkspaceWrite,
            cwd: PathBuf::from("/x"),
            overrides: vec![("model_provider".to_string(), "rdos-vllm".to_string())],
            resume: None,
            skip_git_repo_check: true,
            timeout: None,
        };
        assert_eq!(
            plan.to_args(),
            vec![
                "exec",
                "--json",
                "--skip-git-repo-check",
                "-m",
                "qwen",
                "-s",
                "workspace-write",
                "-c",
                "model_provider=rdos-vllm",
                "--",
                "修 bug",
            ]
        );
    }

    #[test]
    fn to_args_resume_shape() {
        let mut p = plan("继续", Path::new("/x"));
        p.resume = Some("thread-1".to_string());
        let args = p.to_args();
        assert_eq!(
            args[args.len() - 4..],
            ["resume", "--", "thread-1", "继续"].map(str::to_string)
        );
    }

    #[tokio::test]
    async fn happy_path_collects_facts_and_persists_raw_stream() {
        let dir = TempDir::new().expect("tempdir");
        let bin = fake_codex(
            dir.path(),
            concat!(
                "echo '{\"type\":\"thread.started\",\"thread_id\":\"t-123\"}'\n",
                "echo '{\"type\":\"turn.started\"}'\n",
                "echo 'plain text leak'\n",
                "echo '{\"type\":\"item.completed\",\"item\":{\"id\":\"i1\",\"type\":\"agent_message\",\"text\":\"done\"}}'\n",
                "echo '{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":1,\"cached_input_tokens\":0,\"output_tokens\":2,\"reasoning_output_tokens\":0}}'\n",
                "echo 'to stderr' 1>&2\n",
                "exit 0\n",
            ),
        );
        let events_path = dir.path().join("events/task.jsonl");
        let mut running = launch(
            &config(bin, dir.path()),
            TaskId::new(),
            &plan("hi", dir.path()),
            &events_path,
        )
        .await
        .expect("launch");

        let exit = running.join.await.expect("join").expect("exit");
        assert_eq!(exit.exit_code, Some(0));
        assert!(!exit.interrupted && !exit.timed_out);
        assert_eq!(exit.thread_id.as_deref(), Some("t-123"));
        assert_eq!(exit.last_agent_message.as_deref(), Some("done"));
        assert_eq!(exit.usage.as_ref().expect("usage").output_tokens, 2);

        let mut parsed = 0;
        let mut parse_errors = 0;
        let mut stderr_lines = 0;
        while let Ok(event) = running.events.try_recv() {
            match event {
                ExecutorEvent::Parsed(_) => parsed += 1,
                ExecutorEvent::ParseError { .. } => parse_errors += 1,
                ExecutorEvent::StderrLine(_) => stderr_lines += 1,
            }
        }
        assert_eq!((parsed, parse_errors, stderr_lines), (4, 1, 1));

        let raw = std::fs::read_to_string(&events_path).expect("events file");
        assert_eq!(raw.lines().count(), 5, "raw stream keeps unparseable lines");
        let stderr_raw =
            std::fs::read_to_string(events_path.with_extension("stderr.log")).expect("stderr");
        assert_eq!(stderr_raw.trim(), "to stderr");
    }

    #[tokio::test]
    async fn child_env_and_cwd_are_injected() {
        let dir = TempDir::new().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("wt")).expect("mkdir");
        let cwd = dir.path().join("wt").canonicalize().expect("canon");
        let bin = fake_codex(
            dir.path(),
            "printf '{\"type\":\"thread.started\",\"thread_id\":\"%s|%s\"}\\n' \"$CODEX_HOME\" \"$PWD\"\n",
        );
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).expect("mkdir home");
        let mut running = launch(
            &config(bin, &home),
            TaskId::new(),
            &plan("hi", &cwd),
            &dir.path().join("ev.jsonl"),
        )
        .await
        .expect("launch");
        let exit = running.join.await.expect("join").expect("exit");
        let expected = format!("{}|{}", home.display(), cwd.display());
        assert_eq!(exit.thread_id.as_deref(), Some(expected.as_str()));
        while running.events.try_recv().is_ok() {}
    }

    #[tokio::test]
    async fn interrupt_kills_child_quickly() {
        let dir = TempDir::new().expect("tempdir");
        let bin = fake_codex(dir.path(), "sleep 30\n");
        let started = std::time::Instant::now();
        let mut running = launch(
            &config(bin, dir.path()),
            TaskId::new(),
            &plan("hi", dir.path()),
            &dir.path().join("ev.jsonl"),
        )
        .await
        .expect("launch");
        running.interrupt.take().expect("handle").trigger();
        let exit = running.join.await.expect("join").expect("exit");
        assert!(exit.interrupted);
        assert!(!exit.timed_out);
        assert!(exit.exit_code.is_none(), "SIGKILL leaves no exit code");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn timeout_fires_and_is_reported() {
        let dir = TempDir::new().expect("tempdir");
        let bin = fake_codex(dir.path(), "sleep 30\n");
        let mut p = plan("hi", dir.path());
        p.timeout = Some(Duration::from_millis(300));
        let started = std::time::Instant::now();
        let running = launch(
            &config(bin, dir.path()),
            TaskId::new(),
            &p,
            &dir.path().join("ev.jsonl"),
        )
        .await
        .expect("launch");
        let exit = running.join.await.expect("join").expect("exit");
        assert!(exit.timed_out);
        assert!(!exit.interrupted);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn dropping_interrupt_handle_does_not_kill_child() {
        let dir = TempDir::new().expect("tempdir");
        let bin = fake_codex(
            dir.path(),
            "sleep 0.2\necho '{\"type\":\"thread.started\",\"thread_id\":\"alive\"}'\n",
        );
        let mut running = launch(
            &config(bin, dir.path()),
            TaskId::new(),
            &plan("hi", dir.path()),
            &dir.path().join("ev.jsonl"),
        )
        .await
        .expect("launch");
        drop(running.interrupt.take());
        let exit = running.join.await.expect("join").expect("exit");
        assert_eq!(exit.exit_code, Some(0));
        assert!(!exit.interrupted);
        assert_eq!(exit.thread_id.as_deref(), Some("alive"));
    }
}
