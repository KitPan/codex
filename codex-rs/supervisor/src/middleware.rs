//! provider 中间件：参数注入 + 请求体审计（#14；P1-7）。
//!
//! - codex 的 Responses 请求从不携带 temperature 等采样参数（代理逐请求实证）；
//!   服务端合规（DSpark 要求显式 temp=0）只能由中间层保证。
//! - `scripts/dspark_proxy.py`（127.0.0.1:18300）是活体原型，本模块将其收编为
//!   进程内 provider 中间件，顺带获得请求体级观测/审计日志。
