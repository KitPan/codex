# RDOSCli

基于 openai/codex（Rust）fork 的**本地模型 CLI agent**。核心特性：可被 frontier agent
（Claude Desktop / Claude Code）通过 MCP 监督执行——分配任务、监控进度、审批、打断。

- **当前阶段：Phase 0**（零代码链路验证）。完整计划、技术路线决策及理由都在
  [Phase0.md](Phase0.md)——新 session 先读它，按 §3 任务清单从未勾选项继续。
- 本地模型（2026-08-03 实测）：Qwen3.6-35B-A3B **NVFP4**（NVIDIA 官方权重，vLLM @
  spark1.local:8000 即 DGX Spark，profile `qwen36`；`qwen36-deep` 暂同模型占位）、
  Gemma 4-31B（本机 omlx @ 127.0.0.1:9999，profile
  `gemma4`，**用 8bit**——4bit 写任务 apply_patch 死循环不可用；spark2.local:8000 现为
  Google 官方 gemma-4-26B-A4B-it bf16，profile `gemma4-spark`，可写且进 60s MCP 窗口，
  gemma 首选）；DeepSeek-V4-Flash
  （studio.local:8000，**DSpark 要求显式 temp=0**，经 `scripts/dspark_proxy.py` 注入代理
  接入，profile `deepseek-flash`，代理须先起）。codex 0.144 南向只走 Responses 协议
  （wire_api chat 已移除），profile 用 `CODEX_HOME/<p>.config.toml` 独立文件。
- 命名（2026-08-02 定）：命令 `rdos-cli`（软链 → 官方 codex 二进制，`/opt/homebrew/bin`），
  配置家目录 `~/.rdos`（软链 → repo 内 `codex-home/`，config.toml 入库）。
- **隔离约定（必须遵守）**：一切 codex/rdos-cli 调用显式带
  `CODEX_HOME=/Users/kit/.rdos`（与 repo 内 `codex-home/` 是同一目录），绝不读写
  `~/.codex`（那是 Kit 日常 Codex 的数据，含登录凭证）。Phase 1 fork 前编译期默认
  仍是 `~/.codex`，环境变量不能省。
- 测试项目 `testbed/` 的任务结果以 `cargo test` 退出码为准，不靠人眼判断。
