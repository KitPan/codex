# RDOSCli

Local-model CLI agent, forked from [openai/codex](https://github.com/openai/codex) (Rust),
supervisable by frontier agents (Claude Desktop / Claude Code) over MCP:
task assignment, progress monitoring, approval routing, interrupts.

Current status: **Phase 1** — bridge v1 (async supervision bridge: task registry +
MCP `spawn_task`/`status`/`tail`/`reply`/`interrupt`). Sprint plan: [Phase1.md](Phase1.md).
Phase 0 findings (completed 2026-08-03): [Phase0.md](Phase0.md) +
[results/phase0-results.md](results/phase0-results.md).

CLI command: `rdos-cli`, config home: `~/.rdos` — in Phase 0 both are symlinks
(onto the official codex binary and the in-repo `codex-home/`); the defaults get
baked in at the Phase 1 fork.
