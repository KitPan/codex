//! # codex-supervisor — RDOSCli bridge v1（异步监督桥）
//!
//! 把监督链路从「60s 阻塞盲盒」升级为「异步任务注册表 + MCP 五件套」
//! （`spawn_task` / `status` / `tail` / `reply` / `interrupt`）。
//!
//! 需求编号（#n）对应 `results/phase0-results.md` 文末《Phase 1 bridge 需求清单》
//! （RDOSCli 仓库，17 条、六大类、每条附实测出处）；任务编号（P1-n）对应
//! `Phase1.md` §3 任务清单。本 crate 是 Phase 1 唯一新增 crate，薄 fork 原则：
//! 定制集中于此，不散改内核。
//!
//! ## 模块地图
//!
//! | 模块 | 职责 | 需求 | 任务 |
//! |---|---|---|---|
//! | [`paths`] | 状态目录布局、原子持久化、单例锁 | #2 #4 #17 | P1-2 |
//! | [`registry`] | 任务注册表与生命周期状态机 | #1 | P1-2/P1-3 |
//! | [`service`] | 编排层：五件套语义 + 恢复扫描 + judge | #1 #2 #5 | P1-4 |
//! | [`session`] | 会话自持、持久化、所有权与锁 | #2 #3 #4 | P1-2 |
//! | [`models`] | 模型登记表与三维分派路由 | #10 #11 #12 | P1-2/P1-6 |
//! | [`executor`] | 南向执行器（spawn `codex exec --json`） | #7 #9 #17 | P1-3 |
//! | [`worktree`] | git worktree 分配/回收（任务隔离） | §10 硬约束 | P1-3/P1-4 |
//! | [`mcp`] | MCP 北向五件套 | #1 | P1-4 |
//! | [`acceptance`] | 机器判卷、失败熔断、diff-first | #5 #6 #8 | P1-5 |
//! | [`observe`] | status/tail 摘要化与环境自检 | #15 #16 | P1-4/P1-5 |
//! | [`middleware`] | provider 中间件（参数注入+审计） | #14 | P1-7 |

pub mod acceptance;
pub mod executor;
pub mod mcp;
pub mod middleware;
pub mod models;
pub mod observe;
pub mod paths;
pub mod registry;
pub mod service;
pub mod session;
pub mod worktree;
