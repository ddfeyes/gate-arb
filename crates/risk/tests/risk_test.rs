use risk::{RiskConfig, RiskManager};

fn make_risk(max_drawdown_usd: f64, max_positions: usize, ping_ms: u64) -> RiskManager {
    RiskManager::new(RiskConfig {
        max_drawdown_raw: (max_drawdown_usd * 1e8) as i64,
        max_positions,
        ping_threshold_ms: ping_ms,
    })
}

#[test]
fn kill_switch_blocks_in_live_mode() {
    let rm = make_risk(100.0, 5, 200);
    rm.set_paper_mode(false);
    assert!(rm.can_open_position());
    rm.activate_kill_switch();
    assert!(!rm.can_open_position());
    rm.deactivate_kill_switch();
    assert!(rm.can_open_position());
}

#[test]
fn kill_switch_blocks_in_paper_mode_too() {
    // Kill switch is always hard — even paper mode.
    let rm = make_risk(100.0, 5, 200);
    rm.set_paper_mode(true);
    rm.activate_kill_switch();
    assert!(!rm.can_open_position());
}

#[test]
fn max_positions_blocks_in_live_mode() {
    let rm = make_risk(100.0, 2, 200);
    rm.set_paper_mode(false);
    rm.position_opened();
    assert!(rm.can_open_position()); // 1 of 2
    rm.position_opened();
    assert!(!rm.can_open_position()); // 2 of 2 → at limit
}

#[test]
fn max_positions_advisory_in_paper_mode() {
    let rm = make_risk(100.0, 1, 200);
    rm.set_paper_mode(true);
    rm.position_opened();
    rm.position_opened();
    // Paper mode: position count violations are advisory only
    assert!(rm.can_open_position());
}

#[test]
fn drawdown_blocks_in_live_mode() {
    let rm = make_risk(10.0, 5, 200);
    rm.set_paper_mode(false);
    // Gain to set peak, then drop below
    rm.update_pnl(20_000_000_000); // +$200 peak
    rm.update_pnl(-21_000_000_000); // now at -$10 from peak
    assert!(!rm.can_open_position()); // drawdown > $10 limit
}

#[test]
fn high_latency_blocks_in_both_modes() {
    for paper in [true, false] {
        let rm = make_risk(100.0, 5, 100);
        rm.set_paper_mode(paper);
        rm.update_ping_latency(101); // 1ms over threshold
        assert!(
            !rm.can_open_position(),
            "paper={}: high latency should block",
            paper
        );
    }
}

#[test]
fn position_opened_and_closed_tracking() {
    let rm = make_risk(100.0, 3, 200);
    rm.set_paper_mode(false);
    rm.position_opened();
    rm.position_opened();
    rm.position_closed();
    let status = rm.status();
    assert_eq!(status.position_count, 1);
}

#[test]
fn drawdown_calculation() {
    let rm = make_risk(100.0, 5, 200);
    assert_eq!(rm.current_drawdown_raw(), 0);
    rm.update_pnl(1_000_000_000); // +$10 peak
    rm.update_pnl(-500_000_000); // now +$5 cumulative
                                 // Peak = $10, current = $5, drawdown = $5
    assert_eq!(rm.current_drawdown_raw(), 500_000_000);
}

#[test]
fn risk_status_serializes() {
    let rm = make_risk(100.0, 5, 200);
    let status = rm.status();
    let json = serde_json::to_string(&status).unwrap();
    assert!(json.contains("kill_switch"));
    assert!(json.contains("position_count"));
    assert!(json.contains("drawdown_raw"));
}

#[test]
fn risk_violations_reason_priority() {
    // Kill switch takes highest priority
    let rm = make_risk(1.0, 1, 50);
    rm.set_paper_mode(false);
    rm.activate_kill_switch();
    rm.update_ping_latency(200);
    rm.update_pnl(2_000_000_000);
    rm.update_pnl(-3_000_000_000);
    let v = rm.check();
    assert_eq!(v.reason(), Some("kill_switch"));
}
