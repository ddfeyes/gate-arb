use order_manager::auth::build_auth_header;

#[test]
fn auth_signature_format() {
    let key = "test_key";
    let secret = "test_secret";
    let ts = 1_700_000_000u64;

    let header = build_auth_header(key, secret, ts);

    assert_eq!(header.method, "api_key");
    assert_eq!(header.key, "test_key");
    assert_eq!(header.timestamp, ts);
    // SHA512 = 64 bytes = 128 hex chars
    assert_eq!(header.signature.len(), 128);
    assert!(header.signature.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn auth_signature_deterministic() {
    let h1 = build_auth_header("k", "s", 999);
    let h2 = build_auth_header("k", "s", 999);
    assert_eq!(h1.signature, h2.signature);
}

#[test]
fn auth_signature_varies_by_secret() {
    let h1 = build_auth_header("k", "secret1", 999);
    let h2 = build_auth_header("k", "secret2", 999);
    assert_ne!(h1.signature, h2.signature);
}
