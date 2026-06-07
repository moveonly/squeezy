use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use squeezy_core::{AppConfig, Redactor, Result, SqueezyError};

use crate::{RepoProfile, SessionEvent, SessionStore};

const REPORT_SCHEMA_VERSION: u32 = 1;
const MANIFEST_PATH: &str = "manifest.json";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BugReportOptions {
    pub excluded_sections: BTreeSet<String>,
    pub max_section_bytes: usize,
    pub max_archive_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BugReportSectionManifest {
    pub name: String,
    pub path: String,
    pub bytes: usize,
    pub redactions: u64,
    pub omitted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BugReportBundle {
    pub report_id: String,
    pub session_id: String,
    pub archive_bytes: Vec<u8>,
    pub manifest: Value,
    pub sections: Vec<BugReportSectionManifest>,
    pub redactions: u64,
}

impl BugReportBundle {
    pub fn preview_text(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("report_id={}\n", self.report_id));
        out.push_str(&format!("session_id={}\n", self.session_id));
        out.push_str(&format!("archive_bytes={}\n", self.archive_bytes.len()));
        out.push_str(&format!("redactions={}\n", self.redactions));
        out.push_str("sections:\n");
        for section in &self.sections {
            let omitted = if section.omitted { " omitted" } else { "" };
            out.push_str(&format!(
                "- {} {} bytes={} redactions={}{}\n",
                section.name, section.path, section.bytes, section.redactions, omitted
            ));
        }
        out
    }

    pub fn write_archive(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, &self.archive_bytes)?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct BugReportSection {
    manifest: BugReportSectionManifest,
    bytes: Vec<u8>,
}

impl SessionStore {
    pub fn build_bug_report(
        &self,
        config: &AppConfig,
        session_id: &str,
        mut options: BugReportOptions,
    ) -> Result<BugReportBundle> {
        if options.max_section_bytes == 0 {
            options.max_section_bytes = config.session_logs.max_event_bytes;
        }
        if options.max_archive_bytes == 0 {
            options.max_archive_bytes = config.feedback.max_report_bytes;
        }

        let redactor = config.redaction.redactor()?;
        let record = self.show_without_context_attachments(session_id)?;
        let report_id = random_uuid_like();
        let mut sections = Vec::new();
        let mut excluded = BTreeSet::new();

        add_json_section(
            &mut sections,
            &mut excluded,
            &options,
            &redactor,
            "version",
            "version.json",
            json!({
                "squeezy_version": env!("CARGO_PKG_VERSION"),
                "os": std::env::consts::OS,
                "arch": std::env::consts::ARCH,
            }),
        )?;
        add_text_section(
            &mut sections,
            &mut excluded,
            &options,
            &redactor,
            "config",
            "config.toml",
            config.inspect_redacted(),
        )?;
        add_json_section(
            &mut sections,
            &mut excluded,
            &options,
            &redactor,
            "repo_profile",
            "repo_profile.json",
            repo_profile_value(config, &redactor),
        )?;
        add_json_section(
            &mut sections,
            &mut excluded,
            &options,
            &redactor,
            "session_metadata",
            "session/metadata.json",
            serde_json::to_value(&record.metadata).map_err(json_error)?,
        )?;
        let (events, event_redactions) = events_jsonl(&record.events, &redactor)?;
        add_redacted_text_section(
            &mut sections,
            &mut excluded,
            &options,
            "events",
            "session/events.jsonl",
            events,
            event_redactions,
        )?;
        add_json_section(
            &mut sections,
            &mut excluded,
            &options,
            &redactor,
            "summaries",
            "summaries/tool_cost.json",
            json!({
                "cost": record.metadata.cost,
                "metrics": record.metadata.metrics,
            }),
        )?;
        add_json_section(
            &mut sections,
            &mut excluded,
            &options,
            &redactor,
            "permissions",
            "summaries/permissions.json",
            permission_events_value(&record.events),
        )?;
        add_json_section(
            &mut sections,
            &mut excluded,
            &options,
            &redactor,
            "diagnostics",
            "diagnostics/errors.json",
            json!({
                "status": record.metadata.status.as_str(),
                "latest_summary": record.metadata.latest_summary,
                "resume_available": record.metadata.resume_available,
                "resume_unavailable_reason": record.metadata.resume_unavailable_reason,
                "event_warnings": record.event_warnings,
                "failure_events": record.events.iter()
                    .filter(|event| matches!(event.kind.as_str(), "failed" | "cancelled"))
                    .collect::<Vec<_>>(),
            }),
        )?;
        add_json_section(
            &mut sections,
            &mut excluded,
            &options,
            &redactor,
            "replay",
            "replay.json",
            json!({
                "resume_available": record
                    .resume_state
                    .as_ref()
                    .is_some_and(|state| state.resume_available),
                "previous_response_id": record
                    .resume_state
                    .as_ref()
                    .and_then(|state| state.previous_response_id.clone()),
                "tape": record.replay,
            }),
        )?;

        let mut archive_bytes = None;
        let mut converged_archive = None;
        for _ in 0..8 {
            let manifest =
                manifest_value(&report_id, session_id, &sections, &excluded, archive_bytes);
            let manifest_entry = manifest_section(&manifest, &redactor, options.max_section_bytes)?;
            let archive = tar_archive(
                std::iter::once(&manifest_entry)
                    .chain(sections.iter())
                    .collect::<Vec<_>>()
                    .as_slice(),
            )?;
            if archive_bytes == Some(archive.len()) {
                converged_archive = Some((manifest, manifest_entry, archive));
                break;
            }
            archive_bytes = Some(archive.len());
        }
        let Some((manifest, manifest_entry, archive)) = converged_archive else {
            return Err(SqueezyError::Tool(
                "bug report archive size did not stabilize".to_string(),
            ));
        };
        if archive.len() > options.max_archive_bytes {
            return Err(SqueezyError::Tool(format!(
                "bug report archive is {} bytes, exceeding max_report_bytes {}",
                archive.len(),
                options.max_archive_bytes
            )));
        }

        let mut manifests = vec![manifest_entry.manifest.clone()];
        manifests.extend(sections.iter().map(|section| section.manifest.clone()));
        let redactions = manifests
            .iter()
            .map(|section| section.redactions)
            .sum::<u64>();

        Ok(BugReportBundle {
            report_id,
            session_id: session_id.to_string(),
            archive_bytes: archive,
            manifest,
            sections: manifests,
            redactions,
        })
    }
}

pub fn default_bug_report_path(config: &AppConfig, session_id: &str) -> PathBuf {
    let safe_id = session_id.replace(['/', '\\', ':'], "_");
    config
        .workspace_root
        .join(".squeezy")
        .join("reports")
        .join(format!("{safe_id}-bug-report.tar"))
}

pub fn parse_bug_report_section(value: &str) -> Option<&'static str> {
    match value.trim() {
        "version" => Some("version"),
        "config" => Some("config"),
        "repo_profile" | "repo-profile" => Some("repo_profile"),
        "session_metadata" | "session-metadata" | "metadata" => Some("session_metadata"),
        "events" => Some("events"),
        "summaries" | "tool_cost" | "tool-cost" => Some("summaries"),
        "permissions" => Some("permissions"),
        "diagnostics" | "errors" => Some("diagnostics"),
        "replay" => Some("replay"),
        _ => None,
    }
}

fn add_json_section(
    sections: &mut Vec<BugReportSection>,
    excluded: &mut BTreeSet<String>,
    options: &BugReportOptions,
    redactor: &Redactor,
    name: &str,
    path: &str,
    value: Value,
) -> Result<()> {
    if options.excluded_sections.contains(name) {
        excluded.insert(name.to_string());
        return Ok(());
    }
    let mut redactions = 0;
    let redacted = redact_json_value(value, redactor, &mut redactions);
    let bytes = serde_json::to_vec_pretty(&redacted).map_err(json_error)?;
    add_section_bytes(sections, options, name, path, bytes, redactions)
}

fn add_text_section(
    sections: &mut Vec<BugReportSection>,
    excluded: &mut BTreeSet<String>,
    options: &BugReportOptions,
    redactor: &Redactor,
    name: &str,
    path: &str,
    text: String,
) -> Result<()> {
    if options.excluded_sections.contains(name) {
        excluded.insert(name.to_string());
        return Ok(());
    }
    let redacted = redactor.redact(&text);
    add_section_bytes(
        sections,
        options,
        name,
        path,
        redacted.text.into_bytes(),
        redacted.redactions,
    )
}

fn add_redacted_text_section(
    sections: &mut Vec<BugReportSection>,
    excluded: &mut BTreeSet<String>,
    options: &BugReportOptions,
    name: &str,
    path: &str,
    text: String,
    redactions: u64,
) -> Result<()> {
    if options.excluded_sections.contains(name) {
        excluded.insert(name.to_string());
        return Ok(());
    }
    add_section_bytes(sections, options, name, path, text.into_bytes(), redactions)
}

fn add_section_bytes(
    sections: &mut Vec<BugReportSection>,
    options: &BugReportOptions,
    name: &str,
    path: &str,
    mut bytes: Vec<u8>,
    redactions: u64,
) -> Result<()> {
    let mut omitted = false;
    if bytes.len() > options.max_section_bytes {
        omitted = true;
        bytes = serde_json::to_vec_pretty(&json!({
            "omitted": true,
            "reason": "section exceeded max_section_bytes",
            "original_bytes": bytes.len(),
            "max_section_bytes": options.max_section_bytes,
        }))
        .map_err(json_error)?;
    }
    sections.push(BugReportSection {
        manifest: BugReportSectionManifest {
            name: name.to_string(),
            path: path.to_string(),
            bytes: bytes.len(),
            redactions,
            omitted,
        },
        bytes,
    });
    Ok(())
}

fn manifest_section(
    manifest: &Value,
    redactor: &Redactor,
    max_section_bytes: usize,
) -> Result<BugReportSection> {
    let mut redactions = 0;
    let redacted = redact_json_value(manifest.clone(), redactor, &mut redactions);
    let bytes = serde_json::to_vec_pretty(&redacted).map_err(json_error)?;
    let omitted = bytes.len() > max_section_bytes;
    let bytes = if omitted {
        serde_json::to_vec_pretty(&json!({
            "omitted": true,
            "reason": "manifest exceeded max_section_bytes",
            "original_bytes": bytes.len(),
            "max_section_bytes": max_section_bytes,
        }))
        .map_err(json_error)?
    } else {
        bytes
    };
    Ok(BugReportSection {
        manifest: BugReportSectionManifest {
            name: "manifest".to_string(),
            path: MANIFEST_PATH.to_string(),
            bytes: bytes.len(),
            redactions,
            omitted,
        },
        bytes,
    })
}

fn manifest_value(
    report_id: &str,
    session_id: &str,
    sections: &[BugReportSection],
    excluded: &BTreeSet<String>,
    archive_bytes: Option<usize>,
) -> Value {
    json!({
        "schema_version": REPORT_SCHEMA_VERSION,
        "report_id": report_id,
        "session_id": session_id,
        "generated_unix_ms": now_ms(),
        "archive_bytes": archive_bytes,
        "sections": sections
            .iter()
            .map(|section| &section.manifest)
            .collect::<Vec<_>>(),
        "excluded_sections": excluded.iter().collect::<Vec<_>>(),
        "redactions": sections
            .iter()
            .map(|section| section.manifest.redactions)
            .sum::<u64>(),
    })
}

fn repo_profile_value(config: &AppConfig, redactor: &Redactor) -> Value {
    match RepoProfile::detect(&config.workspace_root, &config.graph) {
        Ok(profile) => json!({
            "status": "detected",
            "profile": profile,
        }),
        Err(error) => json!({
            "status": "unavailable",
            "error": redactor.redact(&error.to_string()).text,
        }),
    }
}

fn events_jsonl(events: &[SessionEvent], redactor: &Redactor) -> Result<(String, u64)> {
    let mut out = String::new();
    let mut redactions = 0;
    for event in events {
        let redacted = redact_json_value(
            serde_json::to_value(event).map_err(json_error)?,
            redactor,
            &mut redactions,
        );
        let line = serde_json::to_string(&redacted).map_err(json_error)?;
        out.push_str(&line);
        out.push('\n');
    }
    Ok((out, redactions))
}

fn permission_events_value(events: &[SessionEvent]) -> Value {
    Value::Array(
        events
            .iter()
            .filter(|event| {
                matches!(
                    event.kind.as_str(),
                    "approval_requested" | "approval_decided"
                )
            })
            .map(|event| {
                serde_json::to_value(event)
                    .unwrap_or_else(|_| json!({"error": "event serialization failed"}))
            })
            .collect(),
    )
}

fn redact_json_value(value: Value, redactor: &Redactor, redactions: &mut u64) -> Value {
    match value {
        Value::String(text) => {
            let redacted = redactor.redact(&text);
            *redactions = redactions.saturating_add(redacted.redactions);
            Value::String(redacted.text)
        }
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| redact_json_value(item, redactor, redactions))
                .collect(),
        ),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, value)| {
                    let redacted_key = redactor.redact(&key);
                    *redactions = redactions.saturating_add(redacted_key.redactions);
                    (
                        redacted_key.text,
                        redact_json_value(value, redactor, redactions),
                    )
                })
                .collect(),
        ),
        other => other,
    }
}

fn tar_archive(sections: &[&BugReportSection]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for section in sections {
        append_tar_file(&mut out, &section.manifest.path, &section.bytes)?;
    }
    out.extend_from_slice(&[0u8; 1024]);
    Ok(out)
}

fn append_tar_file(out: &mut Vec<u8>, path: &str, bytes: &[u8]) -> Result<()> {
    if path.len() > 100 {
        return Err(SqueezyError::Tool(format!(
            "bug report tar path is too long: {path}"
        )));
    }
    let mut header = [0u8; 512];
    write_bytes(&mut header[0..100], path.as_bytes());
    write_octal(&mut header[100..108], 0o644);
    write_octal(&mut header[108..116], 0);
    write_octal(&mut header[116..124], 0);
    write_octal(&mut header[124..136], bytes.len() as u64);
    write_octal(&mut header[136..148], now_ms() / 1000);
    for byte in &mut header[148..156] {
        *byte = b' ';
    }
    header[156] = b'0';
    write_bytes(&mut header[257..263], b"ustar\0");
    write_bytes(&mut header[263..265], b"00");
    let checksum = header.iter().map(|byte| *byte as u64).sum::<u64>();
    write_checksum(&mut header[148..156], checksum);
    out.extend_from_slice(&header);
    out.extend_from_slice(bytes);
    let padding = (512 - (bytes.len() % 512)) % 512;
    out.extend(std::iter::repeat_n(0, padding));
    Ok(())
}

fn write_bytes(field: &mut [u8], value: &[u8]) {
    let len = field.len().min(value.len());
    field[..len].copy_from_slice(&value[..len]);
}

fn write_octal(field: &mut [u8], value: u64) {
    let width = field.len().saturating_sub(1);
    let text = format!("{value:0width$o}");
    let bytes = text.as_bytes();
    let start = width.saturating_sub(bytes.len());
    field[..width].fill(b'0');
    field[start..width].copy_from_slice(&bytes[bytes.len().saturating_sub(width)..]);
    field[width] = 0;
}

fn write_checksum(field: &mut [u8], value: u64) {
    let text = format!("{value:06o}");
    field[..6].copy_from_slice(text.as_bytes());
    field[6] = 0;
    field[7] = b' ';
}

fn random_uuid_like() -> String {
    let mut bytes = [0u8; 16];
    if getrandom::fill(&mut bytes).is_err() {
        let fallback = now_ms().to_le_bytes();
        bytes[..fallback.len()].copy_from_slice(&fallback);
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    )
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
}

fn json_error(error: serde_json::Error) -> SqueezyError {
    SqueezyError::Tool(format!("failed to serialize bug report: {error}"))
}
