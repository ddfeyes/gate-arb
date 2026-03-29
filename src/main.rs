//! gate-arb — HFT spot-perp & funding arbitrage engine.
//!
//! Usage: cargo run --release --bin gate-arb
//!
//! Paper trading by default. Set PAPER_MODE=false to enable live trading.

use std::sync::Arc;
use tracing::{info, Level};
use tracing_subscriber::FmtSubscriber;

mod args {
    pub const SPOT_SYMBOL: &str = "BTC_USDT";
    pub const PERP_SYMBOL: &str = "BTC_USDT";
    pub const FRONTEND_PORT: u16 = 8080;
    pub const GATE_WS_URL: &str = "wss://api.gateio.ws/ws/v4/";
    pub const HEALTH_PORT: u16 = 8081;
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

    // --- Health server setup ---
    let health = Arc::new(health::HealthHandle::new(args::HEALTH_PORT));

    // Startup self-check: verify Gate.io REST API is reachable before going live
    info!("Running startup self-check...");
    health.run_startup_check().await;
    {
        let state = health.state.read().await;
        if !state.startup_ok {
            let err = state
                .startup_error
                .clone()
                .unwrap_or_else(|| "unknown".into());
            anyhow::bail!("Startup self-check FAILED: {}. Aborting.", err);
        }
    }
    info!("Startup self-check PASSED");

    // Build engine
    let engine = Arc::new(engine::Engine::<20, 20>::new());
    health.set_engine(health::EngineStatus::Idle).await;

    // threshold_spread_raw is an ABSOLUTE spread in Fixed64 raw units (1e8 = 1 USDT).
    // 50bps on BTC ~50k = $250 = 25_000_000_000 raw.
    // In paper mode this is just a signal threshold — no real orders.
    let _strategy = strategy::Strategy::new(Arc::clone(&engine), 25_000_000_000);

    // Build frontend broadcaster
    let frontend = Arc::new(frontend_ws::FrontendWs::new());

    // Start frontend WS server
    let fe = Arc::clone(&frontend);
    let fe_port = args::FRONTEND_PORT;
    let health_fe = Arc::clone(&health);
    tokio::spawn(async move {
        if let Err(e) = fe.run(fe_port).await {
            tracing::error!("Frontend WS error: {:?}", e);
            health_fe
                .set_frontend_ws(health::FrontendStatus::Stopped)
                .await;
        }
    });
    health
        .set_frontend_ws(health::FrontendStatus::Running)
        .await;

    // Start health HTTP server
    let health_srv = Arc::clone(&health);
    tokio::spawn(async move {
        health_srv.run_server().await;
    });
    info!(
        "Health endpoint on http://0.0.0.0:{}/health",
        args::HEALTH_PORT
    );

    // Connect to Gate.io and run hot path
    let gw = gateway_ws::GatewayWs::new();
    let ws_url = args::GATE_WS_URL;
    info!("Connecting to {}", ws_url);
    health.set_gate_ws(health::GateWsStatus::Connected).await;

    let result = gw.run(args::SPOT_SYMBOL, args::PERP_SYMBOL).await;
    health.set_gate_ws(health::GateWsStatus::Disconnected).await;

    if let Err(e) = result {
        tracing::error!("Gateway WS terminated: {:?}", e);
    }

    Ok(())
}
