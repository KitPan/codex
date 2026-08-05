//! provider 中间件：参数注入 + 请求体审计（#14；P1-7）。
//!
//! - codex 的 Responses 请求从不携带 temperature 等采样参数（代理逐请求实证）；
//!   服务端合规（DSpark 要求显式 temp=0）只能由中间层保证。
//! - `scripts/dspark_proxy.py`（127.0.0.1:18300）是活体原型，本模块将其收编为
//!   进程内 provider 中间件，顺带获得请求体级观测/审计日志（JSONL，一行一请求）。
//! - 形态：**共享 per-model 监听器**（deepseek 审计建议）——注入参数是模型属性
//!   而非任务属性，首个任务惰性起监听器，同模型任务复用；随 supervisor 进程消亡。
//! - 与原型的语义差（有意）：请求体经 axum 聚合读取——原型按 Content-Length 读
//!   会把 chunked 请求体读空（审计发现的真缺陷）；上游只设连接超时，读侧由任务级
//!   硬超时兜底（#9）——任务被杀 → codex 消亡 → 客户端断连 → 转发随之终止。

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use axum::body::Body;
use axum::extract::State;
use axum::http::Method;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use serde::Serialize;

use crate::models::InjectParams;

/// hop-by-hop 头（RFC 7230 §6.1 超集）：含 content-length（body 会改写，由
/// 客户端库重算）与 host（指向上游而非中间件）。
const HOP_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "transfer-encoding",
    "te",
    "trailer",
    "proxy-authorization",
    "proxy-authenticate",
    "upgrade",
    "content-length",
    "host",
];

/// 注入白名单（去掉 `/v1` 前缀后的路径）。codex 0.144 只走 responses；
/// chat/completions 系保留为防御面（原型同款）。
const INJECT_PATHS: &[&str] = &["/responses", "/chat/completions", "/completions"];

/// 上游连接超时。读侧不设超时——流式长响应的止损归任务级硬超时（#9）。
const UPSTREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// 请求体聚合上限（codex 的 Responses 请求体实测远小于此；防御性设置）。
const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

/// 共享 per-model 监听器登记处（Supervisor 持有一份）。
#[derive(Default)]
pub struct MiddlewareHub {
    listeners: Mutex<HashMap<String, Arc<MiddlewareHandle>>>,
}

impl MiddlewareHub {
    /// 确保 model_key 的中间件监听器存在，返回给 codex 的 base_url
    /// （`http://127.0.0.1:<port>/v1`）。幂等：已存在即复用。
    pub async fn ensure(
        &self,
        model_key: &str,
        upstream_base_url: &str,
        inject: &InjectParams,
        audit_dir: &Path,
    ) -> Result<String, MiddlewareError> {
        if let Some(handle) = self.lock_map().get(model_key) {
            return Ok(handle.base_url.clone());
        }
        let audit_path = audit_dir.join(format!("{model_key}.jsonl"));
        let handle = spawn_listener(model_key, upstream_base_url, inject.clone(), &audit_path)
            .await
            .map(Arc::new)?;
        // spawn 期间可能有并发 ensure 抢先——先到者胜，后到的句柄 Drop 即回收。
        let mut map = self.lock_map();
        let winner = map
            .entry(model_key.to_string())
            .or_insert_with(|| Arc::clone(&handle));
        Ok(winner.base_url.clone())
    }

    fn lock_map(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<MiddlewareHandle>>> {
        match self.listeners.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

/// 一个在跑的中间件监听器。Drop 即停（abort serve 任务，端口随之释放）。
pub struct MiddlewareHandle {
    /// 交给 codex 的 provider base_url。
    pub base_url: String,
    /// 审计日志路径（`<audit_dir>/<model_key>.jsonl`）。
    pub audit_path: PathBuf,
    join: tokio::task::JoinHandle<()>,
}

impl Drop for MiddlewareHandle {
    fn drop(&mut self) {
        self.join.abort();
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MiddlewareError {
    #[error("中间件监听器启动失败: {context}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
    #[error("上游 base_url 无效: {0}")]
    InvalidUpstream(String),
}

struct ProxyState {
    model_key: String,
    /// 上游 base（去尾斜杠，如 `http://192.168.3.3:8000/v1`）；转发目标 =
    /// base + (来路 path 去 `/v1` 前缀)。
    upstream_base: String,
    inject: InjectParams,
    client: reqwest::Client,
    audit: Mutex<std::fs::File>,
}

/// 审计行（#14：请求体级观测）。schema 依 deepseek 审计建议裁定；
/// client_disconnected 有意不记——流式转发中客户端断连由连接取消隐式处理，
/// 观测它需要贯穿 Body 流的探针，成本不匹配价值。
#[derive(Serialize)]
struct AuditRecord<'a> {
    ts: String,
    model_key: &'a str,
    method: &'a str,
    path: &'a str,
    /// 回给客户端的状态码（上游异常时为 502）。
    status: u16,
    upstream: &'a str,
    duration_ms: u128,
    body_bytes: usize,
    injected: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature_had: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature_set: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    upstream_error: Option<String>,
}

async fn spawn_listener(
    model_key: &str,
    upstream_base_url: &str,
    inject: InjectParams,
    audit_path: &Path,
) -> Result<MiddlewareHandle, MiddlewareError> {
    let upstream_base = normalize_base(upstream_base_url)?;
    if let Some(parent) = audit_path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| MiddlewareError::Io {
            context: format!("create audit dir {}", parent.display()),
            source,
        })?;
    }
    let audit = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(audit_path)
        .map_err(|source| MiddlewareError::Io {
            context: format!("open audit log {}", audit_path.display()),
            source,
        })?;
    let client = reqwest::Client::builder()
        .connect_timeout(UPSTREAM_CONNECT_TIMEOUT)
        .build()
        .map_err(|e| MiddlewareError::InvalidUpstream(format!("build http client: {e}")))?;

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|source| MiddlewareError::Io {
            context: "bind 127.0.0.1:0".to_string(),
            source,
        })?;
    let port = listener
        .local_addr()
        .map_err(|source| MiddlewareError::Io {
            context: "read local_addr".to_string(),
            source,
        })?
        .port();

    let state = Arc::new(ProxyState {
        model_key: model_key.to_string(),
        upstream_base: upstream_base.clone(),
        inject,
        client,
        audit: Mutex::new(audit),
    });
    let app = axum::Router::new().fallback(forward).with_state(state);
    let join = tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            tracing::error!("middleware listener died: {err}");
        }
    });

    tracing::info!(
        "middleware up for {model_key}: 127.0.0.1:{port} -> {upstream_base}（audit: {}）",
        audit_path.display()
    );
    Ok(MiddlewareHandle {
        base_url: format!("http://127.0.0.1:{port}/v1"),
        audit_path: audit_path.to_path_buf(),
        join,
    })
}

/// 上游 base 规整：必须 http(s)，去尾斜杠。
fn normalize_base(raw: &str) -> Result<String, MiddlewareError> {
    let trimmed = raw.trim_end_matches('/');
    if !trimmed.starts_with("http://") && !trimmed.starts_with("https://") {
        return Err(MiddlewareError::InvalidUpstream(raw.to_string()));
    }
    Ok(trimmed.to_string())
}

/// 转发目标：来路 `/v1/xxx?q` → `<upstream_base>/xxx?q`；异形路径原样接在
/// base 后（防御，不应出现）。
fn target_url(upstream_base: &str, path_and_query: &str) -> String {
    let rest = path_and_query
        .strip_prefix("/v1")
        .unwrap_or(path_and_query);
    format!("{upstream_base}{rest}")
}

fn is_hop_header(name: &str) -> bool {
    HOP_HEADERS.iter().any(|h| name.eq_ignore_ascii_case(h))
}

fn is_inject_path(path_and_query: &str) -> bool {
    let path = path_and_query.split('?').next().unwrap_or(path_and_query);
    let rest = path.strip_prefix("/v1").unwrap_or(path);
    INJECT_PATHS.contains(&rest)
}

/// 注入：body 须是 JSON object 才动手，否则原样透传（原型语义）。
/// 返回 (新 body, 注入前的 temperature)。
fn apply_inject(body: &[u8], inject: &InjectParams) -> Option<(Vec<u8>, Option<f64>)> {
    let temperature = inject.temperature?;
    let mut value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let obj = value.as_object_mut()?;
    let had = obj.get("temperature").and_then(serde_json::Value::as_f64);
    obj.insert(
        "temperature".to_string(),
        serde_json::Number::from_f64(temperature).map(serde_json::Value::Number)?,
    );
    let encoded = serde_json::to_vec(&value).ok()?;
    Some((encoded, had))
}

async fn forward(State(state): State<Arc<ProxyState>>, req: axum::extract::Request) -> Response {
    let started = Instant::now();
    let (parts, body) = req.into_parts();
    let method = parts.method.clone();
    let path_q = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());

    let body_bytes = match axum::body::to_bytes(body, MAX_BODY_BYTES).await {
        Ok(bytes) => bytes,
        Err(err) => {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("read request body: {err}"),
            )
                .into_response();
        }
    };

    let mut injected = false;
    let mut temperature_had = None;
    let out_body: Vec<u8> = if method == Method::POST
        && is_inject_path(&path_q)
        && state.inject.temperature.is_some()
    {
        match apply_inject(&body_bytes, &state.inject) {
            Some((new_body, had)) => {
                injected = true;
                temperature_had = had;
                new_body
            }
            // 非 JSON body：透传不注入（原型语义），审计记 injected=false。
            None => body_bytes.to_vec(),
        }
    } else {
        body_bytes.to_vec()
    };
    let body_len = out_body.len();

    let target = target_url(&state.upstream_base, &path_q);
    let mut request = state.client.request(method.clone(), &target);
    for (name, value) in &parts.headers {
        if !is_hop_header(name.as_str()) {
            request = request.header(name, value);
        }
    }

    match request.body(out_body).send().await {
        Ok(upstream) => {
            let status = upstream.status();
            state.write_audit(AuditRecord {
                ts: now_rfc3339(),
                model_key: &state.model_key,
                method: method.as_str(),
                path: &path_q,
                status: status.as_u16(),
                upstream: &state.upstream_base,
                duration_ms: started.elapsed().as_millis(),
                body_bytes: body_len,
                injected,
                temperature_had,
                temperature_set: injected.then_some(state.inject.temperature).flatten(),
                upstream_error: None,
            });
            let mut builder = Response::builder().status(status.as_u16());
            for (name, value) in upstream.headers() {
                if !is_hop_header(name.as_str()) {
                    builder = builder.header(name.as_str(), value.as_bytes());
                }
            }
            // 流式回传：逐块透传，绝不聚合——SSE 实时性即监督实时性。
            match builder.body(Body::from_stream(upstream.bytes_stream())) {
                Ok(response) => response,
                Err(err) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("assemble response: {err}"),
                )
                    .into_response(),
            }
        }
        Err(err) => {
            state.write_audit(AuditRecord {
                ts: now_rfc3339(),
                model_key: &state.model_key,
                method: method.as_str(),
                path: &path_q,
                status: StatusCode::BAD_GATEWAY.as_u16(),
                upstream: &state.upstream_base,
                duration_ms: started.elapsed().as_millis(),
                body_bytes: body_len,
                injected,
                temperature_had,
                temperature_set: injected.then_some(state.inject.temperature).flatten(),
                upstream_error: Some(err.to_string()),
            });
            (StatusCode::BAD_GATEWAY, format!("upstream error: {err}")).into_response()
        }
    }
}

impl ProxyState {
    /// 审计落盘失败降级为日志——观测面故障不得拖垮数据面。
    fn write_audit(&self, record: AuditRecord<'_>) {
        let Ok(line) = serde_json::to_string(&record) else {
            tracing::warn!("audit encode failed for {}", self.model_key);
            return;
        };
        match self.audit.lock() {
            Ok(mut file) => {
                if let Err(err) = writeln!(file, "{line}") {
                    tracing::warn!("audit write failed for {}: {err}", self.model_key);
                }
            }
            Err(_) => tracing::warn!("audit lock poisoned for {}", self.model_key),
        }
    }
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::Request as AxumRequest;
    use pretty_assertions::assert_eq;
    use std::sync::Mutex as StdMutex;
    use tempfile::TempDir;

    /// 捕获到的上游请求（断言材料）。
    #[derive(Debug, Clone)]
    struct Captured {
        method: String,
        path: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    #[derive(Clone)]
    enum MockReply {
        Json200,
        SlowChunks,
    }

    /// mock 上游：捕获请求，按模式应答。返回 base_url（含 /v1）。
    async fn mock_upstream(
        captured: Arc<StdMutex<Vec<Captured>>>,
        reply: MockReply,
    ) -> String {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind mock");
        let port = listener.local_addr().expect("addr").port();
        let app = axum::Router::new().fallback(move |req: AxumRequest| {
            let captured = Arc::clone(&captured);
            let reply = reply.clone();
            async move {
                let (parts, body) = req.into_parts();
                let body = axum::body::to_bytes(body, usize::MAX)
                    .await
                    .expect("mock read body");
                captured.lock().expect("lock").push(Captured {
                    method: parts.method.to_string(),
                    path: parts
                        .uri
                        .path_and_query()
                        .map(|p| p.as_str().to_string())
                        .unwrap_or_default(),
                    headers: parts
                        .headers
                        .iter()
                        .map(|(k, v)| {
                            (k.as_str().to_string(), String::from_utf8_lossy(v.as_bytes()).to_string())
                        })
                        .collect(),
                    body: body.to_vec(),
                });
                match reply {
                    MockReply::Json200 => {
                        (StatusCode::OK, r#"{"ok":true}"#.to_string()).into_response()
                    }
                    MockReply::SlowChunks => {
                        let stream = futures_stream_two_chunks();
                        Response::builder()
                            .status(StatusCode::OK)
                            .body(Body::from_stream(stream))
                            .expect("chunked response")
                    }
                }
            }
        });
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://127.0.0.1:{port}/v1")
    }

    /// 两块流：第一块立即，第二块 600ms 后——「未被缓冲」的判据。
    fn futures_stream_two_chunks(
    ) -> impl futures::Stream<Item = Result<Vec<u8>, std::io::Error>> {
        futures::stream::unfold(0u8, |step| async move {
            match step {
                0 => Some((Ok(b"first-chunk\n".to_vec()), 1)),
                1 => {
                    tokio::time::sleep(Duration::from_millis(600)).await;
                    Some((Ok(b"second-chunk\n".to_vec()), 2))
                }
                _ => None,
            }
        })
    }

    async fn hub_with_mock(
        reply: MockReply,
        inject_temp: Option<f64>,
    ) -> (MiddlewareHub, Arc<StdMutex<Vec<Captured>>>, String, TempDir) {
        let captured = Arc::new(StdMutex::new(Vec::new()));
        let upstream = mock_upstream(Arc::clone(&captured), reply).await;
        let hub = MiddlewareHub::default();
        let audit_dir = TempDir::new().expect("tempdir");
        let base = hub
            .ensure(
                "test-model",
                &upstream,
                &InjectParams {
                    temperature: inject_temp,
                },
                audit_dir.path(),
            )
            .await
            .expect("ensure");
        (hub, captured, base, audit_dir)
    }

    #[tokio::test]
    async fn injects_temperature_strips_hop_headers_and_audits() {
        let (_hub, captured, base, audit_dir) =
            hub_with_mock(MockReply::Json200, Some(0.0)).await;
        let client = reqwest::Client::new();
        let resp = client
            .post(format!("{base}/responses"))
            .header("proxy-authorization", "basic sesame")
            .header("x-keep-me", "yes")
            .body(r#"{"model":"m","temperature":0.7}"#)
            .send()
            .await
            .expect("send");
        assert_eq!(resp.status().as_u16(), 200);

        let reqs = captured.lock().expect("lock");
        assert_eq!(reqs.len(), 1);
        let got = &reqs[0];
        assert_eq!((got.method.as_str(), got.path.as_str()), ("POST", "/v1/responses"));
        let body: serde_json::Value = serde_json::from_slice(&got.body).expect("json");
        assert_eq!(body["temperature"], serde_json::json!(0.0), "0.7 必须被覆写为 0");
        let names: Vec<&str> = got.headers.iter().map(|(k, _)| k.as_str()).collect();
        assert!(!names.contains(&"proxy-authorization"), "hop 头必须剥离");
        assert!(names.contains(&"x-keep-me"), "普通头必须透传");
        let content_length = got
            .headers
            .iter()
            .find(|(k, _)| k == "content-length")
            .map(|(_, v)| v.clone());
        assert_eq!(
            content_length,
            Some(got.body.len().to_string()),
            "Content-Length 必须按注入后 body 重算"
        );

        let audit_raw =
            std::fs::read_to_string(audit_dir.path().join("test-model.jsonl")).expect("audit");
        let line: serde_json::Value =
            serde_json::from_str(audit_raw.lines().next().expect("one line")).expect("jsonl");
        assert_eq!(line["injected"], serde_json::json!(true));
        assert_eq!(line["temperature_had"], serde_json::json!(0.7));
        assert_eq!(line["temperature_set"], serde_json::json!(0.0));
        assert_eq!(line["status"], serde_json::json!(200));
        assert_eq!(line["model_key"], serde_json::json!("test-model"));
    }

    #[tokio::test]
    async fn non_inject_paths_and_non_json_bodies_pass_through_untouched() {
        let (_hub, captured, base, _audit) = hub_with_mock(MockReply::Json200, Some(0.0)).await;
        let client = reqwest::Client::new();

        // 非注入路径：GET /v1/models。
        client
            .get(format!("{base}/models"))
            .send()
            .await
            .expect("get");
        // 注入路径但非 JSON body：原样透传。
        client
            .post(format!("{base}/responses"))
            .body("raw-bytes-not-json")
            .send()
            .await
            .expect("post");

        let reqs = captured.lock().expect("lock");
        assert_eq!(reqs[0].path, "/v1/models");
        assert!(reqs[0].body.is_empty());
        assert_eq!(reqs[1].body, b"raw-bytes-not-json", "非 JSON 不得被改写");
    }

    #[tokio::test]
    async fn dead_upstream_maps_to_502_and_audit_records_error() {
        // 占坑拿端口再放掉——保证拒连。
        let dead_port = {
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .expect("bind");
            listener.local_addr().expect("addr").port()
        };
        let hub = MiddlewareHub::default();
        let audit_dir = TempDir::new().expect("tempdir");
        let base = hub
            .ensure(
                "dead-model",
                &format!("http://127.0.0.1:{dead_port}/v1"),
                &InjectParams {
                    temperature: Some(0.0),
                },
                audit_dir.path(),
            )
            .await
            .expect("ensure");

        let resp = reqwest::Client::new()
            .post(format!("{base}/responses"))
            .body("{}")
            .send()
            .await
            .expect("send");
        assert_eq!(resp.status().as_u16(), 502);

        let audit_raw =
            std::fs::read_to_string(audit_dir.path().join("dead-model.jsonl")).expect("audit");
        let line: serde_json::Value =
            serde_json::from_str(audit_raw.lines().next().expect("line")).expect("jsonl");
        assert_eq!(line["status"], serde_json::json!(502));
        assert!(
            line["upstream_error"].as_str().is_some_and(|s| !s.is_empty()),
            "502 必须留上游错误现场：{line}"
        );
    }

    #[tokio::test]
    async fn streaming_response_is_not_buffered() {
        use futures::StreamExt;
        let (_hub, _captured, base, _audit) =
            hub_with_mock(MockReply::SlowChunks, Some(0.0)).await;
        let started = Instant::now();
        let resp = reqwest::Client::new()
            .post(format!("{base}/responses"))
            .body("{}")
            .send()
            .await
            .expect("send");
        let mut stream = resp.bytes_stream();
        let first = stream.next().await.expect("first chunk").expect("bytes");
        let first_at = started.elapsed();
        assert!(
            first.starts_with(b"first-chunk"),
            "首块内容: {:?}",
            String::from_utf8_lossy(&first)
        );
        assert!(
            first_at < Duration::from_millis(400),
            "首块应在第二块（600ms 延迟）前到达——被缓冲则 >600ms：{first_at:?}"
        );
        let mut rest = Vec::new();
        while let Some(chunk) = stream.next().await {
            rest.extend_from_slice(&chunk.expect("bytes"));
        }
        assert!(
            rest.ends_with(b"second-chunk\n"),
            "第二块必须完整到达: {:?}",
            String::from_utf8_lossy(&rest)
        );
    }

    #[tokio::test]
    async fn hub_reuses_listener_per_model() {
        let (hub, _captured, base, audit_dir) = hub_with_mock(MockReply::Json200, Some(0.0)).await;
        let again = hub
            .ensure(
                "test-model",
                "http://127.0.0.1:1/v1", // 故意给个坏上游：命中缓存就不该被用到
                &InjectParams {
                    temperature: Some(0.0),
                },
                audit_dir.path(),
            )
            .await
            .expect("ensure again");
        assert_eq!(again, base, "同模型必须复用同一监听器");
    }

    #[test]
    fn target_url_strips_v1_prefix_against_any_base_shape() {
        assert_eq!(
            target_url("http://h:1/v1", "/v1/responses?a=1"),
            "http://h:1/v1/responses?a=1"
        );
        assert_eq!(
            target_url("http://h:1/api", "/v1/responses"),
            "http://h:1/api/responses",
            "异形上游 base 也不丢路径段"
        );
        assert_eq!(target_url("http://h:1/v1", "/health"), "http://h:1/v1/health");
    }

    #[test]
    fn inject_path_matching_is_exact_per_prototype_family() {
        assert!(is_inject_path("/v1/responses"));
        assert!(is_inject_path("/v1/chat/completions?x=1"));
        assert!(!is_inject_path("/v1/models"));
        assert!(!is_inject_path("/v1/responses/extra"));
    }
}
