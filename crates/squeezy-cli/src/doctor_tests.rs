use super::*;
use squeezy_core::{McpPermissionConfig, McpServerConfig, McpTransport, ProviderSettings};
use std::collections::BTreeMap;
use std::sync::Mutex;

// env::set_var/remove_var is process-global; serialize these tests so a parallel
// runner does not let them race.
static ENV_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn env_check_reports_ok_when_var_set() {
    let _guard = ENV_LOCK.lock().expect("env lock");
    // SAFETY: the lock above serializes mutations to the process env.
    unsafe {
        env::set_var("SQUEEZY_DOCTOR_TEST_KEY", "1");
    }
    let (status, detail) = env_check("SQUEEZY_DOCTOR_TEST_KEY");
    unsafe {
        env::remove_var("SQUEEZY_DOCTOR_TEST_KEY");
    }
    assert_eq!(status, Status::Ok);
    assert!(detail.contains("SQUEEZY_DOCTOR_TEST_KEY"));
}

#[test]
fn env_check_warns_when_unset() {
    let _guard = ENV_LOCK.lock().expect("env lock");
    unsafe {
        env::remove_var("SQUEEZY_DOCTOR_TEST_MISSING");
    }
    let (status, _) = env_check("SQUEEZY_DOCTOR_TEST_MISSING");
    assert_eq!(status, Status::Warn);
}

#[test]
fn probe_writable_round_trips_in_tempdir() {
    let dir = std::env::temp_dir().join(format!("squeezy-doctor-probe-{}", std::process::id(),));
    let _ = fs::remove_dir_all(&dir);
    probe_writable(&dir).expect("probe");
    // probe file should have been cleaned up
    assert!(!dir.join(".squeezy-doctor-probe").exists());
    let _ = fs::remove_dir_all(&dir);
}

fn mcp_fixture(enabled: bool, transport: McpTransport) -> McpServerConfig {
    McpServerConfig {
        enabled,
        transport,
        command: None,
        args: Vec::new(),
        url: None,
        timeout_ms: None,
        enabled_tools: None,
        disabled_tools: Vec::new(),
        env: BTreeMap::new(),
        permissions: McpPermissionConfig::default(),
        bearer_token_env_var: None,
        http_headers: BTreeMap::new(),
        env_http_headers: BTreeMap::new(),
    }
}

#[test]
fn mcp_check_with_no_servers_is_ok() {
    let check = mcp_check(&BTreeMap::new());
    assert_eq!(check.status, Status::Ok);
    assert!(check.detail.contains("no MCP servers"));
}

#[test]
fn mcp_check_warns_on_stdio_without_command() {
    let mut servers = BTreeMap::new();
    servers.insert("noisy".to_string(), mcp_fixture(true, McpTransport::Stdio));
    let check = mcp_check(&servers);
    assert_eq!(check.status, Status::Warn);
    assert!(check.detail.contains("stdio transport without command"));
    assert!(check.detail.contains("enabled=1"));
}

#[test]
fn mcp_check_warns_on_http_without_url() {
    let mut servers = BTreeMap::new();
    let mut server = mcp_fixture(true, McpTransport::Http);
    server.url = Some("   ".to_string());
    servers.insert("remote".to_string(), server);
    let check = mcp_check(&servers);
    assert_eq!(check.status, Status::Warn);
    assert!(check.detail.contains("http transport without url"));
}

#[test]
fn mcp_check_accepts_disabled_server_without_command() {
    let mut servers = BTreeMap::new();
    servers.insert("idle".to_string(), mcp_fixture(false, McpTransport::Stdio));
    let check = mcp_check(&servers);
    assert_eq!(check.status, Status::Ok);
    assert!(check.detail.contains("disabled=1"));
    assert!(check.detail.contains("enabled=0"));
}

#[test]
fn mcp_check_is_ok_when_fields_match_transport() {
    let mut servers = BTreeMap::new();
    let mut stdio = mcp_fixture(true, McpTransport::Stdio);
    stdio.command = Some("/usr/bin/example-server".to_string());
    servers.insert("local".to_string(), stdio);
    let mut http = mcp_fixture(true, McpTransport::Sse);
    http.url = Some("https://example.test/mcp".to_string());
    servers.insert("remote".to_string(), http);
    let check = mcp_check(&servers);
    assert_eq!(check.status, Status::Ok);
    assert!(check.detail.contains("enabled=2"));
}

#[test]
fn providers_check_reports_no_sections() {
    let settings = SettingsFile::default();
    let check = providers_check(&settings);
    assert_eq!(check.status, Status::Ok);
    assert!(check.detail.contains("no [providers.*]"));
}

#[test]
fn providers_check_marks_inline_api_key_configured() {
    let _guard = ENV_LOCK.lock().expect("env lock");
    let mut providers = BTreeMap::new();
    providers.insert(
        "openai".to_string(),
        ProviderSettings {
            api_key: Some("sk-test".to_string()),
            ..ProviderSettings::default()
        },
    );
    let settings = SettingsFile {
        providers: Some(providers),
        ..SettingsFile::default()
    };
    let check = providers_check(&settings);
    assert_eq!(check.status, Status::Ok);
    assert!(check.detail.contains("openai=configured"));
}

#[test]
fn providers_check_warns_when_env_unset() {
    let _guard = ENV_LOCK.lock().expect("env lock");
    unsafe {
        env::remove_var("SQUEEZY_DOCTOR_TEST_OPENAI_KEY");
    }
    let mut providers = BTreeMap::new();
    providers.insert(
        "openai".to_string(),
        ProviderSettings {
            api_key_env: Some("SQUEEZY_DOCTOR_TEST_OPENAI_KEY".to_string()),
            ..ProviderSettings::default()
        },
    );
    let settings = SettingsFile {
        providers: Some(providers),
        ..SettingsFile::default()
    };
    let check = providers_check(&settings);
    assert_eq!(check.status, Status::Warn);
    assert!(check.detail.contains("openai=missing api_key"));
}

#[test]
fn providers_check_treats_bedrock_and_ollama_as_keyless() {
    let _guard = ENV_LOCK.lock().expect("env lock");
    let mut providers = BTreeMap::new();
    providers.insert("bedrock".to_string(), ProviderSettings::default());
    providers.insert("ollama".to_string(), ProviderSettings::default());
    let settings = SettingsFile {
        providers: Some(providers),
        ..SettingsFile::default()
    };
    let check = providers_check(&settings);
    assert_eq!(check.status, Status::Ok);
    assert!(check.detail.contains("bedrock=keyless"));
    assert!(check.detail.contains("ollama=keyless"));
}

#[test]
fn state_store_check_opens_redb_in_tempdir() {
    let workspace = std::env::temp_dir().join(format!(
        "squeezy-doctor-state-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = fs::remove_dir_all(&workspace);
    fs::create_dir_all(&workspace).expect("create workspace");
    let mut config = AppConfig::from_env();
    config.workspace_root = workspace.clone();
    config.cache.root = None;
    let check = state_store_check(&config);
    let _ = fs::remove_dir_all(&workspace);
    assert_eq!(check.status, Status::Ok, "detail: {}", check.detail);
    assert!(check.detail.contains("opened"));
}

#[test]
fn state_store_check_fails_when_path_unwritable() {
    // Pointing the cache at /dev/null forces SqueezyStore::open to fail.
    let mut config = AppConfig::from_env();
    config.workspace_root = std::env::temp_dir();
    config.cache.root = Some(PathBuf::from("/dev/null/nope"));
    let check = state_store_check(&config);
    assert_eq!(check.status, Status::Fail, "detail: {}", check.detail);
}
