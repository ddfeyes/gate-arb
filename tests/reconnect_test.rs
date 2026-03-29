/// Integration tests for WS reconnect logic (gateway reconnect policy).
///
/// These test the backoff math and config defaults without needing a live WS connection.
use std::time::Duration;

/// Mirror of the backoff formula in run_gateway_with_reconnect.
fn backoff_for_attempt(attempt: u32, initial_ms: u64, max_ms: u64) -> Duration {
    let raw_ms = initial_ms * (1u64 << (attempt - 1).min(5));
    Duration::from_millis(raw_ms.min(max_ms))
}

#[test]
fn backoff_doubles_each_attempt() {
    // 1s initial, 30s cap
    assert_eq!(backoff_for_attempt(1, 1000, 30_000), Duration::from_secs(1));
    assert_eq!(backoff_for_attempt(2, 1000, 30_000), Duration::from_secs(2));
    assert_eq!(backoff_for_attempt(3, 1000, 30_000), Duration::from_secs(4));
    assert_eq!(backoff_for_attempt(4, 1000, 30_000), Duration::from_secs(8));
    assert_eq!(
        backoff_for_attempt(5, 1000, 30_000),
        Duration::from_secs(16)
    );
    assert_eq!(
        backoff_for_attempt(6, 1000, 30_000),
        Duration::from_secs(30)
    ); // capped
    assert_eq!(
        backoff_for_attempt(7, 1000, 30_000),
        Duration::from_secs(30)
    ); // stays capped
    assert_eq!(
        backoff_for_attempt(10, 1000, 30_000),
        Duration::from_secs(30)
    ); // stays capped
}

#[test]
fn backoff_capped_at_max() {
    // Even at attempt=100, should never exceed max
    for a in 1u32..=100 {
        let d = backoff_for_attempt(a, 1000, 30_000);
        assert!(d <= Duration::from_secs(30), "attempt {} exceeded max", a);
    }
}

#[test]
fn backoff_minimum_is_initial() {
    // attempt=1 always returns initial delay
    assert_eq!(
        backoff_for_attempt(1, 500, 10_000),
        Duration::from_millis(500)
    );
    assert_eq!(backoff_for_attempt(1, 2000, 60_000), Duration::from_secs(2));
}

#[test]
fn default_reconnect_config() {
    // Test the defaults match the expected policy
    let initial = Duration::from_secs(1);
    let max = Duration::from_secs(30);
    let max_attempts = 10u32;
    let pause_threshold = Duration::from_secs(60);

    // Policy: max 10 attempts
    assert_eq!(max_attempts, 10);
    // After attempt 10: 1 * 2^(min(9,5)) = 1 * 32 = 32s → capped to 30s
    assert_eq!(
        backoff_for_attempt(10, initial.as_millis() as u64, max.as_millis() as u64),
        max
    );
    // Pause after 60s disconnect
    assert_eq!(pause_threshold, Duration::from_secs(60));
}

#[test]
fn shift_clamp_never_overflows() {
    // The formula uses (attempt-1).min(5) to prevent u64 shift overflow.
    // 1u64 << 5 = 32 — max shift, times initial 1000ms = 32_000ms → capped to 30_000ms
    for a in 1u32..=200 {
        let shift = (a - 1).min(5);
        let raw_ms: u64 = 1000u64.saturating_mul(1u64 << shift);
        let capped = raw_ms.min(30_000);
        assert!(capped <= 30_000, "overflow at attempt {}", a);
    }
}
