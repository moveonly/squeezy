use std::path::Path;
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::driver::EvalError;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TicketDraft {
    pub id: String,
    pub title: String,
    pub severity: String,
    pub category: String,
    pub summary: String,
    pub repro: String,
    #[serde(default)]
    pub evidence: Vec<EvidencePointer>,
    #[serde(default)]
    pub suggested_fix: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidencePointer {
    #[serde(default)]
    pub trace_event: Option<u64>,
    #[serde(default)]
    pub frame: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct EmitOptions {
    pub emit_github: bool,
    pub gh_repo: Option<String>,
}

pub fn emit(
    run_dir: &Path,
    tickets: &[TicketDraft],
    options: EmitOptions,
) -> Result<(), EvalError> {
    if tickets.is_empty() {
        return Ok(());
    }
    let tickets_dir = run_dir.join("tickets");
    std::fs::create_dir_all(&tickets_dir)
        .map_err(|err| EvalError::Io(format!("create_dir_all {tickets_dir:?}: {err}")))?;

    for (idx, ticket) in tickets.iter().enumerate() {
        let slug = sanitize_slug(if ticket.id.is_empty() {
            &ticket.title
        } else {
            &ticket.id
        });
        let stem = format!("{:02}-{}", idx + 1, slug);
        let md_path = tickets_dir.join(format!("{stem}.md"));
        let json_path = tickets_dir.join(format!("{stem}.json"));

        let body = render_markdown(ticket);
        std::fs::write(&md_path, &body)
            .map_err(|err| EvalError::Io(format!("write {md_path:?}: {err}")))?;
        let json = serde_json::to_vec_pretty(ticket)
            .map_err(|err| EvalError::Internal(format!("serialize ticket: {err}")))?;
        std::fs::write(&json_path, json)
            .map_err(|err| EvalError::Io(format!("write {json_path:?}: {err}")))?;

        if options.emit_github {
            if let Some(repo) = &options.gh_repo {
                if let Err(err) = file_github_issue(repo, &ticket.title, &body) {
                    eprintln!("warning: gh issue create failed: {err}");
                }
            } else {
                eprintln!(
                    "warning: --emit github set but --gh-repo missing; wrote {} only",
                    md_path.display()
                );
            }
        }
    }
    Ok(())
}

fn render_markdown(ticket: &TicketDraft) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "# [squeezy-eval] {}", ticket.title);
    let _ = writeln!(out);
    let _ = writeln!(out, "- **Severity:** {}", ticket.severity);
    let _ = writeln!(out, "- **Category:** {}", ticket.category);
    let _ = writeln!(out);
    let _ = writeln!(out, "## Summary");
    let _ = writeln!(out, "{}", ticket.summary);
    let _ = writeln!(out);
    let _ = writeln!(out, "## Repro");
    let _ = writeln!(out, "{}", ticket.repro);
    let _ = writeln!(out);
    if let Some(fix) = &ticket.suggested_fix {
        let _ = writeln!(out, "## Suggested fix");
        let _ = writeln!(out, "{fix}");
        let _ = writeln!(out);
    }
    if !ticket.evidence.is_empty() {
        let _ = writeln!(out, "## Evidence");
        for ev in &ticket.evidence {
            match (ev.trace_event, ev.frame) {
                (Some(t), Some(f)) => {
                    let _ = writeln!(out, "- trace_event {t}, frame {f}");
                }
                (Some(t), None) => {
                    let _ = writeln!(out, "- trace_event {t}");
                }
                (None, Some(f)) => {
                    let _ = writeln!(out, "- frame {f}");
                }
                (None, None) => {}
            }
        }
    }
    out
}

fn file_github_issue(repo: &str, title: &str, body: &str) -> Result<(), String> {
    let output = Command::new("gh")
        .args([
            "issue",
            "create",
            "--repo",
            repo,
            "--title",
            title,
            "--body-file",
            "-",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|err| format!("spawn gh: {err}"))?;
    let mut child = output;
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        let _ = stdin.write_all(body.as_bytes());
    }
    let output = child
        .wait_with_output()
        .map_err(|err| format!("wait gh: {err}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    Ok(())
}

fn sanitize_slug(input: &str) -> String {
    let mut buf = String::new();
    for c in input.chars() {
        if c.is_alphanumeric() {
            buf.push(c.to_ascii_lowercase());
        } else if matches!(c, '-' | '_' | ' ') && !buf.ends_with('-') {
            buf.push('-');
        }
    }
    let trimmed = buf.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "ticket".into()
    } else if trimmed.len() > 48 {
        trimmed[..48].trim_matches('-').into()
    } else {
        trimmed
    }
}

#[cfg(test)]
#[path = "tickets_tests.rs"]
mod tests;
