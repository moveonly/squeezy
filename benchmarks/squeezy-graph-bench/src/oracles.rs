use squeezy_core::LanguageFamily;

use crate::cli::BenchmarkLanguage;

pub(crate) mod clang;
pub(crate) mod common_scan;
pub(crate) mod cpython_ast;
pub(crate) mod go_types;
pub(crate) mod javac;
pub(crate) mod kotlin_oracle;
pub(crate) mod php_oracle;
pub(crate) mod roslyn;
pub(crate) mod ruby_oracle;
pub(crate) mod rust_analyzer;
pub(crate) mod tsc;

pub(crate) use clang::*;
pub(crate) use common_scan::*;
pub(crate) use cpython_ast::*;
pub(crate) use go_types::*;
pub(crate) use javac::*;
pub(crate) use kotlin_oracle::*;
pub(crate) use php_oracle::*;
pub(crate) use roslyn::*;
pub(crate) use ruby_oracle::*;
pub(crate) use rust_analyzer::*;
pub(crate) use tsc::*;

pub trait LanguageOracle: Sync {
    fn id(&self) -> &'static str;
    fn family(&self) -> LanguageFamily;
    fn benchmark_language(&self) -> BenchmarkLanguage;
    fn supports_mixed_workload(&self) -> bool {
        self.benchmark_language().supports_mixed_workload()
    }
}

struct OracleDescriptor {
    id: &'static str,
    family: LanguageFamily,
    language: BenchmarkLanguage,
}

impl LanguageOracle for OracleDescriptor {
    fn id(&self) -> &'static str {
        self.id
    }

    fn family(&self) -> LanguageFamily {
        self.family
    }

    fn benchmark_language(&self) -> BenchmarkLanguage {
        self.language
    }
}

static RUST_ANALYZER: OracleDescriptor = OracleDescriptor {
    id: "rust_analyzer",
    family: LanguageFamily::Rust,
    language: BenchmarkLanguage::Rust,
};
static CPYTHON_AST: OracleDescriptor = OracleDescriptor {
    id: "cpython_ast",
    family: LanguageFamily::Python,
    language: BenchmarkLanguage::Python,
};
static JAVAC: OracleDescriptor = OracleDescriptor {
    id: "javac",
    family: LanguageFamily::Java,
    language: BenchmarkLanguage::Java,
};
static KOTLIN_ORACLE: OracleDescriptor = OracleDescriptor {
    id: "kotlin_compiler_embeddable",
    family: LanguageFamily::Kotlin,
    language: BenchmarkLanguage::Kotlin,
};
static ROSLYN: OracleDescriptor = OracleDescriptor {
    id: "roslyn",
    family: LanguageFamily::CSharp,
    language: BenchmarkLanguage::CSharp,
};
static GO_TYPES: OracleDescriptor = OracleDescriptor {
    id: "go_types",
    family: LanguageFamily::Go,
    language: BenchmarkLanguage::Go,
};
static CLANG: OracleDescriptor = OracleDescriptor {
    id: "clang",
    family: LanguageFamily::CFamily,
    language: BenchmarkLanguage::C,
};
static TSC: OracleDescriptor = OracleDescriptor {
    id: "tsc",
    family: LanguageFamily::JsTs,
    language: BenchmarkLanguage::TypeScript,
};
static PHP_PARSER: OracleDescriptor = OracleDescriptor {
    id: "nikic_php_parser",
    family: LanguageFamily::Php,
    language: BenchmarkLanguage::Php,
};
static RUBY_PRISM: OracleDescriptor = OracleDescriptor {
    id: "ruby_prism",
    family: LanguageFamily::Ruby,
    language: BenchmarkLanguage::Ruby,
};

static ORACLES: [&'static dyn LanguageOracle; 10] = [
    &RUST_ANALYZER,
    &CPYTHON_AST,
    &JAVAC,
    &KOTLIN_ORACLE,
    &ROSLYN,
    &GO_TYPES,
    &CLANG,
    &TSC,
    &PHP_PARSER,
    &RUBY_PRISM,
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
