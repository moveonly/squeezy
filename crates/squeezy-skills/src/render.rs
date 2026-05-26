use crate::{LoadedSkill, SkillSummary, escape_body_breakouts, xml_escape};
use tracing::warn;

const STUB_DESCRIPTION_MAX_CHARS: usize = 240;
const STUB_WHEN_TO_USE_MAX_CHARS: usize = 240;
const PREAMBLE_DESCRIPTION_MAX_CHARS: usize = 180;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillPreambleRender {
    pub body: String,
    pub omitted_count: usize,
}

/// Counters emitted alongside an `<active_skills>` render so the agent
/// can record them to telemetry without re-walking the inputs.
///
/// `total` counts how many `LoadedSkill` candidates entered the
/// render; `included` is how many appear in the final block (either as
/// full body or as a stub); `dropped` is how many were skipped because
/// the aggregate budget was exhausted; `body_truncated` is how many
/// were emitted as a `<skill truncated="true">` stub rather than full
/// body (either because the per-skill `body_cap_chars` fired or the
/// aggregate fit only after falling back to the stub).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SkillActivationMetrics {
    pub total: usize,
    pub included: usize,
    pub dropped: usize,
    pub body_truncated: usize,
}

pub fn render_active_skills(
    skills: &[LoadedSkill],
    budget_chars: usize,
    body_cap_chars: usize,
) -> Option<String> {
    if skills.is_empty() || budget_chars == 0 {
        return None;
    }

    let mut blocks = Vec::new();
    for skill in skills {
        let body_chars = char_count(&skill.body);
        if body_chars > body_cap_chars {
            warn!(
                target: "squeezy_skills",
                skill = %skill.summary.name,
                body_chars,
                cap_chars = body_cap_chars,
                chars_truncated = body_chars.saturating_sub(body_cap_chars),
                "skill_truncated"
            );
            blocks.push(render_stub(skill, "body_cap"));
        } else {
            blocks.push(skill.prompt_block());
        }
    }

    if let Some(block) = wrap_blocks(&blocks)
        && char_count(&block) <= budget_chars
    {
        return Some(block);
    }

    let mut fitted = Vec::new();
    for (skill, block) in skills.iter().zip(blocks) {
        let candidates = if block_contains_stub(&block) {
            vec![block]
        } else {
            vec![block, render_stub(skill, "aggregate_budget")]
        };
        let mut inserted = false;
        for candidate in candidates {
            let mut attempt = fitted.clone();
            attempt.push(candidate.clone());
            if let Some(rendered) = wrap_blocks(&attempt)
                && char_count(&rendered) <= budget_chars
            {
                if block_contains_stub(&candidate) {
                    let body_chars = char_count(&skill.body);
                    warn!(
                        target: "squeezy_skills",
                        skill = %skill.summary.name,
                        body_chars,
                        cap_chars = budget_chars,
                        chars_truncated = body_chars,
                        "skill_truncated"
                    );
                }
                fitted = attempt;
                inserted = true;
                break;
            }
        }
        if !inserted {
            warn!(
                target: "squeezy_skills",
                skill = %skill.summary.name,
                budget_chars,
                "skill omitted from active skill bundle because the budget is exhausted"
            );
        }
    }

    wrap_blocks(&fitted).filter(|rendered| char_count(rendered) <= budget_chars)
}

pub fn render_skill_preamble(
    summaries: &[SkillSummary],
    budget_chars: usize,
) -> Option<SkillPreambleRender> {
    if summaries.is_empty() || budget_chars == 0 {
        return None;
    }
    let mut sorted = summaries.to_vec();
    sorted.sort_by(|left, right| {
        left.source
            .precedence()
            .cmp(&right.source.precedence())
            .then_with(|| left.name.cmp(&right.name))
    });

    let header = "Available Squeezy skills. Use `load_skill` when a task benefits from one of these local instruction sets.";
    let mut lines = vec![header.to_string()];
    let mut omitted = 0usize;
    for summary in &sorted {
        let line = format!(
            "- {}: {} (source: {}, load_skill name: {})",
            summary.name,
            compact_text(&summary.description, PREAMBLE_DESCRIPTION_MAX_CHARS),
            summary.source.as_str(),
            summary.name
        );
        let mut attempt = lines.clone();
        attempt.push(line);
        let body = wrap_preamble(&attempt);
        if char_count(&body) <= budget_chars {
            lines = attempt;
        } else {
            omitted += 1;
        }
    }

    if lines.len() == 1 {
        return None;
    }
    let body = wrap_preamble(&lines);
    if omitted > 0 {
        warn!(
            target: "squeezy_skills",
            omitted_count = omitted,
            budget_chars,
            "available skills preamble truncated"
        );
    }
    Some(SkillPreambleRender {
        body,
        omitted_count: omitted,
    })
}

/// Render the active-skill block and report counters describing how
/// many skills were included, dropped, or body-truncated.
///
/// This is the metrics-aware companion to `render_active_skills`. The
/// rendered string matches `render_active_skills` exactly for the same
/// inputs; only the second tuple element is new. Callers that just
/// need the string can keep using `render_active_skills`; callers that
/// also want to feed a telemetry handle (e.g. the agent calling
/// `record_skill_activation`) should use this variant.
pub fn render_active_skills_with_metrics(
    skills: &[LoadedSkill],
    budget_chars: usize,
    body_cap_chars: usize,
) -> (Option<String>, SkillActivationMetrics) {
    let mut metrics = SkillActivationMetrics {
        total: skills.len(),
        ..SkillActivationMetrics::default()
    };

    if skills.is_empty() || budget_chars == 0 {
        return (None, metrics);
    }

    let mut blocks = Vec::with_capacity(skills.len());
    let mut body_cap_truncated = vec![false; skills.len()];
    for (index, skill) in skills.iter().enumerate() {
        let body_chars = char_count(&skill.body);
        if body_chars > body_cap_chars {
            body_cap_truncated[index] = true;
            blocks.push(render_stub(skill, "body_cap"));
        } else {
            blocks.push(skill.prompt_block());
        }
    }

    if let Some(block) = wrap_blocks(&blocks)
        && char_count(&block) <= budget_chars
    {
        metrics.included = skills.len();
        metrics.body_truncated = body_cap_truncated.iter().filter(|hit| **hit).count();
        return (Some(block), metrics);
    }

    let mut fitted = Vec::new();
    let mut included_is_stub = Vec::<bool>::new();
    for (skill, block) in skills.iter().zip(blocks) {
        let starts_as_stub = block_contains_stub(&block);
        let candidates = if starts_as_stub {
            vec![(block, true)]
        } else {
            vec![
                (block, false),
                (render_stub(skill, "aggregate_budget"), true),
            ]
        };
        let mut inserted = false;
        for (candidate, candidate_is_stub) in candidates {
            let mut attempt = fitted.clone();
            attempt.push(candidate.clone());
            if let Some(rendered) = wrap_blocks(&attempt)
                && char_count(&rendered) <= budget_chars
            {
                fitted = attempt;
                included_is_stub.push(candidate_is_stub);
                inserted = true;
                break;
            }
        }
        if !inserted {
            metrics.dropped += 1;
        }
    }

    let rendered = wrap_blocks(&fitted).filter(|rendered| char_count(rendered) <= budget_chars);
    if rendered.is_some() {
        metrics.included = included_is_stub.len();
        metrics.body_truncated = included_is_stub.iter().filter(|stub| **stub).count();
    } else {
        // Final wrap failed the budget — treat every block as dropped.
        metrics.dropped = skills.len();
        metrics.included = 0;
        metrics.body_truncated = 0;
    }
    (rendered, metrics)
}

fn wrap_blocks(blocks: &[String]) -> Option<String> {
    (!blocks.is_empty())
        .then(|| format!("<active_skills>\n{}\n</active_skills>", blocks.join("\n")))
}

fn wrap_preamble(lines: &[String]) -> String {
    format!(
        "<available_skills>\n{}\n</available_skills>",
        lines.join("\n")
    )
}

fn render_stub(skill: &LoadedSkill, reason: &str) -> String {
    let summary = &skill.summary;
    let when_to_use = summary
        .when_to_use
        .as_ref()
        .map(|value| {
            format!(
                "\n<when_to_use>{}</when_to_use>",
                xml_escape(&compact_text(value, STUB_WHEN_TO_USE_MAX_CHARS))
            )
        })
        .unwrap_or_default();
    let instruction = format!(
        "Skill body omitted to fit the skills context budget. Call load_skill with name \"{}\" if the full instructions are required.",
        summary.name
    );
    format!(
        "<skill name=\"{}\" source=\"{}\" truncated=\"true\" reason=\"{}\">\n<description>{}</description>{when_to_use}\n<location>{}</location>\n<base_directory>{}</base_directory>\n<content>\n{}\n</content>\n</skill>",
        xml_escape(&summary.name),
        summary.source.as_str(),
        xml_escape(reason),
        xml_escape(&compact_text(
            &summary.description,
            STUB_DESCRIPTION_MAX_CHARS
        )),
        summary.location.display(),
        skill.base_dir.display(),
        escape_body_breakouts(&instruction)
    )
}

fn block_contains_stub(block: &str) -> bool {
    block.contains("truncated=\"true\"")
}

fn compact_text(value: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (index, ch) in value.chars().enumerate() {
        if index >= max_chars {
            out.push_str("...");
            return out;
        }
        out.push(ch);
    }
    out
}

fn char_count(value: &str) -> usize {
    value.chars().count()
}

#[cfg(test)]
#[path = "render_tests.rs"]
mod tests;
