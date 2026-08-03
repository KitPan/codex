//! MCP 北向五件套（#1；P1-4）。
//!
//! rmcp（官方 Rust SDK）stdio server。本层只做参数翻译与错误映射，语义全在
//! [`crate::service`]。每个工具都必须**秒回**——60s 客户端上限是硬红线，且超时
//! 会引发 mcp-server 进程重生、全部内存会话连坐失效（Phase 0 实测）。

use std::str::FromStr;
use std::sync::Arc;

use rmcp::ErrorData;
use rmcp::ServerHandler;
use rmcp::ServiceExt;
use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::model::Content;
use rmcp::model::ServerCapabilities;
use rmcp::model::ServerInfo;
use rmcp::schemars;
use rmcp::tool;
use rmcp::tool_handler;
use rmcp::tool_router;
use serde::Deserialize;

use crate::registry::SessionTarget;
use crate::registry::TaskId;
use crate::registry::WorktreePolicy;
use crate::service::ServiceError;
use crate::service::SpawnRequest;
use crate::service::Supervisor;
use crate::session::SandboxMode;
use crate::session::SessionId;

pub struct SupervisorServer {
    supervisor: Arc<Supervisor>,
    tool_router: ToolRouter<Self>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct SpawnTaskParams {
    /// 任务指令（发给本地模型的完整 prompt）
    pub prompt: String,
    /// 模型登记键（supervisor.models.toml 中的条目名，如 qwen36-nvfp4）
    pub model_key: String,
    /// 任务工作目录（绝对路径；isolated 模式下作为 worktree 母本）
    pub cwd: String,
    /// 沙箱：read-only | workspace-write | danger-full-access（默认 workspace-write）
    pub sandbox: Option<String>,
    /// 判卷命令（如 "cargo test"；退出码定生死，模型自我汇报不作数）
    pub judge_command: Option<String>,
    /// 任务硬超时秒数（默认 900）
    pub timeout_secs: Option<u64>,
    /// worktree 隔离：isolated | in-place（默认 isolated；并行任务必须 isolated）
    pub worktree: Option<String>,
    /// 完成后保留任务分支供审查（默认 true）
    pub diff_first: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct StatusParams {
    /// 任务 id；缺省列出全部任务
    pub task_id: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct TailParams {
    /// 任务 id
    pub task_id: String,
    /// 尾部行数（默认 20）
    pub lines: Option<usize>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ReplyParams {
    /// 会话 id（spawn_task 返回值里的 session_id）
    pub session_id: String,
    /// 续接指令
    pub prompt: String,
    /// 换将：改用另一个模型登记键（缺省沿用会话模型亲和）
    pub model_key: Option<String>,
    /// 判卷命令
    pub judge_command: Option<String>,
    /// 任务硬超时秒数
    pub timeout_secs: Option<u64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct InterruptParams {
    /// 要中止的任务 id
    pub task_id: String,
}

#[tool_router]
impl SupervisorServer {
    pub fn new(supervisor: Arc<Supervisor>) -> Self {
        Self {
            supervisor,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        name = "spawn_task",
        description = "派发一个异步任务给本地模型执行。立即返回 task_id/session_id 句柄；用 status/tail 跟进，完成与否以判卷命令退出码为准。"
    )]
    async fn spawn_task(
        &self,
        Parameters(p): Parameters<SpawnTaskParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let sandbox = parse_sandbox(p.sandbox.as_deref())?;
        let worktree = parse_worktree(p.worktree.as_deref())?;
        let reply = self
            .supervisor
            .spawn_task(SpawnRequest {
                prompt: p.prompt,
                model_key: p.model_key,
                cwd: p.cwd.into(),
                sandbox,
                judge_command: p.judge_command,
                timeout_secs: p.timeout_secs,
                worktree,
                session: SessionTarget::New,
                diff_first: p.diff_first.unwrap_or(true),
            })
            .await
            .map_err(map_err)?;
        json_result(&reply)
    }

    #[tool(
        name = "status",
        description = "查询任务状态摘要（单个 task_id 或全部）。返回状态、判卷结果、token 用量与最后回复摘要——不含原始事件流。"
    )]
    async fn status(
        &self,
        Parameters(p): Parameters<StatusParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let task = p.task_id.as_deref().map(parse_task_id).transpose()?;
        let views = self.supervisor.status(task).map_err(map_err)?;
        json_result(&views)
    }

    #[tool(
        name = "tail",
        description = "取任务事件流尾部 N 行的摘要（每行一条：命令+退出码、补丁、回复片段等）。原始 JSONL 留档在磁盘，不经此透传。"
    )]
    async fn tail(
        &self,
        Parameters(p): Parameters<TailParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let task = parse_task_id(&p.task_id)?;
        let lines = self
            .supervisor
            .tail(task, p.lines.unwrap_or(20))
            .map_err(map_err)?;
        let body = if lines.is_empty() {
            "（尚无事件）".to_string()
        } else {
            lines.join("\n")
        };
        Ok(CallToolResult::success(vec![Content::text(body)]))
    }

    #[tool(
        name = "reply",
        description = "在既有会话上追加指令（续接/纠偏）。沿用会话的模型亲和与 cwd；返回新的 task_id。"
    )]
    async fn reply(
        &self,
        Parameters(p): Parameters<ReplyParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let session = parse_session_id(&p.session_id)?;
        let reply = self
            .supervisor
            .reply(session, p.prompt, p.model_key, p.judge_command, p.timeout_secs)
            .await
            .map_err(map_err)?;
        json_result(&reply)
    }

    #[tool(
        name = "interrupt",
        description = "中止执行中的任务：杀整个任务进程组、回收 worktree、释放会话租约。"
    )]
    async fn interrupt(
        &self,
        Parameters(p): Parameters<InterruptParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let task = parse_task_id(&p.task_id)?;
        self.supervisor.interrupt(task).map_err(map_err)?;
        Ok(CallToolResult::success(vec![Content::text(format!(
            "interrupt sent to {task}"
        ))]))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for SupervisorServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.server_info.name = "rdos-supervisor".to_string();
        info.server_info.version = env!("CARGO_PKG_VERSION").to_string();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.instructions = Some(
            "rdos-supervisor：本地模型 codex 任务的异步监督桥。spawn_task 派发（立即返回句柄），\
             status/tail 跟进，reply 续接，interrupt 中止。判卷以注册命令的退出码为准。"
                .to_string(),
        );
        info
    }
}

/// 在 stdio 上服务直至客户端断开。
pub async fn serve_stdio(supervisor: Arc<Supervisor>) -> anyhow::Result<()> {
    let server = SupervisorServer::new(supervisor);
    let running = server.serve(rmcp::transport::stdio()).await?;
    running.waiting().await?;
    Ok(())
}

fn json_result<T: serde::Serialize>(value: &T) -> Result<CallToolResult, ErrorData> {
    let body = serde_json::to_string_pretty(value)
        .map_err(|e| ErrorData::internal_error(format!("encode reply: {e}"), None))?;
    Ok(CallToolResult::success(vec![Content::text(body)]))
}

fn parse_task_id(raw: &str) -> Result<TaskId, ErrorData> {
    TaskId::from_str(raw)
        .map_err(|_| ErrorData::invalid_params(format!("invalid task_id: {raw}"), None))
}

fn parse_session_id(raw: &str) -> Result<SessionId, ErrorData> {
    SessionId::from_str(raw)
        .map_err(|_| ErrorData::invalid_params(format!("invalid session_id: {raw}"), None))
}

fn parse_sandbox(raw: Option<&str>) -> Result<SandboxMode, ErrorData> {
    match raw {
        None => Ok(SandboxMode::WorkspaceWrite),
        Some("read-only") => Ok(SandboxMode::ReadOnly),
        Some("workspace-write") => Ok(SandboxMode::WorkspaceWrite),
        Some("danger-full-access") => Ok(SandboxMode::DangerFullAccess),
        Some(other) => Err(ErrorData::invalid_params(
            format!("invalid sandbox: {other} (read-only | workspace-write | danger-full-access)"),
            None,
        )),
    }
}

fn parse_worktree(raw: Option<&str>) -> Result<WorktreePolicy, ErrorData> {
    match raw {
        None => Ok(WorktreePolicy::Isolated),
        Some("isolated") => Ok(WorktreePolicy::Isolated),
        Some("in-place") => Ok(WorktreePolicy::InPlace),
        Some(other) => Err(ErrorData::invalid_params(
            format!("invalid worktree: {other} (isolated | in-place)"),
            None,
        )),
    }
}

fn map_err(err: ServiceError) -> ErrorData {
    match &err {
        ServiceError::Registry(_)
        | ServiceError::Ownership(_)
        | ServiceError::SessionNotFound { .. }
        | ServiceError::SessionNotResumable { .. }
        | ServiceError::TaskNotFound { .. }
        | ServiceError::TaskNotRunning { .. } => ErrorData::invalid_params(err.to_string(), None),
        _ => ErrorData::internal_error(err.to_string(), None),
    }
}
