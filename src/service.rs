use crate::{chat_to_sse, translate_request, ChatStreamAssembler, SsePatcher};
use axum::{
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, OriginalUri, State},
    http::{HeaderMap, HeaderValue, Method as HttpMethod, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
    Json, Router,
};
use futures_util::StreamExt;
use reqwest::Client;
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Mutex, RwLock};

pub const DEFAULT_UPSTREAM_BASE: &str = "https://opencode.ai/zen/go/v1";
pub const DEFAULT_FREE_BASE: &str = "https://opencode.ai/zen/v1";
pub const DEFAULT_PORT: u16 = 18_899;

const DEFAULT_TRANSLATE_MODELS: [&str; 3] = ["ox-alpha-free", "qwen3.7-plus", "hy3"];
const FREE_EXTRAS: [&str; 1] = ["big-pickle"];
const FREE_REFRESH_SECS: u64 = 900;
const CONNECT_TIMEOUT_SECS: u64 = 15;
const FREE_RETRIES: usize = 2;
const TRANSIENT_STATUSES: [u16; 6] = [404, 429, 500, 502, 503, 504];
const HOP_BY_HOP: [&str; 6] = [
    "host",
    "content-length",
    "accept-encoding",
    "connection",
    "transfer-encoding",
    "proxy-connection",
];
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36";
const DEFAULT_MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;
const DEFAULT_MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_MAX_SSE_EVENT_BYTES: usize = 1024 * 1024;

#[derive(Clone)]
pub struct ProxyConfig {
    upstream_base: String,
    translate_models: HashSet<String>,
    free_base: Option<String>,
    free_fallback: bool,
    retries: usize,
    timeout_secs: u64,
    default_max_tokens: u64,
    free_cache: PathBuf,
    sync_models: bool,
    models_json: PathBuf,
    max_request_bytes: usize,
    max_response_bytes: usize,
    max_sse_event_bytes: usize,
    free_refresh_secs: u64,
}

impl ProxyConfig {
    pub fn new(upstream_base: impl Into<String>) -> Self {
        Self {
            upstream_base: upstream_base.into(),
            translate_models: default_translate_models(),
            free_base: None,
            free_fallback: true,
            retries: 5,
            timeout_secs: 300,
            default_max_tokens: 16_384,
            free_cache: default_free_cache_path(),
            sync_models: true,
            models_json: default_models_json_path(),
            max_request_bytes: DEFAULT_MAX_REQUEST_BYTES,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            max_sse_event_bytes: DEFAULT_MAX_SSE_EVENT_BYTES,
            free_refresh_secs: FREE_REFRESH_SECS,
        }
    }

    pub fn with_upstream_base(mut self, upstream_base: impl Into<String>) -> Self {
        self.upstream_base = upstream_base.into();
        self
    }
    pub fn with_retries(mut self, retries: usize) -> Self {
        self.retries = retries.max(1);
        self
    }
    pub fn with_timeout_secs(mut self, timeout_secs: u64) -> Self {
        self.timeout_secs = timeout_secs.max(1);
        self
    }
    pub fn with_free_base(mut self, free_base: impl Into<String>) -> Self {
        self.free_base = Some(free_base.into().trim_end_matches('/').to_owned());
        self
    }
    pub fn with_free_fallback(mut self, enabled: bool) -> Self {
        self.free_fallback = enabled;
        self
    }
    pub fn with_free_cache(mut self, path: impl Into<PathBuf>) -> Self {
        self.free_cache = path.into();
        self
    }
    pub fn with_models_json(mut self, path: impl Into<PathBuf>) -> Self {
        self.models_json = path.into();
        self
    }
    pub fn with_sync_models(mut self, enabled: bool) -> Self {
        self.sync_models = enabled;
        self
    }
    pub fn with_max_request_bytes(mut self, limit: usize) -> Self {
        self.max_request_bytes = limit.max(1);
        self
    }
    pub fn with_max_response_bytes(mut self, limit: usize) -> Self {
        self.max_response_bytes = limit.max(1);
        self
    }
    pub fn with_max_sse_event_bytes(mut self, limit: usize) -> Self {
        self.max_sse_event_bytes = limit.max(1);
        self
    }
    pub fn with_free_refresh_secs(mut self, refresh_secs: u64) -> Self {
        self.free_refresh_secs = refresh_secs;
        self
    }
    pub fn upstream_base(&self) -> &str {
        &self.upstream_base
    }
    pub fn translate_models(&self) -> &HashSet<String> {
        &self.translate_models
    }
    pub fn retries(&self) -> usize {
        self.retries
    }
    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs)
    }
    pub fn default_max_tokens(&self) -> u64 {
        self.default_max_tokens
    }
    pub fn free_fallback(&self) -> bool {
        self.free_fallback
    }
    pub fn free_base(&self) -> Option<&str> {
        self.free_base.as_deref()
    }
    pub fn free_cache(&self) -> &Path {
        &self.free_cache
    }
    pub fn sync_models(&self) -> bool {
        self.sync_models
    }
    pub fn models_json(&self) -> &Path {
        &self.models_json
    }
    pub fn max_request_bytes(&self) -> usize {
        self.max_request_bytes
    }
    pub fn max_response_bytes(&self) -> usize {
        self.max_response_bytes
    }
    pub fn max_sse_event_bytes(&self) -> usize {
        self.max_sse_event_bytes
    }
}

fn default_translate_models() -> HashSet<String> {
    DEFAULT_TRANSLATE_MODELS
        .iter()
        .map(|model| (*model).to_owned())
        .collect()
}
fn home_path(relative: &str) -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(relative)
}
fn default_free_cache_path() -> PathBuf {
    home_path(".codex/ox_proxy_free_models.json")
}
fn default_models_json_path() -> PathBuf {
    home_path(".codex/models.json")
}
fn parse_path(value: Option<&str>, default: PathBuf) -> PathBuf {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| {
            value
                .strip_prefix("~/")
                .map(home_path)
                .unwrap_or_else(|| PathBuf::from(value))
        })
        .unwrap_or(default)
}

pub fn proxy_config_from_vars<I, K, V>(variables: I) -> ProxyConfig
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: AsRef<str>,
{
    let variables: HashMap<String, String> = variables
        .into_iter()
        .map(|(key, value)| (key.as_ref().to_owned(), value.as_ref().to_owned()))
        .collect();
    let value = |key: &str| variables.get(key).map(String::as_str);
    let number = |key: &str, default: u64| {
        value(key)
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(default)
    };
    let translate_models = value("OX_TRANSLATE_MODELS")
        .map(|models| {
            models
                .split(',')
                .map(str::trim)
                .filter(|model| !model.is_empty())
                .map(str::to_owned)
                .collect::<HashSet<_>>()
        })
        .unwrap_or_else(default_translate_models);
    ProxyConfig {
        upstream_base: DEFAULT_UPSTREAM_BASE.to_owned(),
        translate_models,
        free_base: Some(
            value("OX_PROXY_FREE_BASE")
                .unwrap_or(DEFAULT_FREE_BASE)
                .trim_end_matches('/')
                .to_owned(),
        ),
        free_fallback: value("OX_PROXY_FREE_FALLBACK") != Some("0"),
        retries: number("OX_PROXY_RETRIES", 5).max(1) as usize,
        timeout_secs: number("OX_PROXY_TIMEOUT", 300).max(1),
        default_max_tokens: number("OX_PROXY_MAX_TOKENS", 16_384),
        free_cache: parse_path(value("OX_PROXY_FREE_CACHE"), default_free_cache_path()),
        sync_models: value("OX_PROXY_SYNC_MODELS") != Some("0"),
        models_json: parse_path(value("OX_PROXY_MODELS_JSON"), default_models_json_path()),
        max_request_bytes: number(
            "OX_PROXY_MAX_REQUEST_BYTES",
            DEFAULT_MAX_REQUEST_BYTES as u64,
        )
        .max(1) as usize,
        max_response_bytes: number(
            "OX_PROXY_MAX_RESPONSE_BYTES",
            DEFAULT_MAX_RESPONSE_BYTES as u64,
        )
        .max(1) as usize,
        max_sse_event_bytes: number(
            "OX_PROXY_MAX_SSE_EVENT_BYTES",
            DEFAULT_MAX_SSE_EVENT_BYTES as u64,
        )
        .max(1) as usize,
        free_refresh_secs: number("OX_PROXY_FREE_REFRESH_SECS", FREE_REFRESH_SECS),
    }
}
pub fn proxy_config_from_env() -> ProxyConfig {
    proxy_config_from_vars(std::env::vars())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupOptions {
    pub port: u16,
    pub upstream_base: String,
}
pub fn parse_startup_options<I, S>(arguments: I) -> Result<StartupOptions, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let arguments = arguments
        .into_iter()
        .map(|argument| argument.as_ref().to_owned())
        .collect::<Vec<_>>();
    if arguments.len() > 3 {
        return Err("usage: ox-sse-proxy [port] [upstream_base]".to_owned());
    }
    let port = arguments
        .get(1)
        .map(|port| {
            port.parse::<u16>()
                .map_err(|_| format!("invalid port: {port}"))
        })
        .transpose()?
        .unwrap_or(DEFAULT_PORT);
    let upstream_base = arguments
        .get(2)
        .cloned()
        .unwrap_or_else(|| DEFAULT_UPSTREAM_BASE.to_owned());
    Ok(StartupOptions {
        port,
        upstream_base,
    })
}

struct FreeModelCache {
    models: Option<HashSet<String>>,
    refreshed_at: Option<Instant>,
}
struct ProxyState {
    config: ProxyConfig,
    client: Client,
    translate_models: HashSet<String>,
    free_models: RwLock<FreeModelCache>,
    free_refresh: Mutex<()>,
}
enum UpstreamResult {
    Success(reqwest::Response),
    Status {
        status: StatusCode,
        content_type: String,
        body: Vec<u8>,
    },
    Transport(String),
    BodyTooLarge,
}

pub fn create_router(config: ProxyConfig) -> Router {
    let max_request_bytes = config.max_request_bytes;
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
        .read_timeout(config.timeout())
        .build()
        .expect("build upstream HTTP client");
    let cache_models = load_free_cache(config.free_cache());
    let cache_loaded = cache_models.is_some();
    let state = Arc::new(ProxyState {
        translate_models: config.translate_models.clone(),
        free_models: RwLock::new(FreeModelCache {
            models: cache_models,
            refreshed_at: cache_loaded.then(Instant::now),
        }),
        free_refresh: Mutex::new(()),
        client,
        config,
    });
    Router::new()
        .route("/responses", any(handle_proxy))
        .fallback(any(handle_proxy))
        .with_state(state)
        .layer(DefaultBodyLimit::max(max_request_bytes))
}

async fn handle_proxy(
    State(state): State<Arc<ProxyState>>,
    method: HttpMethod,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if body.len() > state.config.max_request_bytes {
        return json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request body exceeds configured limit",
        );
    }
    let path = uri
        .path_and_query()
        .map(|path| path.as_str())
        .unwrap_or_else(|| uri.path());
    let path_only = path.split('?').next().unwrap_or(path);
    if method == HttpMethod::POST && path_only == "/responses" {
        let Ok(mut request) = serde_json::from_slice::<Value>(&body) else {
            return json_error(StatusCode::BAD_REQUEST, "request body is not valid JSON");
        };
        sanitize_responses_input(&mut request);
        let model = request
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let is_dynamic_free = state.config.free_base().is_some()
            && may_be_dynamic_free_model(model)
            && is_known_free_model_async(&state, model).await;
        if is_dynamic_free || state.translate_models.contains(model) {
            return handle_translated(state, request, headers, is_dynamic_free).await;
        }
        return forward_upstream(
            &state,
            method,
            path,
            headers,
            Bytes::from(serde_json::to_vec(&request).unwrap_or_else(|_| body.to_vec())),
            Some(model),
        )
        .await;
    }
    forward_upstream(&state, method, path, headers, body, None).await
}
fn sanitize_responses_input(request: &mut Value) {
    let Value::Object(object) = request else {
        return;
    };
    let Some(Value::Array(items)) = object.get_mut("input") else {
        return;
    };
    crate::sanitize_input_items(items);
    crate::fix_tool_order(items);
}

async fn handle_translated(
    state: Arc<ProxyState>,
    request: Value,
    headers: HeaderMap,
    direct_free: bool,
) -> Response {
    let model = request
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let mut chat = translate_request(&request, state.config.default_max_tokens);
    chat["stream"] = json!(true);
    let (response, response_model) = if direct_free {
        let Some(free_base) = state.config.free_base.clone() else {
            return json_error(StatusCode::BAD_GATEWAY, "free channel is not configured");
        };
        let Some(alias) = free_alias(&state, &free_base, &model).await else {
            return json_error(StatusCode::BAD_GATEWAY, "free model is not available");
        };
        chat["model"] = json!(alias);
        match send_upstream(
            &state,
            HttpMethod::POST,
            &format!("{free_base}/chat/completions"),
            anonymous_headers(),
            Bytes::from(serde_json::to_vec(&chat).unwrap_or_default()),
            FREE_RETRIES,
        )
        .await
        {
            UpstreamResult::Success(response) => (response, model),
            UpstreamResult::Status {
                status,
                content_type,
                body,
            } => return status_response(status, &content_type, body),
            UpstreamResult::Transport(error) => return transport_error(&error),
            UpstreamResult::BodyTooLarge => return body_too_large_response(),
        }
    } else {
        let url = format!(
            "{}/chat/completions",
            state.config.upstream_base.trim_end_matches('/')
        );
        match send_upstream(
            &state,
            HttpMethod::POST,
            &url,
            prepare_headers(headers.clone()),
            Bytes::from(serde_json::to_vec(&chat).unwrap_or_default()),
            state.config.retries,
        )
        .await
        {
            UpstreamResult::Success(response) => (response, model.clone()),
            UpstreamResult::Status {
                status,
                content_type,
                body,
            } if state.config.free_fallback && matches!(status.as_u16(), 401 | 403) => {
                let Some(free_base) = state.config.free_base.clone() else {
                    return status_response(status, &content_type, body);
                };
                let Some(alias) = free_alias(&state, &free_base, &model).await else {
                    eprintln!("[proxy] no free alias for model {model}");
                    return status_response(status, &content_type, body);
                };
                chat["model"] = json!(alias);
                match send_upstream(
                    &state,
                    HttpMethod::POST,
                    &format!("{free_base}/chat/completions"),
                    anonymous_headers(),
                    Bytes::from(serde_json::to_vec(&chat).unwrap_or_default()),
                    FREE_RETRIES,
                )
                .await
                {
                    UpstreamResult::Success(response) => {
                        eprintln!("[proxy] translate {model} fell back to free channel {alias}");
                        (response, model)
                    }
                    UpstreamResult::Status {
                        status,
                        content_type,
                        body,
                    } => {
                        eprintln!("[proxy] free fallback for {model} failed: {status}");
                        return status_response(status, &content_type, body);
                    }
                    UpstreamResult::Transport(error) => return transport_error(&error),
                    UpstreamResult::BodyTooLarge => return body_too_large_response(),
                }
            }
            UpstreamResult::Status {
                status,
                content_type,
                body,
            } => return status_response(status, &content_type, body),
            UpstreamResult::Transport(error) => return transport_error(&error),
            UpstreamResult::BodyTooLarge => return body_too_large_response(),
        }
    };
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    if content_type.contains("text/event-stream") {
        return translated_sse_response(
            response,
            Some(&response_model),
            state.config.max_sse_event_bytes,
        );
    }
    let response_log_target = log_target(response.url().as_str());
    match read_bounded_body(response, state.config.max_response_bytes).await {
        Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
            Ok(chat) => sse_response(chat_to_sse(&chat, &response_model)),
            Err(error) => json_error(
                StatusCode::BAD_GATEWAY,
                &format!("bad upstream response: {error}"),
            ),
        },
        Err(BodyReadError::TooLarge) => body_too_large_response(),
        Err(BodyReadError::Transport(_)) => {
            eprintln!("[proxy] {response_log_target} response read failed");
            transport_error("upstream response read failed")
        }
    }
}

async fn forward_upstream(
    state: &Arc<ProxyState>,
    method: HttpMethod,
    path: &str,
    headers: HeaderMap,
    body: Bytes,
    model: Option<&str>,
) -> Response {
    let url = format!(
        "{}{}",
        state.config.upstream_base.trim_end_matches('/'),
        path
    );
    match send_upstream(
        state,
        method,
        &url,
        prepare_headers(headers),
        body,
        state.config.retries,
    )
    .await
    {
        UpstreamResult::Success(response) => {
            let status =
                StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let content_type = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("application/octet-stream")
                .to_owned();
            if status == StatusCode::OK
                && content_type.contains("text/event-stream")
                && path.split('?').next().unwrap_or(path) == "/responses"
            {
                return passthrough_sse_response(response, model, state.config.max_sse_event_bytes);
            }
            match read_bounded_body(response, state.config.max_response_bytes).await {
                Ok(body) => status_response(status, &content_type, body.to_vec()),
                Err(BodyReadError::TooLarge) => body_too_large_response(),
                Err(BodyReadError::Transport(_)) => {
                    eprintln!("[proxy] {} response read failed", log_target(&url));
                    transport_error("upstream response read failed")
                }
            }
        }
        UpstreamResult::Status {
            status,
            content_type,
            body,
        } => status_response(status, &content_type, body),
        UpstreamResult::Transport(error) => {
            eprintln!("[proxy] {} transport failure", log_target(&url));
            transport_error(&error)
        }
        UpstreamResult::BodyTooLarge => body_too_large_response(),
    }
}

async fn send_upstream(
    state: &Arc<ProxyState>,
    method: HttpMethod,
    url: &str,
    mut headers: HeaderMap,
    body: Bytes,
    retries: usize,
) -> UpstreamResult {
    headers
        .entry(reqwest::header::USER_AGENT)
        .or_insert(HeaderValue::from_static(USER_AGENT));
    let method =
        reqwest::Method::from_bytes(method.as_str().as_bytes()).unwrap_or(reqwest::Method::GET);
    for attempt in 0..retries.max(1) {
        match state
            .client
            .request(method.clone(), url)
            .headers(headers.clone())
            .body(body.clone())
            .send()
            .await
        {
            Ok(response) => {
                let status = response.status();
                let retryable = TRANSIENT_STATUSES.contains(&status.as_u16());
                if status.is_success() {
                    return UpstreamResult::Success(response);
                }
                if !retryable || attempt + 1 == retries.max(1) {
                    let content_type = response
                        .headers()
                        .get(reqwest::header::CONTENT_TYPE)
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or("application/json")
                        .to_owned();
                    return match read_bounded_body(response, state.config.max_response_bytes).await
                    {
                        Ok(body) => UpstreamResult::Status {
                            status: StatusCode::from_u16(status.as_u16())
                                .unwrap_or(StatusCode::BAD_GATEWAY),
                            content_type,
                            body: body.to_vec(),
                        },
                        Err(BodyReadError::TooLarge) => UpstreamResult::BodyTooLarge,
                        Err(BodyReadError::Transport(error)) => {
                            eprintln!("[proxy] {} response read failed", log_target(url));
                            UpstreamResult::Transport(error)
                        }
                    };
                }
                eprintln!(
                    "[proxy] {} attempt {} failed: {status}",
                    log_target(url),
                    attempt + 1
                );
            }
            Err(error) => {
                if attempt + 1 == retries.max(1) {
                    eprintln!("[proxy] {} transport failure", log_target(url));
                    return UpstreamResult::Transport(error.to_string());
                }
                eprintln!(
                    "[proxy] {} transport attempt {} failed",
                    log_target(url),
                    attempt + 1
                );
            }
        }
        tokio::time::sleep(backoff_delay(attempt)).await;
    }
    UpstreamResult::Transport("retries exhausted".to_owned())
}
fn backoff_delay(attempt: usize) -> Duration {
    let base = 0.15 * 1.5_f64.powi(attempt.min(12) as i32);
    Duration::from_secs_f64((base + rand::random::<f64>() * 0.05).min(10.0))
}
fn log_target(url: &str) -> String {
    reqwest::Url::parse(url)
        .map(|url| format!("{} {}", url.host_str().unwrap_or("upstream"), url.path()))
        .unwrap_or_else(|_| "/upstream".to_owned())
}
fn prepare_headers(headers: HeaderMap) -> HeaderMap {
    let mut prepared = HeaderMap::new();
    for (name, value) in headers {
        let Some(name) = name else { continue };
        if HOP_BY_HOP
            .iter()
            .any(|blocked| name.as_str().eq_ignore_ascii_case(blocked))
        {
            continue;
        }
        prepared.insert(name, value);
    }
    prepared.insert(
        reqwest::header::ACCEPT_ENCODING,
        HeaderValue::from_static("identity"),
    );
    prepared
}
fn anonymous_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        reqwest::header::AUTHORIZATION,
        HeaderValue::from_static("Bearer public"),
    );
    headers.insert(
        reqwest::header::USER_AGENT,
        HeaderValue::from_static(USER_AGENT),
    );
    headers.insert(
        reqwest::header::ACCEPT,
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(
        reqwest::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(
        reqwest::header::ACCEPT_ENCODING,
        HeaderValue::from_static("identity"),
    );
    headers
}

async fn is_known_free_model_async(state: &Arc<ProxyState>, model: &str) -> bool {
    let Some(base) = state.config.free_base.as_deref() else {
        return false;
    };
    ensure_free_models(state, base).await.contains(model)
}
async fn ensure_free_models(state: &Arc<ProxyState>, free_base: &str) -> HashSet<String> {
    let needed = {
        let cache = state.free_models.read().await;
        cache.models.is_none()
            || cache.refreshed_at.is_none_or(|at| {
                at.elapsed() >= Duration::from_secs(state.config.free_refresh_secs)
            })
    };
    if !needed {
        return state
            .free_models
            .read()
            .await
            .models
            .clone()
            .unwrap_or_default();
    }
    let _guard = state.free_refresh.lock().await;
    let needed = {
        let cache = state.free_models.read().await;
        cache.models.is_none()
            || cache.refreshed_at.is_none_or(|at| {
                at.elapsed() >= Duration::from_secs(state.config.free_refresh_secs)
            })
    };
    if needed {
        let fetched = fetch_free_models(state, free_base).await;
        let models_to_persist = {
            let mut cache = state.free_models.write().await;
            cache.refreshed_at = Some(Instant::now());
            if let Some(models) = fetched {
                cache.models = Some(models.clone());
                Some(models)
            } else {
                None
            }
        };
        // Keep the single-flight guard scoped to discovery and cache publication
        // only. Filesystem persistence is deliberately offloaded and must not
        // make other requests wait on this async mutex.
        drop(_guard);
        if let Some(models) = models_to_persist {
            let config = state.config.clone();
            if let Err(error) = tokio::task::spawn_blocking(move || {
                save_free_cache(config.free_cache(), &models);
                sync_models_json(&config, &models);
            })
            .await
            {
                eprintln!("[proxy] free model persistence task failed: {error}");
            }
        }
    }
    state
        .free_models
        .read()
        .await
        .models
        .clone()
        .unwrap_or_default()
}
async fn free_alias(state: &Arc<ProxyState>, free_base: &str, model: &str) -> Option<String> {
    let free = ensure_free_models(state, free_base).await;
    if free.contains(model) {
        return Some(model.to_owned());
    }
    let suffixed = format!("{model}-free");
    if free.contains(&suffixed) {
        return Some(suffixed);
    }
    (model == "ox-alpha-free" && free.contains("x-preview-f-free"))
        .then(|| "x-preview-f-free".to_owned())
}
async fn fetch_free_models(state: &Arc<ProxyState>, free_base: &str) -> Option<HashSet<String>> {
    let response = state
        .client
        .get(format!("{free_base}/models"))
        .timeout(Duration::from_secs(30))
        .header(reqwest::header::AUTHORIZATION, "Bearer public")
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        eprintln!("[proxy] free model discovery failed: {}", response.status());
        return None;
    }
    let data = read_bounded_body(response, state.config.max_response_bytes)
        .await
        .ok()?;
    let data: Value = serde_json::from_slice(&data).ok()?;
    let values = data
        .get("data")
        .and_then(Value::as_array)
        .cloned()
        .or_else(|| data.as_array().cloned())
        .unwrap_or_default();
    Some(
        values
            .into_iter()
            .filter_map(|value| value.get("id").and_then(Value::as_str).map(str::to_owned))
            .filter(|id| is_dynamic_free_id(id))
            .collect(),
    )
}
fn may_be_dynamic_free_model(model: &str) -> bool {
    model == "big-pickle" || (model.ends_with("-free") && model != "ox-alpha-free")
}
fn is_dynamic_free_id(id: &str) -> bool {
    (id.ends_with("-free") && id != "ox-alpha-free") || FREE_EXTRAS.contains(&id)
}

fn load_free_cache(path: &Path) -> Option<HashSet<String>> {
    let data: Value = serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
    let values = data.get("free").and_then(Value::as_array)?;
    Some(
        values
            .iter()
            .filter_map(Value::as_str)
            .filter(|id| is_dynamic_free_id(id))
            .map(str::to_owned)
            .collect(),
    )
}
fn atomic_write(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    fs::create_dir_all(parent)?;
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let temporary = path.with_extension(format!("tmp-{}-{suffix}", std::process::id()));
    fs::write(&temporary, contents)?;
    fs::rename(temporary, path)
}
fn save_free_cache(path: &Path, models: &HashSet<String>) {
    let data = json!({"free": models.iter().collect::<Vec<_>>()});
    if let Err(error) = serde_json::to_vec_pretty(&data)
        .map_err(std::io::Error::other)
        .and_then(|bytes| atomic_write(path, &bytes))
    {
        eprintln!(
            "[proxy] free cache save failed for {}: {error}",
            path.display()
        );
    }
}
fn sync_models_json(config: &ProxyConfig, free: &HashSet<String>) {
    if !config.sync_models {
        return;
    }
    let path = config.models_json();
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => {
            eprintln!(
                "[proxy] models.json sync skipped (read {}): {error}",
                path.display()
            );
            return;
        }
    };
    let Ok(mut data) = serde_json::from_str::<Value>(&text) else {
        eprintln!(
            "[proxy] models.json sync skipped: invalid JSON in {}",
            path.display()
        );
        return;
    };
    let Some(models) = data.get_mut("models").and_then(Value::as_array_mut) else {
        eprintln!(
            "[proxy] models.json sync skipped: missing models array in {}",
            path.display()
        );
        return;
    };
    let mut have = models
        .iter()
        .filter_map(|model| model.get("slug").and_then(Value::as_str).map(str::to_owned))
        .collect::<HashSet<_>>();
    let template = models
        .iter()
        .find(|model| {
            model
                .get("slug")
                .and_then(Value::as_str)
                .is_some_and(|slug| slug.ends_with("-free") && slug != "ox-alpha-free")
        })
        .cloned()
        .or_else(|| {
            models
                .iter()
                .find(|model| model.get("slug").and_then(Value::as_str) == Some("ox-alpha-free"))
                .cloned()
        });
    let Some(template) = template else {
        eprintln!(
            "[proxy] models.json sync skipped: no compatible template in {}",
            path.display()
        );
        return;
    };
    let mut added = false;
    for id in free {
        if have.contains(id) {
            continue;
        }
        let mut entry = template.clone();
        entry["slug"] = json!(id);
        entry["display_name"] = json!(format!(
            "{} Free",
            id.trim_end_matches("-free").replace('-', " ")
        ));
        entry["description"] = json!(format!("OpenCode Zen free model ({id})."));
        entry["priority"] = json!(1);
        entry["availability_nux"] = Value::Null;
        models.push(entry);
        have.insert(id.clone());
        added = true;
    }
    if !added {
        return;
    }
    let result = serde_json::to_vec_pretty(&data)
        .map_err(std::io::Error::other)
        .and_then(|mut bytes| {
            bytes.push(b'\n');
            atomic_write(path, &bytes)
        });
    if let Err(error) = result {
        eprintln!(
            "[proxy] models.json sync failed for {}: {error}",
            path.display()
        );
    } else {
        eprintln!("[proxy] models.json synced: added free models");
    }
}

fn passthrough_sse_response(
    response: reqwest::Response,
    model: Option<&str>,
    max_event: usize,
) -> Response {
    let stream = futures_util::stream::unfold(
        SseState {
            source: Box::pin(response.bytes_stream()),
            buffer: Vec::new(),
            processor: Processor::Responses(SsePatcher::new(model)),
            max_event,
        },
        |mut state| async move {
            next_processed_chunk(&mut state)
                .await
                .map(|item| (item, state))
        },
    );
    sse_body(Body::from_stream(stream))
}
fn translated_sse_response(
    response: reqwest::Response,
    model: Option<&str>,
    max_event: usize,
) -> Response {
    let stream = futures_util::stream::unfold(
        SseState {
            source: Box::pin(response.bytes_stream()),
            buffer: Vec::new(),
            processor: Processor::Chat(ChatStreamAssembler::new(model)),
            max_event,
        },
        |mut state| async move {
            next_processed_chunk(&mut state)
                .await
                .map(|item| (item, state))
        },
    );
    sse_body(Body::from_stream(stream))
}
fn sse_response(body: String) -> Response {
    sse_body(Body::from(body))
}
fn sse_body(body: Body) -> Response {
    (
        [
            (reqwest::header::CONTENT_TYPE, "text/event-stream"),
            (reqwest::header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}
struct SseState {
    source: std::pin::Pin<Box<dyn futures_util::Stream<Item = reqwest::Result<Bytes>> + Send>>,
    buffer: Vec<u8>,
    processor: Processor,
    max_event: usize,
}
enum Processor {
    Responses(SsePatcher),
    Chat(ChatStreamAssembler),
}
async fn next_processed_chunk(state: &mut SseState) -> Option<Result<Vec<u8>, std::io::Error>> {
    loop {
        if let Some(index) = find_event_boundary(&state.buffer) {
            if index > state.max_event {
                eprintln!(
                    "[proxy] upstream SSE event exceeded {} bytes",
                    state.max_event
                );
                return Some(Err(std::io::Error::other("upstream SSE event too large")));
            }
            let block = take_block(&mut state.buffer, index);
            let output = match &mut state.processor {
                Processor::Responses(patcher) => patcher.process(&block),
                Processor::Chat(assembler) => assembler.process(&block),
            };
            if !output.is_empty() {
                return Some(Ok(output.into_bytes()));
            }
            continue;
        }
        match state.source.next().await {
            Some(Ok(chunk)) => {
                append_normalized(&mut state.buffer, &chunk);
                if state.buffer.len() > state.max_event {
                    eprintln!(
                        "[proxy] upstream SSE event exceeded {} bytes",
                        state.max_event
                    );
                    return Some(Err(std::io::Error::other("upstream SSE event too large")));
                }
            }
            Some(Err(error)) => {
                eprintln!("[proxy] upstream SSE stream error");
                return Some(Err(std::io::Error::other(error.to_string())));
            }
            None => {
                let block = String::from_utf8_lossy(&state.buffer).trim().to_owned();
                state.buffer.clear();
                if block.is_empty() {
                    if let Processor::Chat(assembler) = &mut state.processor {
                        assembler.finish_eof();
                    }
                    return None;
                }
                let output = match &mut state.processor {
                    Processor::Responses(patcher) => patcher.process(&block),
                    Processor::Chat(assembler) => {
                        let output = assembler.process(&block);
                        assembler.finish_eof();
                        eprintln!("[proxy] translate stream ended: finish_reason={:?} truncated={} completed={}", assembler.finish_reason, assembler.truncated, assembler.done());
                        output
                    }
                };
                return (!output.is_empty()).then(|| Ok(output.into_bytes()));
            }
        }
    }
}
fn find_event_boundary(buffer: &[u8]) -> Option<usize> {
    buffer.windows(2).position(|window| window == b"\n\n")
}
fn append_normalized(buffer: &mut Vec<u8>, chunk: &[u8]) {
    buffer.extend(chunk.iter().copied().filter(|byte| *byte != b'\r'));
}
fn take_block(buffer: &mut Vec<u8>, index: usize) -> String {
    let mut block = buffer.drain(..index + 2).collect::<Vec<_>>();
    block.truncate(index);
    while block.first() == Some(&b'\n') {
        block.remove(0);
    }
    String::from_utf8_lossy(&block)
        .trim_end_matches('\n')
        .to_owned()
}

enum BodyReadError {
    TooLarge,
    Transport(String),
}
async fn read_bounded_body(
    response: reqwest::Response,
    limit: usize,
) -> Result<Bytes, BodyReadError> {
    let content_length = response.content_length();
    if content_length.is_some_and(|length| length > limit as u64) {
        return Err(BodyReadError::TooLarge);
    }
    let capacity = content_length
        .and_then(|length| usize::try_from(length).ok())
        .unwrap_or_else(|| limit.min(64 * 1024));
    let mut body = Vec::with_capacity(capacity);
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| BodyReadError::Transport(error.to_string()))?;
        if body.len().saturating_add(chunk.len()) > limit {
            return Err(BodyReadError::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(body))
}
fn status_response(status: StatusCode, content_type: &str, body: Vec<u8>) -> Response {
    (
        status,
        [(
            reqwest::header::CONTENT_TYPE,
            HeaderValue::from_str(content_type)
                .unwrap_or(HeaderValue::from_static("application/json")),
        )],
        body,
    )
        .into_response()
}
fn body_too_large_response() -> Response {
    json_error(
        StatusCode::BAD_GATEWAY,
        "upstream response exceeds configured limit",
    )
}
fn transport_error(_message: &str) -> Response {
    json_error(StatusCode::BAD_GATEWAY, "upstream transport failure")
}
fn json_error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({"type": "error", "message": message}))).into_response()
}
