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

    let mut blocks = Vec::with_capacity(skills.len());
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
            blocks.push(render_stub(skill, "body_cap", STUB_DESCRIPTION_MAX_CHARS));
        } else {
            blocks.push(skill.prompt_block());
        }
    }

    if let Some(block) = wrap_blocks(&blocks)
        && char_count(&block) <= budget_chars
    {
        return Some(block);
    }

    // Aggregate overflow: switch every skill to its minimum-stub form (zero
    // description chars) and verify the floor fits. If even the floor blows
    // the budget we drop the lowest-priority skills until it fits or no skill
    // remains. The roster of survivors is preserved before any per-skill
    // description detail.
    let mut survivors: Vec<&LoadedSkill> = skills.iter().collect();
    let mut min_blocks: Vec<String> = survivors
        .iter()
        .map(|skill| render_stub(skill, "aggregate_budget", 0))
        .collect();
    while let Some(min_block) = wrap_blocks(&min_blocks)
        && char_count(&min_block) > budget_chars
    {
        let Some(dropped) = survivors.pop() else {
            break;
        };
        min_blocks.pop();
        warn!(
            target: "squeezy_skills",
            skill = %dropped.summary.name,
            budget_chars,
            "skill omitted from active skill bundle because the budget is exhausted"
        );
    }
    if survivors.is_empty() {
        return None;
    }

    // Char-by-char description redistribution across the surviving skills.
    // Each skill's description grows one char at a time so short descriptions
    // never strand budget that a longer description could use.
    let max_chars: Vec<usize> = survivors
        .iter()
        .map(|skill| char_count(&skill.summary.description).min(STUB_DESCRIPTION_MAX_CHARS))
        .collect();
    let mut allocations = vec![0usize; survivors.len()];
    let base_cost = char_count(&wrap_blocks(&min_blocks).unwrap_or_default());
    let mut remaining = budget_chars.saturating_sub(base_cost);
    loop {
        let mut changed = false;
        for index in 0..survivors.len() {
            if allocations[index] >= max_chars[index] {
                continue;
            }
            // Each additional description character usually costs one in the
            // rendered output; XML-special chars expand to entities, so
            // `stub_description_delta` measures the real cost between two
            // allocation sizes.
            let candidate = allocations[index] + 1;
            let delta = stub_description_delta(
                &survivors[index].summary.description,
                allocations[index],
                candidate,
            );
            if delta <= remaining {
                allocations[index] = candidate;
                remaining = remaining.saturating_sub(delta);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    let mut rendered = Vec::with_capacity(survivors.len());
    for (index, skill) in survivors.iter().enumerate() {
        let body_chars = char_count(&skill.body);
        warn!(
            target: "squeezy_skills",
            skill = %skill.summary.name,
            body_chars,
            cap_chars = budget_chars,
            chars_truncated = body_chars,
            description_chars = allocations[index],
            "skill_truncated"
        );
        rendered.push(render_stub(skill, "aggregate_budget", allocations[index]));
    }

    wrap_blocks(&rendered).filter(|out| char_count(out) <= budget_chars)
}

/// Render the active-skill block in metadata-only mode.
///
/// Each skill emits a `<skill body="omitted">` block containing its
/// name, source, description, optional `when_to_use`, location, base
/// directory, optional manifest, and an `<instruction>` telling the
/// model to call `load_skill` when the full body is needed. No skill
/// body is included.
///
/// When the aggregate exceeds `budget_chars`, lowest-priority skills
/// are dropped (and warned) until the remaining set fits. Returns
/// `None` if the input is empty, the budget is zero, or no metadata
/// block fits within the budget.
pub fn render_active_skills_metadata(
    skills: &[LoadedSkill],
    budget_chars: usize,
) -> Option<String> {
    if skills.is_empty() || budget_chars == 0 {
        return None;
    }

    let mut survivors: Vec<&LoadedSkill> = skills.iter().collect();
    loop {
        if survivors.is_empty() {
            return None;
        }
        let blocks: Vec<String> = survivors
            .iter()
            .map(|skill| skill.metadata_block())
            .collect();
        let rendered = wrap_blocks(&blocks)?;
        if char_count(&rendered) <= budget_chars {
            return Some(rendered);
        }
        // Drop the lowest-priority (last) skill and retry. This mirrors
        // the inline-mode drop fallback so the metadata bundle remains
        // bounded under a tight budget. The `?` is defensive — the
        // earlier `is_empty` check already guarantees `Some`.
        let dropped = survivors.pop()?;
        warn!(
            target: "squeezy_skills",
            skill = %dropped.summary.name,
            budget_chars,
            "skill omitted from active skill metadata bundle because the budget is exhausted"
        );
    }
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
    let mut lines = Vec::with_capacity(sorted.len() + 1);
    lines.push(header.to_string());
    let mut omitted = 0usize;
    for summary in &sorted {
        let line = format!(
            "- {}: {} (source: {}, load_skill name: {})",
            summary.name,
            compact_text(&summary.description, PREAMBLE_DESCRIPTION_MAX_CHARS),
            summary.source.as_str(),
            summary.name
        );
        lines.push(line);
        if char_count(&wrap_preamble(&lines)) > budget_chars {
            lines.pop();
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
    let rendered = render_active_skills(skills, budget_chars, body_cap_chars);
    let mut metrics = metrics_for_active_skills_render(skills, rendered.as_deref());
    if skills.is_empty() || budget_chars == 0 {
        metrics.dropped = 0;
    }
    (rendered, metrics)
}

fn metrics_for_active_skills_render(
    skills: &[LoadedSkill],
    rendered: Option<&str>,
) -> SkillActivationMetrics {
    let mut metrics = SkillActivationMetrics {
        total: skills.len(),
        ..SkillActivationMetrics::default()
    };
    let Some(rendered) = rendered else {
        metrics.dropped = skills.len();
        return metrics;
    };

    for skill in skills {
        let name = xml_escape(&skill.summary.name);
        let needle = format!("<skill name=\"{name}\"");
        if let Some(start) = rendered.find(&needle) {
            metrics.included += 1;
            let rest = &rendered[start..];
            if let Some(tag_end) = rest.find('>')
                && rest[..tag_end].contains("truncated=\"true\"")
            {
                metrics.body_truncated += 1;
            }
        }
    }
    metrics.dropped = metrics.total.saturating_sub(metrics.included);
    metrics
}

fn wrap_blocks(blocks: &[String]) -> Option<String> {
    (!blocks.is_empty())
        .then(|| format!("<active_skills>\n{}\n</active_skills>", blocks.join("\n")))
}

/// Render fork-mode skills (skills whose frontmatter declares
/// `context: fork`) as a separate `<fork_skills>` system block.
///
/// Fork-mode skills are kept out of `<active_skills>` so the model can
/// tell them apart from inline skills. Their bodies ARE still present in
/// the parent system prompt inside the `<fork_skills>` wrapper; the
/// block carries an `<instruction>` telling the model to treat the body
/// as input for a `delegate` subagent call rather than executing it
/// directly in the parent turn. Returns `None` when there are no
/// fork-mode skills or `budget_chars == 0`.
pub fn render_fork_skills(
    skills: &[LoadedSkill],
    budget_chars: usize,
    body_cap_chars: usize,
) -> Option<String> {
    if skills.is_empty() || budget_chars == 0 {
        return None;
    }
    let mut blocks = Vec::with_capacity(skills.len());
    for skill in skills {
        let body_chars = char_count(&skill.body);
        let body_segment = if body_chars > body_cap_chars {
            warn!(
                target: "squeezy_skills",
                skill = %skill.summary.name,
                body_chars,
                cap_chars = body_cap_chars,
                "fork_skill_body_truncated"
            );
            format!(
                "<content_truncated reason=\"body_cap\">Body omitted because it exceeds {body_cap_chars} chars; call `load_skill` for the full text.</content_truncated>"
            )
        } else {
            format!(
                "<content>\n{}\n</content>",
                escape_body_breakouts(skill.body.trim())
            )
        };
        let when_to_use = skill
            .summary
            .when_to_use
            .as_ref()
            .map(|value| format!("\n<when_to_use>{}</when_to_use>", xml_escape(value)))
            .unwrap_or_default();
        let instruction = "This skill declared context=\"fork\". Treat the body as instructions for a focused subagent (e.g. invoke `delegate` with the relevant slice of the user's task using this body as the subagent system prompt) instead of acting on it inline. Do not execute the body as part of the parent turn.";
        blocks.push(format!(
            "<skill name=\"{}\" source=\"{}\" context_mode=\"fork\">\n<description>{}</description>{when_to_use}\n<location>{}</location>\n<base_directory>{}</base_directory>\n<instruction>{}</instruction>\n{body_segment}\n</skill>",
            xml_escape(&skill.summary.name),
            skill.summary.source.as_str(),
            xml_escape(&skill.summary.description),
            skill.summary.location.display(),
            skill.base_dir.display(),
            xml_escape(instruction),
        ));
    }
    let rendered = format!("<fork_skills>\n{}\n</fork_skills>", blocks.join("\n"));
    if char_count(&rendered) > budget_chars {
        warn!(
            target: "squeezy_skills",
            budget_chars,
            rendered_chars = char_count(&rendered),
            "fork_skills block exceeds budget; emitting anyway because fork-mode skills cannot be silently dropped"
        );
    }
    Some(rendered)
}

fn wrap_preamble(lines: &[String]) -> String {
    format!(
        "<available_skills>\n{}\n</available_skills>",
        lines.join("\n")
    )
}

fn render_stub(skill: &LoadedSkill, reason: &str, description_max_chars: usize) -> String {
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
        xml_escape(&take_chars(&summary.description, description_max_chars)),
        summary.location.display(),
        skill.base_dir.display(),
        escape_body_breakouts(&instruction)
    )
}

fn stub_description_delta(description: &str, from_chars: usize, to_chars: usize) -> usize {
    char_count(&xml_escape(&take_chars(description, to_chars))).saturating_sub(char_count(
        &xml_escape(&take_chars(description, from_chars)),
    ))
}

fn take_chars(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
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
