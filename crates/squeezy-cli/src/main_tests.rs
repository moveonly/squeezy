use super::*;
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn model_choice_label_round_trips_to_model_id() {
    let model = models_for_provider("openai").next().expect("openai model");
    let label = model_choice_label(model);

    assert_eq!(parse_model_choice_id(&label), model.id);
}

#[test]
fn model_selection_state_detects_saved_startup_choice() {
    let settings = SettingsFile::from_toml_str(
        r#"
[model]
provider = "openai"
model = "gpt-5.5"
selection_version = 1
"#,
        "test",
    )
    .expect("settings parse");

    assert!(model_selection_state(&settings).complete());
}

#[test]
fn save_startup_model_selection_preserves_existing_settings() {
    let root = temp_dir("model-selection");
    let path = root.join("settings.toml");
    fs::write(
        &path,
        r#"
[permissions]
read = "deny"
"#,
    )
    .expect("write settings");
    let selection = StartupModelSelection {
        provider: "openai",
        model: "gpt-5.5".to_string(),
        api_key_env: Some("OPENAI_API_KEY".to_string()),
        base_url: None,
        reasoning_effort: Some(ReasoningEffort::XHigh),
    };

    save_startup_model_selection(&path, &selection).expect("save selection");

    let text = fs::read_to_string(&path).expect("read settings");
    assert!(text.contains("read = \"deny\""));
    assert!(text.contains("provider = \"openai\""));
    assert!(text.contains("model = \"gpt-5.5\""));
    assert!(text.contains("reasoning_effort = \"xhigh\""));
    assert!(text.contains("selection_version = 1"));
    assert!(text.contains("api_key_env = \"OPENAI_API_KEY\""));
    assert!(!text.contains("sk-"));

    let settings = SettingsFile::from_toml_str(&text, "test").expect("round-trip");
    assert!(model_selection_state(&settings).complete());
    let _ = fs::remove_dir_all(root);
}

fn temp_dir(name: &str) -> PathBuf {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let path = env::temp_dir().join(format!("squeezy-cli-{name}-{suffix}"));
    fs::create_dir_all(&path).expect("temp dir");
    path
}
