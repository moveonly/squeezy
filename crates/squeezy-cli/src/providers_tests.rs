use super::*;
use std::collections::HashMap;

fn lookup_from(map: HashMap<&'static str, &'static str>) -> impl Fn(&str) -> Option<String> {
    move |name: &str| map.get(name).map(|value| (*value).to_string())
}

#[test]
fn registry_includes_every_known_provider_id() {
    let entries = registry_entries(&|_| None);
    let names: Vec<&str> = entries.iter().map(|entry| entry.name).collect();
    // First-party providers + every `OpenAiCompatiblePreset::all()` entry.
    assert!(names.contains(&"openai"));
    assert!(names.contains(&"anthropic"));
    assert!(names.contains(&"google"));
    assert!(names.contains(&"azure_openai"));
    assert!(names.contains(&"bedrock"));
    assert!(names.contains(&"ollama"));
    assert!(names.contains(&"github_copilot"));
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
    assert_eq!(
        canonicalize_provider_name("github-copilot"),
        Some("github_copilot")
    );
    assert_eq!(
        canonicalize_provider_name("copilot"),
        Some("github_copilot")
    );
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

#[test]
fn ollama_is_always_configured_regardless_of_api_key() {
    // Ollama requires no auth by default; it must not appear unconfigured
    // just because OLLAMA_API_KEY is absent.
    let entries_no_key = registry_entries(&|_| None);
    let ollama_no_key = entries_no_key
        .iter()
        .find(|e| e.name == "ollama")
        .expect("ollama entry");
    assert!(
        ollama_no_key.configured,
        "ollama must be configured even without OLLAMA_API_KEY"
    );

    let mut with_key = HashMap::new();
    with_key.insert("OLLAMA_API_KEY", "ollama-cloud-token");
    let entries_with_key = registry_entries(&lookup_from(with_key));
    let ollama_with_key = entries_with_key
        .iter()
        .find(|e| e.name == "ollama")
        .expect("ollama entry");
    assert!(
        ollama_with_key.configured,
        "ollama still configured with key"
    );
}

#[test]
fn bedrock_configured_when_any_aws_cred_var_set() {
    let no_creds = registry_entries(&|_| None);
    let bedrock_no_creds = no_creds
        .iter()
        .find(|e| e.name == "bedrock")
        .expect("bedrock entry");
    assert!(
        !bedrock_no_creds.configured,
        "bedrock should not be configured when no AWS credential env vars are set"
    );

    // Each of these AWS credential signals should mark Bedrock as configured.
    for var in &[
        "AWS_ACCESS_KEY_ID",
        "AWS_PROFILE",
        "AWS_DEFAULT_PROFILE",
        "AWS_ROLE_ARN",
        "AWS_WEB_IDENTITY_TOKEN_FILE",
        "AWS_BEARER_TOKEN_BEDROCK",
    ] {
        let mut map = HashMap::new();
        map.insert(*var, "some-value");
        let entries = registry_entries(&lookup_from(map));
        let bedrock = entries.iter().find(|e| e.name == "bedrock").unwrap();
        assert!(
            bedrock.configured,
            "bedrock must be configured when {var} is set"
        );
    }
}
