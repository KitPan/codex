//! 南向执行器：per-task spawn `codex exec --json` 子进程（#7 #9 #17；P1-3）。
//!
//! - JSONL 事件流解析 + 任务生命周期驱动；exec 无超时上限（Phase 0 已验证）。
//! - 南向自动重试 + preflight：瞬时故障窗口实测三次（HTTP 000 / SYN 丢包 / 24s
//!   超时），codex 流中断 0 重试一击即溃；端点探测用**裸 IP**（.local mDNS 会
//!   间歇挂死本机 curl）。
//! - 命令硬化：注入 `set -o pipefail`（管道掩蔽真实退出码）+ 强制超时（沙箱禁网
//!   两形态：52.7s 吊死 / 0.0000s 秒退空输出，识别逻辑两种都要覆盖）。
//! - 上游适配（#17）：引擎必须支持 Responses 协议（chat wire 已移除）；
//!   会话建模为常驻服务对象、传输可插拔——Phase 2 换 app-server/Unix sock 不动骨头。
