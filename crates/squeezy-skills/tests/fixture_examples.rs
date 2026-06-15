#[test]
fn rust_code_navigation_fixture_is_a_valid_example_skill() {
    let content = include_str!("artifacts/skills/rust-code-navigation/SKILL.md");

    let name = squeezy_skills::validate_skill_md(content)
        .expect("rust-code-navigation fixture should be a valid SKILL.md");

    assert_eq!(name, "rust-code-navigation");
}
