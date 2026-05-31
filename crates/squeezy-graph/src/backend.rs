use squeezy_core::LanguageFamily;

pub trait LanguageGraphExt: Sync {
    fn family(&self) -> LanguageFamily;
    fn supports_project_facts(&self) -> bool {
        false
    }
    fn supports_mixed_resolution(&self) -> bool {
        true
    }
}

struct RustGraphExt;
struct PythonGraphExt;
struct JavaGraphExt;
struct CSharpGraphExt;
struct GoGraphExt;
struct CFamilyGraphExt;
struct JsTsGraphExt;
struct RubyGraphExt;
struct PhpGraphExt;
struct KotlinGraphExt;
struct SwiftGraphExt;
struct ScalaGraphExt;
struct DartGraphExt;

macro_rules! graph_ext {
    ($type_name:ty, $family:expr $(, project_facts = $project_facts:expr)? $(,)?) => {
        impl LanguageGraphExt for $type_name {
            fn family(&self) -> LanguageFamily {
                $family
            }

            $(
            fn supports_project_facts(&self) -> bool {
                $project_facts
            }
            )?
        }
    };
}

graph_ext!(RustGraphExt, LanguageFamily::Rust);
graph_ext!(PythonGraphExt, LanguageFamily::Python);
graph_ext!(JavaGraphExt, LanguageFamily::Java, project_facts = true);
graph_ext!(CSharpGraphExt, LanguageFamily::CSharp);
graph_ext!(GoGraphExt, LanguageFamily::Go);
graph_ext!(CFamilyGraphExt, LanguageFamily::CFamily);
graph_ext!(JsTsGraphExt, LanguageFamily::JsTs);
graph_ext!(RubyGraphExt, LanguageFamily::Ruby);
graph_ext!(PhpGraphExt, LanguageFamily::Php);
graph_ext!(KotlinGraphExt, LanguageFamily::Kotlin);
graph_ext!(SwiftGraphExt, LanguageFamily::Swift);
graph_ext!(ScalaGraphExt, LanguageFamily::Scala);
graph_ext!(DartGraphExt, LanguageFamily::Dart);

static RUST: RustGraphExt = RustGraphExt;
static PYTHON: PythonGraphExt = PythonGraphExt;
static JAVA: JavaGraphExt = JavaGraphExt;
static CSHARP: CSharpGraphExt = CSharpGraphExt;
static GO: GoGraphExt = GoGraphExt;
static C_FAMILY: CFamilyGraphExt = CFamilyGraphExt;
static JS_TS: JsTsGraphExt = JsTsGraphExt;
static RUBY: RubyGraphExt = RubyGraphExt;
static PHP: PhpGraphExt = PhpGraphExt;
static KOTLIN: KotlinGraphExt = KotlinGraphExt;
static SWIFT: SwiftGraphExt = SwiftGraphExt;
static SCALA: ScalaGraphExt = ScalaGraphExt;
static DART: DartGraphExt = DartGraphExt;

static EXTENSIONS: [&'static dyn LanguageGraphExt; 13] = [
    &RUST, &PYTHON, &JAVA, &CSHARP, &GO, &C_FAMILY, &JS_TS, &RUBY, &PHP, &KOTLIN, &SWIFT, &SCALA,
    &DART,
];

pub fn inventory() -> &'static [&'static dyn LanguageGraphExt] {
    &EXTENSIONS
}

pub fn extension_for_family(family: LanguageFamily) -> Option<&'static dyn LanguageGraphExt> {
    inventory()
        .iter()
        .copied()
        .find(|extension| extension.family() == family)
}
