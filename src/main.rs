//! gate-arb — HFT spot-perp & funding arbitrage engine.
//!
//! Usage: cargo run --release --bin gate-arb
//!
//! Paper trading by default. Set PAPER_MODE=false to enable live trading.

use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{info, Level};
use tracing_subscriber::FmtSubscriber;

mod args {
    pub const SPOT_SYMBOL: &str = "BTC_USDT";
    pub const PERP_SYMBOL: &str = "BTC_USDT";
    pub const FRONTEND_PORT: u16 = 8080;
    pub const GATE_WS_URL: &str = "wss://api.gateio.ws/ws/v4/";
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .with_target(false)
        .compact()
        .init();

    info!("gate-arb starting...");
    info!(
        "Paper mode: {}",
        std::env::var("PAPER_MODE").unwrap_or_else(|_| "true".into())
    );

    // Build engine
    let engine = Arc::new(engine::Engine::<20, 20>::new());
    // threshold_spread_raw is an ABSOLUTE spread in Fixed64 raw units (1e8 = 1 USDT).
    // 50bps on BTC ~50k = $250 = 25_000_000_000 raw.
    // In paper mode this is just a signal threshold — no real orders.
    let _strategy = strategy::Strategy::new(Arc::clone(&engine), 25_000_000_000);

    // Build frontend broadcaster
    let frontend = Arc::new(frontend_ws::FrontendWs::new());

    // Start frontend WS server
    let fe = Arc::clone(&frontend);
    let fe_port = args::FRONTEND_PORT;
    tokio::spawn(async move {
        if let Err(e) = fe.run(fe_port).await {
            tracing::error!("Frontend WS error: {:?}", e);
        }
    });

    // Start HTTP health server on separate port
    let health_port: u16 = std::env::var("HEALTH_PORT")
        .unwrap_or_else(|_| "8081".into())
        .parse()
        .unwrap_or(8081);
    let fe_health = Arc::clone(&frontend);
    let paper_mode = std::env::var("PAPER_MODE").unwrap_or_else(|_| "true".into());
    tokio::spawn(async move {
        run_health_server(health_port, fe_health, paper_mode == "true").await;
    });

    // Connect to Gate.io and run hot path
    let gw = gateway_ws::GatewayWs::new();
    let ws_url = args::GATE_WS_URL;
    info!("Connecting to {}", ws_url);
    gw.run(args::SPOT_SYMBOL, args::PERP_SYMBOL).await?;

    Ok(())
}

/// Minimal HTTP health server — GET /health returns JSON engine state.
/// Runs on HEALTH_PORT (default 8081). No external deps needed.
async fn run_health_server(port: u16, frontend: Arc<frontend_ws::FrontendWs>, paper_mode: bool) {
    let addr = format!("0.0.0.0:{}", port);
    let listener = match TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("Health server failed to bind {}: {}", addr, e);
            return;
        }
    };
    tracing::info!("Health server listening on http://{}/health", addr);

    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let fe = Arc::clone(&frontend);
                tokio::spawn(async move { handle_health(stream, fe, paper_mode).await });
            }
            Err(e) => {
                tracing::warn!("Health server accept error: {}", e);
            }
        }
    }
}

async fn handle_health(
    mut stream: TcpStream,
    frontend: Arc<frontend_ws::FrontendWs>,
    paper_mode: bool,
) {
    let mut buf = [0u8; 512];
    let n = match stream.read(&mut buf).await {
        Ok(n) if n > 0 => n,
        _ => return,
    };
    let request = String::from_utf8_lossy(&buf[..n]);

    // Only handle GET /health
    if !request.starts_with("GET /health") {
        let res = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
        let _ = stream.write_all(res).await;
        return;
    }

    // Read state inside a tight scope — guard MUST be dropped before any await
    let response = {
        let state = frontend.state.read();
        format!(
            r#"{{"status":"ok","paper_mode":{},"spread_bps":{},"bid_price":{},"ask_price":{},"position_open":{},"recent_logs":{}}}"#,
            paper_mode,
            state.spread_bps,
            state.bid_price,
            state.ask_price,
            state.position_open,
            state.recent_logs.len()
        )
    }; // ← RwLockReadGuard dropped here, before any await

    let body_len = response.len();
    let reply = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body_len, response
    );
    let _ = stream.write_all(reply.as_bytes()).await;
}
