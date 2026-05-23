use std::{env, path::PathBuf};

use squeezy_core::{LanguageFamily, LanguageKind, Result, SqueezyError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BenchmarkLanguage {
    C,
    CSharp,
    Cpp,
    Go,
    Java,
    JavaScript,
    Python,
    Rust,
    TypeScript,
}

impl BenchmarkLanguage {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "c" => Ok(Self::C),
            "csharp" | "cs" => Ok(Self::CSharp),
            "cpp" | "c++" => Ok(Self::Cpp),
            "go" => Ok(Self::Go),
            "java" => Ok(Self::Java),
            "javascript" | "js" => Ok(Self::JavaScript),
            "python" => Ok(Self::Python),
            "rust" => Ok(Self::Rust),
            "typescript" | "ts" | "js-ts" => Ok(Self::TypeScript),
            other => Err(SqueezyError::Graph(format!(
                "unknown benchmark language {other}"
            ))),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::C => "c",
            Self::CSharp => "csharp",
            Self::Cpp => "cpp",
            Self::Go => "go",
            Self::Java => "java",
            Self::JavaScript => "javascript",
            Self::Python => "python",
            Self::Rust => "rust",
            Self::TypeScript => "typescript",
        }
    }

    pub fn language_kind(self) -> LanguageKind {
        match self {
            Self::C => LanguageKind::C,
            Self::CSharp => LanguageKind::CSharp,
            Self::Cpp => LanguageKind::Cpp,
            Self::Go => LanguageKind::Go,
            Self::Java => LanguageKind::Java,
            Self::JavaScript => LanguageKind::JavaScript,
            Self::Python => LanguageKind::Python,
            Self::Rust => LanguageKind::Rust,
            Self::TypeScript => LanguageKind::TypeScript,
        }
    }

    /// Alias preserved for callers that still spell the bench language as
    /// its source `LanguageKind`. New code should prefer `language_kind`.
    pub fn source_language(self) -> LanguageKind {
        self.language_kind()
    }

    #[allow(dead_code)]
    pub fn family(self) -> LanguageFamily {
        LanguageFamily::of(self.language_kind()).expect("benchmark language has a family")
    }

    pub fn supports_mixed_workload(self) -> bool {
        matches!(
            self,
            Self::C
                | Self::CSharp
                | Self::Cpp
                | Self::Go
                | Self::JavaScript
                | Self::Rust
                | Self::TypeScript
        )
    }

    pub fn comment_text(self) -> &'static str {
        match self {
            // C/Cpp/CSharp/Go/Java/JavaScript/Rust/TypeScript all share `//`
            // line comments at any position. C# specifically: a `//` line
            // stays valid both at file scope (alongside `using` directives or
            // file-scoped namespaces) and inside any member body.
            Self::C
            | Self::CSharp
            | Self::Cpp
            | Self::Go
            | Self::Java
            | Self::JavaScript
            | Self::Rust
            | Self::TypeScript => "\n// squeezy refresh benchmark edit\n",
            Self::Python => "\n# squeezy refresh benchmark edit\n",
        }
    }
}

pub struct Args {
    pub language: BenchmarkLanguage,
    pub(crate) fixture: PathBuf,
    pub(crate) spec: PathBuf,
    pub(crate) report: PathBuf,
    pub(crate) mixed_repo: Option<PathBuf>,
    pub(crate) mixed_iterations: usize,
    pub(crate) ra_lsp_probes: usize,
    pub(crate) oracle_files: usize,
    pub(crate) no_speed_gate: bool,
}

impl Args {
    pub fn parse() -> Result<Self> {
        let mut fixture = None;
        let mut language = BenchmarkLanguage::Rust;
        let mut spec = None;
        let mut report = None;
        let mut mixed_repo = None;
        let mut mixed_iterations = 0;
        let mut ra_lsp_probes = 25;
        let mut oracle_files = 250;
        let mut no_speed_gate = false;
        let mut args = env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--list-languages" => {
                    for family in LanguageFamily::all() {
                        let kinds = family
                            .kinds()
                            .iter()
                            .map(|kind| kind.display_name())
                            .collect::<Vec<_>>()
                            .join(", ");
                        let extensions = family.file_extensions().join(", ");
                        println!("{}\tkinds={kinds}\textensions={extensions}", family.id());
                    }
                    std::process::exit(0);
                }
                "--list-oracles" => {
                    for oracle in crate::oracles::inventory() {
                        println!(
                            "{}\tfamily={}\tmixed={}",
                            oracle.id(),
                            oracle.family().id(),
                            oracle.supports_mixed_workload()
                        );
                    }
                    std::process::exit(0);
                }
                "--language" => {
                    let raw = args.next().ok_or_else(|| {
                        SqueezyError::Graph("missing --language value".to_string())
                    })?;
                    language = BenchmarkLanguage::parse(&raw)?;
                }
                "--fixture" => fixture = args.next().map(PathBuf::from),
                "--spec" => spec = args.next().map(PathBuf::from),
                "--report" => report = args.next().map(PathBuf::from),
                "--mixed-repo" => mixed_repo = args.next().map(PathBuf::from),
                "--no-speed-gate" => no_speed_gate = true,
                "--mixed-iterations" => {
                    let raw = args.next().ok_or_else(|| {
                        SqueezyError::Graph("missing --mixed-iterations value".to_string())
                    })?;
                    mixed_iterations = raw.parse().map_err(|err| {
                        SqueezyError::Graph(format!("invalid --mixed-iterations {raw}: {err}"))
                    })?;
                }
                "--ra-lsp-probes" => {
                    let raw = args.next().ok_or_else(|| {
                        SqueezyError::Graph("missing --ra-lsp-probes value".to_string())
                    })?;
                    ra_lsp_probes = raw.parse().map_err(|err| {
                        SqueezyError::Graph(format!("invalid --ra-lsp-probes {raw}: {err}"))
                    })?;
                }
                "--oracle-files" => {
                    let raw = args.next().ok_or_else(|| {
                        SqueezyError::Graph("missing --oracle-files value".to_string())
                    })?;
                    oracle_files = raw.parse().map_err(|err| {
                        SqueezyError::Graph(format!("invalid --oracle-files {raw}: {err}"))
                    })?;
                }
                "--help" | "-h" => {
                    println!(
                        "usage: squeezy-graph-bench [--list-languages|--list-oracles] [--language rust|python|java|c|cpp|csharp|go|javascript|typescript|js-ts] --fixture <path> --spec <path> --report <path> [--mixed-repo <path>] [--mixed-iterations <n, 0=all>] [--ra-lsp-probes <n, default=25, 0=off>] [--oracle-files <n, default=250, 0=all>] [--no-speed-gate]"
                    );
                    std::process::exit(0);
                }
                other => {
                    return Err(SqueezyError::Graph(format!("unknown argument {other}")));
                }
            }
        }

        Ok(Self {
            language,
            fixture: fixture.ok_or_else(|| SqueezyError::Graph("missing --fixture".to_string()))?,
            spec: spec.ok_or_else(|| SqueezyError::Graph("missing --spec".to_string()))?,
            report: report.ok_or_else(|| SqueezyError::Graph("missing --report".to_string()))?,
            mixed_repo,
            mixed_iterations,
            ra_lsp_probes,
            oracle_files,
            no_speed_gate,
        })
    }
}
