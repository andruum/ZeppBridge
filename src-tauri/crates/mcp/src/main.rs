//! ZeppBridge MCP server。
//!
//! 让外部模型能查这个人自己的健康数据，而不必先把数据交出去。因此边界画得
//! 很死：
//!
//! * **只读**。用 SQLite 的 `query_only` 连接打开，写操作在连接层就被拒绝，
//!   不靠这个文件里的分支去保证。
//! * **不主动访问 Zepp 云**。默认传输是 stdio；可选 HTTP 模式只接收 MCP 请求，
//!   通过 Bearer token 认证，并且不发布任何端口。同步仍由 `zeppbridge-cli` 负责。
//! * **不吐凭据和本机路径**。返回里没有 Zepp Token、Cookie、完整账号，也没有
//!   数据目录的绝对路径——那些对回答健康问题没有帮助，泄漏出去却是实打实的。
//! * **缺失就是缺失**。没有采样的那一天不会出现在序列里，也不会补 0。
//!   单位、时区、来源和缺失值的定义全部来自 `zeppbridge_core::contract`，
//!   和 GUI、CLI、Local API 是同一份。
//!
//! 协议是 MCP 的 JSON-RPC 2.0 over stdio：一行一条消息。手写而不是引入
//! SDK，是因为这里只需要几个只读方法，而一个只读工具服务不值得为此拖进
//! 一整套运行时。
//!
//! **双时代（dual-era）。** 2026-07-28 那一版把 `initialize` / `initialized`
//! 握手整个取消了：版本、身份和能力改为每一次请求自己带在 `_meta` 里，并
//! 新增了一个 `server/discover`。旧客户端仍然只会发 `initialize`。所以这里
//! 两条都实现：
//!
//! * 收到 `initialize` -> 走 legacy 语义，按旧规矩回；
//! * 收到 `server/discover`，或者请求的 `_meta` 里带了
//!   `io.modelcontextprotocol/protocolVersion` -> 走 modern 语义。
//!
//! 只实现一边的代价是实打实的：只留 legacy，严格按新协议说话的客户端连不
//! 上；只留 modern，今天所有能用的客户端全部连不上。而这个服务本来就是
//! stateless、stdio、只读的——新协议要求的那些性质它天生就满足。

use std::{
    io::{self, BufRead, Write},
    net::SocketAddr,
    sync::Arc,
};

use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use chrono::TimeZone;
use serde_json::{json, Value};
use zeppbridge_core::contract;
use zeppbridge_core::paths;
use zeppbridge_core::storage::Database;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const MCP_TIME_CONVENTION: &str = "All timestamps are RFC 3339 and include a timezone offset. Cloud fetch times (synced_at / fetched_at) differ from sample times (start_time / timestamp) and must not be substituted for one another.";
const MCP_MISSING_VALUE_CONVENTION: &str = "No sample means missing: a field is null or the segment is absent. Missing values are never filled with zero, a previous value, or an estimate. Fewer points than days in a series means those days have no recorded data.";
const MCP_SOURCE_CONVENTION: &str = "source_scope identifies the source: device means reported by a specific watch, user_fused means combined by Zepp Cloud across devices, and unknown means undetermined. unknown is never treated as device data.";
const MCP_PRIVACY_NOTE: &str = "Read-only access to the local SQLite database. The server does not contact Zepp, open a listening port, or return credentials or absolute local paths.";

/// 现代（无握手）协议版本。
const MODERN_PROTOCOL_VERSION: &str = "2026-07-28";
/// 收到不带版本的 legacy `initialize` 时回哪一版。
const LEGACY_PROTOCOL_VERSION: &str = "2024-11-05";

/// 我们愿意按其语义作答的全部版本，新的在前。
///
/// 这个服务的表面只有 `tools/list` 和 `tools/call`，而这两个方法的形状在
/// 2024-11-05 到 2025-11-25 之间没有不兼容的变化，所以这几版都能照直支持。
/// 不在这张表里的版本会收到 `UnsupportedProtocolVersionError`——**宁可明确
/// 拒绝，也不要按一套自己没实现的语义假装答得上来。**
const SUPPORTED_PROTOCOL_VERSIONS: [&str; 5] = [
    MODERN_PROTOCOL_VERSION,
    "2025-11-25",
    "2025-06-18",
    "2025-03-26",
    LEGACY_PROTOCOL_VERSION,
];

/// `_meta` 里那几个保留键的前缀。
const META_PROTOCOL_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
const META_SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";

/// 列表结果的缓存提示。工具定义只跟着构建走，进程活着的时候不会变，
/// 但也别让客户端缓存到下一次升级之后——一小时是个既省往返又不至于
/// 让人拿着旧工具表的折中。
const LIST_TTL_MS: i64 = 3_600_000;

/// JSON-RPC 错误码。前三个是协议规定的，-32000 段是留给应用的。
const ERR_METHOD_NOT_FOUND: i64 = -32601;
const ERR_INVALID_PARAMS: i64 = -32602;
const ERR_NOT_CONFIGURED: i64 = -32001;
const ERR_DATABASE: i64 = -32002;
/// 2026-07-28 规定的 `UnsupportedProtocolVersionError`。
///
/// 它落在 -32020..-32099 这个留给规范的区段里，和上面两个应用自定义码
/// （-32000..-32019，明确被 grandfather 了）不冲突。
const ERR_UNSUPPORTED_PROTOCOL_VERSION: i64 = -32022;

const MAX_REQUEST_BYTES: usize = 1024 * 1024;

enum RequestFrame {
    Message(Vec<u8>),
    TooLarge,
}

// Drain oversized lines without retaining them, then resume at the next message.
fn read_frame(reader: &mut impl BufRead) -> io::Result<Option<RequestFrame>> {
    let mut bytes = Vec::new();
    let mut oversized = false;
    loop {
        let chunk = reader.fill_buf()?;
        if chunk.is_empty() {
            return Ok(if oversized {
                Some(RequestFrame::TooLarge)
            } else if bytes.is_empty() {
                None
            } else {
                Some(RequestFrame::Message(bytes))
            });
        }
        let newline = chunk.iter().position(|byte| *byte == b'\n');
        let count = newline.unwrap_or(chunk.len());
        if !oversized {
            if count > MAX_REQUEST_BYTES - bytes.len() {
                oversized = true;
                bytes.clear();
            } else {
                bytes.extend_from_slice(&chunk[..count]);
            }
        }
        reader.consume(count + usize::from(newline.is_some()));
        if newline.is_some() {
            return Ok(Some(if oversized {
                RequestFrame::TooLarge
            } else {
                RequestFrame::Message(bytes)
            }));
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.as_slice() == ["--http"] || args.as_slice() == ["--transport", "http"] {
        let runtime = match tokio::runtime::Runtime::new() {
            Ok(runtime) => runtime,
            Err(error) => {
                eprintln!(
                    "Could not start the async runtime: {}",
                    english_diagnostic(error.to_string())
                );
                std::process::exit(1);
            }
        };
        if let Err(error) = runtime.block_on(serve_http()) {
            eprintln!(
                "MCP HTTP server failed: {}",
                english_diagnostic(error.to_string())
            );
            std::process::exit(1);
        }
        return;
    }
    if !args.is_empty() {
        eprintln!("Usage: zeppbridge-mcp [--http | --transport http]");
        std::process::exit(2);
    }
    let stdin = io::stdin();
    let stdout = io::stdout();
    if let Err(error) = serve(&mut stdin.lock(), &mut stdout.lock()) {
        eprintln!(
            "MCP stdio server failed: {}",
            english_diagnostic(error.to_string())
        );
        std::process::exit(1);
    }
}

async fn serve_http() -> Result<(), Box<dyn std::error::Error>> {
    const AUTH_TOKEN_ENV: &str = "ZEPPBRIDGE_MCP_AUTH_TOKEN";
    const HTTP_ADDR_ENV: &str = "ZEPPBRIDGE_MCP_HTTP_ADDR";
    let token = std::env::var(AUTH_TOKEN_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("{AUTH_TOKEN_ENV} must be set for HTTP transport"))?;
    let address = std::env::var(HTTP_ADDR_ENV)
        .unwrap_or_else(|_| "127.0.0.1:8080".to_string())
        .parse::<SocketAddr>()?;
    let listener = tokio::net::TcpListener::bind(address).await?;
    eprintln!("ZeppBridge MCP HTTP listening on {address}");
    axum::serve(listener, http_router(token)).await?;
    Ok(())
}

#[derive(Clone)]
struct HttpState {
    bearer_token: Arc<str>,
}

fn http_router(token: String) -> Router {
    Router::new()
        .route("/mcp", post(http_mcp_post).get(http_mcp_get))
        .route("/healthz", get(http_healthz))
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES))
        .with_state(HttpState {
            bearer_token: Arc::from(token),
        })
}

async fn http_healthz() -> &'static str {
    "ok"
}

async fn http_mcp_get() -> StatusCode {
    // Server-to-client event streams are not needed: this stateless service only
    // replies to requests sent to the POST endpoint.
    StatusCode::METHOD_NOT_ALLOWED
}

async fn http_mcp_post(
    State(state): State<HttpState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let supplied = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    if !supplied
        .is_some_and(|value| constant_time_eq(value.as_bytes(), state.bearer_token.as_bytes()))
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if !content_type
        .split(';')
        .next()
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
    {
        return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response();
    }
    if headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            !value.split(',').any(|item| {
                let media_type = item.split(';').next().unwrap_or("").trim();
                media_type.eq_ignore_ascii_case("application/json") || media_type == "*/*"
            })
        })
    {
        return StatusCode::NOT_ACCEPTABLE.into_response();
    }

    let request_protocol_version = headers
        .get("mcp-protocol-version")
        .and_then(|value| value.to_str().ok());
    if request_protocol_version.is_some_and(|version| !version_supported(version)) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let response_protocol_version = request_protocol_version.unwrap_or("2025-03-26");

    let request: Value = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(_) => {
            return Json(json!({
                "jsonrpc": "2.0",
                "id": null,
                "error": { "code": -32700, "message": "Request is not valid JSON or UTF-8" }
            }))
            .into_response();
        }
    };
    if !request.is_object() {
        return Json(json!({
            "jsonrpc": "2.0",
            "id": null,
            "error": { "code": -32600, "message": "Request must be a JSON-RPC object" }
        }))
        .into_response();
    }

    let id = request.get("id").cloned();
    let Some(method) = request.get("method").and_then(Value::as_str) else {
        return Json(json!({
            "jsonrpc": "2.0",
            "id": id.unwrap_or(Value::Null),
            "error": { "code": -32600, "message": "Request method is required" }
        }))
        .into_response();
    };
    let Some(id) = id else {
        return StatusCode::ACCEPTED.into_response();
    };

    let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
    let (payload, result_protocol_version) = match handle(method, &params) {
        Ok(result) => {
            let version = result
                .get("protocolVersion")
                .and_then(Value::as_str)
                .or_else(|| requested_protocol_version(&params))
                .filter(|version| version_supported(version))
                .unwrap_or(response_protocol_version)
                .to_string();
            (
                json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                version,
            )
        }
        Err(error) => (
            json!({ "jsonrpc": "2.0", "id": id, "error": error.to_json() }),
            response_protocol_version.to_string(),
        ),
    };
    let mut response = Json(payload).into_response();
    if let Ok(value) = HeaderValue::from_str(&result_protocol_version) {
        response.headers_mut().insert("mcp-protocol-version", value);
    }
    response
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

fn serve(reader: &mut impl BufRead, stdout: &mut impl Write) -> io::Result<()> {
    while let Some(frame) = read_frame(reader)? {
        let parsed: Result<Value, (i64, &str)> = match frame {
            RequestFrame::Message(bytes) => {
                if bytes.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                serde_json::from_slice(&bytes)
                    .map_err(|_| (-32700, "Request is not valid JSON or UTF-8"))
            }
            RequestFrame::TooLarge => Err((-32600, "Request exceeds the 1 MiB limit")),
        };
        let request = match parsed {
            Ok(value) => value,
            Err((code, message)) => {
                writeln!(
                    stdout,
                    "{}",
                    json!({"jsonrpc":"2.0", "id":null, "error":{"code":code,"message":message}})
                )?;
                stdout.flush()?;
                continue;
            }
        };
        // Notifications have no response.
        let Some(id) = request.get("id").cloned() else {
            continue;
        };
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let params = request.get("params").cloned().unwrap_or(json!({}));
        let response = match handle(method, &params) {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(error) => json!({ "jsonrpc": "2.0", "id": id, "error": error.to_json() }),
        };
        writeln!(stdout, "{response}")?;
        stdout.flush()?;
    }
    Ok(())
}

/// 一个 JSON-RPC 错误。
///
/// 不再用裸 `(i64, String)`：`UnsupportedProtocolVersionError` 规定要带
/// `data.supported` 和 `data.requested`，客户端靠这两项挑一个双方都支持的
/// 版本重试。没有 `data` 的话它只能放弃。
#[derive(Debug)]
struct RpcError {
    code: i64,
    message: String,
    data: Option<Value>,
}

impl RpcError {
    fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    fn to_json(&self) -> Value {
        match &self.data {
            Some(data) => json!({ "code": self.code, "message": self.message, "data": data }),
            None => json!({ "code": self.code, "message": self.message }),
        }
    }
}

impl From<(i64, String)> for RpcError {
    fn from((code, message): (i64, String)) -> Self {
        Self::new(code, message)
    }
}

/// 服务器身份。modern 结果把它放在 `_meta` 里，legacy 放在 `serverInfo`。
fn server_info() -> Value {
    json!({ "name": "zeppbridge", "version": VERSION })
}

/// 第一次握手（或第一次调用）就该看到的边界和缺失值规则。
///
/// 写在这里而不是等调用方拿到一条空序列自己猜：一个模型看到「今天没有心率」
/// 时，最容易做的事就是当成 0。
fn instructions() -> String {
    format!(
        "ZeppBridge provides read-only health data. {}\nTime: {}\nMissing values: {}\nSources: {}",
        MCP_PRIVACY_NOTE, MCP_TIME_CONVENTION, MCP_MISSING_VALUE_CONVENTION, MCP_SOURCE_CONVENTION,
    )
}

fn contains_cjk(text: &str) -> bool {
    text.chars()
        .any(|ch| ('\u{4e00}'..='\u{9fff}').contains(&ch))
}

fn english_diagnostic(message: String) -> String {
    if contains_cjk(&message) {
        "The operation failed. See the local ZeppBridge diagnostics for details.".into()
    } else {
        message
    }
}

fn english_stream_label(stream: &str) -> String {
    match stream {
        "heart_rate" => "Heart rate".into(),
        "daily_summary" => "Daily summary".into(),
        "sleep" => "Sleep".into(),
        "hrv" => "Heart rate variability".into(),
        "wellness" => "Wellness metrics".into(),
        "workouts" => "Workouts".into(),
        "workout_detail" => "Workout details and routes".into(),
        "weight" => "Weight and body composition".into(),
        "vo2max" => "VO2 max".into(),
        "lactate_threshold_hr" => "Lactate threshold heart rate".into(),
        "lactate_threshold_pace" => "Lactate threshold pace".into(),
        "resting_heart_rate" => "Resting heart rate".into(),
        "training_load" => "Training load".into(),
        "blood_oxygen" => "Blood oxygen".into(),
        "breathing_rate" => "Breathing rate".into(),
        "skin_temperature" => "Skin temperature".into(),
        other => other
            .split('_')
            .map(|part| {
                let mut chars = part.chars();
                match chars.next() {
                    Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                    None => String::new(),
                }
            })
            .collect::<Vec<_>>()
            .join(" "),
    }
}

fn english_coverage_note(coverage: &Value, cadence: &str) -> String {
    match coverage["kind"].as_str().unwrap_or_default() {
        "observations" if cadence == "per_event" => {
            "Event-driven data: no record means no matching activity was recorded, not a data gap.".into()
        }
        "observations" => {
            "This metric is recorded occasionally; blank dates are expected and do not indicate missing data.".into()
        }
        _ if coverage["observed_days"].as_i64().unwrap_or(0) == 0 => {
            "No local data was observed in this time window.".into()
        }
        _ if coverage["gap_total"].as_i64().unwrap_or(0) == 0 => {
            "No gaps were observed between the first and latest recorded dates.".into()
        }
        _ => format!(
            "No data was observed on {} date(s) between the first and latest recorded dates. This may occur if the device was not worn, a sync did not run, or the source returned no data.",
            coverage["gap_total"].as_i64().unwrap_or(0)
        ),
    }
}

fn english_data_health(mut health: Value) -> Value {
    for list_name in ["streams", "occasional_metrics"] {
        if let Some(streams) = health[list_name].as_array_mut() {
            for stream in streams {
                let stream_id = stream["stream"].as_str().unwrap_or_default().to_string();
                stream["label"] = json!(english_stream_label(&stream_id));
                let cadence = stream["cadence"].as_str().unwrap_or_default().to_string();
                if let Some(coverage) = stream.get_mut("coverage") {
                    let note = english_coverage_note(coverage, &cadence);
                    coverage["note"] = json!(note);
                }
                for stage_name in ["fetch", "parse", "write"] {
                    if let Some(stage) = stream.get_mut(stage_name) {
                        if stage["message"].as_str().is_some_and(contains_cjk) {
                            let error_kind =
                                stage["error_kind"].as_str().unwrap_or_default().to_string();
                            let message = match error_kind.as_str() {
                                "auth" => "Authentication failed. Reconnect the Zepp account.",
                                "not_available" => "The source does not provide this data for the account or device.",
                                "network" => "The network request failed.",
                                "unrecognized_payload" => "The returned payload could not be interpreted.",
                                _ => "The operation failed. See the local ZeppBridge diagnostics for details.",
                            };
                            stage["message"] = json!(message);
                        }
                    }
                }
            }
        }
    }
    if let Some(actions) = health["actions"].as_array_mut() {
        for action in actions {
            let (label, reason) = match action["code"].as_str().unwrap_or_default() {
                "reauth" => ("Reconnect Zepp account", "Authentication failed for one or more data streams."),
                "reprocess" => ("Reprocess stored local payloads", "Stored payloads are pending normalization. Reprocessing is local and does not contact Zepp or change cloud sync timestamps."),
                "sync_retry" => ("Retry sync", "One or more data streams could not be fetched from Zepp."),
                "sync_first" => ("Run first sync", "No successful cloud sync has been recorded on this device."),
                "integrity_check" => ("Check database integrity", "Run SQLite integrity_check on the local database; this may take time for a large database."),
                "open_data_folder" => ("Open data folder", "The local database, backups, and exports are stored there."),
                _ => continue,
            };
            action["label"] = json!(label);
            action["reason"] = json!(reason);
        }
    }
    if let Some(object) = health.as_object_mut() {
        for value in object.values_mut() {
            scrub_cjk_strings(value);
        }
    }
    health
}

fn scrub_cjk_strings(value: &mut Value) {
    match value {
        Value::String(text) if contains_cjk(text) => {
            *text =
                "Non-English diagnostic text was omitted; see local ZeppBridge diagnostics.".into();
        }
        Value::Array(items) => items.iter_mut().for_each(scrub_cjk_strings),
        Value::Object(object) => object.values_mut().for_each(scrub_cjk_strings),
        _ => {}
    }
}

fn english_workout_insight(mut insight: Value) -> Value {
    let unsupported = match insight["unsupported_code"].as_str().unwrap_or_default() {
        "unsupported_workout_type" => Some("This workout type is not currently supported."),
        _ => None,
    };
    if let Some(reason) = unsupported {
        insight["unsupported_reason"] = json!(reason);
    } else if insight["unsupported_reason"]
        .as_str()
        .is_some_and(contains_cjk)
    {
        insight["unsupported_reason"] =
            json!("This workout type is not currently supported; see unsupported_code.");
    }
    if let Some(facts) = insight["facts"].as_array_mut() {
        for fact in facts {
            let reason = match fact["reason_code"].as_str().unwrap_or_default() {
                "weekly_zero_baseline" | "workout_zero_baseline" => Some(
                    "The previous baseline mean is zero, so relative change cannot be computed.",
                ),
                "weekly_thin_baseline" => Some(
                    "There are too few baseline days with data to make a comparison; only the current value is reported.",
                ),
                "weekly_no_recent_data" => Some(
                    "No data for this metric was recorded locally in the last 7 days.",
                ),
                "workout_thin_baseline" => Some(
                    "Too few comparable historical workouts have this metric; only the current value is reported.",
                ),
                "workout_no_value" => Some("This workout has no value for this metric."),
                _ => None,
            };
            if let Some(reason) = reason {
                fact["reason"] = json!(reason);
            } else if fact["reason"].as_str().is_some_and(contains_cjk) {
                fact["reason"] = json!("The comparison could not be completed; see the reason_code and evidence fields.");
            }
        }
    }
    insight
}

/// 请求的 `_meta` 里声明的协议版本。没有就说明这是个 legacy 客户端。
fn requested_protocol_version(params: &Value) -> Option<&str> {
    params
        .get("_meta")
        .and_then(|meta| meta.get(META_PROTOCOL_VERSION))
        .and_then(Value::as_str)
}

/// 这一版我们认不认。
fn version_supported(version: &str) -> bool {
    SUPPORTED_PROTOCOL_VERSIONS.contains(&version)
}

fn unsupported_version_error(requested: &str) -> RpcError {
    RpcError {
        code: ERR_UNSUPPORTED_PROTOCOL_VERSION,
        message: "Unsupported protocol version".to_string(),
        // 必须带上我们支持哪些版本：客户端就是靠它挑一个再重试的。
        data: Some(json!({
            "supported": SUPPORTED_PROTOCOL_VERSIONS,
            "requested": requested,
        })),
    }
}

/// 给 modern 结果盖上必需的信封：`resultType` 和 `_meta.serverInfo`。
///
/// 2026-07-28 起每个结果都**必须**有 `resultType`；legacy 结果反过来不该有，
/// 所以这一步只在 modern 那条路上做。
fn modern_result(mut result: Value) -> Value {
    if let Some(object) = result.as_object_mut() {
        object.insert("resultType".to_string(), json!("complete"));
        object.insert(
            "_meta".to_string(),
            json!({ META_SERVER_INFO: server_info() }),
        );
    }
    result
}

fn handle(method: &str, params: &Value) -> Result<Value, RpcError> {
    // `server/discover` 本身就是 modern 的入口，也是 stdio 上的时代探针：
    // 客户端拿它试一下，认得就是 modern 服务器，报未知方法就退回 initialize。
    if method == "server/discover" {
        if let Some(version) = requested_protocol_version(params) {
            if !version_supported(version) {
                return Err(unsupported_version_error(version));
            }
        }
        return Ok(modern_result(json!({
            "supportedVersions": SUPPORTED_PROTOCOL_VERSIONS,
            "capabilities": { "tools": {} },
            "instructions": instructions(),
            "ttlMs": LIST_TTL_MS,
            // 这份工具表对谁都一样：没有账号相关的内容，也不随连接变化。
            "cacheScope": "public",
        })));
    }

    // 带了 `_meta` 版本的是 modern 客户端。没带的按 legacy 处理——那是今天
    // 绝大多数客户端的样子。
    if let Some(version) = requested_protocol_version(params) {
        if !version_supported(version) {
            return Err(unsupported_version_error(version));
        }
        return match method {
            "tools/list" => Ok(modern_result(json!({
                "tools": tool_definitions(),
                "ttlMs": LIST_TTL_MS,
                "cacheScope": "public",
            }))),
            "tools/call" => call_tool(params).map(modern_result).map_err(RpcError::from),
            // `initialize` / `ping` 在这一版里已经没有了。收到它们说明客户端
            // 把两个时代混着用，明确说清楚比默默照办好。
            other => Err(RpcError::new(
                ERR_METHOD_NOT_FOUND,
                format!("Unsupported method: {other}. This server only provides read-only tools."),
            )),
        };
    }

    match method {
        "initialize" => {
            // 按 legacy 的规矩：客户端要哪一版，我们支持就回哪一版；不支持
            // 就回我们自己的，由客户端决定要不要继续。
            let requested = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .filter(|version| version_supported(version))
                .unwrap_or(LEGACY_PROTOCOL_VERSION);
            Ok(json!({
                "protocolVersion": requested,
                "capabilities": { "tools": {} },
                "serverInfo": server_info(),
                "instructions": instructions(),
            }))
        }
        "notifications/initialized" | "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_definitions() })),
        "tools/call" => call_tool(params).map_err(RpcError::from),
        other => Err(RpcError::new(
            ERR_METHOD_NOT_FOUND,
            format!("Unsupported method: {other}. This server only provides read-only tools."),
        )),
    }
}

/* ------------------------------ 工具定义 ------------------------------ */

fn tool_definitions() -> Vec<Value> {
    let missing = MCP_MISSING_VALUE_CONVENTION;
    let time = MCP_TIME_CONVENTION;
    vec![
        json!({
            "name": "list_workouts",
            "description": format!(
                "List locally saved workouts, newest first. Distance is in metres, duration is determined from start/end timestamps, and heart rate is in bpm. {missing}"
            ),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 200,
                        "default": 20,
                        "description": "Number of records to return (maximum 200)."
                    }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "get_workout_insight",
            "description": format!(
                "Return deterministic facts for one workout, including comparison with the user's baseline, baseline window, sample count, and confidence.\
                 Return facts and evidence only; do not generate conclusions. If the baseline has too few samples, report the reason rather than lowering the threshold. {missing}"
            ),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "workoutId": { "type": "string", "description": "The workoutId returned by list_workouts." }
                },
                "required": ["workoutId"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "get_metric_series",
            "description": format!(
                "Return one or more per-day metric series. Units are given by each series' unit field. {missing} {time}"
            ),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "metrics": {
                        "type": "array",
                        "items": { "type": "string", "enum": contract::metric_names() },
                        "minItems": 1,
                        "description": "Metric names. Unknown metrics are ignored."
                    },
                    "days": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 1825,
                        "default": 90,
                        "description": "Number of days to look back, including today."
                    }
                },
                "required": ["metrics"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "get_sleep_detail",
            "description": format!(
                "Return one night's sleep details. Stage durations are in minutes. Stages not reported by the device are omitted, not filled with zero. {missing}"
            ),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sleepId": { "type": "string", "description": "Sleep record ID. If omitted, return the most recent night." }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "get_sleep_for_date",
            "description": format!(
                "Return all sleep sessions assigned to the requested local sleep date, using the local date on which each session ends.\
                 Requires an ISO date and IANA timezone; the result may contain zero or multiple sessions.\
                 Stage durations are in minutes; missing stages are not filled with zero. {missing} {time}"
            ),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sleepDate": { "type": "string", "format": "date", "description": "Local sleep date, YYYY-MM-DD." },
                    "timezone": { "type": "string", "description": "IANA timezone, for example Europe/Berlin." }
                },
                "required": ["sleepDate", "timezone"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "get_data_health",
            "description": format!(
                "Report local data health: fetch, parse, and write states, coverage, and last-success times for each stream.\
                 Use it to distinguish data that was not synced from data that was absent in the period.\
                 `pending_normalization` counts raw payloads not yet processed by the current normalizer.\
                 `normalization_by_stream` reports pending, normalized, processed_without_output (processed with no output; not necessarily recognized), and quarantined (parse failed and was quarantined) counts per stream.\
                 `normalizer_replay_pending` means historical records need replay; compare `stored_normalizer_revision` with `normalizer_revision`. Derived fields such as workout type and sleep stages may be stale until replayed.\
                 Replay locally with `zeppbridge-cli reprocess` or launch the desktop app; this read-only server cannot perform replay. {time} {missing}"
            ),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "windowDays": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 365,
                        "default": 30,
                        "description": "Coverage lookback window in days."
                    }
                },
                "additionalProperties": false
            }
        }),
    ]
}

/* ------------------------------ 工具调用 ------------------------------ */

fn open_db() -> Result<(Database, u64), (i64, String)> {
    let dir = paths::resolve_data_dir().map_err(|error| {
        (
            ERR_DATABASE,
            format!("Cannot determine the data directory: {error}"),
        )
    })?;
    let db_path = dir.join("zepp.db");
    if !db_path.exists() {
        return Err((
            ERR_NOT_CONFIGURED,
            "No ZeppBridge database exists on this machine. Connect the account and sync once in the desktop app.".into(),
        ));
    }
    let bytes = std::fs::metadata(&db_path)
        .map(|meta| meta.len())
        .unwrap_or(0);
    // query_only 连接：写操作在 SQLite 层就被拒绝，只读不是靠这里的分支保证的。
    let db = Database::open_read_only(db_path)
        .map_err(|error| (ERR_DATABASE, english_diagnostic(error.user_message())))?;
    Ok((db, bytes))
}

fn call_tool(params: &Value) -> Result<Value, (i64, String)> {
    call_tool_with_db(params, open_db)
}

fn call_tool_with_db(
    params: &Value,
    open: impl FnOnce() -> Result<(Database, u64), (i64, String)>,
) -> Result<Value, (i64, String)> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or((ERR_INVALID_PARAMS, "Missing tool name.".to_string()))?;
    if !tool_definitions()
        .iter()
        .any(|tool| tool["name"].as_str() == Some(name))
    {
        return Err((ERR_METHOD_NOT_FOUND, format!("Unknown tool: {name}")));
    }
    if params
        .get("arguments")
        .is_some_and(|args| !args.is_object())
    {
        return Err((ERR_INVALID_PARAMS, "arguments must be an object".into()));
    }
    match execute_tool_with_db(name, params, open) {
        Ok(result) => Ok(result),
        Err((_code, message)) => Ok(json!({
            "content": [{"type":"text", "text":english_diagnostic(message)}],
            "isError": true,
        })),
    }
}

fn execute_tool_with_db(
    name: &str,
    params: &Value,
    open: impl FnOnce() -> Result<(Database, u64), (i64, String)>,
) -> Result<Value, (i64, String)> {
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    let (db, database_bytes) = open()?;

    let payload = match name {
        "list_workouts" => {
            let limit = args
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(20)
                .clamp(1, 200) as usize;
            let workouts = db
                .get_recent_workouts(limit)
                .map_err(|error| (ERR_DATABASE, english_diagnostic(error.user_message())))?;
            json!({
                "workouts": workouts.iter().map(|workout| json!({
                    "workoutId": workout.workout_id,
                    "type": workout.effective_type,
                    "customLabel": workout.custom_label,
                    "startTime": workout.start_time.to_rfc3339(),
                    "endTime": workout.end_time.to_rfc3339(),
                    "distanceMeters": workout.distance_meters,
                    "calories": workout.calories,
                    "avgHr": workout.avg_hr,
                    "maxHr": workout.max_hr,
                    "sourceScope": workout.source_scope,
                    "gpsAvailable": workout.gps_available,
                    "sampleCount": workout.sample_count,
                })).collect::<Vec<_>>(),
                "units": { "distance": "m", "heartRate": "bpm", "calories": "kcal" },
                "missingValues": MCP_MISSING_VALUE_CONVENTION,
            })
        }
        "get_workout_insight" => {
            let workout_id = args
                .get("workoutId")
                .and_then(Value::as_str)
                .ok_or((ERR_INVALID_PARAMS, "Missing workoutId.".to_string()))?;
            let insight = db
                .workout_insight(workout_id)
                .map_err(|error| (ERR_DATABASE, english_diagnostic(error.user_message())))?;
            let insight = serde_json::to_value(insight).map_err(|error| {
                (
                    ERR_DATABASE,
                    format!("Failed to serialize tool result: {error}"),
                )
            })?;
            english_workout_insight(insight)
        }
        "get_metric_series" => {
            let metrics: Vec<String> = args
                .get("metrics")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            if metrics.is_empty() {
                return Err((ERR_INVALID_PARAMS, "metrics must not be empty.".into()));
            }
            let days = args.get("days").and_then(Value::as_i64).unwrap_or(90);
            let series = db
                .metric_series(&metrics, days)
                .map_err(|error| (ERR_DATABASE, english_diagnostic(error.user_message())))?;
            json!({
                "series": serde_json::to_value(&series)
                    .map_err(|error| (ERR_DATABASE, format!("Failed to serialize tool result: {error}")))?,
                "requestedMetrics": metrics,
                "missingValues": MCP_MISSING_VALUE_CONVENTION,
                "time": MCP_TIME_CONVENTION,
            })
        }
        "get_sleep_detail" => {
            let session = match args.get("sleepId").and_then(Value::as_str) {
                Some(id) => db
                    .get_sleep_detail(id)
                    .map_err(|error| (ERR_DATABASE, english_diagnostic(error.user_message())))?,
                None => {
                    let latest = db
                        .get_recent_sleep_sessions(1)
                        .map_err(|error| (ERR_DATABASE, english_diagnostic(error.user_message())))?
                        .into_iter()
                        .next();
                    // The list deliberately omits stages; load the same detail
                    // as an explicit sleepId instead of returning that summary.
                    match latest {
                        Some(session) => {
                            db.get_sleep_detail(&session.sleep_id).map_err(|error| {
                                (ERR_DATABASE, english_diagnostic(error.user_message()))
                            })?
                        }
                        None => None,
                    }
                }
            };
            match session {
                Some(session) => json!({
                    "sleep": serde_json::to_value(&session)
                        .map_err(|error| (ERR_DATABASE, format!("Failed to serialize tool result: {error}")))?,
                    "units": { "stageMinutes": "min", "heartRate": "bpm" },
                    "missingValues": MCP_MISSING_VALUE_CONVENTION,
                }),
                // 「本机没有这一晚」和「这一晚没有数据」是同一句话：
                // 不返回一个各项为 0 的空壳。
                None => {
                    json!({ "sleep": Value::Null, "reason": "No matching sleep record was found locally." })
                }
            }
        }
        "get_sleep_for_date" => {
            let date_text = args
                .get("sleepDate")
                .and_then(Value::as_str)
                .ok_or((ERR_INVALID_PARAMS, "Missing sleepDate (YYYY-MM-DD).".into()))?;
            let date = chrono::NaiveDate::parse_from_str(date_text, "%Y-%m-%d").map_err(|_| {
                (
                    ERR_INVALID_PARAMS,
                    "sleepDate must use YYYY-MM-DD format.".into(),
                )
            })?;
            let timezone_text = args
                .get("timezone")
                .and_then(Value::as_str)
                .ok_or((ERR_INVALID_PARAMS, "Missing IANA timezone.".into()))?;
            let timezone = timezone_text.parse::<chrono_tz::Tz>().map_err(|_| {
                (
                    ERR_INVALID_PARAMS,
                    "timezone must be a valid IANA timezone.".into(),
                )
            })?;
            let next_date = date.succ_opt().ok_or((
                ERR_INVALID_PARAMS,
                "sleepDate is outside the supported range.".into(),
            ))?;
            let local_boundary = |day: chrono::NaiveDate| {
                let local = day.and_hms_opt(0, 0, 0).ok_or_else(|| {
                    (
                        ERR_INVALID_PARAMS,
                        "Could not construct the local date boundary.".into(),
                    )
                })?;
                match timezone.from_local_datetime(&local) {
                    chrono::LocalResult::Single(value) => Ok(value.with_timezone(&chrono::Utc)),
                    chrono::LocalResult::Ambiguous(first, second) => {
                        Ok(first.min(second).with_timezone(&chrono::Utc))
                    }
                    chrono::LocalResult::None => Err((
                        ERR_INVALID_PARAMS,
                        "Local midnight does not exist in this timezone, so the date boundary is ambiguous.".into(),
                    )),
                }
            };
            let start = local_boundary(date)?;
            let end = local_boundary(next_date)?;
            let sessions = db
                .get_sleep_sessions_ending_between(start, end)
                .map_err(|error| (ERR_DATABASE, english_diagnostic(error.user_message())))?;
            json!({
                "sleepDate": date_text,
                "timezone": timezone_text,
                "assignment": "local date on which the sleep session ends",
                "sessions": serde_json::to_value(&sessions)
                    .map_err(|error| (ERR_DATABASE, format!("Failed to serialize tool result: {error}")))?,
                "units": { "stageMinutes": "min", "heartRate": "bpm" },
                "missingValues": MCP_MISSING_VALUE_CONVENTION,
                "time": MCP_TIME_CONVENTION,
            })
        }
        "get_data_health" => {
            let window = args
                .get("windowDays")
                .and_then(Value::as_i64)
                .unwrap_or(30)
                .clamp(1, 365);
            let health = db
                .data_health(window, database_bytes)
                .map_err(|error| (ERR_DATABASE, english_diagnostic(error.user_message())))?;
            let health = serde_json::to_value(health).map_err(|error| {
                (
                    ERR_DATABASE,
                    format!("Failed to serialize tool result: {error}"),
                )
            })?;
            english_data_health(health)
        }
        other => {
            return Err((
                ERR_METHOD_NOT_FOUND,
                format!(
                    "No tool named {other} exists. This server only provides read-only queries."
                ),
            ))
        }
    };

    // MCP 的 content 是给模型读的文本；结构化数据同时放进 structuredContent，
    // 让能用结构的客户端不必再解析一遍字符串。
    let text = serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".into());
    Ok(json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": payload,
        "isError": false
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_failures_are_results_but_bad_envelopes_remain_rpc_errors() {
        for code in [ERR_DATABASE, ERR_NOT_CONFIGURED] {
            let result = call_tool_with_db(&json!({"name":"list_workouts"}), || {
                Err((code, "Local data is unavailable".into()))
            })
            .unwrap();
            assert_eq!(result["isError"], true);
            assert_eq!(result["content"][0]["text"], "Local data is unavailable");
        }
        let library = TestLibrary::new(&[]);
        let invalid = call_tool_with_db(
            &json!({"name":"get_metric_series","arguments":{"metrics":[]}}),
            || {
                Ok((
                    Database::open_read_only(library.0.join("zepp.db")).unwrap(),
                    0,
                ))
            },
        )
        .unwrap();
        assert_eq!(invalid["isError"], true);
        for params in [
            json!({}),
            json!({"name":"unknown"}),
            json!({"name":"list_workouts","arguments":[]}),
        ] {
            assert!(call_tool_with_db(&params, || panic!(
                "invalid request must not open the database"
            ))
            .is_err());
        }
    }

    #[test]
    fn request_reader_enforces_limit_and_resumes_after_bad_lines() {
        let mut input = vec![b'x'; MAX_REQUEST_BYTES + 10];
        input.extend_from_slice(b"\n\xff\n{\"jsonrpc\":\"2.0\",\"id\":9,\"method\":\"ping\"}\n");
        let mut reader = io::BufReader::with_capacity(13, input.as_slice());
        let mut output = Vec::new();
        serve(&mut reader, &mut output).unwrap();
        let responses: Vec<Value> = output
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect();
        assert_eq!(responses.len(), 3);
        assert_eq!(responses[0]["error"]["code"], -32600);
        assert_eq!(responses[1]["error"]["code"], -32700);
        assert_eq!(responses[2]["id"], 9);
        assert_eq!(responses[2]["result"], json!({}));
        let at_limit = vec![b' '; MAX_REQUEST_BYTES];
        let mut reader = io::Cursor::new(at_limit);
        assert!(
            matches!(read_frame(&mut reader).unwrap(), Some(RequestFrame::Message(bytes)) if bytes.len()==MAX_REQUEST_BYTES)
        );
    }
    use chrono::{TimeZone, Utc};
    use std::path::PathBuf;
    use zeppbridge_core::models::{SleepSession, SleepStageSlice, SourceScope};

    struct TestLibrary(PathBuf);

    impl TestLibrary {
        fn new(sessions: &[SleepSession]) -> Self {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir = std::env::temp_dir().join(format!(
                "zeppbridge-mcp-sleep-{}-{nonce}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let library = Self(dir);
            let db = Database::open_migrated(&library.0.join("zepp.db")).unwrap();
            for session in sessions {
                db.insert_sleep_session(session).unwrap();
            }
            library
        }

        fn call_sleep(&self, arguments: Value) -> Value {
            self.call_tool("get_sleep_detail", arguments)
        }

        fn call_tool(&self, name: &str, arguments: Value) -> Value {
            call_tool_with_db(&json!({ "name": name, "arguments": arguments }), || {
                let db = Database::open_read_only(self.0.join("zepp.db"))
                    .map_err(|error| (ERR_DATABASE, english_diagnostic(error.user_message())))?;
                Ok((db, 0))
            })
            .unwrap()
        }
    }

    impl Drop for TestLibrary {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn sleep_session(id: &str, day: u32, stage: Option<&str>) -> SleepSession {
        let start = Utc.with_ymd_and_hms(2026, 1, day, 20, 0, 0).unwrap();
        let end = start + chrono::Duration::minutes(60);
        SleepSession {
            sleep_id: id.into(),
            start_time: start,
            end_time: end,
            score: Some(80),
            duration_minutes: 60,
            deep_minutes: Some(30),
            light_minutes: Some(30),
            rem_minutes: None,
            awake_minutes: Some(0),
            source_scope: SourceScope::Device,
            device_id: None,
            synced_at: Some(end + chrono::Duration::hours(1)),
            time_in_bed_minutes: None,
            stages: stage
                .map(|stage| SleepStageSlice {
                    stage: stage.into(),
                    start_time: start,
                    end_time: start + chrono::Duration::minutes(30),
                    raw_mode: Some(5),
                })
                .into_iter()
                .collect(),
            wake_count: Some(1),
        }
    }

    #[test]
    fn tool_definitions_and_server_instructions_are_english() {
        let schema = serde_json::to_string(&tool_definitions()).unwrap();
        assert!(!contains_cjk(&schema));
        assert!(!contains_cjk(&instructions()));
    }

    #[test]
    fn diagnostic_errors_with_non_english_text_are_safely_rendered_in_english() {
        let text = english_diagnostic("数据不可用: 响应 items 为空".into());
        assert_eq!(
            text,
            "The operation failed. See the local ZeppBridge diagnostics for details."
        );
        assert!(!contains_cjk(&text));
    }

    #[test]
    fn data_health_tool_output_translates_localized_generated_text() {
        let library = TestLibrary::new(&[]);
        let result = library.call_tool("get_data_health", json!({ "windowDays": 7 }));
        assert_eq!(result["isError"], json!(false));
        let output = serde_json::to_string(&result["structuredContent"]).unwrap();
        assert!(
            !contains_cjk(&output),
            "MCP health output must be English: {output}"
        );
    }

    #[test]
    fn workout_insight_output_uses_stable_reason_codes_in_english() {
        let insight = json!({
            "unsupported_reason": "此运动类型暂不支持。",
            "unsupported_code": "unsupported_workout_type",
            "facts": [{
                "reason": "最近 7 天本机没有这项数据。",
                "reason_code": "weekly_no_recent_data"
            }]
        });
        let output = serde_json::to_string(&english_workout_insight(insight)).unwrap();
        assert!(!contains_cjk(&output));
        assert!(output.contains("This workout type is not currently supported."));
        assert!(output.contains("No data for this metric was recorded locally in the last 7 days."));
    }

    #[test]
    fn latest_sleep_returns_the_same_full_detail_as_an_explicit_id() {
        let older = sleep_session("older", 1, Some("light"));
        let latest = sleep_session("latest", 2, Some("deep"));
        let library = TestLibrary::new(&[older, latest.clone()]);

        let implicit = library.call_sleep(json!({}));
        let explicit = library.call_sleep(json!({ "sleepId": "latest" }));
        assert_eq!(implicit, explicit);
        assert_eq!(implicit["isError"], json!(false));
        assert_eq!(
            implicit["structuredContent"]["sleep"],
            serde_json::to_value(latest).unwrap()
        );
        let text: Value =
            serde_json::from_str(implicit["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(text, implicit["structuredContent"]);

        let previous = library.call_sleep(json!({ "sleepId": "older" }));
        assert_eq!(previous["structuredContent"]["sleep"]["sleep_id"], "older");
        assert_eq!(
            previous["structuredContent"]["sleep"]["stages"][0]["stage"],
            "light"
        );
    }

    #[test]
    fn get_sleep_for_date_uses_local_end_date_and_iana_timezone() {
        let mut matching = sleep_session("night-ending-jan-2", 1, Some("light"));
        matching.start_time = Utc.with_ymd_and_hms(2026, 1, 1, 23, 30, 0).unwrap();
        matching.end_time = Utc.with_ymd_and_hms(2026, 1, 2, 0, 30, 0).unwrap();
        let mut next_day = sleep_session("night-ending-jan-3", 2, None);
        next_day.start_time = Utc.with_ymd_and_hms(2026, 1, 2, 22, 30, 0).unwrap();
        next_day.end_time = Utc.with_ymd_and_hms(2026, 1, 2, 23, 30, 0).unwrap();
        let library = TestLibrary::new(&[matching, next_day]);

        let result = library.call_tool(
            "get_sleep_for_date",
            json!({ "sleepDate": "2026-01-02", "timezone": "Europe/Berlin" }),
        );
        assert_eq!(result["isError"], json!(false));
        assert_eq!(
            result["structuredContent"]["sleepDate"],
            json!("2026-01-02")
        );
        assert_eq!(
            result["structuredContent"]["sessions"][0]["sleep_id"],
            json!("night-ending-jan-2")
        );
        assert_eq!(
            result["structuredContent"]["sessions"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn get_sleep_for_date_rejects_invalid_date_and_timezone() {
        let library = TestLibrary::new(&[]);
        for args in [
            json!({ "sleepDate": "yesterday", "timezone": "Europe/Berlin" }),
            json!({ "sleepDate": "2026-01-02", "timezone": "Mars/Olympus" }),
        ] {
            let result = library.call_tool("get_sleep_for_date", args);
            assert_eq!(result["isError"], json!(true));
        }
    }

    #[test]
    fn sleep_queries_preserve_missing_sessions_and_missing_stages() {
        let empty = TestLibrary::new(&[]);
        let recent = empty.call_sleep(json!({}));
        assert_eq!(recent["isError"], json!(false));
        assert_eq!(recent["structuredContent"]["sleep"], Value::Null);

        let library = TestLibrary::new(&[sleep_session("no-stages", 1, None)]);
        let recent = library.call_sleep(json!({}));
        assert_eq!(
            recent["structuredContent"]["sleep"]["sleep_id"],
            "no-stages"
        );
        assert_eq!(recent["structuredContent"]["sleep"]["stages"], json!([]));
        let missing = library.call_sleep(json!({ "sleepId": "unknown" }));
        assert_eq!(missing["structuredContent"]["sleep"], Value::Null);
    }

    #[test]
    fn every_tool_declares_units_and_the_missing_value_rule() {
        // 一个不说单位的健康数据工具，等于把换算责任推给模型去猜。
        for tool in tool_definitions() {
            let description = tool["description"].as_str().unwrap_or_default();
            let name = tool["name"].as_str().unwrap_or_default();
            assert!(
                description.contains("Missing values are never filled"),
                "{name} does not explain the missing-value rule"
            );
            assert!(
                tool["inputSchema"]["additionalProperties"] == json!(false),
                "{name} 应当拒绝未知参数，避免调用方以为某个开关生效了"
            );
        }
    }

    #[test]
    fn the_tool_surface_is_read_only() {
        // 只读是这个进程存在的前提。新增任何会写库的工具都应当先推翻这条测试。
        let names: Vec<String> = tool_definitions()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap_or_default().to_string())
            .collect();
        for name in &names {
            for verb in [
                "sync", "delete", "write", "set", "update", "import", "restore",
            ] {
                assert!(
                    !name.contains(verb),
                    "{name} 看起来会改数据，不该出现在这里"
                );
            }
        }
        assert_eq!(names.len(), 6);
    }

    #[test]
    fn unknown_methods_and_tools_are_refused_rather_than_guessed() {
        let error = handle("tools/execute", &json!({})).unwrap_err();
        assert_eq!(error.code, ERR_METHOD_NOT_FOUND);
        let missing_name = call_tool(&json!({ "arguments": {} })).unwrap_err();
        assert_eq!(missing_name.0, ERR_INVALID_PARAMS);
    }

    #[test]
    fn initialize_tells_the_caller_the_privacy_boundary_up_front() {
        let result = handle("initialize", &json!({})).unwrap();
        let instructions = result["instructions"].as_str().unwrap();
        assert!(instructions.contains("does not contact Zepp"));
        assert!(instructions.contains("never filled with zero"));
        assert_eq!(result["serverInfo"]["version"], json!(VERSION));
    }

    /// 现代客户端（无握手）必须能只靠 `server/discover` 就把这台服务器认全。
    #[test]
    fn server_discover_answers_a_modern_client_without_any_handshake() {
        let result = handle(
            "server/discover",
            &json!({
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientInfo": { "name": "probe", "version": "1.0" },
                    "io.modelcontextprotocol/clientCapabilities": {}
                }
            }),
        )
        .unwrap();

        // 2026-07-28 起每个结果都必须带 resultType。
        assert_eq!(result["resultType"], json!("complete"));
        assert_eq!(
            result["supportedVersions"][0],
            json!(MODERN_PROTOCOL_VERSION)
        );
        assert_eq!(result["capabilities"]["tools"], json!({}));
        // 身份挪进了 _meta，不再是顶层的 serverInfo。
        assert_eq!(result["_meta"][META_SERVER_INFO]["version"], json!(VERSION));
        assert!(result["instructions"]
            .as_str()
            .unwrap()
            .contains("does not contact Zepp"));
        assert_eq!(result["cacheScope"], json!("public"));
    }

    /// 带了版本 `_meta` 的 `tools/list` 要按新规矩答：resultType + 缓存提示。
    #[test]
    fn a_modern_tools_list_carries_the_required_envelope() {
        let modern =
            json!({ "_meta": { "io.modelcontextprotocol/protocolVersion": "2026-07-28" } });
        let result = handle("tools/list", &modern).unwrap();
        assert_eq!(result["resultType"], json!("complete"));
        assert!(result["ttlMs"].as_i64().unwrap() > 0);
        assert_eq!(result["cacheScope"], json!("public"));
        assert_eq!(result["tools"].as_array().unwrap().len(), 6);
    }

    /// 认不出来的版本必须明确拒绝，并**把我们支持的版本列出来**——客户端就
    /// 是靠那张表挑一个再重试的。默默按某个版本作答才是最坏的结果。
    #[test]
    fn an_unknown_protocol_version_is_refused_with_a_list_to_retry_from() {
        let error = handle(
            "tools/list",
            &json!({ "_meta": { "io.modelcontextprotocol/protocolVersion": "1900-01-01" } }),
        )
        .unwrap_err();
        assert_eq!(error.code, ERR_UNSUPPORTED_PROTOCOL_VERSION);
        let data = error.data.unwrap();
        assert_eq!(data["requested"], json!("1900-01-01"));
        assert!(data["supported"]
            .as_array()
            .unwrap()
            .contains(&json!(MODERN_PROTOCOL_VERSION)));
    }

    #[tokio::test]
    async fn http_transport_requires_bearer_auth_and_handles_initialize() {
        use axum::{body::Body, http::Request};
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        let app = http_router("test-secret-token".to_string());
        let request = || {
            Request::builder()
                .method("POST")
                .uri("/mcp")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{}}}"#,
                ))
                .unwrap()
        };
        let unauthorized = app.clone().oneshot(request()).await.unwrap();
        assert_eq!(unauthorized.status(), axum::http::StatusCode::UNAUTHORIZED);

        let authorized = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer test-secret-token")
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{}}}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(authorized.status(), axum::http::StatusCode::OK);
        assert_eq!(
            authorized.headers().get("mcp-protocol-version").unwrap(),
            "2025-06-18"
        );
        let body = authorized.into_body().collect().await.unwrap().to_bytes();
        let response: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(response["result"]["serverInfo"]["name"], "zeppbridge");
    }

    #[tokio::test]
    async fn http_transport_acknowledges_notifications_without_a_body() {
        use axum::{body::Body, http::Request};
        use tower::ServiceExt;

        let response = http_router("test-secret-token".to_string())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer test-secret-token")
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::ACCEPTED);
    }

    /// 旧客户端一个字都不用改。这条测试挡的是「升级新协议顺手把老路拆了」。
    #[test]
    fn a_legacy_initialize_still_works_and_echoes_a_version_it_asked_for() {
        let result = handle(
            "initialize",
            &json!({ "protocolVersion": "2025-06-18", "capabilities": {} }),
        )
        .unwrap();
        assert_eq!(result["protocolVersion"], json!("2025-06-18"));
        assert_eq!(result["serverInfo"]["version"], json!(VERSION));
        // legacy 结果不该带 modern 的信封。
        assert!(result.get("resultType").is_none());

        // 客户端要一个我们不支持的版本时，回我们自己的，由它决定继不继续。
        let fallback = handle("initialize", &json!({ "protocolVersion": "1900-01-01" })).unwrap();
        assert_eq!(fallback["protocolVersion"], json!(LEGACY_PROTOCOL_VERSION));

        // 不带 _meta 的 tools/list 走 legacy 形状。
        let listed = handle("tools/list", &json!({})).unwrap();
        assert!(listed.get("resultType").is_none());
        assert!(listed.get("ttlMs").is_none());
    }
}
