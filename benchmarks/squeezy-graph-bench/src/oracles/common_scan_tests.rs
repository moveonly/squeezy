use std::fs;

use squeezy_core::LanguageKind;

use crate::{execution::build_graph, util::temp_dir};

use super::{collect_c_family_squeezy_symbol_scan, collect_kotlin_squeezy_symbol_scan};

#[test]
fn c_family_squeezy_scan_excludes_template_specializations() {
    let root = temp_dir("c-family-template-spec").unwrap();
    let fixture = root.join("specialization.cpp");
    fs::write(
        &fixture,
        r#"
template <typename T>
class Box {};

template <>
class Box<int> {
public:
    int value;
};
"#,
    )
    .unwrap();

    let build = build_graph(&root).unwrap();
    let scan = collect_c_family_squeezy_symbol_scan(
        &build.graph,
        LanguageKind::Cpp,
        &std::collections::BTreeSet::new(),
    );

    // The `Box<int>` specialization is tagged with
    // `c++:template-specialization` and must be excluded from the
    // comparable-symbol scan so it doesn't show up as a Class FP against
    // the clang AST oracle (which emits `ClassTemplateSpecializationDecl`,
    // a kind our normalizer skips).
    assert!(
        scan.excluded_by_kind.contains_key("TemplateSpecialization"),
        "expected at least one TemplateSpecialization exclusion in {:?}",
        scan.excluded_by_kind
    );
}

#[test]
fn kotlin_squeezy_scan_excludes_anonymous_object_symbols() {
    let root = temp_dir("kotlin-anonymous-object-scan").unwrap();
    let fixture = root.join("Factory.kt");
    fs::write(
        &fixture,
        r#"
interface Greeter {
    fun greet(): String
}

fun buildGreeter(): Greeter = object : Greeter {
    override fun greet(): String = "hi"
}
"#,
    )
    .unwrap();

    let build = build_graph(&root).unwrap();
    let scan = collect_kotlin_squeezy_symbol_scan(&build.graph);

    assert!(
        scan.excluded_by_kind
            .contains_key("KotlinAnonymousObject"),
        "expected anonymous object exclusion in {:?}",
        scan.excluded_by_kind
    );
    assert!(
        scan.excluded_by_kind
            .contains_key("KotlinAnonymousObjectMember"),
        "expected anonymous object member exclusion in {:?}",
        scan.excluded_by_kind
    );
    let greet_count = scan
        .counts
        .iter()
        .filter(|(key, _)| key.kind == "Method" && key.name == "greet")
        .map(|(_, count)| *count)
        .sum::<usize>();
    assert_eq!(
        greet_count, 1,
        "only the interface method should remain comparable: {:?}",
        scan.counts
    );
}
