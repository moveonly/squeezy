//! Shareable Session Bundle (§12.6.6).
//!
//! A *bundle* is a single, self-contained artifact — one Markdown or JSON file —
//! that a user can hand to support or a teammate to reproduce the context of a
//! session: the rendered transcript plus a small manifest of session metadata, a
//! checksum over the transcript body, the terminal/environment diagnostics, and
//! an explicit redaction status. It deliberately reuses the existing
//! render/export pipeline: the transcript text fed in here is the **same**
//! payload [`crate::copy`] / [`crate::handle_export_command`] produce, so the
//! bundle never grows a second, divergent transcript renderer.
//!
//! ## Pure by construction
//!
//! This module owns only *pure* assembly: it takes a [`BundleMeta`] (the session
//! facts the crate root already holds), the pre-rendered transcript string, a
//! list of diagnostic key/value rows, and a redaction toggle, and returns the
//! finished artifact string plus a [`BundleReport`] describing what it did
//! (bytes, checksum, redaction count). It never touches `TuiApp`, the
//! filesystem, or a terminal — `lib.rs` owns the atomic write and the status
//! toast, exactly like the `/export` flow. Keeping it pure is what makes every
//! redaction rule, the manifest shape, and the checksum unit-testable without
//! standing up an app or a TTY.
//!
//! ## Redaction is on by default
//!
//! Bundles are meant to be *shared*, so the privacy-safe default is to redact:
//! [`redact_secrets`] masks the common secret shapes (bearer/authorization
//! tokens, `KEY=secret` assignments for sensitive-looking keys, high-entropy
//! `sk-`/`ghp_`/`AKIA`/`AIza`-style API keys, `eyJ…` JWTs, PEM private-key
//! blocks, and `scheme://user:pass@host` URL passwords) before the transcript
//! is embedded, and the manifest records both that redaction ran and how many
//! spans were masked. A caller can opt out (`redact = false`) for a fully local
//! bundle, and the manifest says so plainly so a reader is never misled about
//! whether a bundle was sanitized.
//!
//! Redaction is a best-effort HEURISTIC, not an exhaustive guarantee: it
//! recognises known secret shapes and cannot catch every credential a free-form
//! transcript might contain. Treat a redacted bundle as "common secrets masked",
//! and still review one before sharing it widely.
//!
//! ## Portable paths
//!
//! The manifest records the workspace directory with forward slashes
//! ([`portable_path`]) so a bundle generated on Windows reads the same on
//! macOS/Linux, matching the spec's "portable slash paths" platform note.

use std::fmt::Write as _;

/// The container format of the finished bundle artifact.
///
/// Both formats carry the *same* information (manifest + diagnostics + redacted
/// transcript + checksum); they differ only in packaging. Markdown is the
/// human-first default (open it in any viewer); JSON is the machine-readable
/// form for tooling that wants to parse the manifest and transcript fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BundleFormat {
    /// A single Markdown document: a manifest table, a diagnostics table, then
    /// the transcript under a heading.
    Markdown,
    /// A single JSON object: `{ "manifest", "diagnostics", "transcript" }`.
    Json,
}

impl BundleFormat {
    /// Default bundle format. Markdown — the share-with-a-human default.
    pub(crate) const fn default_format() -> Self {
        BundleFormat::Markdown
    }

    /// Parse the `/bundle` format token (`md`/`markdown`, `json`).
    /// Case-insensitive. `None` for anything else so the caller can surface a
    /// usage error.
    pub(crate) fn from_token(token: &str) -> Option<Self> {
        match token.trim().to_ascii_lowercase().as_str() {
            "md" | "markdown" => Some(BundleFormat::Markdown),
            "json" => Some(BundleFormat::Json),
            _ => None,
        }
    }

    /// Conventional file extension for the bundle artifact.
    pub(crate) fn file_extension(self) -> &'static str {
        match self {
            BundleFormat::Markdown => "md",
            BundleFormat::Json => "json",
        }
    }
}

/// The session facts embedded in a bundle manifest. Supplied verbatim by the
/// crate root from the live [`crate::TuiApp`] — this module never reaches into
/// the app to compute them, keeping the assembly pure and testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BundleMeta {
    /// Session id (or `"default"` before one exists), the primary support key.
    pub(crate) session_id: String,
    /// Model the session is running against.
    pub(crate) model: String,
    /// Session mode label (`build` / `plan` / …).
    pub(crate) mode: String,
    /// Provider name (`scripted`, the LLM provider, …).
    pub(crate) provider: String,
    /// Squeezy version string.
    pub(crate) version: String,
    /// Workspace directory, stored with portable forward slashes.
    pub(crate) workspace: String,
    /// Number of transcript entries the session held when the bundle was built.
    pub(crate) transcript_entries: usize,
    /// Wall-clock generation time as a unix timestamp (seconds). The crate root
    /// passes the real clock; tests pass a fixed value so the manifest is golden.
    pub(crate) generated_at_unix: u64,
}

/// What a bundle build produced, for the status line / toast and for tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BundleReport {
    /// Total byte length of the finished artifact.
    pub(crate) bytes: usize,
    /// Lowercase hex SHA-256 over the (post-redaction) transcript body — the
    /// integrity checksum the manifest also embeds.
    pub(crate) transcript_sha256: String,
    /// Whether redaction was applied to the transcript before embedding.
    pub(crate) redacted: bool,
    /// Number of secret spans masked by redaction (0 when redaction was off or
    /// nothing matched).
    pub(crate) redactions: usize,
}

/// A finished bundle: the artifact text plus the report describing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionBundle {
    /// The complete artifact, ready to write to a single file.
    pub(crate) artifact: String,
    /// Build report (bytes, checksum, redaction status).
    pub(crate) report: BundleReport,
}

impl SessionBundle {
    /// Assemble a self-contained bundle.
    ///
    /// `meta` supplies the manifest facts, `diagnostics` is the same key/value
    /// terminal/environment rows the `/terminal` command shows, `transcript` is
    /// the pre-rendered transcript (the export pipeline's Markdown payload), and
    /// `redact` chooses whether to sanitize the transcript before embedding.
    ///
    /// The checksum is taken over the *embedded* (post-redaction) transcript so a
    /// reader can verify the bytes they actually received, not a pre-redaction
    /// original that never left the host.
    pub(crate) fn build(
        format: BundleFormat,
        meta: &BundleMeta,
        diagnostics: &[(String, String)],
        transcript: &str,
        redact: bool,
    ) -> Self {
        let (body, redactions) = if redact {
            redact_secrets(transcript)
        } else {
            (transcript.to_string(), 0)
        };
        let checksum = sha256_hex(body.as_bytes());

        let artifact = match format {
            BundleFormat::Markdown => {
                render_markdown(meta, diagnostics, &body, &checksum, redact, redactions)
            }
            BundleFormat::Json => {
                render_json(meta, diagnostics, &body, &checksum, redact, redactions)
            }
        };

        let report = BundleReport {
            bytes: artifact.len(),
            transcript_sha256: checksum,
            redacted: redact,
            redactions,
        };
        SessionBundle { artifact, report }
    }

    /// A short, single-paragraph preview of the bundle for the in-app transcript
    /// echo — the mandatory "preview before share" affordance (the spec calls out
    /// preview/redaction as mandatory). Reports the manifest essentials and the
    /// redaction status without dumping the whole (possibly large) artifact.
    pub(crate) fn preview(&self, meta: &BundleMeta) -> String {
        let redaction = if self.report.redacted {
            // Redaction is on by default; surface the opt-out keyword inline so a
            // user wanting an unmasked local bundle does not have to already know
            // it exists.
            format!(
                "redacted ({} masked) — pass no-redact for an unmasked local bundle",
                self.report.redactions
            )
        } else {
            "NOT redacted (local share only)".to_string()
        };
        format!(
            "Session bundle ready — review before sharing:\n\
             • session: {session}\n\
             • model: {model}  mode: {mode}\n\
             • entries: {entries}  size: {bytes} bytes\n\
             • redaction: {redaction}\n\
             • sha256: {sha}",
            session = meta.session_id,
            model = meta.model,
            mode = meta.mode,
            entries = meta.transcript_entries,
            bytes = self.report.bytes,
            redaction = redaction,
            sha = self.report.transcript_sha256,
        )
    }
}

/// Shared usage hint for `/bundle`.
pub(crate) const BUNDLE_USAGE: &str = "usage: /bundle [md|json] [no-redact]";

/// A fully-parsed `/bundle` invocation: the artifact format and whether to
/// redact. Kept as a tiny pure struct so the argument grammar is unit-testable
/// without the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BundleRequest {
    pub(crate) format: BundleFormat,
    pub(crate) redact: bool,
}

/// Parse `/bundle [md|json] [no-redact]`.
///
/// Both tokens are optional and order-independent. The format defaults to
/// Markdown; redaction defaults to **on** (the share-safe default). The opt-out
/// keyword is `no-redact` (also `noredact` / `raw`). An unrecognised token is a
/// usage error so a typo never silently produces an unredacted bundle.
pub(crate) fn parse_bundle_request(rest: &str) -> Result<BundleRequest, String> {
    let mut format: Option<BundleFormat> = None;
    let mut redact = true;
    for token in rest.split_whitespace() {
        let lowered = token.to_ascii_lowercase();
        match lowered.as_str() {
            "no-redact" | "noredact" | "raw" => redact = false,
            "redact" => redact = true,
            other => match BundleFormat::from_token(other) {
                Some(f) => {
                    if format.is_some() {
                        return Err(format!("duplicate format token {token:?}. {BUNDLE_USAGE}"));
                    }
                    format = Some(f);
                }
                None => return Err(format!("unknown bundle option {token:?}. {BUNDLE_USAGE}")),
            },
        }
    }
    Ok(BundleRequest {
        format: format.unwrap_or_else(BundleFormat::default_format),
        redact,
    })
}

// ---------------------------------------------------------------------------
// Redaction
// ---------------------------------------------------------------------------

/// Mask the obvious secret shapes in `text`, returning the sanitized text and
/// the number of spans masked.
///
/// This is intentionally conservative — it targets shapes that are *almost
/// always* secrets so it does not mangle normal prose:
///
///   * `Authorization: Bearer <token>` / `Bearer <token>` headers.
///   * `KEY=value` / `KEY: value` assignments where KEY looks sensitive
///     (contains `KEY`, `TOKEN`, `SECRET`, `PASSWORD`, `PASSWD`, `API`).
///   * Long high-entropy API keys with a known prefix (`sk-`, `ghp_`, `gho_`,
///     `xoxb-`, `AKIA…`).
///
/// Each masked span is replaced with `***REDACTED***`. Redaction is line-based
/// so a leaked secret never survives by being on its own line.
pub(crate) fn redact_secrets(text: &str) -> (String, usize) {
    let mut out = String::with_capacity(text.len());
    let mut count = 0usize;
    // PEM private-key blocks span multiple lines: once a `-----BEGIN ... PRIVATE
    // KEY-----` header is seen, every base64 body line through the matching
    // `-----END ...-----` is masked. The whole block counts as a single mask.
    let mut in_pem = false;
    for line in text.split_inclusive('\n') {
        // Preserve the trailing newline (if any) verbatim; redact only content.
        let (content, newline) = match line.strip_suffix('\n') {
            Some(rest) => (rest, "\n"),
            None => (line, ""),
        };
        if in_pem {
            // Inside a PEM block: mask the body, and watch for the END marker.
            if is_pem_end(content) {
                in_pem = false;
                out.push_str(content); // the END marker itself is not a secret
            } else {
                out.push_str("***REDACTED***");
            }
            out.push_str(newline);
            continue;
        }
        if is_pem_private_key_begin(content) {
            in_pem = true;
            count += 1; // count the whole block once, at its header
            out.push_str(content); // the BEGIN marker itself is not a secret
            out.push_str(newline);
            continue;
        }
        let (redacted, n) = redact_line(content);
        out.push_str(&redacted);
        out.push_str(newline);
        count += n;
    }
    (out, count)
}

/// True for a `-----BEGIN ... PRIVATE KEY-----` header (RSA/EC/OPENSSH/PGP/…).
fn is_pem_private_key_begin(line: &str) -> bool {
    let t = line.trim();
    t.starts_with("-----BEGIN ") && t.ends_with("PRIVATE KEY-----")
}

/// True for any `-----END ...-----` footer (closes a PEM block).
fn is_pem_end(line: &str) -> bool {
    let t = line.trim();
    t.starts_with("-----END ") && t.ends_with("-----")
}

/// Redact a single line, returning the masked line and the number of spans
/// masked on it.
fn redact_line(line: &str) -> (String, usize) {
    let mut count = 0usize;

    // 1) Bearer / authorization tokens: mask everything after the keyword.
    if let Some(masked) = redact_bearer(line) {
        return (masked, 1);
    }

    // 2) KEY=value / KEY: value with a sensitive-looking key.
    if let Some(masked) = redact_assignment(line) {
        return (masked, 1);
    }

    // 3) URL userinfo password: mask the `pass` in `scheme://user:pass@host`.
    if let Some(masked) = redact_url_userinfo(line) {
        return (masked, 1);
    }

    // 4) Inline high-entropy API keys with a known prefix, plus JWTs.
    let mut result = String::with_capacity(line.len());
    for token in split_keep_delims(line) {
        if is_api_key_token(token) || is_jwt_token(token) {
            result.push_str("***REDACTED***");
            count += 1;
        } else {
            result.push_str(token);
        }
    }
    if count > 0 {
        (result, count)
    } else {
        (line.to_string(), 0)
    }
}

/// Mask the password in a `scheme://user:pass@host` URL (CWE: leaked DB/HTTP
/// credentials), keeping the scheme, user, and host so the line still reads.
/// Returns `None` when the line has no `://user:pass@` shape.
fn redact_url_userinfo(line: &str) -> Option<String> {
    let scheme_at = line.find("://")?;
    let after_scheme = scheme_at + "://".len();
    // The authority ends at the first `/`, `?`, `#`, or whitespace.
    let rest = &line[after_scheme..];
    let authority_len = rest.find(['/', '?', '#', ' ', '\t']).unwrap_or(rest.len());
    let authority = &rest[..authority_len];
    // userinfo (`user:pass`) precedes the LAST `@` in the authority.
    let at = authority.rfind('@')?;
    let userinfo = &authority[..at];
    let colon = userinfo.find(':')?;
    let pass = &userinfo[colon + 1..];
    if pass.is_empty() {
        return None;
    }
    let pass_start = after_scheme + colon + 1;
    let pass_end = pass_start + pass.len();
    Some(format!(
        "{}***REDACTED***{}",
        &line[..pass_start],
        &line[pass_end..]
    ))
}

/// True when `token` has the three-segment base64url JWT shape
/// `eyJ<header>.<payload>.<signature>` (the `eyJ` prefix is the base64 of
/// `{"`, the universal JWT header start).
fn is_jwt_token(token: &str) -> bool {
    if !token.starts_with("eyJ") {
        return false;
    }
    let parts: Vec<&str> = token.split('.').collect();
    parts.len() == 3
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        })
}

/// Mask `Bearer <token>` / `Authorization: <token>` suffixes.
fn redact_bearer(line: &str) -> Option<String> {
    let lowered = line.to_ascii_lowercase();
    // Find a `bearer ` keyword and mask the rest of the line after it.
    if let Some(pos) = lowered.find("bearer ") {
        let keep = &line[..pos + "bearer ".len()];
        if !line[pos + "bearer ".len()..].trim().is_empty() {
            return Some(format!("{keep}***REDACTED***"));
        }
    }
    // `authorization:` / `authorization =` header value.
    for sep in [':', '='] {
        let needle = format!("authorization{sep}");
        if let Some(pos) = lowered.find(&needle) {
            let after = pos + needle.len();
            if !line[after..].trim().is_empty() {
                let keep = &line[..after];
                let lead_ws: String = line[after..]
                    .chars()
                    .take_while(|c| c.is_whitespace())
                    .collect();
                return Some(format!("{keep}{lead_ws}***REDACTED***"));
            }
        }
    }
    None
}

/// Mask the value of a `KEY=value` / `KEY: value` assignment when the key looks
/// sensitive.
fn redact_assignment(line: &str) -> Option<String> {
    for sep in ['=', ':'] {
        if let Some(idx) = line.find(sep) {
            let key = line[..idx].trim();
            // A key with surrounding whitespace/quotes is fine; just test the
            // bare identifier for sensitive substrings.
            let bare: String = key
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if bare.is_empty() {
                continue;
            }
            let upper = bare.to_ascii_uppercase();
            // Match on whole `_`-delimited segments (or the whole key), NOT a
            // bare substring, so a benign plural like `Tokens` (segment
            // `TOKENS`) does not false-positive while `API_KEY`/`DB_PASSWORD`
            // still do.
            let sensitive = is_sensitive_key(&upper);
            let value = &line[idx + 1..];
            if sensitive && !value.trim().is_empty() {
                let lead_ws: String = value.chars().take_while(|c| c.is_whitespace()).collect();
                return Some(format!("{}{sep}{lead_ws}***REDACTED***", &line[..idx]));
            }
        }
    }
    None
}

/// True when an upper-cased bare key (already stripped to `[A-Z0-9_]`) names a
/// secret. Matches on whole `_`-delimited segments, plus a short list of common
/// separator-less names, so `API_KEY`/`DB_PASSWORD`/`SECRET` redact while a
/// benign plural like `TOKENS` (which only *contains* `TOKEN`) does not.
fn is_sensitive_key(upper: &str) -> bool {
    const SEGMENTS: [&str; 7] = [
        "KEY", "TOKEN", "SECRET", "PASSWORD", "PASSWD", "API", "APIKEY",
    ];
    const WHOLE: [&str; 4] = ["APIKEY", "ACCESSTOKEN", "AUTHTOKEN", "PRIVATEKEY"];
    if WHOLE.contains(&upper) {
        return true;
    }
    upper.split('_').any(|seg| SEGMENTS.contains(&seg))
}

/// True when `token` looks like a high-entropy API key with a known prefix.
fn is_api_key_token(token: &str) -> bool {
    let known = ["sk-", "ghp_", "gho_", "ghs_", "ghr_", "xoxb-", "xoxp-"];
    if known.iter().any(|p| token.starts_with(p)) && token.len() >= 12 {
        return true;
    }
    // AWS access key ids: AKIA + 16 uppercase alphanumerics.
    if let Some(rest) = token.strip_prefix("AKIA")
        && rest.len() >= 16
        && rest
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
    {
        return true;
    }
    // Google API keys: AIza + 35 base64url chars.
    if let Some(rest) = token.strip_prefix("AIza")
        && rest.len() >= 35
        && rest
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return true;
    }
    false
}

/// Split a line into tokens while keeping the whitespace/punctuation delimiters,
/// so re-joining the pieces reproduces the line exactly (minus any masked
/// token).
fn split_keep_delims(line: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut in_word = false;
    for (i, ch) in line.char_indices() {
        // A "word" char is anything that can appear in an API key or JWT token
        // (`.` keeps a three-segment `eyJ….….…` JWT as a single token).
        let is_word = ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.';
        if is_word != in_word {
            if i > start {
                out.push(&line[start..i]);
            }
            start = i;
            in_word = is_word;
        }
    }
    if start < line.len() {
        out.push(&line[start..]);
    }
    out
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Convert a native path string to a portable forward-slash form for the
/// manifest, so a bundle built on Windows reads the same on Unix.
pub(crate) fn portable_path(path: &str) -> String {
    path.replace('\\', "/")
}

fn redaction_label(redact: bool, redactions: usize) -> String {
    if redact {
        format!("yes ({redactions} masked)")
    } else {
        "no (NOT sanitized — local share only)".to_string()
    }
}

fn render_markdown(
    meta: &BundleMeta,
    diagnostics: &[(String, String)],
    body: &str,
    checksum: &str,
    redact: bool,
    redactions: usize,
) -> String {
    let mut out = String::new();
    out.push_str("# Squeezy Session Bundle\n\n");

    out.push_str("## Manifest\n\n");
    out.push_str("| field | value |\n| --- | --- |\n");
    let manifest_rows: [(&str, String); 8] = [
        ("session", meta.session_id.clone()),
        ("model", meta.model.clone()),
        ("mode", meta.mode.clone()),
        ("provider", meta.provider.clone()),
        ("version", meta.version.clone()),
        ("workspace", meta.workspace.clone()),
        ("transcript_entries", meta.transcript_entries.to_string()),
        ("generated_at_unix", meta.generated_at_unix.to_string()),
    ];
    for (key, value) in &manifest_rows {
        let _ = writeln!(out, "| {key} | {} |", md_cell(value));
    }
    let _ = writeln!(
        out,
        "| redacted | {} |",
        redaction_label(redact, redactions)
    );
    let _ = writeln!(out, "| transcript_sha256 | {checksum} |");
    out.push('\n');

    out.push_str("## Diagnostics\n\n");
    if diagnostics.is_empty() {
        out.push_str("_(none)_\n\n");
    } else {
        out.push_str("| key | value |\n| --- | --- |\n");
        for (key, value) in diagnostics {
            let _ = writeln!(out, "| {} | {} |", md_cell(key), md_cell(value));
        }
        out.push('\n');
    }

    out.push_str("## Transcript\n\n");
    out.push_str(body);
    if !body.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Escape a value for a Markdown table cell: flatten newlines and escape the
/// cell delimiter so a value never breaks the table layout.
fn md_cell(value: &str) -> String {
    value.replace('\n', " ").replace('|', "\\|")
}

fn render_json(
    meta: &BundleMeta,
    diagnostics: &[(String, String)],
    body: &str,
    checksum: &str,
    redact: bool,
    redactions: usize,
) -> String {
    #[derive(serde::Serialize)]
    struct Manifest<'a> {
        session: &'a str,
        model: &'a str,
        mode: &'a str,
        provider: &'a str,
        version: &'a str,
        workspace: &'a str,
        transcript_entries: usize,
        generated_at_unix: u64,
        redacted: bool,
        redactions: usize,
        transcript_sha256: &'a str,
    }
    #[derive(serde::Serialize)]
    struct Diagnostic<'a> {
        key: &'a str,
        value: &'a str,
    }
    #[derive(serde::Serialize)]
    struct Bundle<'a> {
        manifest: Manifest<'a>,
        diagnostics: Vec<Diagnostic<'a>>,
        transcript: &'a str,
    }

    let bundle = Bundle {
        manifest: Manifest {
            session: &meta.session_id,
            model: &meta.model,
            mode: &meta.mode,
            provider: &meta.provider,
            version: &meta.version,
            workspace: &meta.workspace,
            transcript_entries: meta.transcript_entries,
            generated_at_unix: meta.generated_at_unix,
            redacted: redact,
            redactions,
            transcript_sha256: checksum,
        },
        diagnostics: diagnostics
            .iter()
            .map(|(k, v)| Diagnostic { key: k, value: v })
            .collect(),
        transcript: body,
    };
    // The shape is plain owned/borrowed strings + numbers, so serialization
    // cannot fail; degrade to an empty object rather than panic if it somehow
    // does.
    serde_json::to_string_pretty(&bundle).unwrap_or_else(|_| "{}".to_string())
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for b in digest {
        let _ = write!(out, "{b:02x}");
    }
    out
}

#[cfg(test)]
#[path = "session_bundle_tests.rs"]
mod tests;
