use super::*;
use std::collections::HashMap;

fn lookup_from(map: HashMap<&'static str, &'static str>) -> impl Fn(&str) -> Option<String> {
    move |name: &str| map.get(name).map(|value| (*value).to_string())
}

#[test]
fn registry_includes_every_known_provider_id() {
    let entries = registry_entries(&|_| None);
    let names: Vec<&str> = entries.iter().map(|entry| entry.name).collect();
    // Six first-party providers + every `OpenAiCompatiblePreset::all()` entry.
    assert!(names.contains(&"openai"));
    assert!(names.contains(&"anthropic"));
    assert!(names.contains(&"google"));
    assert!(names.contains(&"azure_openai"));
    assert!(names.contains(&"bedrock"));
    assert!(names.contains(&"ollama"));
    assert!(names.contains(&"openrouter"));
    assert!(names.contains(&"groq"));
    assert!(names.contains(&"xai"));
    assert!(names.contains(&"deepseek"));
    assert_eq!(
        entries.len(),
        BASE_PROVIDERS.len() + OpenAiCompatiblePreset::all().len(),
        "registry size must equal base + preset counts: {names:?}",
    );
}

#[test]
fn configured_flag_follows_env_var_state() {
    let mut populated = HashMap::new();
    populated.insert("OPENAI_API_KEY", "sk-test");
    populated.insert("OPENROUTER_API_KEY", "or-test");
    let lookup = lookup_from(populated);
    let entries = registry_entries(&lookup);
    let openai = entries
        .iter()
        .find(|entry| entry.name == "openai")
        .expect("openai entry");
    assert!(openai.configured, "openai should be flagged configured");
    let openrouter = entries
        .iter()
        .find(|entry| entry.name == "openrouter")
        .expect("openrouter entry");
    assert!(openrouter.configured);
    let groq = entries
        .iter()
        .find(|entry| entry.name == "groq")
        .expect("groq entry");
    assert!(
        !groq.configured,
        "groq should not be configured when GROQ_API_KEY is unset"
    );
}

#[test]
fn canonicalize_provider_name_accepts_known_aliases() {
    assert_eq!(canonicalize_provider_name("OpenAI"), Some("openai"));
    assert_eq!(canonicalize_provider_name("claude"), Some("anthropic"));
    assert_eq!(canonicalize_provider_name("gemini"), Some("google"));
    assert_eq!(canonicalize_provider_name("aws"), Some("bedrock"));
    assert_eq!(canonicalize_provider_name("grok"), Some("xai"));
    assert_eq!(canonicalize_provider_name("openrouter"), Some("openrouter"));
    assert_eq!(canonicalize_provider_name("nope"), None);
}

#[test]
fn env_set_treats_blank_strings_as_unset() {
    let mut map = HashMap::new();
    map.insert("FILLED", "yes");
    map.insert("BLANK", "   ");
    let lookup = lookup_from(map);
    assert!(env_set(&lookup, "FILLED"));
    assert!(!env_set(&lookup, "BLANK"));
    assert!(!env_set(&lookup, "MISSING"));
    assert!(!env_set(&lookup, ""));
}
