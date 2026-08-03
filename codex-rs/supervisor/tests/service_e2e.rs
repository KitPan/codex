//! P1-4 编排层端到端测试：假 codex 二进制驱动完整任务生命周期。
//! 全程零网络零模型——判定一律走磁盘状态与 git 事实。

#![cfg(unix)]
#![allow(clippy::expect_used)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use codex_supervisor::models::EngineInfo;
use codex_supervisor::models::LatencyClass;
use codex_supervisor::models::ModelEntry;
use codex_supervisor::models::ModelRegistry;
use codex_supervisor::models::ProviderRef;
use codex_supervisor::models::Quantization;
use codex_supervisor::models::ThinkingDefault;
use codex_supervisor::models::WriteTaskSupport;
use codex_supervisor::paths::SupervisorHome;
use codex_supervisor::registry::SessionTarget;
use codex_supervisor::registry::TaskRecord;
use codex_supervisor::registry::TaskState;
use codex_supervisor::registry::WorktreePolicy;
use codex_supervisor::service::SpawnRequest;
use codex_supervisor::service::Supervisor;
use codex_supervisor::service::SupervisorConfig;
use codex_supervisor::service::TaskStatusView;
use codex_supervisor::session::ApprovalPolicy;
use codex_supervisor::session::ModelAffinity;
use codex_supervisor::session::SandboxMode;
use codex_supervisor::session::SessionOwner;
use codex_supervisor::session::SessionRecord;
use tempfile::TempDir;

fn run_git(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .expect("git spawn");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn init_repo() -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    run_git(root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("README.md"), "# testbed\n").expect("write");
    run_git(root, &["add", "."]);
    run_git(
        root,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "-m",
            "init",
        ],
    );
    dir
}

fn write_fake_codex(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join("fake-codex.sh");
    std::fs::write(&path, format!("#!/bin/sh\n{body}")).expect("write script");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    path
}

fn seed_registry(codex_home: &Path) {
    let home = SupervisorHome::new(codex_home);
    let mut registry = ModelRegistry::new();
    registry.models.insert(
        "fake".to_string(),
        ModelEntry {
            served_name: "fake-model".to_string(),
            thinking: ThinkingDefault::Off,
            latency: LatencyClass::Fast,
            write_tasks: WriteTaskSupport::Reliable,
            quant_damage: Vec::new(),
            family_traits: Vec::new(),
            endpoint_probe: None,
            escalation_note: None,
            notes: None,
            provider: ProviderRef {
                id: "rdos-test".to_string(),
                base_url: None,
                wire_api: None,
            },
            engine: EngineInfo {
                kind: "fake".to_string(),
                version: None,
            },
            quantization: Quantization {
                format: "none".to_string(),
                bits: None,
            },
            inject: None,
        },
    );
    registry.save(&home).expect("seed registry");
}

fn start_supervisor(codex_home: &Path, codex_bin: PathBuf) -> Arc<Supervisor> {
    let (supervisor, _report) = Supervisor::start(SupervisorConfig {
        codex_home: codex_home.to_path_buf(),
        codex_bin,
    })
    .expect("start supervisor");
    supervisor
}

async fn wait_for<F>(supervisor: &Arc<Supervisor>, task_id: &str, pred: F) -> TaskStatusView
where
    F: Fn(&TaskStatusView) -> bool,
{
    let task = task_id.parse().expect("task id");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let views = supervisor.status(Some(task)).expect("status");
        let view = views.first().expect("view").clone();
        if pred(&view) {
            return view;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for task condition; last state: {}",
            view.state
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn spawn_request(prompt: &str, repo: &Path) -> SpawnRequest {
    SpawnRequest {
        prompt: prompt.to_string(),
        model_key: "fake".to_string(),
        cwd: repo.to_path_buf(),
        sandbox: SandboxMode::WorkspaceWrite,
        judge_command: None,
        timeout_secs: Some(30),
        worktree: WorktreePolicy::Isolated,
        session: SessionTarget::New,
        diff_first: true,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_completes_judge_passes_and_cleans_up() {
    let repo = init_repo();
    let home_dir = TempDir::new().expect("home");
    seed_registry(home_dir.path());
    let bin = write_fake_codex(
        home_dir.path(),
        concat!(
            "echo '{\"type\":\"thread.started\",\"thread_id\":\"t-e2e\"}'\n",
            "echo '{\"type\":\"item.completed\",\"item\":{\"id\":\"i1\",\"type\":\"agent_message\",\"text\":\"done\"}}'\n",
            "echo '{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":3,\"cached_input_tokens\":0,\"output_tokens\":7,\"reasoning_output_tokens\":0}}'\n",
        ),
    );
    let supervisor = start_supervisor(home_dir.path(), bin);

    let mut req = spawn_request("修 bug", repo.path());
    req.judge_command = Some("true".to_string());
    let reply = supervisor.spawn_task(req).await.expect("spawn");

    let view = wait_for(&supervisor, &reply.task_id, |v| {
        v.state.starts_with("completed")
    })
    .await;
    assert_eq!(view.state, "completed · judge passed");
    assert_eq!(view.output_tokens, Some(7));
    assert_eq!(view.last_agent_message.as_deref(), Some("done"));

    // session↔rollout 映射已持久化，租约已释放（#2 #4）。
    let home = SupervisorHome::new(home_dir.path());
    let sessions = SessionRecord::load_all(&home).expect("sessions");
    assert!(sessions.skipped.is_empty());
    let session = &sessions.records[0];
    assert_eq!(session.rollout.conversation_id.as_deref(), Some("t-e2e"));
    assert_eq!(session.owner, SessionOwner::Free);

    // diff_first：worktree 目录已回收、审查分支保留（#8）。
    let branch = reply.worktree_branch.expect("branch");
    let listed = run_git(repo.path(), &["branch", "--list", &branch]);
    assert!(listed.contains(&branch), "review branch must survive");
    assert!(
        !home.worktrees_dir().join(&reply.task_id).exists(),
        "worktree dir must be reclaimed"
    );

    // 判卷失败路径：false → completed · judge FAILED。
    let mut req2 = spawn_request("再修", repo.path());
    req2.judge_command = Some("false".to_string());
    let reply2 = supervisor.spawn_task(req2).await.expect("spawn2");
    let view2 = wait_for(&supervisor, &reply2.task_id, |v| {
        v.state.starts_with("completed")
    })
    .await;
    assert!(view2.state.contains("judge FAILED"), "got: {}", view2.state);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_kills_and_discards_branch() {
    let repo = init_repo();
    let home_dir = TempDir::new().expect("home");
    seed_registry(home_dir.path());
    let bin = write_fake_codex(
        home_dir.path(),
        concat!(
            "echo '{\"type\":\"thread.started\",\"thread_id\":\"t-int\"}'\n",
            "sleep 30\n",
        ),
    );
    let supervisor = start_supervisor(home_dir.path(), bin);
    let reply = supervisor
        .spawn_task(spawn_request("长任务", repo.path()))
        .await
        .expect("spawn");

    wait_for(&supervisor, &reply.task_id, |v| v.state.starts_with("running")).await;
    supervisor
        .interrupt(reply.task_id.parse().expect("id"))
        .expect("interrupt");
    let view = wait_for(&supervisor, &reply.task_id, |v| v.state == "interrupted").await;
    assert_eq!(view.state, "interrupted");

    // 分支与 worktree 均清场（验收：interrupt 后状态干净）。
    let branch = reply.worktree_branch.expect("branch");
    let listed = run_git(repo.path(), &["branch", "--list", &branch]);
    assert_eq!(listed.trim(), "", "interrupted task branch must be deleted");

    // 会话租约释放。
    let home = SupervisorHome::new(home_dir.path());
    let sessions = SessionRecord::load_all(&home).expect("sessions");
    assert_eq!(sessions.records[0].owner, SessionOwner::Free);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_sweeps_stale_running_tasks() {
    let home_dir = TempDir::new().expect("home");
    seed_registry(home_dir.path());
    let home = SupervisorHome::new(home_dir.path());
    home.ensure_layout().expect("layout");

    // 手工制造上一世的残局：Running 任务 + 被它持有的会话。
    let mut task = TaskRecord::new(codex_supervisor::registry::TaskSpec {
        prompt: "遗留任务".to_string(),
        model_key: "fake".to_string(),
        cwd: home_dir.path().to_path_buf(),
        sandbox: SandboxMode::WorkspaceWrite,
        approval: ApprovalPolicy::Never,
        judge: None,
        diff_first: true,
        worktree: WorktreePolicy::InPlace,
        session: SessionTarget::New,
        timeout_secs: None,
    });
    let mut session = SessionRecord::new(
        ModelAffinity {
            model: "fake-model".to_string(),
            provider: ProviderRef {
                id: "rdos-test".to_string(),
                base_url: None,
                wire_api: None,
            },
            reasoning: None,
        },
        SandboxMode::WorkspaceWrite,
        ApprovalPolicy::Never,
        home_dir.path().to_path_buf(),
    );
    session.claim(task.id).expect("claim");
    task.session_id = Some(session.id);
    task.transition(TaskState::Running { pid: Some(999_999) })
        .expect("running");
    task.save(&home).expect("save task");
    session.save(&home).expect("save session");

    let (supervisor, report) = Supervisor::start(SupervisorConfig {
        codex_home: home_dir.path().to_path_buf(),
        codex_bin: PathBuf::from("/nonexistent"),
    })
    .expect("start");
    assert_eq!(report.tasks_loaded, 1);
    assert_eq!(report.sessions_loaded, 1);
    assert_eq!(report.swept_tasks, vec![task.id]);

    let views = supervisor.status(Some(task.id)).expect("status");
    assert!(views[0].state.starts_with("failed"), "got {}", views[0].state);
    let sessions = SessionRecord::load_all(&home).expect("sessions");
    assert_eq!(sessions.records[0].owner, SessionOwner::Free);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reply_resumes_with_conversation_and_model_affinity() {
    let repo = init_repo();
    let home_dir = TempDir::new().expect("home");
    seed_registry(home_dir.path());
    // 假 codex 把收到的完整 argv 落盘到 cwd 的 args.txt，再报 happy path。
    let bin = write_fake_codex(
        home_dir.path(),
        concat!(
            "printf '%s ' \"$@\" > args.txt\n",
            "echo '{\"type\":\"thread.started\",\"thread_id\":\"t-first\"}'\n",
            "echo '{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":1,\"cached_input_tokens\":0,\"output_tokens\":1,\"reasoning_output_tokens\":0}}'\n",
        ),
    );
    let supervisor = start_supervisor(home_dir.path(), bin);

    let first = supervisor
        .spawn_task(spawn_request("第一轮", repo.path()))
        .await
        .expect("spawn");
    wait_for(&supervisor, &first.task_id, |v| v.state.starts_with("completed")).await;

    let second = supervisor
        .reply(
            first.session_id.parse().expect("session id"),
            "改用迭代器风格".to_string(),
            None,
            None,
            None,
        )
        .await
        .expect("reply");
    wait_for(&supervisor, &second.task_id, |v| v.state.starts_with("completed")).await;

    // reply 是 InPlace：args.txt 落在原 repo；检验 resume + 模型亲和注入（#3）。
    let args = std::fs::read_to_string(repo.path().join("args.txt")).expect("args.txt");
    assert!(args.contains("resume"), "args: {args}");
    assert!(args.contains("t-first"), "args: {args}");
    assert!(args.contains("-m fake-model"), "args: {args}");
    assert!(args.contains("model_provider="), "args: {args}");
    assert!(args.contains("改用迭代器风格"), "args: {args}");
}
