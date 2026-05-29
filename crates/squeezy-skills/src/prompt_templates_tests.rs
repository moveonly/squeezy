use std::{
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use super::*;

fn temp_dir(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("squeezy_prompts_{label}_{nonce}"));
    fs::create_dir_all(&path).expect("create temp dir");
    path
}

fn write_template(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(format!("{name}.md"));
    fs::write(&path, body).expect("write template");
    path
}

#[test]
fn parse_template_extracts_description_and_args() {
    let parsed = parse_template(concat!(
        "---\n",
        "description: \"Summarize a file\"\n",
        "argument-hint: <path>\n",
        "args: [path]\n",
        "---\n",
        "Summarize {path}.\n",
    ))
    .expect("parse");
    assert_eq!(parsed.description, "Summarize a file");
    assert_eq!(parsed.argument_hint.as_deref(), Some("<path>"));
    assert_eq!(parsed.args, vec!["path".to_string()]);
    assert_eq!(parsed.body, "Summarize {path}.");
}

#[test]
fn parse_template_supports_block_args_list() {
    let parsed = parse_template(concat!(
        "---\n",
        "description: foo\n",
        "args:\n",
        "  - first\n",
        "  - second\n",
        "---\n",
        "Body uses {first} and {second}.\n",
    ))
    .expect("parse");
    assert_eq!(parsed.args, vec!["first".to_string(), "second".to_string()]);
}

#[test]
fn parse_template_without_frontmatter_uses_first_line_as_description() {
    let parsed = parse_template("Just a plain prompt body.\nMore stuff here.\n").expect("parse");
    assert_eq!(parsed.description, "Just a plain prompt body.");
    assert!(parsed.argument_hint.is_none());
    assert!(parsed.args.is_empty());
    assert_eq!(parsed.body, "Just a plain prompt body.\nMore stuff here.");
}

#[test]
fn parse_template_truncates_long_inferred_description() {
    let long = "a".repeat(80);
    let parsed = parse_template(&long).expect("parse");
    assert_eq!(parsed.description.len(), 60 + "...".len());
    assert!(parsed.description.ends_with("..."));
}

#[test]
fn parse_template_rejects_unterminated_frontmatter() {
    let err = parse_template("---\ndescription: x\nbody without closing fence").unwrap_err();
    assert!(err.contains("unterminated"));
}

#[test]
fn parse_command_args_handles_quotes_and_whitespace() {
    let args = parse_command_args(r#"  foo "two words" 'single quoted'   bar  "#);
    assert_eq!(
        args,
        vec![
            "foo".to_string(),
            "two words".to_string(),
            "single quoted".to_string(),
            "bar".to_string(),
        ]
    );
}

#[test]
fn parse_command_args_returns_empty_for_blank_input() {
    assert!(parse_command_args("").is_empty());
    assert!(parse_command_args("   ").is_empty());
}

#[test]
fn substitute_args_uses_named_schema() {
    let out = substitute_args(
        "review {file} for {focus}",
        &["src/lib.rs".into(), "perf".into()],
        &["file".into(), "focus".into()],
    );
    assert_eq!(out, "review src/lib.rs for perf");
}

#[test]
fn substitute_args_numeric_brace_positions() {
    let out = substitute_args(
        "first={1} second={2} all={ARGUMENTS}",
        &["a".into(), "b".into()],
        &[],
    );
    assert_eq!(out, "first=a second=b all=a b");
}

#[test]
fn substitute_args_dollar_compat_tokens() {
    let out = substitute_args(
        "$1 then $@ and $ARGUMENTS plus ${@:2}",
        &["one".into(), "two".into(), "three".into()],
        &[],
    );
    assert_eq!(
        out,
        "one then one two three and one two three plus two three"
    );
}

#[test]
fn substitute_args_dollar_slice_length() {
    let out = substitute_args(
        "slice=${@:2:1}",
        &["one".into(), "two".into(), "three".into()],
        &[],
    );
    assert_eq!(out, "slice=two");
}

#[test]
fn substitute_args_preserves_unknown_braces_and_dollars() {
    let out = substitute_args("price ${cost} and {literal}", &[], &[]);
    assert_eq!(out, "price ${cost} and {literal}");
}

#[test]
fn substitute_args_missing_positional_renders_empty() {
    let out = substitute_args("[$1][$2]", &["only".into()], &[]);
    assert_eq!(out, "[only][]");
}

#[test]
fn substitute_args_handles_utf8_body() {
    let out = substitute_args("héllo {name}", &["wörld".into()], &["name".into()]);
    assert_eq!(out, "héllo wörld");
}

#[test]
fn catalog_discovers_user_and_project_dirs() {
    let root = temp_dir("discover");
    let user = root.join("user");
    let project = root.join("project");
    fs::create_dir_all(&user).unwrap();
    fs::create_dir_all(&project).unwrap();
    write_template(
        &user,
        "review",
        "---\ndescription: user review\n---\nuser body\n",
    );
    write_template(
        &project,
        "ship",
        "---\ndescription: ship plan\n---\nship body\n",
    );
    let catalog = PromptTemplateCatalog::from_dirs(Some(&user), Some(&project));
    let mut names = catalog.names();
    names.sort();
    assert_eq!(names, vec!["review", "ship"]);
    assert_eq!(
        catalog.get("review").unwrap().source,
        PromptTemplateSource::User
    );
    assert_eq!(
        catalog.get("ship").unwrap().source,
        PromptTemplateSource::Project,
    );
}

#[test]
fn catalog_project_overrides_user_same_name() {
    let root = temp_dir("override");
    let user = root.join("user");
    let project = root.join("project");
    fs::create_dir_all(&user).unwrap();
    fs::create_dir_all(&project).unwrap();
    write_template(
        &user,
        "review",
        "---\ndescription: user version\n---\nuser body\n",
    );
    write_template(
        &project,
        "review",
        "---\ndescription: project version\n---\nproject body\n",
    );
    let catalog = PromptTemplateCatalog::from_dirs(Some(&user), Some(&project));
    let template = catalog.get("review").expect("template exists");
    assert_eq!(template.source, PromptTemplateSource::Project);
    assert_eq!(template.description, "project version");
    assert!(template.content.contains("project body"));
}

#[test]
fn catalog_skips_invalid_names_and_non_md_files() {
    let root = temp_dir("filter");
    let user = root.join("user");
    fs::create_dir_all(&user).unwrap();
    fs::write(user.join("not-a-prompt.txt"), "plain text").unwrap();
    fs::write(
        user.join("bad name.md"),
        "---\ndescription: rejected\n---\nbody\n",
    )
    .unwrap();
    fs::write(
        user.join("-bad-start.md"),
        "---\ndescription: rejected\n---\nbody\n",
    )
    .unwrap();
    fs::write(user.join("good.md"), "---\ndescription: keep\n---\nbody\n").unwrap();
    let catalog = PromptTemplateCatalog::from_dirs(Some(&user), None);
    let names = catalog.names();
    assert_eq!(names, vec!["good"]);
}

#[test]
fn catalog_ignores_missing_directory() {
    let catalog = PromptTemplateCatalog::from_dirs(
        Some(std::path::Path::new("/nonexistent/squeezy-prompts")),
        Some(std::path::Path::new("/another/missing")),
    );
    assert!(catalog.is_empty());
}

#[test]
fn catalog_expand_renders_named_arguments() {
    let root = temp_dir("expand");
    let project = root.join("project");
    fs::create_dir_all(&project).unwrap();
    write_template(
        &project,
        "review",
        "---\ndescription: review file\nargs: [file]\n---\nReview {file} for issues.\n",
    );
    let catalog = PromptTemplateCatalog::from_dirs(None, Some(&project));
    let expanded = catalog
        .expand("/review src/lib.rs")
        .expect("expansion matched");
    assert_eq!(expanded, "Review src/lib.rs for issues.");
}

#[test]
fn catalog_expand_returns_none_for_unknown_template() {
    let catalog = PromptTemplateCatalog::empty();
    assert!(catalog.expand("/anything").is_none());
    assert!(catalog.expand("not a slash").is_none());
    assert!(catalog.expand("/").is_none());
}

#[test]
fn catalog_expand_returns_none_when_input_is_not_a_slash() {
    let root = temp_dir("non-slash");
    let project = root.join("project");
    fs::create_dir_all(&project).unwrap();
    write_template(&project, "review", "---\ndescription: r\n---\nReview.\n");
    let catalog = PromptTemplateCatalog::from_dirs(None, Some(&project));
    assert!(catalog.expand("review src/lib.rs").is_none());
}

#[test]
fn catalog_expand_quoted_argument_keeps_internal_spaces() {
    let root = temp_dir("quoted");
    let project = root.join("project");
    fs::create_dir_all(&project).unwrap();
    write_template(
        &project,
        "echo",
        "---\ndescription: e\nargs: [msg]\n---\nSay: {msg}\n",
    );
    let catalog = PromptTemplateCatalog::from_dirs(None, Some(&project));
    let expanded = catalog.expand(r#"/echo "hello world""#).expect("matched");
    assert_eq!(expanded, "Say: hello world");
}

#[test]
fn catalog_expand_unknown_named_token_passes_through() {
    let root = temp_dir("unknown-token");
    let project = root.join("project");
    fs::create_dir_all(&project).unwrap();
    // {file} is unschemed and {ARGUMENTS} resolves to the full args list.
    write_template(
        &project,
        "raw",
        "---\ndescription: r\n---\nLiteral {file} and all={ARGUMENTS}.\n",
    );
    let catalog = PromptTemplateCatalog::from_dirs(None, Some(&project));
    let expanded = catalog.expand("/raw foo bar").expect("matched");
    assert_eq!(expanded, "Literal {file} and all=foo bar.");
}
