//! 任务注册表与生命周期状态机（#1；P1-2/P1-3）。
//!
//! - `spawn_task` 立即返回任务句柄；进度经 `status`/`tail` 拉取、结果推送通知。
//!   实证：MCP 阻塞调用 60s 上限四重钉死（bypass/always-ask × 审批挂起/纯长命令），
//!   同步模型对本地模型的真实任务不可用。
//! - 句柄必须跨 bridge 进程重启可恢复（对照 threadId 随 mcp-server 进程重生连坐全灭）。
//! - 生命周期含 interrupt（杀进程 + worktree 回收，状态干净）。
