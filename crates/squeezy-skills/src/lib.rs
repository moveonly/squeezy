pub mod help;
pub mod implicit;
pub mod prompt_templates;
pub mod render;

mod catalog;
mod frontmatter;
mod hooks;
mod installer;
mod manifest;
mod validation;

pub(crate) use catalog::SkillEntry;
pub use catalog::{
    LoadedSkill, SkillActivation, SkillActivationKind, SkillActivationWarning, SkillCatalog,
    SkillContextBreakdown, SkillDiscoverySummary, SkillSource, SkillSummary, escape_body_breakouts,
    xml_escape,
};
pub use frontmatter::SkillContextMode;
pub use help::{
    APPROVAL_POLICY_DOC_PATH, BundledDoc, DocSection, HelpAnswer, HelpAnswerSource, HelpCitation,
    HelpStatus, SQUEEZY_REPO_SLUG, SQUEEZY_REPO_URL, SQUEEZY_WEBSITE_URL, SqueezyHelp, bundled_doc,
    bundled_doc_paths, bundled_docs, chunk_doc_sections, matches_squeezy_help_input,
    relevant_doc_sections_for_input, relevant_docs_for_input, slash_command_help_names,
};
pub use hooks::{
    DEFAULT_HOOK_TIMEOUT_SECS, HookFailurePolicy, SkillHookHandler, SkillHookMatcher,
    SkillHookSpec, register_skill_hooks,
};
pub use installer::{bundled_skills, install_bundled_skills};
pub use manifest::SkillManifest;
pub use prompt_templates::{
    PROJECT_PROMPTS_DIR, PromptTemplate, PromptTemplateCatalog, PromptTemplateSource,
    USER_PROMPTS_SUBPATH, parse_command_args as parse_prompt_template_args,
    substitute_args as substitute_prompt_template_args,
};
pub use render::SkillPreambleRender;
pub use validation::{
    HookDoctorIssue, SkillValidationResult, catalog_hook_issues, lint_skill_extended,
    parse_skill_triggers, skill_scan_dirs, unmet_tool_deps, validate_skill_dirs, validate_skill_md,
};

pub(crate) const SKILL_FILE: &str = "SKILL.md";
pub(crate) const SKILL_MANIFEST_FILE: &str = "skill.toml";
pub(crate) const PROJECT_SKILLS_DIR: &str = ".squeezy/skills";
pub(crate) const COMPAT_PROJECT_SKILLS_DIR: &str = ".agents/skills";
