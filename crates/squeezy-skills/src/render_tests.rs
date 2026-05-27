use super::*;
use crate::{SkillContextMode, SkillSource, SkillSummary};
use std::collections::BTreeMap;
use std::path::PathBuf;

fn skill(name: &str, body: &str) -> LoadedSkill {
    LoadedSkill {
        summary: SkillSummary {
            name: name.to_string(),
            description: format!("desc for {name}"),
            when_to_use: None,
            source: SkillSource::Project,
            location: PathBuf::from(format!(".squeezy/skills/{name}/SKILL.md")),
            disabled: false,
            manifest: None,
            context_mode: SkillContextMode::Inline,
        },
        base_dir: PathBuf::from(format!(".squeezy/skills/{name}")),
        body: body.to_string(),
        hooks: BTreeMap::new(),
    }
}

#[test]
fn metrics_count_included_skills_when_all_fit() {
    let skills = vec![skill("alpha", "alpha body"), skill("beta", "beta body")];
    let (rendered, metrics) =
        render_active_skills_with_metrics(&skills, /* budget */ 4_000, /* cap */ 16_000);
    assert!(rendered.is_some(), "expected an <active_skills> block");
    assert_eq!(
        metrics,
        SkillActivationMetrics {
            total: 2,
            included: 2,
            dropped: 0,
            body_truncated: 0,
        }
    );
}

#[test]
fn metrics_count_body_cap_truncation() {
    let big = "x".repeat(2_000);
    let skills = vec![skill("alpha", &big)];
    let (rendered, metrics) =
        render_active_skills_with_metrics(&skills, /* budget */ 8_000, /* cap */ 100);
    let body = rendered.expect("stub should still fit the budget");
    assert!(
        body.contains("truncated=\"true\""),
        "expected body-cap stub in {body}"
    );
    assert!(body.contains("reason=\"body_cap\""));
    assert_eq!(
        metrics,
        SkillActivationMetrics {
            total: 1,
            included: 1,
            dropped: 0,
            body_truncated: 1,
        }
    );
}

#[test]
fn metrics_count_dropped_when_aggregate_overflows() {
    // A small budget plus three full bodies forces at least one to be
    // dropped even after stub fallback.
    let body = "y".repeat(600);
    let skills = vec![
        skill("alpha", &body),
        skill("beta", &body),
        skill("gamma", &body),
    ];
    let (rendered, metrics) =
        render_active_skills_with_metrics(&skills, /* budget */ 700, /* cap */ 16_000);
    assert!(rendered.is_some(), "expected a partial block");
    assert_eq!(metrics.total, 3);
    assert!(
        metrics.dropped >= 1,
        "expected at least one drop; got {metrics:?}",
    );
    assert_eq!(metrics.included + metrics.dropped, metrics.total);
}

#[test]
fn metrics_zero_when_inputs_empty() {
    let (rendered, metrics) =
        render_active_skills_with_metrics(&[], /* budget */ 4_000, /* cap */ 16_000);
    assert!(rendered.is_none());
    assert_eq!(metrics, SkillActivationMetrics::default());
}

#[test]
fn metrics_zero_when_budget_zero() {
    let skills = vec![skill("alpha", "alpha body")];
    let (rendered, metrics) =
        render_active_skills_with_metrics(&skills, /* budget */ 0, /* cap */ 16_000);
    assert!(rendered.is_none());
    assert_eq!(metrics.total, 1);
    assert_eq!(metrics.included, 0);
    assert_eq!(metrics.dropped, 0);
    assert_eq!(metrics.body_truncated, 0);
}

#[test]
fn render_active_skills_matches_metrics_variant_string() {
    // The non-metrics helper must keep returning the same rendered
    // string so existing callers stay byte-identical.
    let skills = vec![skill("alpha", "alpha body"), skill("beta", "beta body")];
    let (with_metrics, _) = render_active_skills_with_metrics(&skills, 4_000, 16_000);
    let without = render_active_skills(&skills, 4_000, 16_000);
    assert_eq!(with_metrics, without);
}
