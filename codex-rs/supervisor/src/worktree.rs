//! git worktree 分配与回收（Phase0 §10 文件系统硬约束；P1-3/P1-4）。
//!
//! - 多实例同时写同一 checkout 必丢工作（社区共识）；`spawn_task` 内置
//!   worktree 分配（任务开始建、结束合并/丢弃），interrupt 路径必须回收。
//! - 验收标准之一：两个任务在两个 worktree 并行执行互不串扰。
