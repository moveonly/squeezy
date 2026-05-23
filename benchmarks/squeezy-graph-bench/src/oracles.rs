use squeezy_core::LanguageFamily;

use crate::cli::BenchmarkLanguage;

pub trait LanguageOracle: Sync {
    fn id(&self) -> &'static str;
    fn family(&self) -> LanguageFamily;
    fn benchmark_language(&self) -> BenchmarkLanguage;
    fn supports_mixed_workload(&self) -> bool {
        self.benchmark_language().supports_mixed_workload()
    }
}

macro_rules! oracle {
    ($module:ident, $type_name:ident, $id:literal, $family:expr, $language:expr) => {
        pub mod $module {
            use squeezy_core::LanguageFamily;

            use crate::{cli::BenchmarkLanguage, oracles::LanguageOracle};

            pub struct $type_name;

            impl LanguageOracle for $type_name {
                fn id(&self) -> &'static str {
                    $id
                }

                fn family(&self) -> LanguageFamily {
                    $family
                }

                fn benchmark_language(&self) -> BenchmarkLanguage {
                    $language
                }
            }
        }
    };
}

oracle!(
    rust_analyzer,
    RustAnalyzerOracle,
    "rust_analyzer",
    LanguageFamily::Rust,
    BenchmarkLanguage::Rust
);
oracle!(
    cpython_ast,
    CpythonAstOracle,
    "cpython_ast",
    LanguageFamily::Python,
    BenchmarkLanguage::Python
);
oracle!(
    javac,
    JavacOracle,
    "javac",
    LanguageFamily::Java,
    BenchmarkLanguage::Java
);
oracle!(
    roslyn,
    RoslynOracle,
    "roslyn",
    LanguageFamily::CSharp,
    BenchmarkLanguage::CSharp
);
oracle!(
    go_types,
    GoTypesOracle,
    "go_types",
    LanguageFamily::Go,
    BenchmarkLanguage::Go
);
oracle!(
    clang,
    ClangOracle,
    "clang",
    LanguageFamily::CFamily,
    BenchmarkLanguage::C
);
oracle!(
    tsc,
    TscOracle,
    "tsc",
    LanguageFamily::JsTs,
    BenchmarkLanguage::TypeScript
);

static RUST_ANALYZER: rust_analyzer::RustAnalyzerOracle = rust_analyzer::RustAnalyzerOracle;
static CPYTHON_AST: cpython_ast::CpythonAstOracle = cpython_ast::CpythonAstOracle;
static JAVAC: javac::JavacOracle = javac::JavacOracle;
static ROSLYN: roslyn::RoslynOracle = roslyn::RoslynOracle;
static GO_TYPES: go_types::GoTypesOracle = go_types::GoTypesOracle;
static CLANG: clang::ClangOracle = clang::ClangOracle;
static TSC: tsc::TscOracle = tsc::TscOracle;

static ORACLES: [&'static dyn LanguageOracle; 7] = [
    &RUST_ANALYZER,
    &CPYTHON_AST,
    &JAVAC,
    &ROSLYN,
    &GO_TYPES,
    &CLANG,
    &TSC,
];

pub fn inventory() -> &'static [&'static dyn LanguageOracle] {
    &ORACLES
}

#[allow(dead_code)]
pub fn oracle_for_family(family: LanguageFamily) -> Option<&'static dyn LanguageOracle> {
    inventory()
        .iter()
        .copied()
        .find(|oracle| oracle.family() == family)
}

#[cfg(test)]
#[path = "oracles_tests.rs"]
mod tests;
