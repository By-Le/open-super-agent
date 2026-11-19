//! osa-server：和 `uvicorn osa.server:app --host … --port …` 一样的用法。

use std::net::SocketAddr;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_env("OSA_LOG").unwrap_or_else(|_| "info".into()))
        .with_target(false)
        .init();
    let mut host = "127.0.0.1".to_string();
    let mut port: u16 = 8765;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--host" => host = args.next().unwrap_or(host),
            "--port" => port = args.next().and_then(|p| p.parse().ok()).unwrap_or(port),
            _ => {}
        }
    }
    osa::server::migrate();
    osa::server::startup().await;
    let addr: SocketAddr = format!("{host}:{port}").parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("osa-server 在 http://{addr} 上运行");
    let mut stop = osa::server::SHUTDOWN.1.clone();
    axum::serve(listener, osa::server::app().into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(async move {
            #[cfg(unix)]
            {
                let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {},
                    _ = term.recv() => {},
                    _ = stop.changed() => {},
                }
            }
            #[cfg(not(unix))]
            {
                let _ = tokio::signal::ctrl_c().await;
            }
        })
        .await?;
    osa::server::shutdown().await;
    osa::backup::restart_if_requested(); // 网页上确认了恢复：用同样的命令行重新启动，启动时换上备份
    Ok(())
}
