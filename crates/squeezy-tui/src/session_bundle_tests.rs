//! Unit tests for the pure Shareable Session Bundle assembly (§12.6.6).

use super::*;

fn sample_meta() -> BundleMeta {
    BundleMeta {
        session_id: "sess-123".to_string(),
        model: "gpt-test".to_string(),
        mode: "build".to_string(),
        provider: "scripted".to_string(),
        version: "9.9.9".to_string(),
        workspace: "/work/space".to_string(),
        transcript_entries: 3,
        // Fixed clock so the manifest is golden.
        generated_at_unix: 1_700_000_000,
    }
}

// ---------------------------------------------------------------------------
// Format token parsing
// ---------------------------------------------------------------------------

#[test]
fn format_from_token_accepts_md_and_json() {
    assert_eq!(BundleFormat::from_token("md"), Some(BundleFormat::Markdown));
    assert_eq!(
        BundleFormat::from_token("MARKDOWN"),
        Some(BundleFormat::Markdown)
    );
    assert_eq!(BundleFormat::from_token("json"), Some(BundleFormat::Json));
    assert_eq!(BundleFormat::from_token("yaml"), None);
}

#[test]
fn format_extension_matches_format() {
    assert_eq!(BundleFormat::Markdown.file_extension(), "md");
    assert_eq!(BundleFormat::Json.file_extension(), "json");
    assert_eq!(BundleFormat::default_format(), BundleFormat::Markdown);
}

// ---------------------------------------------------------------------------
// /bundle argument parsing
// ---------------------------------------------------------------------------

#[test]
fn parse_request_defaults_to_markdown_redacted() {
    let request = parse_bundle_request("").expect("empty parses");
    assert_eq!(request.format, BundleFormat::Markdown);
    assert!(request.redact, "redaction is on by default");
}

#[test]
fn parse_request_accepts_format_and_no_redact_in_any_order() {
    let a = parse_bundle_request("json no-redact").expect("parses");
    assert_eq!(a.format, BundleFormat::Json);
    assert!(!a.redact);

    let b = parse_bundle_request("no-redact json").expect("parses (order-independent)");
    assert_eq!(b.format, BundleFormat::Json);
    assert!(!b.redact);

    // `raw` / `noredact` aliases also opt out.
    assert!(!parse_bundle_request("raw").unwrap().redact);
    assert!(!parse_bundle_request("md noredact").unwrap().redact);
}

#[test]
fn parse_request_rejects_unknown_and_duplicate_tokens() {
    let err = parse_bundle_request("yaml").expect_err("unknown format token is an error");
    assert!(err.contains("unknown bundle option"), "{err}");

    let dup = parse_bundle_request("md json").expect_err("duplicate format is an error");
    assert!(dup.contains("duplicate format"), "{dup}");
}

// ---------------------------------------------------------------------------
// Redaction
// ---------------------------------------------------------------------------

#[test]
fn redact_masks_bearer_tokens() {
    let (out, n) = redact_secrets("Authorization: Bearer abcdef.token.value");
    assert_eq!(n, 1, "one span masked");
    assert!(out.contains("***REDACTED***"), "{out}");
    assert!(!out.contains("abcdef.token.value"), "secret gone: {out}");
    // The keyword itself is preserved so the line still reads sensibly.
    assert!(out.to_lowercase().contains("bearer"), "{out}");
}

#[test]
fn redact_masks_sensitive_assignments_but_keeps_benign_ones() {
    let (out, n) = redact_secrets("API_KEY=supersecret123");
    assert_eq!(n, 1);
    assert!(out.starts_with("API_KEY="), "key preserved: {out}");
    assert!(out.contains("***REDACTED***"), "{out}");
    assert!(!out.contains("supersecret123"));

    // A non-sensitive key is left alone — redaction must not mangle normal prose.
    let (benign, m) = redact_secrets("color=blue");
    assert_eq!(m, 0, "benign assignment untouched");
    assert_eq!(benign, "color=blue");
}

#[test]
fn redact_masks_prefixed_api_keys_inline() {
    let (out, n) = redact_secrets("here is my key sk-abcdefghijklmnop in text");
    assert_eq!(n, 1, "the sk- key is masked");
    assert!(out.contains("***REDACTED***"), "{out}");
    assert!(!out.contains("sk-abcdefghijklmnop"), "{out}");
    // Surrounding words survive.
    assert!(out.contains("here is my key"), "{out}");
    assert!(out.contains("in text"), "{out}");
}

#[test]
fn redact_counts_multiple_lines() {
    let text = "API_KEY=one\nnothing here\nAuthorization: Bearer two";
    let (out, n) = redact_secrets(text);
    assert_eq!(n, 2, "both secret lines masked: {out}");
    assert!(out.contains("nothing here"), "benign line survives: {out}");
}

#[test]
fn redact_preserves_trailing_newline_and_blank_lines() {
    let (out, n) = redact_secrets("plain text\n\n");
    assert_eq!(n, 0);
    assert_eq!(out, "plain text\n\n", "structure preserved exactly");
}

/// deep-review #63: the matcher must cover the advertised "share-safe" classes —
/// PEM private-key blocks, JWTs, Google `AIza` keys, and `scheme://user:pass@`
/// URL passwords — and must NOT mangle a benign `Tokens: 1,234` line (whose key
/// only *contains* the substring `TOKEN`).
#[test]
fn redact_covers_pem_jwt_google_and_url_secrets_without_false_positives() {
    let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dQw4w9WgXcQabcDEF_-12";
    let aiza = "AIzaSyA1234567890abcdefghijklmnopqrstuv";
    let transcript = format!(
        "intro line\n\
         -----BEGIN RSA PRIVATE KEY-----\n\
         MIIEowIBAAKCAQEAabcdefghijklmnop\n\
         qrstuvwxyz0123456789ABCDEFGHIJKL\n\
         -----END RSA PRIVATE KEY-----\n\
         my jwt is {jwt} ok\n\
         google key {aiza} here\n\
         db url postgres://user:hunter2@db.example.com:5432/app\n\
         Tokens: 1,234\n"
    );

    let (out, n) = redact_secrets(&transcript);

    // Exactly four secret classes masked: PEM block (counted once), JWT, AIza,
    // URL password.
    assert_eq!(n, 4, "four secret classes masked:\n{out}");

    // Each secret is gone.
    assert!(!out.contains("MIIEowIBAAKCAQEA"), "PEM body masked:\n{out}");
    assert!(
        !out.contains("qrstuvwxyz0123456789"),
        "PEM body masked:\n{out}"
    );
    assert!(!out.contains(jwt), "JWT masked:\n{out}");
    assert!(!out.contains(aiza), "AIza key masked:\n{out}");
    assert!(!out.contains("hunter2"), "URL password masked:\n{out}");

    // The PEM markers survive so the block still reads as a key block.
    assert!(out.contains("-----BEGIN RSA PRIVATE KEY-----"), "{out}");
    assert!(out.contains("-----END RSA PRIVATE KEY-----"), "{out}");
    // The URL scheme/user/host survive; only the password is masked.
    assert!(
        out.contains("postgres://user:***REDACTED***@db.example.com"),
        "{out}"
    );

    // The benign Tokens line is untouched (no false positive on `TOKEN`).
    assert!(
        out.contains("Tokens: 1,234"),
        "benign Tokens line must be left verbatim:\n{out}"
    );
}

// ---------------------------------------------------------------------------
// Build / manifest / checksum
// ---------------------------------------------------------------------------

#[test]
fn markdown_bundle_carries_manifest_diagnostics_transcript_and_checksum() {
    let meta = sample_meta();
    let diagnostics = vec![("term".to_string(), "xterm-256color".to_string())];
    let bundle = SessionBundle::build(
        BundleFormat::Markdown,
        &meta,
        &diagnostics,
        "hello body",
        true,
    );

    let art = &bundle.artifact;
    assert!(art.contains("# Squeezy Session Bundle"), "{art}");
    assert!(art.contains("## Manifest"), "{art}");
    assert!(art.contains("| session | sess-123 |"), "{art}");
    assert!(art.contains("| model | gpt-test |"), "{art}");
    assert!(art.contains("generated_at_unix"), "{art}");
    assert!(art.contains("## Diagnostics"), "{art}");
    assert!(art.contains("xterm-256color"), "{art}");
    assert!(art.contains("## Transcript"), "{art}");
    assert!(art.contains("hello body"), "{art}");
    // Checksum is embedded and matches the report.
    assert!(
        art.contains(&bundle.report.transcript_sha256),
        "manifest carries the checksum"
    );
    assert_eq!(bundle.report.transcript_sha256.len(), 64, "sha256 hex");
    assert_eq!(bundle.report.bytes, art.len());
    assert!(bundle.report.redacted);
}

#[test]
fn checksum_is_over_redacted_body_not_original() {
    let meta = sample_meta();
    let secret = "API_KEY=topsecretvalue\nrest of body";
    let redacted = SessionBundle::build(BundleFormat::Markdown, &meta, &[], secret, true);
    let raw = SessionBundle::build(BundleFormat::Markdown, &meta, &[], secret, false);

    // The redacted artifact must not leak the secret.
    assert!(
        !redacted.artifact.contains("topsecretvalue"),
        "redacted bundle hides the secret"
    );
    assert!(redacted.report.redactions >= 1, "redaction counted");
    // The raw bundle keeps it (opt-out) and its manifest says so.
    assert!(raw.artifact.contains("topsecretvalue"));
    assert!(!raw.report.redacted);
    assert!(
        raw.artifact.contains("NOT sanitized"),
        "raw manifest is honest about redaction status: {}",
        raw.artifact
    );
    // Different bodies → different checksums.
    assert_ne!(
        redacted.report.transcript_sha256,
        raw.report.transcript_sha256
    );
}

#[test]
fn json_bundle_is_valid_json_with_manifest_and_transcript() {
    let meta = sample_meta();
    let diagnostics = vec![("term".to_string(), "dumb".to_string())];
    let bundle = SessionBundle::build(BundleFormat::Json, &meta, &diagnostics, "body text", true);

    let value: serde_json::Value =
        serde_json::from_str(&bundle.artifact).expect("bundle is valid JSON");
    assert_eq!(value["manifest"]["session"], "sess-123");
    assert_eq!(value["manifest"]["model"], "gpt-test");
    assert_eq!(value["manifest"]["redacted"], true);
    assert_eq!(
        value["manifest"]["transcript_sha256"],
        bundle.report.transcript_sha256
    );
    assert_eq!(value["transcript"], "body text");
    assert_eq!(value["diagnostics"][0]["key"], "term");
    assert_eq!(value["diagnostics"][0]["value"], "dumb");
}

#[test]
fn empty_transcript_still_produces_a_valid_bundle() {
    let meta = BundleMeta {
        transcript_entries: 0,
        ..sample_meta()
    };
    let bundle = SessionBundle::build(BundleFormat::Markdown, &meta, &[], "", true);
    assert!(
        bundle.artifact.contains("## Transcript"),
        "edge: empty body"
    );
    assert!(bundle.artifact.contains("transcript_entries"));
    assert_eq!(bundle.report.redactions, 0);
    // The checksum of the empty string is the well-known SHA-256 of "".
    assert_eq!(
        bundle.report.transcript_sha256,
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    // Diagnostics fall back to an explicit empty marker, never a broken table.
    assert!(bundle.artifact.contains("_(none)_"), "{}", bundle.artifact);
}

#[test]
fn markdown_cells_escape_pipes_and_newlines() {
    let meta = BundleMeta {
        workspace: "/has|pipe".to_string(),
        ..sample_meta()
    };
    let bundle = SessionBundle::build(BundleFormat::Markdown, &meta, &[], "x", false);
    // The pipe in the workspace value must be escaped so the table is not broken.
    assert!(
        bundle.artifact.contains("/has\\|pipe"),
        "pipe escaped in cell: {}",
        bundle.artifact
    );
}

#[test]
fn preview_reports_essentials_and_redaction_state() {
    let meta = sample_meta();
    let redacted = SessionBundle::build(BundleFormat::Markdown, &meta, &[], "API_KEY=x", true);
    let preview = redacted.preview(&meta);
    assert!(preview.contains("sess-123"), "{preview}");
    assert!(preview.contains("gpt-test"), "{preview}");
    assert!(preview.contains("redacted"), "{preview}");
    assert!(
        preview.contains(&redacted.report.transcript_sha256),
        "preview shows the checksum"
    );

    let raw = SessionBundle::build(BundleFormat::Markdown, &meta, &[], "body", false);
    let raw_preview = raw.preview(&meta);
    assert!(
        raw_preview.contains("NOT redacted"),
        "preview warns when unredacted: {raw_preview}"
    );
}

#[test]
fn portable_path_normalizes_backslashes() {
    assert_eq!(portable_path(r"C:\Users\me\work"), "C:/Users/me/work");
    assert_eq!(portable_path("/already/unix"), "/already/unix");
}
