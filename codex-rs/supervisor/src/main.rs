//! `rdos-supervisor` 二进制入口。
//!
//! P1-4 起将以 MCP server（stdio）形态运行，北向暴露五件套。
//! P1-1 骨架阶段仅打印版本信息，证明 workspace 集成成立。

fn main() {
    let version = env!("CARGO_PKG_VERSION");
    println!("rdos-supervisor {version} (codex-supervisor crate, Phase 1 skeleton)");
    println!("northbound MCP tools (spawn_task/status/tail/reply/interrupt): not wired yet (P1-4)");
}
