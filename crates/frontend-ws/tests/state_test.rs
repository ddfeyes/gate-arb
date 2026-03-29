use frontend_ws::FrontendWs;

#[test]
fn update_spread_and_prices() {
    let fw = FrontendWs::new();
    fw.update(42.5, 50000.0, 50050.0);
    let s = fw.state.read();
    assert!((s.spread_bps - 42.5).abs() < f64::EPSILON);
    assert!((s.bid_price - 50000.0).abs() < f64::EPSILON);
    assert!((s.ask_price - 50050.0).abs() < f64::EPSILON);
    assert!(s.timestamp_ms > 0);
}

#[test]
fn update_pnl_calculates_usd_and_pct() {
    let fw = FrontendWs::new();
    // 1 USDT raw = 100_000_000 raw
    fw.update_pnl(100_000_000, 5, 4, 1, true);
    let s = fw.state.read();
    assert!((s.pnl_usd - 1.0).abs() < 1e-6, "pnl_usd should be 1.0");
    // pnl_pct = 1.0 / 500.0 * 100 = 0.2%
    assert!((s.pnl_pct - 0.2).abs() < 1e-6, "pnl_pct should be 0.2%");
    assert_eq!(s.total_trades, 5);
    assert_eq!(s.wins, 4);
    assert_eq!(s.losses, 1);
    assert!(s.position_open);
}

#[test]
fn update_pnl_negative() {
    let fw = FrontendWs::new();
    fw.update_pnl(-50_000_000, 3, 1, 2, false);
    let s = fw.state.read();
    assert!((s.pnl_usd - (-0.5)).abs() < 1e-6, "pnl_usd should be -0.5");
    assert!(!s.position_open);
}

#[test]
fn update_funding_rate() {
    let fw = FrontendWs::new();
    fw.update_funding_rate(150); // 0.015% per 8h
    let s = fw.state.read();
    assert_eq!(s.funding_rate_bps, 150);
}

#[test]
fn log_appends_and_trims() {
    let fw = FrontendWs::new();
    // Add 120 entries — should trim to MAX_LOG_ENTRIES (100)
    for i in 0..120 {
        fw.log(format!("entry {}", i));
    }
    let logs = fw.logs.read();
    assert_eq!(logs.len(), 100, "ring should cap at 100 entries");
    // Oldest entry should be 20 (0-19 were evicted)
    assert_eq!(logs.front().unwrap(), "entry 20");
}

#[test]
fn state_serializes_to_json() {
    let fw = FrontendWs::new();
    fw.update(12.3, 48000.0, 48100.0);
    fw.update_pnl(500_000_000, 10, 7, 3, true);
    fw.update_funding_rate(-80);
    fw.log("test event".to_string());

    let s = fw.state.read();
    let json = serde_json::to_string(&*s).expect("should serialize");
    assert!(json.contains("spread_bps"));
    assert!(json.contains("funding_rate_bps"));
    assert!(json.contains("timestamp_ms"));
    assert!(json.contains("recent_logs"));
    assert!(json.contains("test event"));
}

#[test]
fn dashboard_html_not_empty() {
    // Smoke test: the embedded HTML is present (include_str! would fail at compile time
    // if the file is missing, but this confirms it has actual content)
    assert!(
        frontend_ws::DASHBOARD_HTML.len() > 1000,
        "embedded dashboard HTML should be substantial"
    );
    assert!(
        frontend_ws::DASHBOARD_HTML.contains("gate-arb"),
        "dashboard should reference the product name"
    );
}
