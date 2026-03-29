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
    pub const FRONTEND_PORT: u16 = 8081;
    pub const GATE_WS_URL: &str = "wss://api.gateio.ws/ws/v4/";
    pub fn db_path() -> String {
        std::env::var("DB_PATH").unwrap_or_else(|_| "gate-arb.db".into())
    }
    pub fn db_port() -> u16 {
        std::env::var("DB_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(8082)
    }
    /// Returns true if running in paper mode (default: true).
    pub fn paper_mode() -> bool {
        std::env::var("PAPER_MODE")
            .map(|v| v != "false")
            .unwrap_or(true)
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .with_target(false)
        .compact()
        .init();

    let paper = args::paper_mode();
    info!("gate-arb starting...");
    info!("Paper mode: {}", paper);

    // Initialize risk manager
    let risk_config = risk::RiskConfig::from_env();
    let risk = Arc::new(risk::RiskManager::new(risk_config));
    risk.set_paper_mode(paper);

    // Initialize DB writer
    let db_path = args::db_path();
    let db_port = args::db_port();
    info!("Opening DB at {} (API on :{})", db_path, db_port);
    let db_writer = match db::DbWriter::new(&db_path) {
        Ok(db) => db,
        Err(e) => {
            tracing::warn!("DB init failed (continuing without persistence): {}", e);
            anyhow::bail!("DB init failed: {}", e);
        }
    };

    // Build engine
    let engine: Arc<engine::Engine<20, 20>> = Arc::new(engine::Engine::new());

    // Build strategy with DB writer and risk manager
    let mut strategy = strategy::Strategy::new(Arc::clone(&engine), 25_000_000_000);
    strategy.set_db(db_writer.clone());
    strategy.set_risk(Arc::clone(&risk));

    // Spawn DB HTTP API server
    let db_for_api = db_writer.clone();
    tokio::spawn(async move {
        if let Err(e) = db::start_http_server(db_port, db_for_api).await {
            tracing::error!("DB HTTP server error: {}", e);
        }
    });

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

    // Build gateway and inject OB sender + risk manager
    let mut gw = gateway_ws::GatewayWs::new();
    gw.set_ob_tx(ob_tx);
    gw.set_risk(Arc::clone(&risk));

    let ws_url = args::GATE_WS_URL;
    info!("Connecting to {}", ws_url);
    gw.run(args::SPOT_SYMBOL, args::PERP_SYMBOL).await?;

    Ok(())
}
