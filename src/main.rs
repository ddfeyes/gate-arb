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
    let _subscriber = FmtSubscriber::builder()
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
    let engine: Arc<engine::Engine<20, 20>> = Arc::new(engine::Engine::new());
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

    // Create OB update channel: gateway → engine
    let (ob_tx, ob_rx) = tokio::sync::mpsc::channel(1000);

    // Spawn engine OB receiver
    engine.spawn_ob_receiver(
        ob_rx,
        args::SPOT_SYMBOL.to_string(),
        args::PERP_SYMBOL.to_string(),
    );

    // Build gateway and inject OB sender
    let gw = gateway_ws::GatewayWs::new();
    gw.set_ob_tx(ob_tx);

    let ws_url = args::GATE_WS_URL;
    info!("Connecting to {}", ws_url);
    gw.run(args::SPOT_SYMBOL, args::PERP_SYMBOL).await?;

    Ok(())
}
