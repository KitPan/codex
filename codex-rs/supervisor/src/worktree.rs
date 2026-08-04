//! git worktree 分配与回收（Phase0 §10 文件系统硬约束；P1-3/P1-4）。
//!
//! - 多实例同时写同一 checkout 必丢工作（社区共识）；`spawn_task` 内置
//!   worktree 分配（任务开始建、结束合并/丢弃），interrupt 路径必须回收。
//! - worktree 落在 `<CODEX_HOME>/supervisor/worktrees/<task-id>`，分支名
//!   `rdos/task/<task-id>`——任务结束后即使 worktree 移除，产出仍可经分支引用
//!   （merge/discard 是 supervisor 的裁决，不在本层）。
//! - 全部 git 操作为同步短命令；async 调用方自行 `spawn_blocking`。

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use crate::paths::SupervisorHome;
use crate::registry::TaskId;

/// 一次分配的完整坐标。
#[derive(Debug, Clone, PartialEq)]
pub struct WorktreeHandle {
    /// 母本 repo 根（`git rev-parse --show-toplevel`）。
    pub repo_root: PathBuf,
    /// 新 worktree 根目录。
    pub worktree_root: PathBuf,
    /// 任务 cwd 在 worktree 内的对应点（spec.cwd 的相对位置平移）。
    pub task_cwd: PathBuf,
    /// 任务分支 `rdos/task/<id>`。
    pub branch: String,
    /// 分支出生点 sha（diff 基准，与 main 后续移动无关）。
    pub base: String,
}

/// 回收策略：分支去留（worktree 目录一律移除）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReclaimMode {
    /// 保留分支（任务产出待 supervisor 审查/合并——#8 diff-first 的承接点）。
    KeepBranch,
    /// 连分支一起删（丢弃产出：interrupt/失败清场）。
    DeleteBranch,
}

/// 为任务分配独立 worktree。
///
/// `spec_cwd` 是任务声明的工作目录（可为 repo 内任意子目录）；返回的
/// [`WorktreeHandle::task_cwd`] 已把它平移进新 worktree。
pub fn allocate(
    home: &SupervisorHome,
    task: TaskId,
    spec_cwd: &Path,
) -> Result<WorktreeHandle, WorktreeError> {
    let spec_cwd = spec_cwd
        .canonicalize()
        .map_err(|source| WorktreeError::BadCwd {
            path: spec_cwd.to_path_buf(),
            source,
        })?;
    let toplevel = run_git(&spec_cwd, &["rev-parse", "--show-toplevel"]).map_err(|e| match e {
        WorktreeError::GitFailed { stderr, .. } => WorktreeError::NotAGitRepo {
            path: spec_cwd.clone(),
            detail: stderr,
        },
        other => other,
    })?;
    let repo_root = PathBuf::from(toplevel.trim());
    let rel = spec_cwd
        .strip_prefix(&repo_root)
        .map_err(|_| WorktreeError::CwdOutsideRepo {
            cwd: spec_cwd.clone(),
            repo_root: repo_root.clone(),
        })?
        .to_path_buf();

    std::fs::create_dir_all(home.worktrees_dir()).map_err(|source| WorktreeError::BadCwd {
        path: home.worktrees_dir(),
        source,
    })?;
    let worktree_root = home.worktrees_dir().join(task.to_string());
    let branch = format!("rdos/task/{task}");
    let base = run_git(&repo_root, &["rev-parse", "HEAD"])?.trim().to_string();

    run_git(
        &repo_root,
        &[
            "worktree",
            "add",
            "-b",
            &branch,
            &worktree_root.to_string_lossy(),
            "HEAD",
        ],
    )?;

    Ok(WorktreeHandle {
        task_cwd: worktree_root.join(&rel),
        repo_root,
        worktree_root,
        branch,
        base,
    })
}

/// #8 diff-first 落盘：把 worktree 脏区自动 commit 到任务分支。
///
/// 模型改文件通常不 commit——不落盘就回收 worktree 等于把产出连同证据一起
/// 丢掉。返回是否发生了提交（干净工作区 = false）。
pub fn commit_dirty(handle: &WorktreeHandle, message: &str) -> Result<bool, WorktreeError> {
    let dirty = run_git(&handle.worktree_root, &["status", "--porcelain"])?;
    if dirty.trim().is_empty() {
        return Ok(false);
    }
    run_git(&handle.worktree_root, &["add", "-A"])?;
    run_git(
        &handle.worktree_root,
        &[
            "-c",
            "user.name=rdos-supervisor",
            "-c",
            "user.email=supervisor@rdos.local",
            "commit",
            "-q",
            "-m",
            message,
        ],
    )?;
    Ok(true)
}

/// 任务分支相对出生点的 `diff --stat` 摘要（无差异 = None）。
pub fn diff_stat(handle: &WorktreeHandle) -> Result<Option<String>, WorktreeError> {
    let stat = run_git(
        &handle.repo_root,
        &["diff", "--stat", &handle.base, &handle.branch],
    )?;
    let stat = stat.trim();
    if stat.is_empty() {
        Ok(None)
    } else {
        Ok(Some(stat.to_string()))
    }
}

/// 回收 worktree（目录移除；分支按 [`ReclaimMode`] 处置）。
pub fn reclaim(handle: &WorktreeHandle, mode: ReclaimMode) -> Result<(), WorktreeError> {
    run_git(
        &handle.repo_root,
        &[
            "worktree",
            "remove",
            "--force",
            &handle.worktree_root.to_string_lossy(),
        ],
    )?;
    if mode == ReclaimMode::DeleteBranch {
        run_git(&handle.repo_root, &["branch", "-D", &handle.branch])?;
    }
    Ok(())
}

fn run_git(cwd: &Path, args: &[&str]) -> Result<String, WorktreeError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .map_err(|source| WorktreeError::GitSpawn { source })?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(WorktreeError::GitFailed {
            args: args.iter().map(|s| (*s).to_string()).collect(),
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WorktreeError {
    #[error("task cwd {path} is not usable")]
    BadCwd {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} is not inside a git repository: {detail}")]
    NotAGitRepo { path: PathBuf, detail: String },
    #[error("task cwd {cwd} lies outside repo root {repo_root}")]
    CwdOutsideRepo { cwd: PathBuf, repo_root: PathBuf },
    #[error("failed to spawn git")]
    GitSpawn {
        #[source]
        source: std::io::Error,
    },
    #[error("git {args:?} failed (code {code:?}): {stderr}")]
    GitFailed {
        args: Vec<String>,
        code: Option<i32>,
        stderr: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use tempfile::TempDir;

    fn init_repo() -> TempDir {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path();
        run_git(root, &["init", "-q", "-b", "main"]).expect("git init");
        std::fs::create_dir_all(root.join("proj/src")).expect("mkdir");
        std::fs::write(root.join("proj/src/lib.rs"), "// hi\n").expect("write");
        run_git(root, &["add", "."]).expect("add");
        run_git(
            root,
            &[
                "-c",
                "user.name=rdos-test",
                "-c",
                "user.email=rdos@test.local",
                "commit",
                "-q",
                "-m",
                "init",
            ],
        )
        .expect("commit");
        dir
    }

    #[test]
    fn allocate_maps_nested_cwd_and_reclaim_deletes() {
        let repo = init_repo();
        let home_dir = TempDir::new().expect("home");
        let home = SupervisorHome::new(home_dir.path());
        let task = TaskId::new();

        let handle = allocate(&home, task, &repo.path().join("proj")).expect("allocate");
        assert!(handle.worktree_root.is_dir());
        assert!(handle.task_cwd.ends_with("proj"));
        assert!(handle.task_cwd.join("src/lib.rs").is_file());
        assert_eq!(handle.branch, format!("rdos/task/{task}"));

        reclaim(&handle, ReclaimMode::DeleteBranch).expect("reclaim");
        assert!(!handle.worktree_root.exists());
        let branches =
            run_git(&handle.repo_root, &["branch", "--list", &handle.branch]).expect("list");
        assert_eq!(branches.trim(), "");
    }

    #[test]
    fn parallel_worktrees_do_not_collide() {
        let repo = init_repo();
        let home_dir = TempDir::new().expect("home");
        let home = SupervisorHome::new(home_dir.path());

        let a = allocate(&home, TaskId::new(), repo.path()).expect("a");
        let b = allocate(&home, TaskId::new(), repo.path()).expect("b");
        assert_ne!(a.worktree_root, b.worktree_root);
        assert!(a.worktree_root.is_dir() && b.worktree_root.is_dir());

        // 各自写各自的文件，互不可见（并行隔离的最小证明）。
        std::fs::write(a.worktree_root.join("only-a.txt"), "a").expect("write a");
        assert!(!b.worktree_root.join("only-a.txt").exists());

        reclaim(&a, ReclaimMode::DeleteBranch).expect("reclaim a");
        reclaim(&b, ReclaimMode::KeepBranch).expect("reclaim b");
        let branches = run_git(&b.repo_root, &["branch", "--list", &b.branch]).expect("list");
        assert!(branches.contains(&b.branch), "KeepBranch must preserve branch");
    }

    #[test]
    fn dirty_worktree_survives_reclaim_via_commit() {
        let repo = init_repo();
        let home_dir = TempDir::new().expect("home");
        let home = SupervisorHome::new(home_dir.path());
        let handle = allocate(&home, TaskId::new(), repo.path()).expect("allocate");

        // 干净区不产生提交。
        assert!(!commit_dirty(&handle, "noop").expect("clean check"));
        assert_eq!(diff_stat(&handle).expect("no diff"), None);

        // 模型式改动：改一个、加一个，不 commit。
        std::fs::write(handle.worktree_root.join("proj/src/lib.rs"), "// changed\n")
            .expect("modify");
        std::fs::write(handle.worktree_root.join("proj/new.rs"), "// new\n").expect("add");
        assert!(commit_dirty(&handle, "task output").expect("commit"));

        let stat = diff_stat(&handle).expect("stat").expect("some diff");
        assert!(stat.contains("proj/src/lib.rs"), "stat: {stat}");
        assert!(stat.contains("2 files changed"), "stat: {stat}");

        // 回收目录、保留分支后，产出仍可经分支取回。
        reclaim(&handle, ReclaimMode::KeepBranch).expect("reclaim");
        let show = run_git(
            &handle.repo_root,
            &["show", &format!("{}:proj/new.rs", handle.branch)],
        )
        .expect("show");
        assert_eq!(show, "// new\n");
    }

    #[test]
    fn non_git_dir_is_rejected() {
        let plain = TempDir::new().expect("tempdir");
        let home_dir = TempDir::new().expect("home");
        let home = SupervisorHome::new(home_dir.path());
        let err = allocate(&home, TaskId::new(), plain.path()).expect_err("must fail");
        assert!(matches!(err, WorktreeError::NotAGitRepo { .. }));
    }
}
