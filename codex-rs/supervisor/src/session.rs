//! 会话自持与持久化（#2 #3 #4；P1-2）。
//!
//! - session↔rollout 映射由 bridge 自持并落盘：threadId 寿命 = mcp-server 进程
//!   寿命，不可作持久句柄（实测超时重连即全灭，rollout 在盘却不可达）。
//! - 会话状态全字段（#3）= 消息历史 + 模型亲和 + 沙箱策略 + cwd + 锁。
//!   其中**消息历史不复制**：rollout JSONL 是历史的唯一权威载体（codex 拥有写权），
//!   bridge 只持有指向它的 [`RolloutRef`]——复制历史会制造第二个真相源，正是
//!   控制隧道实验「磁盘一条线、两个进程两条脑」要禁绝的形态。
//! - 所有权与锁（#4）：一切续接/注入必须先 [`SessionRecord::claim`] 成为持有者；
//!   文件级旁路双写禁止。

use std::path::PathBuf;

use chrono::DateTime;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;
use uuid::Uuid;

use crate::models::ProviderRef;
use crate::paths::LoadReport;
use crate::paths::StoreError;
use crate::paths::SupervisorHome;
use crate::registry::TaskId;

pub const SESSION_SCHEMA_VERSION: u32 = 1;

/// bridge 自有的会话句柄：跨进程重启稳定（对照 threadId 的进程级寿命）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(Uuid);

impl SessionId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for SessionId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(Uuid::parse_str(s)?))
    }
}

/// 沙箱策略（serde 形态与 codex CLI 字符串一致，执行器直接透传）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxMode {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

impl SandboxMode {
    /// codex CLI `--sandbox` 的传值形态（与 serde 形态一致）。
    pub fn cli_name(self) -> &'static str {
        match self {
            SandboxMode::ReadOnly => "read-only",
            SandboxMode::WorkspaceWrite => "workspace-write",
            SandboxMode::DangerFullAccess => "danger-full-access",
        }
    }
}

/// 审批策略。v1 缺省 `Never` + 沙箱兜底（#13 审批路由挂账 Phase 2——
/// 实测 Claude Code 客户端不呈现 elicitation，审批请求会挂死 60s 窗口）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalPolicy {
    Untrusted,
    OnFailure,
    OnRequest,
    Never,
}

/// reasoning 档位（#11 分派三维之一；thinking 开使简单任务耗时 2× 实测）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReasoningEffort {
    Minimal,
    Low,
    Medium,
    High,
}

/// 模型亲和（#3）：`exec resume` 不继承原会话模型（实测回落 base 缺省），
/// 因此 bridge 必须持有并在每次续接时显式注入。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelAffinity {
    /// 服务端 model 名（`--model` / config.model 传值）。
    pub model: String,
    pub provider: ProviderRef,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningEffort>,
}

/// session↔rollout 映射（#2）。两字段均可空：rollout 文件**首轮提交才创建**
/// （lazy，实测），conversation id 亦在首轮事件流中才可得。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RolloutRef {
    /// codex 侧 conversation/thread id（`exec resume <id>` 用）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    /// rollout JSONL 的磁盘路径（观察面；写权归 codex）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rollout_path: Option<PathBuf>,
}

/// 会话所有权（#4）：同一时刻至多一个任务持有执行租约。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum SessionOwner {
    /// 空闲——可被 claim（reply/续接的前置状态）。
    Free,
    /// 被某任务独占执行中。
    Task { task: TaskId },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRecord {
    #[serde(default)]
    pub schema_version: u32,
    pub id: SessionId,
    pub rollout: RolloutRef,
    pub model: ModelAffinity,
    pub sandbox: SandboxMode,
    pub approval: ApprovalPolicy,
    pub cwd: PathBuf,
    pub owner: SessionOwner,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl SessionRecord {
    pub fn new(
        model: ModelAffinity,
        sandbox: SandboxMode,
        approval: ApprovalPolicy,
        cwd: PathBuf,
    ) -> Self {
        let now = Utc::now();
        Self {
            schema_version: SESSION_SCHEMA_VERSION,
            id: SessionId::new(),
            rollout: RolloutRef::default(),
            model,
            sandbox,
            approval,
            cwd,
            owner: SessionOwner::Free,
            created_at: now,
            updated_at: now,
        }
    }

    /// 任务申请执行租约（#4：租约唯一）。
    pub fn claim(&mut self, task: TaskId) -> Result<(), OwnershipError> {
        match self.owner {
            SessionOwner::Free => {
                self.owner = SessionOwner::Task { task };
                self.touch();
                Ok(())
            }
            SessionOwner::Task { task: holder } if holder == task => Ok(()),
            SessionOwner::Task { task: holder } => Err(OwnershipError::Held {
                session: self.id,
                holder,
                requester: task,
            }),
        }
    }

    /// 释放租约。只有持有者能释放——错误释放视为 bug，必须可见。
    pub fn release(&mut self, task: TaskId) -> Result<(), OwnershipError> {
        match self.owner {
            SessionOwner::Task { task: holder } if holder == task => {
                self.owner = SessionOwner::Free;
                self.touch();
                Ok(())
            }
            SessionOwner::Task { task: holder } => Err(OwnershipError::NotHolder {
                session: self.id,
                holder,
                requester: task,
            }),
            SessionOwner::Free => Err(OwnershipError::AlreadyFree {
                session: self.id,
                requester: task,
            }),
        }
    }

    pub fn touch(&mut self) {
        self.updated_at = Utc::now();
    }

    pub fn save(&self, home: &SupervisorHome) -> Result<(), StoreError> {
        let path = home.sessions_dir().join(format!("{}.json", self.id));
        crate::paths::write_json_atomic(&path, self)
    }

    pub fn load_all(home: &SupervisorHome) -> Result<LoadReport<SessionRecord>, StoreError> {
        crate::paths::load_dir(&home.sessions_dir())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OwnershipError {
    #[error("session {session} is held by task {holder}; task {requester} cannot claim it")]
    Held {
        session: SessionId,
        holder: TaskId,
        requester: TaskId,
    },
    #[error("session {session} is held by task {holder}; task {requester} cannot release it")]
    NotHolder {
        session: SessionId,
        holder: TaskId,
        requester: TaskId,
    },
    #[error("session {session} is already free; task {requester} released nothing")]
    AlreadyFree {
        session: SessionId,
        requester: TaskId,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use tempfile::TempDir;

    fn sample() -> SessionRecord {
        SessionRecord::new(
            ModelAffinity {
                model: "nvidia/Qwen3.6-35B-A3B-NVFP4".to_string(),
                provider: ProviderRef {
                    id: "rdos-vllm".to_string(),
                    base_url: Some("http://192.168.3.1:8000/v1".to_string()),
                    wire_api: Some("responses".to_string()),
                },
                reasoning: Some(ReasoningEffort::Low),
            },
            SandboxMode::WorkspaceWrite,
            ApprovalPolicy::Never,
            PathBuf::from("/tmp/worktree-a"),
        )
    }

    #[test]
    fn session_roundtrip_via_disk() {
        let home = TempDir::new().expect("tempdir");
        let sup = SupervisorHome::new(home.path());
        sup.ensure_layout().expect("layout");
        let mut record = sample();
        record.rollout.conversation_id = Some("019fc991-deadbeef".to_string());
        record.save(&sup).expect("save");

        let report = SessionRecord::load_all(&sup).expect("load");
        assert!(report.skipped.is_empty());
        assert_eq!(report.records, vec![record]);
    }

    #[test]
    fn lease_is_exclusive_and_idempotent_for_holder() {
        let mut record = sample();
        let a = TaskId::new();
        let b = TaskId::new();

        record.claim(a).expect("first claim");
        record.claim(a).expect("holder re-claim is idempotent");
        assert!(matches!(
            record.claim(b),
            Err(OwnershipError::Held { .. })
        ));

        assert!(matches!(
            record.release(b),
            Err(OwnershipError::NotHolder { .. })
        ));
        record.release(a).expect("holder release");
        assert_eq!(record.owner, SessionOwner::Free);
        assert!(matches!(
            record.release(a),
            Err(OwnershipError::AlreadyFree { .. })
        ));
    }

    #[test]
    fn sandbox_and_approval_serialize_as_codex_cli_strings() {
        let json = serde_json::to_string(&SandboxMode::WorkspaceWrite).expect("encode");
        assert_eq!(json, "\"workspace-write\"");
        let json = serde_json::to_string(&ApprovalPolicy::OnRequest).expect("encode");
        assert_eq!(json, "\"on-request\"");
    }
}
