//! `rdos-supervisor` 二进制入口：MCP server（stdio）。
//!
//! 北向接线（Claude Code）：
//! ```bash
//! claude mcp add rdos-supervisor --env CODEX_HOME=/Users/kit/.rdos -- rdos-supervisor
//! ```
//!
//! stdout 属于 MCP 协议；一切日志走 stderr。

use std::path::PathBuf;

use clap::Parser;
use codex_supervisor::mcp;
use codex_supervisor::service::Supervisor;
use codex_supervisor::service::SupervisorConfig;

#[derive(Parser, Debug)]
#[command(name = "rdos-supervisor", version, about = "RDOSCli 异步监督桥（MCP server, stdio）")]
struct Cli {
    /// CODEX_HOME（缺省：环境变量 CODEX_HOME，其次 ~/.rdos）
    #[arg(long = "codex-home")]
    codex_home: Option<PathBuf>,

    /// codex 二进制路径（缺省：本二进制同目录的 rdos-cli，其次 PATH 里的 rdos-cli）
    #[arg(long = "codex-bin")]
    codex_bin: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // stdout 是协议通道，日志必须去 stderr。
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let codex_home = match cli.codex_home {
        Some(path) => path,
        None => codex_utils_home_dir::find_codex_home()?.as_path().to_path_buf(),
    };
    let codex_bin = cli.codex_bin.unwrap_or_else(default_codex_bin);
    tracing::info!(
        "rdos-supervisor {} starting: CODEX_HOME={} codex_bin={}",
        env!("CARGO_PKG_VERSION"),
        codex_home.display(),
        codex_bin.display()
    );

    let (supervisor, report) = Supervisor::start(SupervisorConfig {
        codex_home,
        codex_bin,
    })?;
    tracing::info!(
        "recovered: {} task(s), {} session(s); swept {} stale, {} corrupt file(s) skipped",
        report.tasks_loaded,
        report.sessions_loaded,
        report.swept_tasks.len(),
        report.skipped.len()
    );
    for (path, reason) in &report.skipped {
        tracing::warn!("skipped corrupt record {}: {reason}", path.display());
    }

    mcp::serve_stdio(supervisor).await
}

/// 兄弟优先：与本二进制同目录的 rdos-cli（同一次构建产物），退回 PATH。
fn default_codex_bin() -> PathBuf {
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let sibling = dir.join("rdos-cli");
        if sibling.is_file() {
            return sibling;
        }
    }
    PathBuf::from("rdos-cli")
}
