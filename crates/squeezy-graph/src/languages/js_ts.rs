use crate::*;

impl SemanticGraph {
    pub(crate) fn unresolved_js_ts_imported_direct_call(
        &self,
        caller_id: &SymbolId,
        call: &ParsedCall,
    ) -> bool {
        if call.kind != ParsedCallKind::Direct || call.receiver.is_some() {
            return false;
        }
        let Some(caller) = self.symbols.get(caller_id) else {
            return false;
        };
        if !self
            .files
            .get(&caller.file_id)
            .map(|file| is_js_ts_language(file.language))
            .unwrap_or(false)
        {
            return false;
        }
        self.imports_for_file(&caller.file_id)
            .filter(|import| self.import_visible_from_symbol(import, caller))
            .filter(|import| import.span.start_byte <= call.span.start_byte)
            .any(|import| {
                import
                    .alias
                    .as_deref()
                    .map(|alias| alias == call.name)
                    .unwrap_or_else(|| last_path_segment(&import.path) == call.name)
            })
    }

    /// For C/C++ Direct calls, treat `#include "header.h"` as an
    /// authoritative cross-TU import: when the called name resolves to a
    /// unique Function/Method declared in a workspace file whose relative
    /// path matches one of the caller file's includes (by trailing path
    /// suffix on a `/` boundary, or by basename), bind to it. This is the
    /// closest syntactic analogue to the Rust `use module::*;` shape that
    /// is already handled — without it, every cross-file C call falls back
    /// to `CandidateSet`.
    pub(crate) fn js_ts_import_matches_symbol(
        &self,
        import: &ParsedImport,
        symbol: &GraphSymbol,
    ) -> bool {
        let Some(symbol_file) = self.files.get(&symbol.file_id) else {
            return false;
        };
        let Some(module) = js_ts_import_module_part(import) else {
            return false;
        };
        let import_file = self.files.get(&import.file_id);
        let module_candidates = self.js_ts_resolver.module_candidates(module, import_file);
        if module_candidates.is_empty() {
            return false;
        }
        let symbol_modules = js_ts_file_module_variants(&symbol_file.relative_path);
        module_candidates
            .iter()
            .any(|candidate| symbol_modules.contains(candidate))
    }
}

pub(crate) fn is_js_ts_language(language: LanguageKind) -> bool {
    matches!(
        language,
        LanguageKind::JavaScript | LanguageKind::Jsx | LanguageKind::TypeScript | LanguageKind::Tsx
    )
}

#[derive(Debug, Clone, Default)]
pub(crate) struct JsTsResolver {
    path_mappings: Vec<JsTsPathMapping>,
    packages: Vec<JsTsPackage>,
}

#[derive(Debug, Clone)]
pub(crate) struct JsTsPathMapping {
    config_dir: String,
    base_url: Option<String>,
    pattern: String,
    targets: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct JsTsPackage {
    root: String,
    name: String,
    exports: Vec<(String, String)>,
    main_entries: Vec<String>,
}

impl JsTsResolver {
    pub(crate) fn from_files(files: &HashMap<FileId, FileRecord>) -> Self {
        let mut resolver = Self::default();
        for file in files.values() {
            if file.relative_path.ends_with("tsconfig.json") {
                resolver.add_tsconfig(file);
            } else if file.relative_path.ends_with("package.json") {
                resolver.add_package(file);
            }
        }
        resolver
    }

    fn add_tsconfig(&mut self, file: &FileRecord) {
        let Ok(raw) = std::fs::read_to_string(&file.path) else {
            return;
        };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&raw) else {
            return;
        };
        let Some(options) = json
            .get("compilerOptions")
            .and_then(|value| value.as_object())
        else {
            return;
        };
        let config_dir = parent_dir_string(&file.relative_path);
        let base_url = options
            .get("baseUrl")
            .and_then(|value| value.as_str())
            .map(|value| js_ts_normalize_module_path(&join_module_path(&config_dir, value)));
        if let Some(paths) = options.get("paths").and_then(|value| value.as_object()) {
            for (pattern, targets) in paths {
                let targets = targets
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|value| value.as_str())
                    .map(ToString::to_string)
                    .collect::<Vec<_>>();
                if !targets.is_empty() {
                    self.path_mappings.push(JsTsPathMapping {
                        config_dir: config_dir.clone(),
                        base_url: base_url.clone(),
                        pattern: pattern.clone(),
                        targets,
                    });
                }
            }
        }
        if let Some(base_url) = base_url {
            self.path_mappings.push(JsTsPathMapping {
                config_dir,
                base_url: None,
                pattern: "*".to_string(),
                targets: vec![format!("{base_url}/*")],
            });
        }
    }

    fn add_package(&mut self, file: &FileRecord) {
        let Ok(raw) = std::fs::read_to_string(&file.path) else {
            return;
        };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&raw) else {
            return;
        };
        let Some(name) = json.get("name").and_then(|value| value.as_str()) else {
            return;
        };
        let root = parent_dir_string(&file.relative_path);
        let mut main_entries = Vec::new();
        for field in ["types", "typings", "module", "main"] {
            if let Some(value) = json.get(field).and_then(|value| value.as_str()) {
                main_entries.push(value.to_string());
            }
        }
        let mut exports = Vec::new();
        if let Some(value) = json.get("exports") {
            collect_js_ts_exports(".", value, &mut exports);
        }
        self.packages.push(JsTsPackage {
            root,
            name: name.to_string(),
            exports,
            main_entries,
        });
    }

    fn module_candidates(
        &self,
        module: &str,
        import_file: Option<&FileRecord>,
    ) -> BTreeSet<String> {
        let mut candidates = BTreeSet::new();
        if module.starts_with('.') {
            if let Some(import_file) = import_file {
                let base = parent_dir_string(&import_file.relative_path);
                insert_js_ts_module_variants(&mut candidates, &join_module_path(&base, module));
            }
            return candidates;
        }

        insert_js_ts_module_variants(&mut candidates, module);

        for mapping in &self.path_mappings {
            let Some(star) = match_js_ts_path_pattern(&mapping.pattern, module) else {
                continue;
            };
            for target in &mapping.targets {
                let replaced = target.replace('*', &star);
                let with_base = mapping
                    .base_url
                    .as_deref()
                    .map(|base| join_module_path(base, &replaced))
                    .unwrap_or_else(|| join_module_path(&mapping.config_dir, &replaced));
                insert_js_ts_module_variants(&mut candidates, &with_base);
            }
        }

        for package in &self.packages {
            let Some(subpath) = js_ts_package_subpath(&package.name, module) else {
                continue;
            };
            let package_subpath = subpath.unwrap_or_default();
            if package_subpath.is_empty() {
                insert_js_ts_module_variants(&mut candidates, &package.root);
                insert_js_ts_module_variants(
                    &mut candidates,
                    &join_module_path(&package.root, "src"),
                );
                insert_js_ts_module_variants(
                    &mut candidates,
                    &join_module_path(&package.root, "index"),
                );
                insert_js_ts_module_variants(
                    &mut candidates,
                    &join_module_path(&package.root, "src/index"),
                );
                for entry in &package.main_entries {
                    insert_js_ts_module_variants(
                        &mut candidates,
                        &join_module_path(&package.root, entry),
                    );
                }
            } else {
                insert_js_ts_module_variants(
                    &mut candidates,
                    &join_module_path(&package.root, &package_subpath),
                );
                insert_js_ts_module_variants(
                    &mut candidates,
                    &join_module_path(&join_module_path(&package.root, "src"), &package_subpath),
                );
            }
            let export_key = if package_subpath.is_empty() {
                ".".to_string()
            } else {
                format!("./{package_subpath}")
            };
            for (_, target) in package.exports.iter().filter(|(key, _)| key == &export_key) {
                insert_js_ts_module_variants(
                    &mut candidates,
                    &join_module_path(&package.root, target),
                );
            }
        }

        candidates
    }
}

pub(crate) fn collect_js_ts_exports(
    key: &str,
    value: &serde_json::Value,
    out: &mut Vec<(String, String)>,
) {
    if let Some(target) = value.as_str() {
        out.push((key.to_string(), target.to_string()));
        return;
    }
    let Some(object) = value.as_object() else {
        return;
    };
    if key == "." {
        for (child_key, child_value) in object {
            if child_key == "." || child_key.starts_with("./") {
                collect_js_ts_exports(child_key, child_value, out);
            }
        }
    }
    for preferred in ["types", "import", "require", "default"] {
        if let Some(child) = object.get(preferred) {
            collect_js_ts_exports(key, child, out);
        }
    }
}

pub(crate) fn js_ts_import_module_part(import: &ParsedImport) -> Option<&str> {
    let path = if import.is_glob {
        import.path.strip_suffix(".*").unwrap_or(&import.path)
    } else {
        import
            .path
            .rsplit_once('.')
            .map(|(module, _)| module)
            .unwrap_or(&import.path)
    };
    Some(path).filter(|path| !path.is_empty())
}

pub(crate) fn js_ts_package_subpath(package_name: &str, module: &str) -> Option<Option<String>> {
    if module == package_name {
        return Some(None);
    }
    module
        .strip_prefix(package_name)
        .and_then(|rest| rest.strip_prefix('/'))
        .map(|rest| Some(rest.to_string()))
}

pub(crate) fn match_js_ts_path_pattern(pattern: &str, module: &str) -> Option<String> {
    let Some((prefix, suffix)) = pattern.split_once('*') else {
        return (pattern == module).then(String::new);
    };
    module
        .strip_prefix(prefix)
        .and_then(|rest| rest.strip_suffix(suffix))
        .map(ToString::to_string)
}

pub(crate) fn insert_js_ts_module_variants(candidates: &mut BTreeSet<String>, path: &str) {
    let normalized = js_ts_module_path_for_file(path);
    if normalized.is_empty() {
        return;
    }
    candidates.insert(normalized.clone());
    if !normalized.ends_with("/index") {
        candidates.insert(format!("{normalized}/index"));
    }
}

pub(crate) fn js_ts_file_module_variants(path: &str) -> BTreeSet<String> {
    let mut variants = BTreeSet::new();
    insert_js_ts_module_variants(&mut variants, path);
    variants
}

pub(crate) fn js_ts_module_path_for_file(path: &str) -> String {
    let without_ext = path
        .trim_end_matches(".jsx")
        .trim_end_matches(".tsx")
        .trim_end_matches(".mjs")
        .trim_end_matches(".cjs")
        .trim_end_matches(".mts")
        .trim_end_matches(".cts")
        .trim_end_matches(".js")
        .trim_end_matches(".ts")
        .trim_end_matches(".d");
    let normalized = js_ts_normalize_module_path(without_ext);
    normalized
        .strip_suffix("/index")
        .unwrap_or(&normalized)
        .to_string()
}

pub(crate) fn parent_dir_string(path: &str) -> String {
    path.rsplit_once('/')
        .map(|(dir, _)| dir.to_string())
        .unwrap_or_default()
}

pub(crate) fn join_module_path(base: &str, child: &str) -> String {
    if base.is_empty() {
        child.to_string()
    } else if child.is_empty() {
        base.to_string()
    } else {
        format!("{base}/{child}")
    }
}

pub(crate) fn js_ts_normalize_module_path(path: &str) -> String {
    let mut parts = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}
