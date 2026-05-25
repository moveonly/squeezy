use std::{
    env, fs, io,
    path::{Path, PathBuf},
};

const DOCS: &[(&str, &str)] = &[
    ("README.md", "docs/external/README.md"),
    ("AGENT_APPROACH.md", "docs/external/AGENT_APPROACH.md"),
    ("APPROVAL_POLICY.md", "docs/external/APPROVAL_POLICY.md"),
    ("CHECKPOINTS.md", "docs/external/CHECKPOINTS.md"),
    ("CONFIGURATION.md", "docs/external/CONFIGURATION.md"),
    ("FEEDBACK.md", "docs/external/FEEDBACK.md"),
    ("INSTALL.md", "docs/external/INSTALL.md"),
    ("LANGUAGES.md", "docs/external/LANGUAGES.md"),
    ("MCP_AND_WEB.md", "docs/external/MCP_AND_WEB.md"),
    ("PLATFORMS.md", "docs/external/PLATFORMS.md"),
    ("PROVIDERS.md", "docs/external/PROVIDERS.md"),
    ("REPO_PROFILE.md", "docs/external/REPO_PROFILE.md"),
    ("SESSIONS.md", "docs/external/SESSIONS.md"),
    ("SHELL_SANDBOXING.md", "docs/external/SHELL_SANDBOXING.md"),
    ("SKILLS.md", "docs/external/SKILLS.md"),
    ("TELEMETRY.md", "docs/external/TELEMETRY.md"),
    ("TOOLS.md", "docs/external/TOOLS.md"),
    ("TROUBLESHOOTING.md", "docs/external/TROUBLESHOOTING.md"),
    (
        "tool-call-saving-strategy.md",
        "docs/external/tool-call-saving-strategy.md",
    ),
];

fn main() -> io::Result<()> {
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let docs_dir = find_docs_dir(&manifest_dir)?;
    println!("cargo:rerun-if-changed={}", docs_dir.display());

    let mut generated = String::from("const BUNDLED_DOCS: &[BundledDoc] = &[\n");
    for (file_name, docs_path) in DOCS {
        let full_path = docs_dir.join(file_name);
        println!("cargo:rerun-if-changed={}", full_path.display());
        let content = fs::read_to_string(&full_path)?;
        generated.push_str("    BundledDoc {\n");
        generated.push_str(&format!("        path: {docs_path:?},\n"));
        generated.push_str("        content: ");
        generated.push_str(&raw_string_literal(&content));
        generated.push_str(",\n    },\n");
    }
    generated.push_str("];\n");

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("out dir"));
    fs::write(out_dir.join("bundled_docs.rs"), generated)
}

fn find_docs_dir(manifest_dir: &Path) -> io::Result<PathBuf> {
    for candidate in [
        manifest_dir.join("../../docs/external"),
        manifest_dir.join("bundled-docs/external"),
    ] {
        if candidate.is_dir() {
            return Ok(candidate);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!(
            "could not find external docs from {}",
            manifest_dir.display()
        ),
    ))
}

fn raw_string_literal(content: &str) -> String {
    for hashes in 0..16 {
        let marker = "#".repeat(hashes);
        let terminator = format!("\"{marker}");
        if !content.contains(&terminator) {
            return format!("r{marker}\"{content}\"{marker}");
        }
    }
    panic!("could not generate raw string literal for bundled docs");
}
