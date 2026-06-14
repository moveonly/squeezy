use std::{
    env, fs, io,
    path::{Path, PathBuf},
};

fn main() -> io::Result<()> {
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let docs_dir = manifest_dir.join("external-docs");
    println!("cargo:rerun-if-changed={}", docs_dir.display());

    let mut generated = String::from("const BUNDLED_DOCS: &[BundledDoc] = &[\n");
    for full_path in external_doc_paths(&docs_dir)? {
        let file_name = full_path
            .file_name()
            .expect("external docs entry has a filename")
            .to_string_lossy();
        let docs_path = format!("docs/external/{file_name}");
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

fn external_doc_paths(docs_dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(docs_dir)? {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("md") {
            paths.push(path);
        }
    }
    paths.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    Ok(paths)
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
