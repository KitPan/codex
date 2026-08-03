# RDOSCli

Local-model CLI agent, forked from [openai/codex](https://github.com/openai/codex) (Rust),
supervisable by frontier agents (Claude Desktop / Claude Code) over MCP:
task assignment, progress monitoring, approval routing, interrupts.

Current status: **Phase 0** — zero-code validation of the supervision loop.
Plan and full decision context: [Phase0.md](Phase0.md).

CLI command: `rdos-cli`, config home: `~/.rdos` — in Phase 0 both are symlinks
(onto the official codex binary and the in-repo `codex-home/`); the defaults get
baked in at the Phase 1 fork.
