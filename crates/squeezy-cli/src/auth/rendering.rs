use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::provider::{InlineKeyList, StatusRow};

pub(super) fn expires_in_human(expires_at_unix_ms: u64) -> Option<String> {
    let expiry = UNIX_EPOCH.checked_add(Duration::from_millis(expires_at_unix_ms))?;
    let now = SystemTime::now();
    let remaining = expiry.duration_since(now).ok()?;
    let secs = remaining.as_secs();
    if secs < 60 {
        Some(format!("{secs}s"))
    } else if secs < 3600 {
        Some(format!("{}m", secs / 60))
    } else {
        Some(format!("{}h {}m", secs / 3600, (secs % 3600) / 60))
    }
}

pub(super) fn redact_secret(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return "<empty>".to_string();
    }
    let len = trimmed.chars().count();
    if len <= 8 {
        return "*".repeat(len);
    }
    let prefix: String = trimmed.chars().take(4).collect();
    let suffix: String = trimmed
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{prefix}…{suffix}")
}

pub(super) fn print_inline_keys_table(list: &InlineKeyList) {
    if list.entries.is_empty() {
        println!("No inline provider api_key entries found in user, project, or local settings.");
        return;
    }
    let mut rows: Vec<[String; 4]> = Vec::with_capacity(list.entries.len() + 1);
    rows.push([
        "PROVIDER".to_string(),
        "TIER".to_string(),
        "KEY".to_string(),
        "PATH".to_string(),
    ]);
    for entry in &list.entries {
        rows.push([
            entry.provider.clone(),
            entry.tier.as_str().to_string(),
            entry.redacted.clone(),
            entry.path.display().to_string(),
        ]);
    }
    print_table_rows(&rows);
}

pub(super) fn print_status_table(rows: &[StatusRow]) {
    if rows.is_empty() {
        println!("No providers to report.");
        return;
    }
    let mut grid: Vec<[String; 4]> = Vec::with_capacity(rows.len() + 1);
    grid.push([
        "PROVIDER".to_string(),
        "SOURCE".to_string(),
        "ENV VAR".to_string(),
        "INLINE".to_string(),
    ]);
    for row in rows {
        let env_cell = if row.env_set {
            format!("{} (set)", row.env_var)
        } else if let Some(fallback) = row.fallback_env_var
            && row.fallback_env_set
        {
            format!("{} (fallback set)", fallback)
        } else {
            row.env_var.to_string()
        };
        let inline_cell = match (&row.inline_tier, &row.inline_path) {
            (Some(tier), Some(path)) => format!("{} ({})", tier.as_str(), path.display()),
            _ => "(unset)".to_string(),
        };
        // On Windows, annotate file-backed sources so users know the key
        // is not protected by Windows Credential Manager or DPAPI.
        let source_cell = if cfg!(windows) && row.is_file_backed() {
            format!("{} [file-backed]", row.effective_source())
        } else {
            row.effective_source().to_string()
        };
        grid.push([row.provider.to_string(), source_cell, env_cell, inline_cell]);
    }
    print_table_rows(&grid);
    println!("(set) = env var is set · (fallback set) = fallback env var is set");
}

pub(super) fn redact_oauth_token(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return "<empty>".to_string();
    }
    let prefix: String = trimmed.chars().take(12).collect();
    format!("{prefix}…")
}

fn print_table_rows(rows: &[[String; 4]]) {
    let widths: Vec<usize> = (0..4)
        .map(|col| rows.iter().map(|row| row[col].len()).max().unwrap_or(0))
        .collect();
    for row in rows {
        println!(
            "{:<w0$}  {:<w1$}  {:<w2$}  {}",
            row[0],
            row[1],
            row[2],
            row[3],
            w0 = widths[0],
            w1 = widths[1],
            w2 = widths[2],
        );
    }
}
