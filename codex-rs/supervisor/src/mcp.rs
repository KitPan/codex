//! MCP 北向五件套（#1；P1-4）。
//!
//! `spawn_task`（含 worktree 分配）/ `status` / `tail`（摘要，非原始流）/
//! `reply` / `interrupt`。
//!
//! - 每次工具调用必须秒回（60s 客户端上限是硬红线，且超时会引发 mcp-server
//!   进程重生、全部内存会话连坐失效）。
//! - 骨架实现可复用 codex 自带 mcp-server 或 rmcp SDK，P1-4 决策。
