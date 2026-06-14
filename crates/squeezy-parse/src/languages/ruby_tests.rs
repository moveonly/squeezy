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

#[test]
fn detects_test_and_spec_files() {
    assert!(ruby_is_test_filename("test/models/user_test.rb"));
    assert!(ruby_is_test_filename("spec/models/user_spec.rb"));
    assert!(ruby_is_test_filename("test/models/user.rb"));
    assert!(ruby_is_test_filename("spec/support/helper.rb"));
    assert!(ruby_is_test_filename("APP/SPEC/Thing_Spec.RB"));
    assert!(!ruby_is_test_filename("app/models/user.rb"));
    assert!(!ruby_is_test_filename("lib/contest/runner.rb"));
}

#[test]
fn classifies_macro_dispatch_sinks() {
    for sink in [
        "send",
        "public_send",
        "__send__",
        "define_method",
        "method",
        "eval",
        "instance_eval",
        "class_eval",
        "module_eval",
    ] {
        assert!(ruby_is_macro_dispatch(sink), "{sink} should be macro");
    }
    assert!(!ruby_is_macro_dispatch("call"));
    assert!(!ruby_is_macro_dispatch("each"));
}

#[test]
fn maps_visibility_keywords_and_calls() {
    assert_eq!(ruby_visibility_keyword("private"), Some("private"));
    assert_eq!(ruby_visibility_keyword("protected"), Some("protected"));
    assert_eq!(ruby_visibility_keyword("public"), Some("public"));
    // `module_function` (bare) is private on the instance side.
    assert_eq!(ruby_visibility_keyword("module_function"), Some("private"));
    assert_eq!(ruby_visibility_keyword("attr_reader"), None);

    assert_eq!(ruby_visibility_call("private"), Some("private"));
    assert_eq!(
        ruby_visibility_call("private_class_method"),
        Some("private")
    );
    assert_eq!(ruby_visibility_call("public_class_method"), Some("public"));
    assert_eq!(ruby_visibility_call("protected"), Some("protected"));
    assert_eq!(ruby_visibility_call("nope"), None);
}
