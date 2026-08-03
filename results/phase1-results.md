# Phase 1 实测记录（进行中）

任务清单与验收标准见 [Phase1.md](../Phase1.md)；fork 档案见 [FORK.md](../FORK.md)。
本文件按事件追加，P1-9 收尾时汇总。

## 首次实弹端到端（2026-08-04 早，P1-4 落成即测）

栈：Claude（驱动脚本模拟 supervisor 客户端）→ `rdos-supervisor`（MCP stdio，fork
`23f82f30d8` debug build）→ `rdos-cli exec --json`（同构建产物）→ qwen36-nvfp4 @
spark1（vLLM，Responses）。任务：T1-lite 只读分析（testbed src/lib.rs 一句话总结），
`sandbox=read-only`、`worktree=in-place`、无判卷。

| 观测点 | 结果 |
|---|---|
| `spawn_task` 返回延迟 | **0.00s**（句柄即回，#1 异步模型成立） |
| 任务全程 | 25.0s 后台执行，`status` 轮询全程可见（含 pid） |
| 60s 天花板 | **不存在**——每次 MCP 调用皆秒回，模型耗时与窗口解耦 |
| session↔rollout | thread id `019fca02-…` 经 thread.started 即时写入会话记录（#2） |
| `tail` 摘要 | 7 行人读摘要（thread/命令+退出码/回复片段/tokens），零原始 JSONL 透传（#15） |
| 模型路由 | 登记表 `qwen36-nvfp4` → `-m` + `model_provider=rdos-vllm`（#12），应答内容正确 |
| 登记表 | `codex-home/supervisor.models.toml` 首版三台入表（P1-6 数据基座） |

附带观察：codex 的 "Model metadata not found → fallback" 警告以 `item error` 事件
形态进入流（tail 可见）——无碍运行，P1-6/P1-9 可补模型元数据消除（Phase 0 已知）。

## 工程记录

- **P1-3 进程组语义**（fork `d7dd19dde2`）：SIGKILL 只杀 codex 本体会留孤儿工具
  进程（占 stdout 管道 → EOF 永不到来，测试实测 30s 挂等）。修复 = 任务自成
  进程组 + 负 pid 群杀 + pump 收尾宽限 5s。interrupt 语义自此为「杀整棵树」。
- **flock 描述符瞬时残留**（macOS 实测）：并发 spawn 场景下单例锁 drop 后立即
  重取有 ~50% 概率撞 `AlreadyRunning`，孤立复现 0/20。修复 = acquire 内置有界
  重试（10×50ms，与上游 message-history 同惯例）；重试耗尽仍占 = 真双 bridge。
- **clippy `allow-expect-in-tests` 不识别 `cfg(all(test, unix))`**：拆成并列
  `#[cfg(test)] #[cfg(unix)]` 即可；集成测试（tests/ 目录无 cfg(test)）按上游
  惯例文件头 `#![allow(clippy::expect_used)]`。
