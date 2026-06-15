use clap::ValueEnum;

use super::{DoctorArgs, parser_health_check, terminal_capability_check};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "lowercase")]
pub enum DoctorStatusFilter {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Status {
    Ok,
    Warn,
    Fail,
}

impl Status {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Warn => "warn",
            Status::Fail => "fail",
        }
    }

    /// Severity rank for the human-readable ordering: failures first, then
    /// warnings, then ok rows. The `--json` body keeps source order.
    pub(super) fn severity_rank(self) -> u8 {
        match self {
            Status::Fail => 0,
            Status::Warn => 1,
            Status::Ok => 2,
        }
    }

    fn matches_filter(self, filter: DoctorStatusFilter) -> bool {
        matches!(
            (self, filter),
            (Status::Ok, DoctorStatusFilter::Ok)
                | (Status::Warn, DoctorStatusFilter::Warn)
                | (Status::Fail, DoctorStatusFilter::Fail)
        )
    }
}

#[derive(Debug, Clone)]
pub(super) struct Check {
    pub(super) name: String,
    pub(super) status: Status,
    pub(super) detail: String,
    /// Optional structured metadata included in `--json` output. Used by the
    /// sandbox row to expose machine-readable platform fields (`backend`,
    /// `userns`, `landlock`, `required_mode_supported`) without scraping prose.
    pub(super) extra: Option<serde_json::Value>,
}

struct DoctorCheckSpec {
    name: &'static str,
    prefix_family: bool,
    requires_config: bool,
    run: Option<fn() -> Check>,
}

const DOCTOR_CHECKS: &[DoctorCheckSpec] = &[
    DoctorCheckSpec {
        name: "config",
        prefix_family: false,
        requires_config: true,
        run: None,
    },
    DoctorCheckSpec {
        name: "repo_profile",
        prefix_family: false,
        requires_config: true,
        run: None,
    },
    DoctorCheckSpec {
        name: "workspace_paths",
        prefix_family: false,
        requires_config: true,
        run: None,
    },
    DoctorCheckSpec {
        name: "provider",
        prefix_family: true,
        requires_config: true,
        run: None,
    },
    DoctorCheckSpec {
        name: "providers",
        prefix_family: false,
        requires_config: true,
        run: None,
    },
    DoctorCheckSpec {
        name: "probe",
        prefix_family: true,
        requires_config: true,
        run: None,
    },
    DoctorCheckSpec {
        name: "mcp",
        prefix_family: false,
        requires_config: true,
        run: None,
    },
    DoctorCheckSpec {
        name: "skills",
        prefix_family: false,
        requires_config: true,
        run: None,
    },
    DoctorCheckSpec {
        name: "hooks",
        prefix_family: true,
        requires_config: true,
        run: None,
    },
    DoctorCheckSpec {
        name: "skills_roots",
        prefix_family: false,
        requires_config: true,
        run: None,
    },
    DoctorCheckSpec {
        name: "session_store",
        prefix_family: false,
        requires_config: true,
        run: None,
    },
    DoctorCheckSpec {
        name: "session_home",
        prefix_family: false,
        requires_config: true,
        run: None,
    },
    DoctorCheckSpec {
        name: "session_xdg_state_home",
        prefix_family: false,
        requires_config: true,
        run: None,
    },
    DoctorCheckSpec {
        name: "session_legacy_index",
        prefix_family: false,
        requires_config: true,
        run: None,
    },
    DoctorCheckSpec {
        name: "session_paths",
        prefix_family: false,
        requires_config: true,
        run: None,
    },
    DoctorCheckSpec {
        name: "user_global_storage",
        prefix_family: false,
        requires_config: true,
        run: None,
    },
    DoctorCheckSpec {
        name: "state_store",
        prefix_family: false,
        requires_config: true,
        run: None,
    },
    DoctorCheckSpec {
        name: "graph_store",
        prefix_family: false,
        requires_config: true,
        run: None,
    },
    DoctorCheckSpec {
        name: "cache",
        prefix_family: false,
        requires_config: true,
        run: None,
    },
    DoctorCheckSpec {
        name: "settings_location",
        prefix_family: false,
        requires_config: true,
        run: None,
    },
    DoctorCheckSpec {
        name: "sandbox",
        prefix_family: false,
        requires_config: false,
        run: None,
    },
    DoctorCheckSpec {
        name: "update",
        prefix_family: false,
        requires_config: false,
        run: None,
    },
    DoctorCheckSpec {
        name: "terminal",
        prefix_family: false,
        requires_config: false,
        run: Some(terminal_capability_check),
    },
    DoctorCheckSpec {
        name: "parser_health",
        prefix_family: false,
        requires_config: false,
        run: Some(parser_health_check),
    },
];

pub(super) fn needs_config(args: &DoctorArgs) -> bool {
    if args.only.is_empty() {
        return true;
    }
    args.only
        .iter()
        .any(|selector| check_requires_config(selector.trim()))
}

fn check_requires_config(selector: &str) -> bool {
    if selector.is_empty() {
        return false;
    }
    DOCTOR_CHECKS
        .iter()
        .find(|check| {
            check.name == selector
                || (check.prefix_family && check_name_matches(check.name, selector))
        })
        .map(|check| check.requires_config)
        .unwrap_or(true)
}

pub(super) fn run_local_doctor_checks(args: &DoctorArgs, checks: &mut Vec<Check>) {
    for check in DOCTOR_CHECKS {
        if let Some(run) = check.run
            && should_include_check(args, check.name)
        {
            checks.push(run());
        }
    }
}

pub(super) fn check_counts(checks: &[Check]) -> (usize, usize) {
    checks
        .iter()
        .fold((0, 0), |(warnings, failures), check| match check.status {
            Status::Warn => (warnings + 1, failures),
            Status::Fail => (warnings, failures + 1),
            Status::Ok => (warnings, failures),
        })
}

pub(super) fn exit_code_for_checks(checks: &[Check]) -> i32 {
    let (_, failures) = check_counts(checks);
    if failures > 0 { 1 } else { 0 }
}

pub(super) fn should_include_check(args: &DoctorArgs, name: &str) -> bool {
    args.only.iter().all(|selector| selector.trim().is_empty())
        || args
            .only
            .iter()
            .any(|selector| check_name_matches(selector, name))
}

fn check_name_matches(selector: &str, name: &str) -> bool {
    let selector = selector.trim();
    if selector.is_empty() {
        return false;
    }
    name == selector
        || (name.len() > selector.len()
            && name.as_bytes().get(selector.len()) == Some(&b':')
            && name.starts_with(selector))
}

pub(super) fn filter_checks(args: &DoctorArgs, checks: Vec<Check>) -> Vec<Check> {
    if args.status.is_empty() {
        return checks;
    }
    checks
        .into_iter()
        .filter(|check| {
            args.status
                .iter()
                .any(|filter| check.status.matches_filter(*filter))
        })
        .collect()
}

pub(super) fn unmatched_selector_checks(
    args: &DoctorArgs,
    checks: &[Check],
    config_failed: bool,
) -> Vec<Check> {
    // Split the unmatched selectors into two buckets:
    //   * `unknown` -- typos / unrecognised selector names. These are
    //     hard failures and tell the user to fix the command line.
    //   * `skipped_due_to_config` -- selectors that *would* have been
    //     evaluated, but config failed to load so the underlying check
    //     never ran (e.g. `--only providers` against a corrupt
    //     `settings.toml`). These are not user errors; they are a
    //     downstream consequence of the `config` row already failing,
    //     so we surface them as a single `selector` warn note instead
    //     of silently dropping them.
    let mut unknown = Vec::new();
    let mut skipped_due_to_config = Vec::new();
    for selector in &args.only {
        let selector = selector.trim();
        if selector.is_empty()
            || checks
                .iter()
                .any(|check| check_name_matches(selector, &check.name))
            || (selector == "update" && (args.skip_update || args.no_update_check))
        {
            continue;
        }
        if config_failed && check_requires_config(selector) {
            skipped_due_to_config.push(selector.to_string());
        } else {
            unknown.push(selector.to_string());
        }
    }
    let mut out = Vec::new();
    if !unknown.is_empty() {
        out.push(Check {
            name: "selector".to_string(),
            status: Status::Fail,
            detail: format!("unknown doctor --only selector(s): {}", unknown.join(", ")),
            extra: None,
        });
    }
    if !skipped_due_to_config.is_empty() {
        // Distinct name so the "always re-include selector failures
        // after status filtering" loop in `run()` does not treat this
        // warn row and the `selector` fail row above as the same entry.
        out.push(Check {
            name: "selector:skipped".to_string(),
            status: Status::Warn,
            detail: format!(
                "unable to evaluate {} because the `config` row failed to load; \
                 fix the configuration to re-enable {}",
                skipped_due_to_config.join(", "),
                if skipped_due_to_config.len() == 1 {
                    "this check"
                } else {
                    "these checks"
                }
            ),
            extra: None,
        });
    }
    out
}
