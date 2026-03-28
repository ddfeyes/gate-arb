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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .with_target(false)
        .compact()
        .init();

    info!("gate-arb starting...");
    info!("Paper mode: {}", std::env::var("PAPER_MODE").unwrap_or_else(|_| "true".into()));

    // Build engine
    let engine = Arc::new(engine::Engine::<20, 20>::new());
    let strategy = strategy::Strategy::new(Arc::clone(&engine), 50_000_000); // 50bps = 0.5%

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

    // Connect to Gate.io and run hot path
    let gw = gateway_ws::GatewayWs::new();
    let ws_url = args::GATE_WS_URL;
    info!("Connecting to {}", ws_url);
    gw.run(args::SPOT_SYMBOL, args::PERP_SYMBOL).await?;

    Ok(())
}
