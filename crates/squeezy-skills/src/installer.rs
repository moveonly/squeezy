use std::{
    fs,
    path::{Path, PathBuf},
};

use squeezy_core::Result;

use crate::frontmatter::{is_valid_skill_name, parse_skill_file};
use crate::{LoadedSkill, SKILL_FILE, SkillSource, SkillSummary};

/// Source identifier used for the in-binary skills returned by
/// [`bundled_skills`]; the on-disk catalog uses real filesystem roots, so the
/// `location` and `base_dir` on these summaries reference a sentinel path that
/// will never collide with a real skill on disk.
const BUNDLED_VIRTUAL_ROOT: &str = "<squeezy-builtin>";

struct BundledSkillSource {
    dir_name: &'static str,
    content: &'static str,
}

const BUNDLED_SKILL_SOURCES: &[BundledSkillSource] = &[
    BundledSkillSource {
        dir_name: "customize-squeezy",
        content: include_str!("../builtin/customize-squeezy/SKILL.md"),
    },
    BundledSkillSource {
        dir_name: "release-notes",
        content: include_str!("../builtin/release-notes/SKILL.md"),
    },
    BundledSkillSource {
        dir_name: "skill-creator",
        content: include_str!("../builtin/skill-creator/SKILL.md"),
    },
    BundledSkillSource {
        dir_name: "trace-symbol",
        content: include_str!("../builtin/trace-symbol/SKILL.md"),
    },
];

/// Return the in-binary sample skills that ship with Squeezy.
///
/// These are not registered into a [`SkillCatalog`] automatically; callers
/// that want to surface them as first-run examples can write them under a
/// user-controlled skills root (typically `~/.squeezy/skills/`) before
/// constructing the catalog, or render them directly without disk install.
/// The on-disk discovery flow remains the authoritative path for normal use.
pub fn bundled_skills() -> Vec<LoadedSkill> {
    BUNDLED_SKILL_SOURCES
        .iter()
        .map(|source| {
            let (metadata, body) = parse_skill_file(source.content).unwrap_or_else(|err| {
                panic!("bundled skill {} is malformed: {err}", source.dir_name)
            });
            assert!(
                is_valid_skill_name(&metadata.name),
                "bundled skill {} has invalid name {}",
                source.dir_name,
                metadata.name
            );
            assert_eq!(
                metadata.name, source.dir_name,
                "bundled skill {} has mismatched frontmatter name {}",
                source.dir_name, metadata.name
            );
            let virtual_root = PathBuf::from(BUNDLED_VIRTUAL_ROOT);
            let base_dir = virtual_root.join(source.dir_name);
            let location = base_dir.join(SKILL_FILE);
            LoadedSkill {
                summary: SkillSummary {
                    name: metadata.name,
                    description: metadata.description,
                    when_to_use: metadata.when_to_use,
                    source: SkillSource::User,
                    location,
                    disabled: false,
                    manifest: None,
                    context_mode: metadata.context_mode,
                },
                base_dir,
                body,
                hooks: metadata.hooks,
            }
        })
        .collect()
}

/// Materialise the in-binary [`bundled_skills`] under `user_dir` so a
/// fresh install actually has discoverable sample skills.
///
/// Each bundled skill is written to `<user_dir>/<name>/SKILL.md`. If
/// the target directory or file already exists the entry is skipped
/// — repeat calls and partial installs stay idempotent and never
/// clobber an edited user copy.
///
/// Returns the list of skill names that were written this call.
pub fn install_bundled_skills(user_dir: &Path) -> Result<Vec<String>> {
    fs::create_dir_all(user_dir)?;
    let mut written = Vec::new();
    for source in BUNDLED_SKILL_SOURCES {
        let target_dir = user_dir.join(source.dir_name);
        let target_file = target_dir.join(SKILL_FILE);
        if target_file.exists() {
            continue;
        }
        fs::create_dir_all(&target_dir)?;
        fs::write(&target_file, source.content)?;
        written.push(source.dir_name.to_string());
    }
    Ok(written)
}

#[cfg(test)]
#[path = "installer_tests.rs"]
mod tests;
