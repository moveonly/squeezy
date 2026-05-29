use std::path::PathBuf;

use squeezy_skills::{
    PromptTemplate, PromptTemplateSource, SkillContextMode, SkillSource, SkillSummary,
};
use squeezy_store::{SessionMetadata, SessionStatus};

use super::*;
use crate::providers::ProviderEntry;

fn skill(name: &str, source: SkillSource, description: &str) -> SkillSummary {
    SkillSummary {
        name: name.to_string(),
        description: description.to_string(),
        when_to_use: None,
        source,
        location: PathBuf::from(format!("/skills/{name}/SKILL.md")),
        disabled: false,
        manifest: None,
        context_mode: SkillContextMode::Inline,
    }
}

fn provider(name: &'static str, api_key_env: &'static str, configured: bool) -> ProviderEntry {
    ProviderEntry {
        name,
        display_name: name,
        base_url: "https://example.test",
        api_key_env,
        configured,
        full_tier: true,
        model_count: 3,
    }
}

fn session(id: &str, status: SessionStatus, label: &str) -> SessionMetadata {
    SessionMetadata {
        session_id: id.to_string(),
        status,
        cwd: "/repo".to_string(),
        first_user_task: Some(label.to_string()),
        provider: "openai".to_string(),
        model: "gpt-5.5".to_string(),
        ..SessionMetadata::default()
    }
}

fn template(name: &str, source: PromptTemplateSource, description: &str) -> PromptTemplate {
    PromptTemplate {
        name: name.to_string(),
        description: description.to_string(),
        argument_hint: None,
        args: Vec::new(),
        content: String::new(),
        source,
        path: PathBuf::from(format!("/prompts/{name}.md")),
    }
}

#[test]
fn render_text_emits_empty_section_for_each_resource_kind() {
    let inputs = BrowseInputs::default();

    let text = render_text(&inputs);

    assert!(text.contains("SKILLS (0)"));
    assert!(text.contains("(none discovered)"));
    assert!(text.contains("PROVIDERS (0 known, 0 configured)"));
    assert!(text.contains("(none registered)"));
    assert!(text.contains("SESSIONS (0)"));
    assert!(text.contains("(no sessions recorded)"));
    assert!(text.contains("PROMPT TEMPLATES (0)"));
}

#[test]
fn render_text_lists_each_resource_in_its_section() {
    let inputs = BrowseInputs {
        skills: vec![
            skill("review-pr", SkillSource::Project, "Review the open PR"),
            skill("ship-it", SkillSource::User, "Personal shipping checklist"),
        ],
        providers: vec![
            provider("openai", "OPENAI_API_KEY", true),
            provider("groq", "GROQ_API_KEY", false),
        ],
        sessions: vec![
            session("ses-abc", SessionStatus::Running, "fix the bug"),
            session("ses-def", SessionStatus::Completed, "add feature"),
        ],
        templates: vec![
            template("review", PromptTemplateSource::Project, "Review a file"),
            template("summarize", PromptTemplateSource::User, "Summarize content"),
        ],
    };

    let text = render_text(&inputs);

    // Section headers count the populated entries.
    assert!(text.contains("SKILLS (2)"));
    assert!(text.contains("PROVIDERS (2 known, 1 configured)"));
    assert!(text.contains("SESSIONS (2)"));
    assert!(text.contains("PROMPT TEMPLATES (2)"));

    // Skills surface name + source + description.
    assert!(text.contains("review-pr"));
    assert!(text.contains("project"));
    assert!(text.contains("Review the open PR"));
    assert!(text.contains("ship-it"));
    assert!(text.contains("Personal shipping checklist"));

    // Provider rows include env var and configured state.
    assert!(text.contains("OPENAI_API_KEY"));
    assert!(text.contains("configured"));
    assert!(text.contains("GROQ_API_KEY"));
    assert!(text.contains("unset"));

    // Sessions surface id + status + label.
    assert!(text.contains("ses-abc"));
    assert!(text.contains("running"));
    assert!(text.contains("fix the bug"));
    assert!(text.contains("ses-def"));
    assert!(text.contains("completed"));
    assert!(text.contains("add feature"));

    // Prompt templates render with the leading slash for discoverability.
    assert!(text.contains("/review"));
    assert!(text.contains("Review a file"));
    assert!(text.contains("/summarize"));
    assert!(text.contains("Summarize content"));
}

#[test]
fn render_text_flags_disabled_skills() {
    let mut disabled = skill("legacy", SkillSource::Project, "old skill");
    disabled.disabled = true;
    let inputs = BrowseInputs {
        skills: vec![disabled],
        ..BrowseInputs::default()
    };

    let text = render_text(&inputs);

    assert!(text.contains("legacy"));
    assert!(text.contains("(disabled)"));
}

#[test]
fn render_text_caps_sessions_section_at_preview_limit() {
    let sessions: Vec<SessionMetadata> = (0..SESSION_PREVIEW_LIMIT + 5)
        .map(|i| session(&format!("ses-{i:02}"), SessionStatus::Completed, "task"))
        .collect();
    let inputs = BrowseInputs {
        sessions,
        ..BrowseInputs::default()
    };

    let text = render_text(&inputs);

    // Header still reports the full count so callers can tell there are
    // more than the visible preview.
    assert!(
        text.contains(&format!("SESSIONS ({})", SESSION_PREVIEW_LIMIT + 5)),
        "expected full session count in header, got:\n{text}",
    );
    // The first preview session appears; the post-cap session is hidden.
    assert!(text.contains("ses-00"));
    assert!(!text.contains(&format!("ses-{:02}", SESSION_PREVIEW_LIMIT + 1)));
    // The overflow hint points users at `squeezy sessions list`.
    assert!(text.contains("more (run `squeezy sessions list`)"));
}

#[test]
fn render_text_template_argument_hint_appears_inline() {
    let mut tmpl = template("review", PromptTemplateSource::User, "Review");
    tmpl.argument_hint = Some("<path>".to_string());
    let inputs = BrowseInputs {
        templates: vec![tmpl],
        ..BrowseInputs::default()
    };

    let text = render_text(&inputs);

    assert!(text.contains("/review"));
    assert!(text.contains("Review <path>"));
}

#[test]
fn render_json_serializes_every_section_with_expected_shape() {
    let inputs = BrowseInputs {
        skills: vec![skill("review-pr", SkillSource::Project, "Review PR")],
        providers: vec![provider("openai", "OPENAI_API_KEY", true)],
        sessions: vec![session("ses-abc", SessionStatus::Running, "fix")],
        templates: vec![template(
            "review",
            PromptTemplateSource::Project,
            "Review a file",
        )],
    };

    let value = render_json(&inputs);

    let skills = value["skills"].as_array().expect("skills array");
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0]["name"], "review-pr");
    assert_eq!(skills[0]["source"], "project");
    assert_eq!(skills[0]["disabled"], false);

    let providers = value["providers"].as_array().expect("providers array");
    assert_eq!(providers.len(), 1);
    assert_eq!(providers[0]["name"], "openai");
    assert_eq!(providers[0]["api_key_env"], "OPENAI_API_KEY");
    assert_eq!(providers[0]["configured"], true);

    let sessions = value["sessions"].as_array().expect("sessions array");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["session_id"], "ses-abc");
    assert_eq!(sessions[0]["status"], "running");
    assert_eq!(sessions[0]["label"], "fix");
    assert_eq!(value["session_count"], 1);

    let templates = value["prompt_templates"]
        .as_array()
        .expect("templates array");
    assert_eq!(templates.len(), 1);
    assert_eq!(templates[0]["name"], "review");
    assert_eq!(templates[0]["source"], "project");
    assert_eq!(templates[0]["description"], "Review a file");
}

#[test]
fn render_json_session_section_respects_preview_limit() {
    let total = SESSION_PREVIEW_LIMIT + 3;
    let sessions: Vec<SessionMetadata> = (0..total)
        .map(|i| session(&format!("ses-{i:02}"), SessionStatus::Completed, "task"))
        .collect();
    let inputs = BrowseInputs {
        sessions,
        ..BrowseInputs::default()
    };

    let value = render_json(&inputs);

    assert_eq!(
        value["sessions"].as_array().map(Vec::len),
        Some(SESSION_PREVIEW_LIMIT),
        "json preview should also cap at SESSION_PREVIEW_LIMIT",
    );
    assert_eq!(value["session_count"], total);
}

#[test]
fn session_label_prefers_display_name_then_first_task_then_summary() {
    let mut renamed = session("ses-1", SessionStatus::Running, "first task");
    renamed.display_name = Some("Memorable Name".to_string());
    assert_eq!(session_label(&renamed), "Memorable Name");

    let task_only = session("ses-2", SessionStatus::Running, "first task");
    assert_eq!(session_label(&task_only), "first task");

    let mut summary_only = session("ses-3", SessionStatus::Completed, "task");
    summary_only.first_user_task = None;
    summary_only.latest_summary = Some("wrap-up\nnotes".to_string());
    // Newlines collapse so the label stays one row in the listing.
    assert_eq!(session_label(&summary_only), "wrap-up notes");

    let mut blank = session("ses-4", SessionStatus::Running, "task");
    blank.first_user_task = None;
    blank.latest_summary = None;
    blank.display_name = None;
    assert_eq!(session_label(&blank), "");
}
