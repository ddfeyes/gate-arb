use gateway_ws::parse_price;

use types::Fixed64;

/// Regression test for critical price parsing bug (Masami review 2026-03-28).
/// parse_price was counting ALL digits as frac_digits instead of only post-decimal.
/// "50000.12345678" returned 50_000_123 raw instead of 5_000_012_345_678.

#[test]
fn parse_price_basic_integer() {
    // "50000" → 50000 * 1e8 = 5_000_000_000_000
    let p = parse_price("50000");
    assert_eq!(p.raw(), 5_000_000_000_000, "integer price");
}

#[test]
fn parse_price_8_decimal_places() {
    // "50000.12345678" → 5_000_012_345_678
    let p = parse_price("50000.12345678");
    assert_eq!(p.raw(), 5_000_012_345_678, "8 decimal places");
}

#[test]
fn parse_price_fewer_decimals() {
    // "50000.5" → 50000 * 1e8 + 5 * 1e7 = 5_000_050_000_000
    let p = parse_price("50000.5");
    assert_eq!(p.raw(), 5_000_050_000_000, "1 decimal place");
}

#[test]
fn parse_price_two_decimals() {
    // "1.23" → 1 * 1e8 + 23 * 1e6 = 123_000_000
    let p = parse_price("1.23");
    assert_eq!(p.raw(), 123_000_000, "2 decimal places");
}

#[test]
fn parse_price_zero_fractional() {
    // "100.00000000" → 10_000_000_000
    let p = parse_price("100.00000000");
    assert_eq!(p.raw(), 10_000_000_000, "zero frac");
}

#[test]
fn parse_price_small_price() {
    // "0.00010000" → 10_000 raw (BTC sat-level price)
    let p = parse_price("0.00010000");
    assert_eq!(p.raw(), 10_000, "small price 0.0001");
}

#[test]
fn parse_price_truncates_beyond_8_decimals() {
    // "1.123456789" → truncate at 8 digits → "12345678" → 1_123_456_780 raw
    // (last digit 9 is dropped)
    let p = parse_price("1.123456789");
    assert_eq!(p.raw(), 112_345_678, "truncate >8 decimals");
}

#[test]
fn parse_price_regression_was_wrong() {
    // This is the exact case that was broken:
    // Old code produced 50_000_123 (counted int+frac digits as frac_digits=13, shift by 5 → /100000)
    // Correct value: 5_000_012_345_678
    let p = parse_price("50000.12345678");
    let old_wrong = Fixed64::from_raw(50_000_123);
    assert_ne!(p.raw(), old_wrong.raw(), "must not produce old wrong value");
    assert_eq!(p.raw(), 5_000_012_345_678);
}
