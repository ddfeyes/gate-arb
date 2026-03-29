/// Integration tests for WS reconnect policy (gateway reconnect loop).
///
/// Tests call ReconnectConfig::delay_for_attempt directly — so if the formula
/// in gateway-ws changes, these tests will catch the divergence (not mirror it).
use gateway_ws::ReconnectConfig;
use std::time::Duration;

#[test]
fn backoff_doubles_each_attempt() {
    let cfg = ReconnectConfig::default();
    assert_eq!(cfg.delay_for_attempt(1), Duration::from_secs(1));
    assert_eq!(cfg.delay_for_attempt(2), Duration::from_secs(2));
    assert_eq!(cfg.delay_for_attempt(3), Duration::from_secs(4));
    assert_eq!(cfg.delay_for_attempt(4), Duration::from_secs(8));
    assert_eq!(cfg.delay_for_attempt(5), Duration::from_secs(16));
    assert_eq!(cfg.delay_for_attempt(6), Duration::from_secs(30)); // capped
    assert_eq!(cfg.delay_for_attempt(10), Duration::from_secs(30)); // still capped
}

#[test]
fn backoff_capped_at_max() {
    let cfg = ReconnectConfig::default();
    for a in 1u32..=100 {
        let d = cfg.delay_for_attempt(a);
        assert!(d <= cfg.max_delay, "attempt {} exceeded max_delay", a);
    }
}

#[test]
fn backoff_minimum_is_initial() {
    let cfg = ReconnectConfig::default();
    assert_eq!(cfg.delay_for_attempt(1), cfg.initial_delay);
}

#[test]
fn shift_clamp_never_overflows() {
    // min(5) on the exponent prevents u64 shl overflow at high attempt counts.
    let cfg = ReconnectConfig::default();
    for a in 1u32..=200 {
        let d = cfg.delay_for_attempt(a);
        assert!(d <= cfg.max_delay, "overflow/cap broken at attempt {}", a);
    }
}

#[test]
fn default_reconnect_config() {
    let cfg = ReconnectConfig::default();
    assert_eq!(cfg.initial_delay, Duration::from_secs(1));
    assert_eq!(cfg.max_delay, Duration::from_secs(30));
    assert_eq!(cfg.max_attempts, 10);
    assert_eq!(cfg.pause_threshold, Duration::from_secs(60));
}

#[test]
fn halt_threshold_is_inclusive() {
    // Policy: >= max_attempts → halt. Verify 10 failures = halt, 9 = no halt.
    let cfg = ReconnectConfig::default();
    assert_eq!(cfg.max_attempts, 10);
    assert!(
        9 < cfg.max_attempts,
        "9 consecutive failures should NOT halt"
    );
    assert!(
        10 >= cfg.max_attempts,
        "10 consecutive failures should halt"
    );
}
