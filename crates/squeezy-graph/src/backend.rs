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

static RUST: RustGraphExt = RustGraphExt;
static PYTHON: PythonGraphExt = PythonGraphExt;
static JAVA: JavaGraphExt = JavaGraphExt;
static CSHARP: CSharpGraphExt = CSharpGraphExt;
static GO: GoGraphExt = GoGraphExt;
static C_FAMILY: CFamilyGraphExt = CFamilyGraphExt;
static JS_TS: JsTsGraphExt = JsTsGraphExt;

static EXTENSIONS: [&'static dyn LanguageGraphExt; 7] =
    [&RUST, &PYTHON, &JAVA, &CSHARP, &GO, &C_FAMILY, &JS_TS];

pub fn inventory() -> &'static [&'static dyn LanguageGraphExt] {
    &EXTENSIONS
}

pub fn extension_for_family(family: LanguageFamily) -> Option<&'static dyn LanguageGraphExt> {
    inventory()
        .iter()
        .copied()
        .find(|extension| extension.family() == family)
}
