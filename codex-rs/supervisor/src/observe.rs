//! 观测：status/tail 摘要化与环境自检（#15 #16；P1-4/P1-5）。
//!
//! - `tail` 从 events 原始流（磁盘）读尾部 N 行并**逐行摘要化**——北向绝不
//!   透传原始 JSONL。实测透传率 ~1%（助手侧 250–300 万 tokens → supervisor
//!   窗口 ~2 万）是监督经济学生命线。
//! - 无内存缓冲：摘要直接读盘，bridge 重启后 tail 依旧可用（#2 恢复语义）。
//! - 环境自检（#16：代理存活、离线分诊）在 P1-5/P1-6 接线。

use std::path::Path;

use codex_exec::CommandExecutionStatus;
use codex_exec::ThreadEvent;
use codex_exec::ThreadItemDetails;

/// 单条文本的最大展示长度（超出截断加省略号）。
const SNIPPET: usize = 120;

/// 读取 events 文件尾部 `lines` 行并摘要化。文件尚不存在（任务未产出任何
/// 事件）返回空列表。
pub fn tail_summarized(events_path: &Path, lines: usize) -> std::io::Result<Vec<String>> {
    let raw = match std::fs::read_to_string(events_path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    let all: Vec<&str> = raw.lines().collect();
    let start = all.len().saturating_sub(lines);
    Ok(all[start..].iter().map(|line| summarize_line(line)).collect())
}

/// 一行原始 JSONL → 一行人读摘要。
pub fn summarize_line(line: &str) -> String {
    match serde_json::from_str::<ThreadEvent>(line) {
        Ok(event) => summarize_event(&event),
        // 无法解析的行原样截断上报——工具调用吐进文本通道正是实测过的
        // 故障形态，这本身就是诊断信号。
        Err(_) => format!("⚠ unparsed: {}", snippet(line, SNIPPET)),
    }
}

pub fn summarize_event(event: &ThreadEvent) -> String {
    match event {
        ThreadEvent::ThreadStarted(started) => format!("▶ thread {}", started.thread_id),
        ThreadEvent::TurnStarted(_) => "· turn started".to_string(),
        ThreadEvent::TurnCompleted(completed) => {
            let usage = &completed.usage;
            format!(
                "✓ turn completed (in {} / cached {} / out {})",
                usage.input_tokens, usage.cached_input_tokens, usage.output_tokens
            )
        }
        ThreadEvent::TurnFailed(failed) => {
            format!("✗ turn failed: {}", snippet(&failed.error.message, SNIPPET))
        }
        ThreadEvent::Error(error) => format!("‼ stream error: {}", snippet(&error.message, SNIPPET)),
        ThreadEvent::ItemStarted(event) => format!("… {}", summarize_item(&event.item.details)),
        ThreadEvent::ItemUpdated(event) => format!("↻ {}", summarize_item(&event.item.details)),
        ThreadEvent::ItemCompleted(event) => format!("✓ {}", summarize_item(&event.item.details)),
    }
}

fn summarize_item(details: &ThreadItemDetails) -> String {
    match details {
        ThreadItemDetails::AgentMessage(message) => {
            format!("agent: {}", snippet(&message.text, SNIPPET))
        }
        ThreadItemDetails::Reasoning(reasoning) => {
            format!("thinking ({} chars)", reasoning.text.chars().count())
        }
        ThreadItemDetails::CommandExecution(command) => {
            let exit = match (&command.status, command.exit_code) {
                (CommandExecutionStatus::InProgress, _) => "…".to_string(),
                (_, Some(code)) => format!("exit {code}"),
                (status, None) => format!("{status:?}").to_lowercase(),
            };
            format!("$ {} → {}", snippet(&command.command, 100), exit)
        }
        ThreadItemDetails::FileChange(change) => {
            let paths: Vec<&str> = change
                .changes
                .iter()
                .take(3)
                .map(|c| c.path.as_str())
                .collect();
            format!(
                "patch [{:?}] {} file(s): {}{}",
                change.status,
                change.changes.len(),
                paths.join(", "),
                if change.changes.len() > 3 { ", …" } else { "" }
            )
        }
        ThreadItemDetails::McpToolCall(call) => {
            format!("mcp {}::{} [{:?}]", call.server, call.tool, call.status)
        }
        ThreadItemDetails::WebSearch(search) => {
            format!("web search: {}", snippet(&search.query, 80))
        }
        ThreadItemDetails::TodoList(todo) => format!("todo list ({} items)", todo.items.len()),
        ThreadItemDetails::Error(error) => format!("item error: {}", snippet(&error.message, SNIPPET)),
        // 未特化的 item 类型只报类型名，不透传内容。
        other => serde_json::to_value(other)
            .ok()
            .and_then(|v| v.get("type").and_then(|t| t.as_str().map(str::to_string)))
            .unwrap_or_else(|| "unknown item".to_string()),
    }
}

/// 按字符截断（多字节安全），超长加省略号。
fn snippet(text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (count, ch) in text.chars().enumerate() {
        if count >= max_chars {
            out.push('…');
            return out;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn summarizes_core_events_one_line_each() {
        let lines = [
            r#"{"type":"thread.started","thread_id":"t-9"}"#,
            r#"{"type":"item.completed","item":{"id":"i1","type":"command_execution","command":"cargo test","aggregated_output":"...","exit_code":101,"status":"failed"}}"#,
            r#"{"type":"item.completed","item":{"id":"i2","type":"agent_message","text":"修好了"}}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":10,"cached_input_tokens":5,"output_tokens":3,"reasoning_output_tokens":0}}"#,
            "raw garbage",
        ];
        let summaries: Vec<String> = lines.iter().map(|l| summarize_line(l)).collect();
        assert_eq!(summaries[0], "▶ thread t-9");
        assert_eq!(summaries[1], "✓ $ cargo test → exit 101");
        assert_eq!(summaries[2], "✓ agent: 修好了");
        assert_eq!(summaries[3], "✓ turn completed (in 10 / cached 5 / out 3)");
        assert_eq!(summaries[4], "⚠ unparsed: raw garbage");
        for s in &summaries {
            assert!(!s.contains("aggregated_output"), "raw payload must not leak");
        }
    }

    #[test]
    fn tail_reads_last_n_lines_and_missing_file_is_empty() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let path = dir.path().join("ev.jsonl");
        assert_eq!(tail_summarized(&path, 5).expect("missing ok"), Vec::<String>::new());

        let mut body = String::new();
        for i in 0..10 {
            body.push_str(&format!("{{\"type\":\"thread.started\",\"thread_id\":\"t-{i}\"}}\n"));
        }
        std::fs::write(&path, body).expect("write");
        let tail = tail_summarized(&path, 3).expect("tail");
        assert_eq!(tail, vec!["▶ thread t-7", "▶ thread t-8", "▶ thread t-9"]);
    }

    #[test]
    fn snippet_is_multibyte_safe() {
        assert_eq!(snippet("深bug修复完成", 3), "深bu…");
        assert_eq!(snippet("short", 100), "short");
    }
}
