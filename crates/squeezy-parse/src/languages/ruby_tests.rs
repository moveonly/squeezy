use super::*;

fn resolve(relative: &str, target: &str) -> String {
    ruby_resolve_relative_path(relative, target)
}

#[test]
fn resolves_relative_imports() {
    assert_eq!(resolve("app/models/admin.rb", "user"), "app/models/user.rb");
    assert_eq!(
        resolve("app/models/admin.rb", "../services/greeter"),
        "app/services/greeter.rb"
    );
    assert_eq!(
        resolve("lib/runner.rb", "../app/services/greeter"),
        "app/services/greeter.rb"
    );
    assert_eq!(resolve("a.rb", "./b"), "b.rb");
}

#[test]
fn imported_name_strips_directories() {
    assert_eq!(
        ruby_imported_name_from_path("app/models/user.rb").as_deref(),
        Some("user")
    );
    assert_eq!(
        ruby_imported_name_from_path("json").as_deref(),
        Some("json")
    );
}
