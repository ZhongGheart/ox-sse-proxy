use ox_sse_proxy::{parse_startup_options, proxy_config_from_env};

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let arguments = std::env::args().collect::<Vec<_>>();
    let Ok(options) = parse_startup_options(arguments) else {
        return std::process::ExitCode::FAILURE;
    };

    let config = proxy_config_from_env().with_upstream_base(options.upstream_base);
    let address = format!("127.0.0.1:{}", options.port);
    println!(
        "ox_sse_proxy listening on {address} -> {}",
        config.upstream_base()
    );

    let listener = match tokio::net::TcpListener::bind(&address).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("failed to bind {address}: {error}");
            return std::process::ExitCode::FAILURE;
        }
    };

    if let Err(error) = axum::serve(listener, ox_sse_proxy::create_router(config)).await {
        eprintln!("proxy server stopped with error: {error}");
        return std::process::ExitCode::FAILURE;
    }

    std::process::ExitCode::SUCCESS
}
