use super::*;

use crate::scala_language;

fn scala_record(relative_path: &str, source: &str) -> FileRecord {
    FileRecord {
        id: FileId::new(relative_path),
        path: std::path::PathBuf::from(relative_path),
        relative_path: relative_path.to_string(),
        hash: ContentHash::new("0"),
        size_bytes: source.len() as u64,
        modified_unix_millis: 0,
        language: LanguageKind::Scala,
        freshness: Freshness::Fresh,
    }
}

fn parse_scala(source: &str) -> ParsedFile {
    let mut parser = Parser::new();
    parser
        .set_language(&scala_language())
        .expect("load scala grammar");
    let tree = parser.parse(source, None).expect("parse scala source");
    extract_scala(scala_record("Main.scala", source), source, &tree)
}

#[test]
fn keeps_nested_qualified_path_prefix_reference() {
    // A 3+ segment qualified selection `a.b.c` parses as nested
    // `field_expression` nodes whose left-nested prefix `a.b` shares the outer
    // node's start byte. Deduping references on (start_byte, kind) alone
    // collapses the distinct prefix; the text-aware key keeps both.
    let parsed = parse_scala("object Main { val x = a.b.c }");

    let field_refs: Vec<&str> = parsed
        .references
        .iter()
        .filter(|reference| reference.kind == ReferenceKind::Field)
        .map(|reference| reference.text.as_str())
        .collect();
    assert!(
        field_refs.contains(&"a.b.c"),
        "expected full qualified selection reference, got {field_refs:?}"
    );
    assert!(
        field_refs.contains(&"a.b"),
        "expected nested qualified-prefix reference, got {field_refs:?}"
    );

    let path_hits: Vec<&str> = parsed
        .body_hits
        .iter()
        .filter(|hit| hit.kind == BodyHitKind::Path)
        .map(|hit| hit.text.as_str())
        .collect();
    assert!(
        path_hits.contains(&"a.b.c"),
        "expected full qualified selection path body hit, got {path_hits:?}"
    );
    assert!(
        path_hits.contains(&"a.b"),
        "expected nested qualified-prefix path body hit, got {path_hits:?}"
    );
}

fn scala_symbol<'a>(parsed: &'a ParsedFile, name: &str) -> &'a ParsedSymbol {
    parsed
        .symbols
        .iter()
        .find(|symbol| symbol.name == name)
        .unwrap_or_else(|| panic!("expected symbol {name}, got {:?}", parsed.symbols))
}

#[test]
fn separates_superclass_from_with_mixins() {
    // `extends Base with A with B` records the first parent as the superclass
    // (`base:`) and each `with`-mixin as a trait mixin (`mixin:`), so
    // decl_search(iface:/mixin:) and the generic inheritance edge pass can tell
    // an extended class from a mixed-in trait.
    let parsed = parse_scala("class Admin extends User with Auditable with Loggable");
    let admin = scala_symbol(&parsed, "Admin");
    assert!(
        admin.attributes.iter().any(|a| a == "base:User"),
        "expected base:User, got {:?}",
        admin.attributes
    );
    assert!(
        admin.attributes.iter().any(|a| a == "mixin:Auditable"),
        "expected mixin:Auditable, got {:?}",
        admin.attributes
    );
    assert!(
        admin.attributes.iter().any(|a| a == "mixin:Loggable"),
        "expected mixin:Loggable, got {:?}",
        admin.attributes
    );
    assert!(
        !admin.attributes.iter().any(|a| a == "base:Auditable"),
        "with-mixin must not be flattened to base:, got {:?}",
        admin.attributes
    );
}

#[test]
fn records_derives_clause_typeclasses() {
    // A Scala 3 `derives` clause records each derived typeclass structurally so
    // "which types derive X" is a queryable attribute, not a bare Type mention.
    let parsed = parse_scala("case class Point(x: Int, y: Int) derives Eq, Show");
    let point = scala_symbol(&parsed, "Point");
    assert!(
        point.attributes.iter().any(|a| a == "derives:Eq"),
        "expected derives:Eq, got {:?}",
        point.attributes
    );
    assert!(
        point.attributes.iter().any(|a| a == "derives:Show"),
        "expected derives:Show, got {:?}",
        point.attributes
    );
}

#[test]
fn records_self_type_required_mixins() {
    // The cake-pattern self-type `self: T with U =>` records each required type
    // as a `scala:self-type:` constraint so cake dependencies are enumerable.
    let parsed =
        parse_scala("trait Service {\n  self: Repository with Logger =>\n  def run(): Unit\n}");
    let service = scala_symbol(&parsed, "Service");
    assert!(
        service
            .attributes
            .iter()
            .any(|a| a == "scala:self-type:Repository"),
        "expected scala:self-type:Repository, got {:?}",
        service.attributes
    );
    assert!(
        service
            .attributes
            .iter()
            .any(|a| a == "scala:self-type:Logger"),
        "expected scala:self-type:Logger, got {:?}",
        service.attributes
    );
}

#[test]
fn records_type_parameter_bounds() {
    // Context bounds and upper bounds are recorded as structured attributes so
    // `def sort[T: Ordering]` carries the Ordering constraint and `[A <: Animal]`
    // carries its upper bound — not just a bare Type mention.
    let parsed = parse_scala("object M {\n  def sort[T: Ordering](xs: List[T]): List[T] = xs\n}");
    let sort = scala_symbol(&parsed, "sort");
    assert!(
        sort.attributes.iter().any(|a| a == "bound:Ordering"),
        "expected bound:Ordering, got {:?}",
        sort.attributes
    );

    let bounded = parse_scala("class Cage[A <: Animal, B >: Puppy]");
    let cage = scala_symbol(&bounded, "Cage");
    assert!(
        cage.attributes.iter().any(|a| a == "upper-bound:Animal"),
        "expected upper-bound:Animal, got {:?}",
        cage.attributes
    );
    assert!(
        cage.attributes.iter().any(|a| a == "lower-bound:Puppy"),
        "expected lower-bound:Puppy, got {:?}",
        cage.attributes
    );
}

#[test]
fn emits_unapply_call_for_case_class_pattern() {
    // `case Email(user, domain) =>` deconstructs via the extractor's `unapply`,
    // so the parser emits a synthetic `unapply` Method call with the pattern
    // type as receiver — "where is this extractor used" then resolves to a call.
    let parsed = parse_scala(
        "object M {\n  def f(x: Any): Int = x match {\n    case Email(user, domain) => 1\n    case _ => 0\n  }\n}",
    );
    let unapply = parsed
        .calls
        .iter()
        .find(|call| call.name == "unapply" && call.receiver.as_deref() == Some("Email"))
        .unwrap_or_else(|| panic!("expected Email.unapply call, got {:?}", parsed.calls));
    assert_eq!(unapply.kind, ParsedCallKind::Method);
    assert_eq!(unapply.arity, 2, "expected arity 2, got {}", unapply.arity);
    assert_eq!(unapply.confidence, Confidence::Heuristic);
}
