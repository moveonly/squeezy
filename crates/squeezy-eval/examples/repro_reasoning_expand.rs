//! Drive the live `TuiHarness` against a real OpenAI call and dump
//! the rendered frame BEFORE and AFTER Ctrl+O. Reproduces the
//! "expansion not visible" symptom (or proves the body lines DO
//! appear, in which case the bug is a terminal-side rendering
//! issue the harness can't see).
//!
//! Run with `OPENAI_API_KEY=... cargo run -p squeezy-eval
//! --example repro_reasoning_expand --release`.

use std::sync::Arc;

use squeezy_core::{AppConfig, PermissionMode, PermissionPolicy, SessionMode, TranscriptDefault};
use squeezy_llm::provider_from_config;
use squeezy_tui::testing::{TuiHarness, parse_key};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut config = AppConfig::from_env_and_settings_with_provider("openai")?;
    config.workspace_root = std::env::current_dir()?;
    config.model = "gpt-5.4-mini".to_string();
    config.session_mode = SessionMode::Build;
    config.permissions = PermissionPolicy {
        edit: PermissionMode::Allow,
        shell: PermissionMode::Allow,
        web: PermissionMode::Allow,
        ..config.permissions
    };
    config.reasoning_effort = Some(squeezy_core::ReasoningEffort::High);
    config.tui.show_reasoning_usage = true;
    config.tui.transcript_default = TranscriptDefault::Compact;
    config.max_output_tokens = Some(800);

    let provider = provider_from_config(&config.provider)?;
    let mut harness = TuiHarness::new(config, SessionMode::Build, provider, 160, 60)?;

    println!("=== sending prompt ===");
    harness.start_user_turn("hey");
    harness.pump_until_idle().await?;

    println!("\n=== transcript entries after turn ===");
    for (i, entry) in harness.transcript_entries().iter().enumerate() {
        println!(
            "  [{i}] kind={} collapsed={} preview={:?}",
            entry.kind, entry.collapsed, entry.preview
        );
    }
    println!("\n=== status (before Ctrl+O) ===");
    println!("  {:?}", harness.status_text());

    println!("\n=== FRAME BEFORE Ctrl+O ===");
    let before = harness.render_frame()?;
    dump_frame(&before.plain_text);

    let ctrl_o = parse_key("Ctrl+O").expect("Ctrl+O parses");
    harness.send_key(ctrl_o).await?;

    println!("\n=== transcript entries after Ctrl+O ===");
    for (i, entry) in harness.transcript_entries().iter().enumerate() {
        println!(
            "  [{i}] kind={} collapsed={} preview={:?}",
            entry.kind, entry.collapsed, entry.preview
        );
    }
    println!("\n=== status (after Ctrl+O) ===");
    println!("  {:?}", harness.status_text());

    println!("\n=== FRAME AFTER Ctrl+O ===");
    let after = harness.render_frame()?;
    dump_frame(&after.plain_text);

    println!("\n=== DIFF (lines only in AFTER) ===");
    let before_lines: std::collections::HashSet<&str> = before.plain_text.lines().collect();
    for line in after.plain_text.lines() {
        if !before_lines.contains(line) {
            println!("  + {line:?}");
        }
    }

    Ok(())
}

fn dump_frame(text: &str) {
    for (i, line) in text.lines().enumerate() {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            continue;
        }
        // Skip startup-card box borders and pure-whitespace lines so
        // the dump highlights actual transcript content.
        if trimmed
            .chars()
            .all(|c| matches!(c, '╭' | '╰' | '│' | '─' | '╮' | '╯' | ' '))
        {
            continue;
        }
        println!("  {i:3}: {trimmed}");
    }
}
