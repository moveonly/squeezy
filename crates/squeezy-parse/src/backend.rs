use squeezy_core::{LanguageFamily, LanguageKind, Result};
use tree_sitter::Parser;

use crate::{ParsedFile, parser_for_language_kind};
use squeezy_workspace::FileRecord;
use tree_sitter::Tree;

pub trait LanguageBackend: Sync {
    fn family(&self) -> LanguageFamily;
    fn kinds(&self) -> &'static [LanguageKind];
    fn tree_sitter_language(&self, kind: LanguageKind) -> Option<tree_sitter::Language>;
    fn extract(&self, file: FileRecord, source: &str, tree: &Tree) -> ParsedFile;
    fn file_extensions(&self) -> &'static [&'static str];
    fn filename_hints(&self) -> &'static [&'static str] {
        &[]
    }

    fn parser(&self, kind: LanguageKind) -> Result<Parser> {
        parser_for_language_kind(kind)
    }
}

struct RustBackend;
struct PythonBackend;
struct JavaBackend;
struct CSharpBackend;
struct GoBackend;
struct CFamilyBackend;
struct JsTsBackend;

macro_rules! backend {
    ($type_name:ty, $family:expr, $extract:path) => {
        impl LanguageBackend for $type_name {
            fn family(&self) -> LanguageFamily {
                $family
            }

            fn kinds(&self) -> &'static [LanguageKind] {
                self.family().kinds()
            }

            fn tree_sitter_language(&self, kind: LanguageKind) -> Option<tree_sitter::Language> {
                if self.kinds().contains(&kind) {
                    crate::language_for_kind(kind)
                } else {
                    None
                }
            }

            fn extract(&self, file: FileRecord, source: &str, tree: &Tree) -> ParsedFile {
                $extract(file, source, tree)
            }

            fn file_extensions(&self) -> &'static [&'static str] {
                self.family().file_extensions()
            }
        }
    };
}

backend!(RustBackend, LanguageFamily::Rust, crate::extract_rust);
backend!(PythonBackend, LanguageFamily::Python, crate::extract_python);
backend!(JavaBackend, LanguageFamily::Java, crate::extract_java);
backend!(CSharpBackend, LanguageFamily::CSharp, crate::extract_csharp);
backend!(GoBackend, LanguageFamily::Go, crate::extract_go);
backend!(
    CFamilyBackend,
    LanguageFamily::CFamily,
    crate::extract_c_family
);
backend!(JsTsBackend, LanguageFamily::JsTs, crate::extract_js_ts);

static RUST: RustBackend = RustBackend;
static PYTHON: PythonBackend = PythonBackend;
static JAVA: JavaBackend = JavaBackend;
static CSHARP: CSharpBackend = CSharpBackend;
static GO: GoBackend = GoBackend;
static C_FAMILY: CFamilyBackend = CFamilyBackend;
static JS_TS: JsTsBackend = JsTsBackend;

static BACKENDS: [&'static dyn LanguageBackend; 7] =
    [&RUST, &PYTHON, &JAVA, &CSHARP, &GO, &C_FAMILY, &JS_TS];

pub fn inventory() -> &'static [&'static dyn LanguageBackend] {
    &BACKENDS
}

pub fn backend_for_family(family: LanguageFamily) -> Option<&'static dyn LanguageBackend> {
    inventory()
        .iter()
        .copied()
        .find(|backend| backend.family() == family)
}

pub fn backend_for_kind(kind: LanguageKind) -> Option<&'static dyn LanguageBackend> {
    LanguageFamily::of(kind).and_then(backend_for_family)
}

pub fn is_supported_language(language: LanguageKind) -> bool {
    backend_for_kind(language).is_some()
}
