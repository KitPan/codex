//! 模型登记表与分派路由（#10 #11 #12；P1-2/P1-6）。
//!
//! - 一等属性：quantization（**格式+档位**，非位数——MLX-int4 死循环、AWQ-int4
//!   假完成+盲改、MLX-8bit 稳、NVFP4/bf16 全过）、serve 引擎与版本、thinking 档、
//!   延迟档；「量化损伤」与「家族短板」分列（已可实验区分）。
//! - 分派三维：模型延迟 × 任务颗粒度 × reasoning 档（thinking 开使耗时 2×）；
//!   escalation 规范注入任务模板（on-request 依赖模型自觉，不教不会）。
//! - 路由机制：MCP 侧无 profile，显式 `model + model_provider`；provider 可嵌套
//!   临时定义（继承 codex `config` 能力）。
