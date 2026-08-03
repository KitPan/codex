//! supervisor 状态目录布局与单例锁（#2 #4 #17；P1-2）。
//!
//! 布局（一切位于 `CODEX_HOME` 之下，与 codex 自身配置分仓——codex 会运行时
//! 改写 `config.toml`，supervisor 状态绝不与其混放）：
//!
//! ```text
//! <CODEX_HOME>/
//! ├── supervisor.models.toml   ← 模型登记表（顶层 *.toml，随 repo 版本化）
//! └── supervisor/              ← 运行态（gitignore 排除）
//!     ├── supervisor.lock      ← bridge 单例锁（#4：会话路由必须单进程）
//!     ├── tasks/<uuid>.json    ← TaskRecord
//!     └── sessions/<uuid>.json ← SessionRecord
//! ```

use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

/// supervisor 状态目录句柄：只负责路径计算、目录创建与单例锁，
/// 不理解任何记录的内容。
#[derive(Debug, Clone)]
pub struct SupervisorHome {
    codex_home: PathBuf,
}

impl SupervisorHome {
    pub fn new(codex_home: impl Into<PathBuf>) -> Self {
        Self {
            codex_home: codex_home.into(),
        }
    }

    pub fn codex_home(&self) -> &Path {
        &self.codex_home
    }

    /// 运行态根目录 `<CODEX_HOME>/supervisor`。
    pub fn state_root(&self) -> PathBuf {
        self.codex_home.join("supervisor")
    }

    pub fn tasks_dir(&self) -> PathBuf {
        self.state_root().join("tasks")
    }

    pub fn sessions_dir(&self) -> PathBuf {
        self.state_root().join("sessions")
    }

    /// 模型登记表：放 CODEX_HOME 顶层（`*.toml` 白名单使其随 repo 版本化）。
    pub fn models_path(&self) -> PathBuf {
        self.codex_home.join("supervisor.models.toml")
    }

    fn lock_path(&self) -> PathBuf {
        self.state_root().join("supervisor.lock")
    }

    /// 创建全部运行态目录（幂等）。
    pub fn ensure_layout(&self) -> io::Result<()> {
        std::fs::create_dir_all(self.tasks_dir())?;
        std::fs::create_dir_all(self.sessions_dir())?;
        Ok(())
    }

    /// 获取 bridge 单例锁（advisory flock，进程退出自动释放）。
    ///
    /// #4 的第一道闸：attach/steering 必须经单一 bridge 进程路由，双 bridge
    /// 并存等于回到控制隧道实验的「文件级双写」状态。
    pub fn acquire_singleton_lock(&self) -> Result<SupervisorLock, LockError> {
        self.ensure_layout().map_err(LockError::Io)?;
        let path = self.lock_path();
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(LockError::Io)?;
        match file.try_lock() {
            Ok(()) => {
                // 诊断信息尽力而为：拿到锁后写入 pid，失败不影响锁本身。
                let pid = std::process::id();
                let _ = file.set_len(0);
                let _ = file.seek(SeekFrom::Start(0));
                let _ = writeln!(file, "pid={pid}");
                let _ = file.flush();
                Ok(SupervisorLock { _file: file, path })
            }
            Err(std::fs::TryLockError::WouldBlock) => Err(LockError::AlreadyRunning { path }),
            Err(std::fs::TryLockError::Error(err)) => Err(LockError::Io(err)),
        }
    }
}

/// 持有期间独占 bridge 身份；drop 即释放（flock 随 fd 关闭解除）。
#[derive(Debug)]
pub struct SupervisorLock {
    _file: File,
    path: PathBuf,
}

impl SupervisorLock {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("another supervisor instance holds the lock at {path}")]
    AlreadyRunning { path: PathBuf },
    #[error("failed to acquire supervisor lock")]
    Io(#[source] io::Error),
}

/// 记录持久化的公共错误类型（JSON 与 TOML 两条持久化路径共用）。
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("io error at {path}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to encode record for {path}")]
    Encode {
        path: PathBuf,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("failed to decode record at {path}")]
    Decode {
        path: PathBuf,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

/// 原子写 JSON（tempfile + rename，复用 codex-utils-path）。
pub(crate) fn write_json_atomic<T: serde::Serialize>(
    path: &Path,
    value: &T,
) -> Result<(), StoreError> {
    let json = serde_json::to_string_pretty(value).map_err(|e| StoreError::Encode {
        path: path.to_path_buf(),
        source: Box::new(e),
    })?;
    codex_utils_path::write_atomically(path, &json).map_err(|e| StoreError::Io {
        path: path.to_path_buf(),
        source: e,
    })
}

pub(crate) fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, StoreError> {
    let raw = std::fs::read_to_string(path).map_err(|e| StoreError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    serde_json::from_str(&raw).map_err(|e| StoreError::Decode {
        path: path.to_path_buf(),
        source: Box::new(e),
    })
}

/// 扫描目录读取全部 `.json` 记录。
///
/// 损坏文件**不中断启动**（跨进程恢复是 #2 的硬指标），但一个不漏地报告给
/// 调用方——静默丢弃会话等于重蹈 threadId 连坐失效的覆辙。
pub(crate) fn load_dir<T: serde::de::DeserializeOwned>(dir: &Path) -> Result<LoadReport<T>, StoreError> {
    let mut report = LoadReport {
        records: Vec::new(),
        skipped: Vec::new(),
    };
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(report),
        Err(e) => {
            return Err(StoreError::Io {
                path: dir.to_path_buf(),
                source: e,
            });
        }
    };
    for entry in entries {
        let entry = entry.map_err(|e| StoreError::Io {
            path: dir.to_path_buf(),
            source: e,
        })?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        match read_json::<T>(&path) {
            Ok(record) => report.records.push(record),
            Err(err) => report.skipped.push((path, err.to_string())),
        }
    }
    Ok(report)
}

/// 目录扫描结果：成功的记录 + 被跳过的损坏文件（路径与原因）。
#[derive(Debug)]
pub struct LoadReport<T> {
    pub records: Vec<T>,
    pub skipped: Vec<(PathBuf, String)>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use tempfile::TempDir;

    #[test]
    fn singleton_lock_conflicts_within_process() {
        let home = TempDir::new().expect("tempdir");
        let sup = SupervisorHome::new(home.path());
        let first = sup.acquire_singleton_lock().expect("first lock");
        let second = sup.acquire_singleton_lock();
        assert!(
            matches!(second, Err(LockError::AlreadyRunning { .. })),
            "second acquisition must fail while first is held"
        );
        drop(first);
        let third = sup.acquire_singleton_lock();
        assert!(third.is_ok(), "lock must be reacquirable after release");
    }

    #[test]
    fn load_dir_reports_corrupt_files_without_failing() {
        let home = TempDir::new().expect("tempdir");
        let sup = SupervisorHome::new(home.path());
        sup.ensure_layout().expect("layout");
        let dir = sup.tasks_dir();
        std::fs::write(dir.join("good.json"), "{\"x\": 1}").expect("write good");
        std::fs::write(dir.join("bad.json"), "not json").expect("write bad");
        std::fs::write(dir.join("ignored.txt"), "whatever").expect("write txt");

        #[derive(serde::Deserialize, Debug)]
        struct Row {
            x: u32,
        }
        let report = load_dir::<Row>(&dir).expect("load");
        assert_eq!(report.records.len(), 1);
        assert_eq!(report.records[0].x, 1);
        assert_eq!(report.skipped.len(), 1);
    }

    #[test]
    fn load_dir_on_missing_dir_is_empty() {
        let home = TempDir::new().expect("tempdir");
        let sup = SupervisorHome::new(home.path());
        let report = load_dir::<serde_json::Value>(&sup.sessions_dir()).expect("load");
        assert!(report.records.is_empty());
        assert!(report.skipped.is_empty());
    }
}
