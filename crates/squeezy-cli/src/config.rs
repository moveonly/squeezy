use std::fs;

use clap::{Args, Subcommand};
use squeezy_core::{
    ConfigInitScope, ConfigLocator, SqueezyError,
    config_explain::{
        explain_effective_value, find_config_field_for_path, resolve_explain_field_source,
        split_config_field_path,
    },
    config_schema::FieldSource,
    load_separated_settings_sources,
    settings_writer::write_settings_atomic,
};

use crate::{Cli, config_browse::handle_browse_command, config_from_cli};

#[derive(Debug, Subcommand)]
pub(crate) enum ConfigCommand {
    #[command(
        about = "List discoverable resources (skills, providers, sessions, prompt templates)"
    )]
    Browse(ConfigBrowseArgs),
    #[command(about = "Print the effective merged configuration with secrets redacted")]
    Inspect,
    #[command(about = "Create a default user or project settings file")]
    Init {
        #[command(flatten)]
        scope: InitScope,
        #[arg(long, help = "Overwrite an existing file")]
        force: bool,
        #[arg(
            long = "with-bundled-skills",
            help = "After writing settings, install the in-binary bundled sample skills under the user skills directory (--user only)"
        )]
        with_bundled_skills: bool,
    },
    #[command(
        about = "Validate the active settings files for unknown fields",
        long_about = "Validate the active settings files for unknown fields.\n\
                      By default (without --strict), unknown fields are warnings.\n\
                      With --strict, any unknown field is treated as an error."
    )]
    Validate {
        #[arg(
            long,
            help = "Treat unknown config fields as errors instead of warnings"
        )]
        strict: bool,
    },
    #[command(
        about = "Show the winning tier and shadowed values for a config field",
        long_about = "Show which tier owns a config field and whether lower tiers are \
                      shadowed.\n\
                      Example: squeezy config explain model.provider"
    )]
    Explain {
        /// Dotted TOML path, e.g. `model.provider` or `tui.tick_rate_ms`.
        field: String,
    },
    #[command(
        about = "Emit the config schema as JSON for external tooling",
        long_about = "Print the CONFIG_SECTIONS schema as a JSON array. Each entry \
                      describes a section with its fields, TOML paths, editor kinds, \
                      apply tiers, env overrides, and default display values."
    )]
    Schema,
}

#[derive(Debug, Args, Default)]
pub(crate) struct ConfigBrowseArgs {
    #[arg(long, help = "Emit machine-readable JSON instead of the human listing")]
    pub(crate) json: bool,
}

#[derive(Debug, Args)]
#[group(required = true, multiple = false)]
pub(crate) struct InitScope {
    #[arg(long, help = "Write the user-level settings file")]
    user: bool,
    #[arg(long, help = "Write the project-level settings file")]
    project: bool,
    #[arg(
        long,
        help = "Write the per-machine repo-local settings file (~/.squeezy/projects/<hash>/settings.toml)"
    )]
    local: bool,
}

pub(crate) fn handle_config_command(
    command: Option<&ConfigCommand>,
    cli: &Cli,
) -> squeezy_core::Result<()> {
    match command {
        None => {
            let config = config_from_cli(cli)?;
            handle_browse_command(&config, &ConfigBrowseArgs::default())
        }
        Some(ConfigCommand::Browse(args)) => {
            let config = config_from_cli(cli)?;
            handle_browse_command(&config, args)
        }
        Some(ConfigCommand::Inspect) => handle_inspect(cli),
        Some(ConfigCommand::Init {
            scope,
            force,
            with_bundled_skills,
        }) => handle_init(cli, scope, *force, *with_bundled_skills),
        Some(ConfigCommand::Validate { strict }) => handle_validate(cli, *strict),
        Some(ConfigCommand::Explain { field }) => handle_explain(cli, field),
        Some(ConfigCommand::Schema) => handle_schema(),
    }
}

fn handle_inspect(cli: &Cli) -> squeezy_core::Result<()> {
    let config = config_from_cli(cli)?;
    if let Ok(sources) = load_separated_settings_sources() {
        let tier_paths: [(&str, Option<&std::path::Path>); 3] = [
            ("user", sources.user.as_ref().map(|t| t.path.as_path())),
            ("repo", sources.project.as_ref().map(|t| t.path.as_path())),
            ("local", sources.repo.as_ref().map(|t| t.path.as_path())),
        ];
        for (label, path) in tier_paths {
            if let Some(p) = path {
                for escaped_line in scan_file_for_shell_escapes(p) {
                    eprintln!(
                        "warning: shell-escape value in {label} config ({}): {escaped_line}",
                        p.display()
                    );
                }
            }
        }
    }
    print!("{}", config.inspect_redacted());
    Ok(())
}

fn handle_init(
    cli: &Cli,
    scope: &InitScope,
    force: bool,
    with_bundled_skills: bool,
) -> squeezy_core::Result<()> {
    let init_scope = if scope.user {
        ConfigInitScope::User
    } else if scope.local {
        ConfigInitScope::Local
    } else {
        ConfigInitScope::Project
    };
    let target = ConfigLocator::for_current_dir().init_target(init_scope);
    let path = target.path;
    let template = target.template;
    if with_bundled_skills && !scope.user {
        return Err(SqueezyError::Config(
            "--with-bundled-skills is only supported under --user".to_string(),
        ));
    }
    if path.exists() && !force {
        return Err(SqueezyError::Config(format!(
            "{} already exists; pass --force to overwrite",
            path.display()
        )));
    }
    write_settings_atomic(&path, template.as_bytes())?;
    println!("wrote {}", path.display());
    #[cfg(target_os = "windows")]
    if scope.user {
        let powershell_literal = path.display().to_string().replace('\'', "''");
        println!("  Open (PowerShell): Invoke-Item '{}'", powershell_literal);
    }
    if with_bundled_skills {
        let config = config_from_cli(cli)?;
        let target = &config.skills.user_dir;
        let written = squeezy_skills::install_bundled_skills(target).map_err(|err| {
            SqueezyError::Config(format!(
                "failed to install bundled skills under {}: {err}",
                target.display()
            ))
        })?;
        if written.is_empty() {
            println!("bundled skills already present under {}", target.display());
        } else {
            println!(
                "installed {} bundled skill(s) under {}: {}",
                written.len(),
                target.display(),
                written.join(", ")
            );
        }
    }
    Ok(())
}

fn handle_validate(cli: &Cli, strict: bool) -> squeezy_core::Result<()> {
    let config = config_from_cli(cli)?;
    let unknown_field_warnings: Vec<&squeezy_core::ConfigWarning> = config
        .config_warnings
        .iter()
        .filter(|w| !w.field.contains(' '))
        .collect();
    if unknown_field_warnings.is_empty() {
        println!("config OK — no unknown fields found");
        return Ok(());
    }
    for w in &unknown_field_warnings {
        let prefix = if strict { "error" } else { "warning" };
        eprintln!(
            "{prefix}: unknown config field `{}` in {}",
            w.field, w.source
        );
    }
    if strict {
        return Err(SqueezyError::Config(format!(
            "{} unknown config field(s) found (--strict mode)",
            unknown_field_warnings.len()
        )));
    }
    Ok(())
}

fn handle_explain(cli: &Cli, field_path: &str) -> squeezy_core::Result<()> {
    let parts_owned = split_config_field_path(field_path).map_err(|reason| {
        SqueezyError::Config(format!(
            "could not parse config field {field_path:?}: {reason}. \
             Quote keys that contain `.`, e.g. \
             `model_limits.\"openai:gpt-5.5\".context_window`."
        ))
    })?;
    let parts: Vec<&str> = parts_owned.iter().map(String::as_str).collect();
    let Some(field_meta) = find_config_field_for_path(&parts) else {
        return Err(SqueezyError::Config(format!(
            "unknown config field {field_path:?}; \
             use `squeezy config schema` to list all fields. \
             If a key contains `.` (e.g. a model id), quote it: \
             `model_limits.\"openai:gpt-5.5\".context_window`."
        )));
    };
    let config = config_from_cli(cli)?;
    let effective_value = explain_effective_value(&config, field_meta, &parts);
    let sources = load_separated_settings_sources()
        .map_err(|e| SqueezyError::Config(format!("failed to load settings tiers: {e}")))?;
    let winning_source = resolve_explain_field_source(&sources, field_meta, &parts);
    let source_path = explain_source_path(winning_source, field_meta.env_override, &sources);
    println!("field:   {field_path}");
    println!("value:   {effective_value}");
    println!("source:  {} ({})", winning_source.badge(), source_path);
    if let Some(env_var) = field_meta.env_override {
        if winning_source != FieldSource::Env {
            println!("env:     ${env_var} (not set — would override if set)");
        } else {
            println!("env:     ${env_var} (active)");
        }
    }
    println!("apply:   {}", field_meta.tier.label());
    print_shadowed_tiers(winning_source, &sources, &parts);
    Ok(())
}

fn explain_source_path(
    winning_source: FieldSource,
    env_override: Option<&'static str>,
    sources: &squeezy_core::SeparatedSources,
) -> String {
    match winning_source {
        FieldSource::Env => env_override
            .map(|v| format!("${v}"))
            .unwrap_or_else(|| "env".to_string()),
        FieldSource::Repo => sources
            .repo
            .as_ref()
            .map(|t| t.path.display().to_string())
            .unwrap_or_else(|| "local tier".to_string()),
        FieldSource::Project => sources
            .project
            .as_ref()
            .map(|t| t.path.display().to_string())
            .unwrap_or_else(|| "project tier".to_string()),
        FieldSource::User => sources
            .user
            .as_ref()
            .map(|t| t.path.display().to_string())
            .unwrap_or_else(|| "user tier".to_string()),
        FieldSource::Default => "(built-in default)".to_string(),
    }
}

fn print_shadowed_tiers(
    winning_source: FieldSource,
    sources: &squeezy_core::SeparatedSources,
    parts: &[&str],
) {
    let tier_entries: [(FieldSource, &str, Option<&squeezy_core::TierSource>); 3] = [
        (FieldSource::Repo, "local", sources.repo.as_ref()),
        (FieldSource::Project, "repo", sources.project.as_ref()),
        (FieldSource::User, "user", sources.user.as_ref()),
    ];
    let mut printed_header = false;
    for (src, label, tier) in tier_entries {
        if src == winning_source {
            continue;
        }
        if tier.is_some_and(|t| t.contains_path(parts)) {
            if !printed_header {
                println!("shadowed: (highest precedence first)");
                printed_header = true;
            }
            let path_str = tier
                .map(|t| t.path.display().to_string())
                .unwrap_or_default();
            println!("  {label}: {path_str}");
        }
    }
}

fn handle_schema() -> squeezy_core::Result<()> {
    use squeezy_core::config_schema::CONFIG_SECTIONS;
    let sections: Vec<serde_json::Value> = CONFIG_SECTIONS
        .iter()
        .map(|section| {
            let fields: Vec<serde_json::Value> = section
                .fields
                .iter()
                .map(|f| {
                    let kind_json = field_kind_to_json(&f.kind);
                    serde_json::json!({
                        "label": f.label,
                        "toml_path": f.toml_path,
                        "kind": kind_json,
                        "apply_tier": f.tier.label(),
                        "default_display": f.default_display,
                        "help": f.help,
                        "env_override": f.env_override,
                        "secret": f.secret,
                    })
                })
                .collect();
            serde_json::json!({
                "id": section.id.slug(),
                "label": section.label,
                "description": section.description,
                "fields": fields,
            })
        })
        .collect();
    let json = serde_json::to_string_pretty(&sections)
        .map_err(|e| SqueezyError::Config(format!("schema serialization failed: {e}")))?;
    println!("{json}");
    Ok(())
}

fn scan_file_for_shell_escapes(path: &std::path::Path) -> Vec<String> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter(|line| {
            let t = line.trim();
            !t.starts_with('#') && t.contains('=')
        })
        .filter_map(|line| {
            let eq_pos = line.find('=')?;
            let val = line[eq_pos + 1..].trim();
            let inner = val
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .or_else(|| val.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
                .unwrap_or(val);
            if inner.starts_with('!') {
                Some(line.trim().to_string())
            } else {
                None
            }
        })
        .collect()
}

fn field_kind_to_json(kind: &squeezy_core::config_schema::FieldKind) -> serde_json::Value {
    use squeezy_core::config_schema::FieldKind;
    match kind {
        FieldKind::Bool => serde_json::json!({"type": "bool"}),
        FieldKind::Integer { min, max, suffix } => serde_json::json!({
            "type": "integer", "min": min, "max": max, "suffix": suffix
        }),
        FieldKind::OptionalInteger { min, max, suffix } => serde_json::json!({
            "type": "optional_integer", "min": min, "max": max, "suffix": suffix
        }),
        FieldKind::OptionalFloat { min, max } => serde_json::json!({
            "type": "optional_float", "min": min, "max": max
        }),
        FieldKind::Enum { options } => serde_json::json!({
            "type": "enum", "options": options
        }),
        FieldKind::OptionalEnum { options } => serde_json::json!({
            "type": "optional_enum", "options": options
        }),
        FieldKind::String { multiline } => serde_json::json!({
            "type": "string", "multiline": multiline
        }),
        FieldKind::DurationMs => serde_json::json!({"type": "duration_ms"}),
        FieldKind::StringList { min, max } => serde_json::json!({
            "type": "string_list", "min": min, "max": max
        }),
        FieldKind::Path {
            must_exist,
            dir_only,
        } => serde_json::json!({
            "type": "path", "must_exist": must_exist, "dir_only": dir_only
        }),
        FieldKind::Secret { env_var } => serde_json::json!({
            "type": "secret", "env_var": env_var
        }),
        FieldKind::Info => serde_json::json!({"type": "info"}),
    }
}
