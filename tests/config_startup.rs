use std::collections::HashSet;
use std::time::Duration;

use ox_sse_proxy::{parse_startup_options, proxy_config_from_vars};

#[test]
fn proxy_config_uses_python_compatible_defaults() {
    let config = proxy_config_from_vars(Vec::<(&str, &str)>::new());

    assert_eq!(config.upstream_base(), "https://opencode.ai/zen/go/v1");
    assert_eq!(
        config.translate_models(),
        &HashSet::from([
            "ox-alpha-free".to_owned(),
            "qwen3.7-plus".to_owned(),
            "hy3".to_owned(),
        ])
    );
    assert_eq!(config.retries(), 5);
    assert_eq!(config.timeout(), Duration::from_secs(300));
    assert_eq!(config.default_max_tokens(), 16_384);
    assert!(config.free_fallback());
    assert_eq!(
        config.free_base().map(str::to_owned),
        Some("https://opencode.ai/zen/v1".to_owned())
    );
}

#[test]
fn proxy_config_reads_python_environment_names() {
    let config = proxy_config_from_vars([
        (
            "OX_TRANSLATE_MODELS",
            " ox-alpha-free , hy3 ,,custom-model ",
        ),
        ("OX_PROXY_RETRIES", "3"),
        ("OX_PROXY_TIMEOUT", "45"),
        ("OX_PROXY_MAX_TOKENS", "2048"),
        ("OX_PROXY_FREE_FALLBACK", "0"),
        ("OX_PROXY_FREE_BASE", "http://127.0.0.1:9000/free/"),
    ]);

    assert_eq!(
        config.translate_models(),
        &HashSet::from([
            "ox-alpha-free".to_owned(),
            "hy3".to_owned(),
            "custom-model".to_owned(),
        ])
    );
    assert_eq!(config.retries(), 3);
    assert_eq!(config.timeout(), Duration::from_secs(45));
    assert_eq!(config.default_max_tokens(), 2_048);
    assert!(!config.free_fallback());
    assert_eq!(
        config.free_base().map(str::to_owned),
        Some("http://127.0.0.1:9000/free".to_owned())
    );
}

#[test]
fn proxy_config_falls_back_when_numeric_environment_is_invalid() {
    let config = proxy_config_from_vars([
        ("OX_PROXY_RETRIES", "zero"),
        ("OX_PROXY_TIMEOUT", "-1"),
        ("OX_PROXY_MAX_TOKENS", ""),
    ]);

    assert_eq!(config.retries(), 5);
    assert_eq!(config.timeout(), Duration::from_secs(300));
    assert_eq!(config.default_max_tokens(), 16_384);
}

#[test]
fn proxy_config_reads_cache_sync_and_body_limits() {
    let config = proxy_config_from_vars([
        ("OX_PROXY_FREE_CACHE", "/tmp/ox-free-cache.json"),
        ("OX_PROXY_MODELS_JSON", "/tmp/ox-models.json"),
        ("OX_PROXY_SYNC_MODELS", "0"),
        ("OX_PROXY_MAX_REQUEST_BYTES", "2400000"),
        ("OX_PROXY_MAX_RESPONSE_BYTES", "1200000"),
        ("OX_PROXY_MAX_SSE_EVENT_BYTES", "640000"),
    ]);
    assert_eq!(
        config.free_cache().to_str(),
        Some("/tmp/ox-free-cache.json")
    );
    assert_eq!(config.models_json().to_str(), Some("/tmp/ox-models.json"));
    assert!(!config.sync_models());
    assert_eq!(config.max_request_bytes(), 2_400_000);
    assert_eq!(config.max_response_bytes(), 1_200_000);
    assert_eq!(config.max_sse_event_bytes(), 640_000);
}

#[test]
fn startup_options_parse_port_and_upstream_overrides() {
    let defaults = parse_startup_options(["ox-sse-proxy"]).unwrap();
    assert_eq!(defaults.port, 18_899);
    assert_eq!(defaults.upstream_base, "https://opencode.ai/zen/go/v1");

    let overrides =
        parse_startup_options(["ox-sse-proxy", "20001", "http://127.0.0.1:7000/v1/"]).unwrap();
    assert_eq!(overrides.port, 20_001);
    assert_eq!(overrides.upstream_base, "http://127.0.0.1:7000/v1/");

    assert!(parse_startup_options(["ox-sse-proxy", "http"]).is_err());
}
