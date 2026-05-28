use super::*;

#[test]
fn generated_pkce_round_trips_through_challenge_for() {
    let codes = generate_pkce().expect("generate pkce");
    let recomputed = challenge_for(&codes.verifier);
    assert_eq!(
        codes.challenge, recomputed,
        "challenge must match sha256(verifier) base64url",
    );
}

#[test]
fn generated_pkce_verifier_uses_url_safe_alphabet() {
    let codes = generate_pkce().expect("generate pkce");
    for ch in codes.verifier.chars() {
        assert!(
            ch.is_ascii_alphanumeric() || ch == '-' || ch == '_',
            "verifier must be base64url without padding: {:?}",
            ch
        );
    }
    assert_eq!(
        codes.verifier.len(),
        43,
        "32-byte verifier encodes to 43 base64url chars",
    );
}

#[test]
fn generated_pkce_challenge_is_43_chars_base64url() {
    let codes = generate_pkce().expect("generate pkce");
    assert_eq!(codes.challenge.len(), 43);
    for ch in codes.challenge.chars() {
        assert!(
            ch.is_ascii_alphanumeric() || ch == '-' || ch == '_',
            "challenge must be base64url without padding: {:?}",
            ch
        );
    }
}

#[test]
fn challenge_for_known_verifier_matches_rfc_test_vector() {
    // RFC 7636 Appendix B test vector: sha256("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk")
    // encoded base64url equals "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM".
    let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    let challenge = challenge_for(verifier);
    assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
}

#[test]
fn two_consecutive_calls_yield_different_verifiers() {
    // Tiny smoke test: the CSPRNG must not emit identical 32-byte
    // outputs on back-to-back calls. A collision is overwhelmingly
    // unlikely; if it happens the entropy source is broken.
    let first = generate_pkce().expect("first");
    let second = generate_pkce().expect("second");
    assert_ne!(first.verifier, second.verifier);
    assert_ne!(first.challenge, second.challenge);
}
