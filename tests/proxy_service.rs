use ox_sse_proxy::{create_router, ProxyConfig};
use serde_json::json;
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener as StdTcpListener, TcpStream},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tempfile::tempdir;
struct HttpRequest {
    method: String,
    path: String,
    authorization: String,
    cookie: String,
    x_api_key: String,
    proxy_authorization: String,
    body: String,
}

struct HttpResponse {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

impl HttpResponse {
    fn json(status: u16, value: &serde_json::Value) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: value.to_string().into_bytes(),
        }
    }

    fn sse(body: &'static str) -> Self {
        Self {
            status: 200,
            content_type: "text/event-stream",
            body: body.as_bytes().to_vec(),
        }
    }
}

fn read_request(stream: &mut TcpStream) -> HttpRequest {
    let mut reader = BufReader::new(stream.try_clone().expect("clone mock stream"));
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .expect("read request line");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let path = parts.next().unwrap_or_default().to_owned();

    let mut content_length = 0usize;
    let mut authorization = String::new();
    let mut cookie = String::new();
    let mut x_api_key = String::new();
    let mut proxy_authorization = String::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).expect("read header");
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(value) = line
            .strip_prefix("content-length:")
            .or_else(|| line.strip_prefix("Content-Length:"))
        {
            content_length = value.trim().parse().unwrap_or_default();
        }
        if let Some(value) = line
            .strip_prefix("authorization:")
            .or_else(|| line.strip_prefix("Authorization:"))
        {
            authorization = value.trim().to_owned();
        }
        if let Some(value) = line
            .strip_prefix("cookie:")
            .or_else(|| line.strip_prefix("Cookie:"))
        {
            cookie = value.trim().to_owned();
        }
        if let Some(value) = line
            .strip_prefix("x-api-key:")
            .or_else(|| line.strip_prefix("X-Api-Key:"))
        {
            x_api_key = value.trim().to_owned();
        }
        if let Some(value) = line
            .strip_prefix("proxy-authorization:")
            .or_else(|| line.strip_prefix("Proxy-Authorization:"))
        {
            proxy_authorization = value.trim().to_owned();
        }
    }

    let mut body = vec![0_u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body).expect("read mock body");
    }
    HttpRequest {
        method,
        path,
        authorization,
        cookie,
        x_api_key,
        proxy_authorization,
        body: String::from_utf8_lossy(&body).to_string(),
    }
}

fn write_response(stream: &mut TcpStream, response: &HttpResponse, close: bool) {
    let reason = match response.status {
        200 => "OK",
        401 => "Unauthorized",
        503 => "Service Unavailable",
        _ => "OK",
    };
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: {}\r\n\r\n",
        response.status,
        reason,
        response.content_type,
        response.body.len(),
        if close { "close" } else { "keep-alive" },
    )
    .into_bytes();
    head.extend_from_slice(&response.body);
    stream.write_all(&head).expect("write mock response");
    stream.flush().expect("flush mock response");
}

fn start_mock<F>(handler: F) -> String
where
    F: Fn(HttpRequest) -> HttpResponse + Send + Sync + 'static,
{
    let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind mock upstream");
    let address = listener
        .local_addr()
        .expect("mock upstream address")
        .to_string();
    let handler = Arc::new(handler);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Some(mut stream) = stream.ok() else {
                continue;
            };
            let handler = handler.clone();
            std::thread::spawn(move || {
                let request = read_request(&mut stream);
                let response = handler(request);
                write_response(&mut stream, &response, true);
            });
        }
    });
    format!("http://{address}")
}

async fn start_proxy(config: ProxyConfig) -> String {
    let router = create_router(config);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy");
    let address = listener.local_addr().expect("proxy address").to_string();
    tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("proxy server stopped cleanly");
    });
    format!("http://{address}")
}

#[tokio::test]
async fn passthrough_sse_patches_missing_message_items() {
    let upstream = start_mock(|request| {
        assert_eq!(request.path, "/go/responses");
        HttpResponse::sse(concat!(
            "event: response.output_text.delta\ndata: {\"delta\":\"你好\"}\n\n",
            "event: response.completed\ndata: {\"response\":{\"id\":\"r1\",\"status\":\"completed\",\"output\":[]}}\n\n",
        ))
    });
    let proxy = start_proxy(ProxyConfig::new(format!("{upstream}/go"))).await;

    let response = reqwest::Client::new()
        .post(format!("{proxy}/responses"))
        .json(&json!({"model":"deepseek-v4-flash","input":[]}))
        .send()
        .await
        .expect("send passthrough request");
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let body = response.text().await.expect("read passthrough body");

    assert!(content_type.starts_with("text/event-stream"));
    assert!(body.contains("response.output_item.added"));
    assert!(body.contains("\"type\":\"message\""));
    assert!(body.contains("\"model\":\"deepseek-v4-flash\""));
    assert!(body.contains("response.output_text.done"));
    assert!(body.contains("response.completed"));
}

#[tokio::test]
async fn translates_request_and_retries_transient_failures() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = attempts.clone();
    let upstream = start_mock(move |request| {
        assert_eq!(request.path, "/v1/chat/completions");
        let attempt = counter.fetch_add(1, Ordering::SeqCst) + 1;
        if attempt <= 2 {
            return HttpResponse::json(503, &json!({"error":"busy"}));
        }
        HttpResponse::json(
            200,
            &json!({
                "choices":[{"message":{"role":"assistant","content":"重试成功"}}],
                "usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}
            }),
        )
    });
    let proxy = start_proxy(
        ProxyConfig::new(format!("{upstream}/v1"))
            .with_retries(5)
            .with_timeout_secs(2),
    )
    .await;

    let response = reqwest::Client::new()
        .post(format!("{proxy}/responses"))
        .header(reqwest::header::COOKIE, "session=secret")
        .header("x-api-key", "api-secret")
        .header("proxy-authorization", "Basic secret")
        .json(&json!({
            "model":"ox-alpha-free",
            "instructions":"x",
            "input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]
        }))
        .send()
        .await
        .expect("send translated request");
    let status = response.status();
    let body = response.text().await.expect("read translated body");
    assert_eq!(status, reqwest::StatusCode::OK);
    assert!(body.contains("重试成功"));
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn auth_failure_falls_back_to_anonymous_free_channel() {
    let go = start_mock(|request| {
        assert_eq!(request.path, "/go/chat/completions");
        assert_ne!(request.authorization, "Bearer public");
        HttpResponse::json(401, &json!({"error":{"message":"blocked"}}))
    });
    let seen_authorization = Arc::new(Mutex::new(String::new()));
    let seen_body = Arc::new(Mutex::new(String::new()));
    let authorization = seen_authorization.clone();
    let body_capture = seen_body.clone();
    let free = start_mock(move |request| {
        if request.method == "GET" && request.path == "/free/models" {
            return HttpResponse::json(
                200,
                &json!({"data":[{"id":"x-preview-f-free"},{"id":"hy3-free"}]}),
            );
        }
        *authorization.lock().unwrap() = request.authorization.clone();
        assert!(request.cookie.is_empty());
        assert!(request.x_api_key.is_empty());
        assert!(request.proxy_authorization.is_empty());
        *body_capture.lock().unwrap() = request.body.clone();
        HttpResponse::sse(concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"免费通道\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2}}\n\n",
            "data: [DONE]\n\n",
        ))
    });
    let proxy = start_proxy(
        ProxyConfig::new(format!("{go}/go"))
            .with_free_base(format!("{free}/free"))
            .with_free_fallback(true)
            .with_retries(1),
    )
    .await;

    let response = reqwest::Client::new()
        .post(format!("{proxy}/responses"))
        .json(&json!({
            "model":"ox-alpha-free",
            "instructions":"x",
            "input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]
        }))
        .send()
        .await
        .expect("send fallback request");
    let status = response.status();
    let body = response.text().await.expect("read fallback body");

    assert_eq!(status, reqwest::StatusCode::OK, "body={body}");
    assert!(body.contains("免费通道"));
    assert_eq!(*seen_authorization.lock().unwrap(), "Bearer public");
    assert!(seen_body.lock().unwrap().contains("\"x-preview-f-free\""));
}

#[tokio::test]
async fn passthrough_sse_rejects_an_overlarge_event() {
    let upstream = start_mock(|_| HttpResponse {
        status: 200,
        content_type: "text/event-stream",
        body: format!("data: {}\n\n", "x".repeat(2_048)).into_bytes(),
    });
    let proxy = start_proxy(
        ProxyConfig::new(format!("{upstream}/go"))
            .with_retries(1)
            .with_max_sse_event_bytes(1_024),
    )
    .await;
    let send_result = reqwest::Client::new()
        .post(format!("{proxy}/responses"))
        .json(&json!({"model":"deepseek-v4-flash","input":[]}))
        .send()
        .await;
    assert!(send_result.is_err(), "stream error must be observable");
}

#[tokio::test]
async fn retries_exhaust_at_configured_count_and_return_terminal_status() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = attempts.clone();
    let upstream = start_mock(move |_| {
        counter.fetch_add(1, Ordering::SeqCst);
        HttpResponse::json(503, &json!({"error":"still busy"}))
    });
    let proxy = start_proxy(ProxyConfig::new(format!("{upstream}/go")).with_retries(3)).await;
    let response = reqwest::Client::new()
        .post(format!("{proxy}/responses"))
        .json(&json!({"model":"ox-alpha-free","input":[]}))
        .send()
        .await
        .expect("send exhausted request");
    assert_eq!(response.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn translated_sse_eof_does_not_emit_completed() {
    let upstream = start_mock(|_| {
        HttpResponse::sse("data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n")
    });
    let proxy = start_proxy(ProxyConfig::new(format!("{upstream}/v1")).with_retries(1)).await;
    let response = reqwest::Client::new()
        .post(format!("{proxy}/responses"))
        .json(&json!({"model":"ox-alpha-free","input":[]}))
        .send()
        .await
        .expect("send EOF request");
    let body = response.text().await.expect("read EOF response");
    assert!(body.contains("partial"));
    assert!(!body.contains("response.completed"));
}

#[tokio::test]
async fn dynamic_free_model_routes_directly_and_syncs_models_once() {
    let temp = tempdir().expect("temp directory");
    let cache = temp.path().join("free-cache.json");
    let models = temp.path().join("models.json");
    std::fs::write(
        &models,
        json!({
            "models": [{
                "slug": "example-free",
                "display_name": "Example",
                "supports_search_tool": false,
                "priority": 2
            }]
        })
        .to_string(),
    )
    .expect("write models template");
    let go_calls = Arc::new(AtomicUsize::new(0));
    let go_counter = go_calls.clone();
    let go = start_mock(move |_| {
        go_counter.fetch_add(1, Ordering::SeqCst);
        HttpResponse::json(500, &json!({"error":"should not use go channel"}))
    });
    let free = start_mock(|request| {
        if request.method == "GET" && request.path == "/free/models" {
            return HttpResponse::json(
                200,
                &json!({"data":[{"id":"hy3-free"},{"id":"big-pickle"},{"id":"ox-alpha-free"}]}),
            );
        }
        assert_eq!(request.authorization, "Bearer public");
        assert!(request.cookie.is_empty());
        assert!(request.x_api_key.is_empty());
        assert!(request.proxy_authorization.is_empty());
        HttpResponse::json(
            200,
            &json!({"choices":[{"message":{"role":"assistant","content":"direct free"}}]}),
        )
    });
    let proxy = start_proxy(
        ProxyConfig::new(format!("{go}/go"))
            .with_free_base(format!("{free}/free"))
            .with_retries(1)
            .with_free_cache(cache.clone())
            .with_models_json(models.clone())
            .with_sync_models(true),
    )
    .await;
    let response = reqwest::Client::new()
        .post(format!("{proxy}/responses"))
        .header(reqwest::header::COOKIE, "session=secret")
        .header("x-api-key", "api-secret")
        .header("proxy-authorization", "Basic secret")
        .json(&json!({"model":"hy3-free","input":[]}))
        .send()
        .await
        .expect("send dynamic free request");
    let status = response.status();
    let body = response.text().await.expect("read dynamic free response");
    assert_eq!(status, reqwest::StatusCode::OK);
    assert!(body.contains("direct free"));
    assert_eq!(go_calls.load(Ordering::SeqCst), 0);
    let cache_text = std::fs::read_to_string(&cache).expect("read free cache");
    assert!(cache_text.contains("hy3-free"));
    assert!(!cache_text.contains("ox-alpha-free"));
    let models_text = std::fs::read_to_string(&models).expect("read synced models");
    assert!(models_text.contains("\"slug\": \"hy3-free\""));
    assert!(models_text.contains("example-free"));

    let big_pickle = reqwest::Client::new()
        .post(format!("{proxy}/responses"))
        .json(&json!({"model":"big-pickle","input":[]}))
        .send()
        .await
        .expect("send big-pickle request");
    assert_eq!(big_pickle.status(), reqwest::StatusCode::OK);
    let ox = reqwest::Client::new()
        .post(format!("{proxy}/responses"))
        .json(&json!({"model":"ox-alpha-free","input":[]}))
        .send()
        .await
        .expect("send Go model request");
    assert_eq!(ox.status(), reqwest::StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(go_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cached_free_models_work_when_discovery_fails() {
    let temp = tempdir().expect("temp directory");
    let cache = temp.path().join("free-cache.json");
    std::fs::write(&cache, json!({"free":["hy3-free"]}).to_string()).expect("write cache");
    let go_calls = Arc::new(AtomicUsize::new(0));
    let go_counter = go_calls.clone();
    let go = start_mock(move |_| {
        go_counter.fetch_add(1, Ordering::SeqCst);
        HttpResponse::json(500, &json!({}))
    });
    let discovery_calls = Arc::new(AtomicUsize::new(0));
    let discovery_counter = discovery_calls.clone();
    let free = start_mock(move |request| {
        if request.method == "GET" {
            discovery_counter.fetch_add(1, Ordering::SeqCst);
            return HttpResponse::json(503, &json!({"error":"offline"}));
        }
        HttpResponse::json(200, &json!({"choices":[{"message":{"content":"cached"}}]}))
    });
    let proxy = start_proxy(
        ProxyConfig::new(format!("{go}/go"))
            .with_free_base(format!("{free}/free"))
            .with_free_cache(cache)
            .with_free_refresh_secs(1),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    let response = reqwest::Client::new()
        .post(format!("{proxy}/responses"))
        .json(&json!({"model":"hy3-free","input":[]}))
        .send()
        .await
        .expect("send cached request");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert!(response
        .text()
        .await
        .expect("read cached response")
        .contains("cached"));
    assert_eq!(go_calls.load(Ordering::SeqCst), 0);
    assert_eq!(discovery_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn models_sync_bad_or_missing_file_does_not_fail_free_request() {
    for schema in ["bad-json", "missing", "non-array"] {
        let temp = tempdir().expect("temp directory");
        let models = temp.path().join("models.json");
        if schema == "bad-json" {
            std::fs::write(&models, "{not-json").expect("write bad models");
        } else if schema == "non-array" {
            std::fs::write(&models, r#"{"models":{"slug":"wrong"}}"#)
                .expect("write incompatible models schema");
        }
        let before = std::fs::read(&models).ok();
        let free = start_mock(|request| {
            if request.method == "GET" {
                return HttpResponse::json(200, &json!({"data":[{"id":"hy3-free"}]}));
            }
            HttpResponse::json(200, &json!({"choices":[{"message":{"content":"ok"}}]}))
        });
        let proxy = start_proxy(
            ProxyConfig::new("http://127.0.0.1:9")
                .with_free_base(format!("{free}/free"))
                .with_free_cache(temp.path().join("cache.json"))
                .with_models_json(models.clone()),
        )
        .await;
        let response = reqwest::Client::new()
            .post(format!("{proxy}/responses"))
            .json(&json!({"model":"hy3-free","input":[]}))
            .send()
            .await
            .expect("send models sync request");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(std::fs::read(&models).ok(), before);
    }
}

#[tokio::test]
async fn primary_auth_failure_is_not_retried_and_free_fallback_uses_two_attempts() {
    let primary = Arc::new(AtomicUsize::new(0));
    let primary_counter = primary.clone();
    let go = start_mock(move |_| {
        primary_counter.fetch_add(1, Ordering::SeqCst);
        HttpResponse::json(401, &json!({"error":"auth"}))
    });
    let free_posts = Arc::new(AtomicUsize::new(0));
    let free_counter = free_posts.clone();
    let free = start_mock(move |request| {
        if request.method == "GET" {
            return HttpResponse::json(200, &json!({"data":[{"id":"x-preview-f-free"}]}));
        }
        free_counter.fetch_add(1, Ordering::SeqCst);
        HttpResponse::json(503, &json!({"error":"busy"}))
    });
    let proxy = start_proxy(
        ProxyConfig::new(format!("{go}/go"))
            .with_free_base(format!("{free}/free"))
            .with_retries(5),
    )
    .await;
    let response = reqwest::Client::new()
        .post(format!("{proxy}/responses"))
        .json(&json!({"model":"ox-alpha-free","input":[]}))
        .send()
        .await
        .expect("send auth fallback request");
    assert_eq!(response.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(primary.load(Ordering::SeqCst), 1);
    assert_eq!(free_posts.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn concurrent_dynamic_free_requests_single_flight_model_discovery() {
    let temp = tempdir().expect("temp directory");
    let discovery_calls = Arc::new(AtomicUsize::new(0));
    let calls = discovery_calls.clone();
    let free = start_mock(move |request| {
        if request.method == "GET" && request.path == "/free/models" {
            calls.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(60));
            return HttpResponse::json(200, &json!({"data":[{"id":"hy3-free"}]}));
        }
        HttpResponse::json(200, &json!({"choices":[{"message":{"content":"ok"}}]}))
    });
    let proxy = start_proxy(
        ProxyConfig::new("http://127.0.0.1:9")
            .with_free_base(format!("{free}/free"))
            .with_free_cache(temp.path().join("cache.json"))
            .with_sync_models(false),
    )
    .await;
    let client = reqwest::Client::new();
    let mut tasks = Vec::new();
    for _ in 0..6 {
        let client = client.clone();
        let url = format!("{proxy}/responses");
        tasks.push(tokio::spawn(async move {
            client
                .post(url)
                .json(&json!({"model":"hy3-free","input":[]}))
                .send()
                .await
                .expect("send concurrent request")
                .status()
        }));
    }
    for task in tasks {
        assert_eq!(task.await.expect("join request"), reqwest::StatusCode::OK);
    }
    assert_eq!(discovery_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn request_and_upstream_response_limits_are_observable() {
    let upstream = start_mock(|request| {
        if request.path.ends_with("/responses") {
            assert!(request.body.len() > 2_300_000);
            assert!(request.body.contains("\"history\""));
            return HttpResponse::json(
                200,
                &json!({"choices":[{"message":{"content":"small success"}}]}),
            );
        }
        if request.path.ends_with("/large") {
            return HttpResponse {
                status: 200,
                content_type: "application/json",
                body: vec![b'x'; 1_100_000],
            };
        }
        HttpResponse::json(200, &json!({}))
    });
    let proxy = start_proxy(
        ProxyConfig::new(format!("{upstream}/v1"))
            .with_retries(1)
            .with_max_request_bytes(4_000_000)
            .with_max_response_bytes(1_000_000),
    )
    .await;
    let large_request = json!({
        "model":"deepseek-v4-flash",
        "input":[],
        "history":"x".repeat(2_300_000)
    });
    let response = reqwest::Client::new()
        .post(format!("{proxy}/responses"))
        .json(&large_request)
        .send()
        .await
        .expect("send large request");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert!(response
        .text()
        .await
        .expect("read small success")
        .contains("small success"));

    let large_response = reqwest::Client::new()
        .get(format!("{proxy}/large"))
        .send()
        .await
        .expect("send large response request");
    assert_eq!(large_response.status(), reqwest::StatusCode::BAD_GATEWAY);

    let limited_proxy = start_proxy(
        ProxyConfig::new(format!("{upstream}/v1"))
            .with_retries(1)
            .with_max_request_bytes(1024),
    )
    .await;
    let response = reqwest::Client::new()
        .post(format!("{limited_proxy}/responses"))
        .body(large_request.to_string())
        .send()
        .await
        .expect("send over-limit request");
    assert_eq!(response.status(), reqwest::StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn transport_error_response_does_not_leak_query_secret() {
    let secret = "query-secret-should-not-leak";
    let proxy = start_proxy(
        ProxyConfig::new(format!("http://127.0.0.1:9/v1?token={secret}")).with_retries(1),
    )
    .await;
    let response = reqwest::Client::new()
        .post(format!("{proxy}/responses"))
        .json(&json!({"model":"deepseek-v4-flash","input":[]}))
        .send()
        .await
        .expect("send transport failure request");
    let status = response.status();
    let body = response.text().await.expect("read transport failure");
    assert_eq!(status, reqwest::StatusCode::BAD_GATEWAY);
    assert!(body.contains("upstream transport failure"));
    assert!(!body.contains(secret));
}

#[tokio::test]
async fn invalid_responses_json_returns_400_without_touching_upstream() {
    let touched = Arc::new(AtomicUsize::new(0));
    let counter = touched.clone();
    let upstream = start_mock(move |_| {
        counter.fetch_add(1, Ordering::SeqCst);
        HttpResponse::json(500, &json!({}))
    });
    let proxy = start_proxy(ProxyConfig::new(format!("{upstream}/go")).with_retries(1)).await;

    let response = reqwest::Client::new()
        .post(format!("{proxy}/responses"))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body("{not-json")
        .send()
        .await
        .expect("send malformed request");

    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(touched.load(Ordering::SeqCst), 0);
}
