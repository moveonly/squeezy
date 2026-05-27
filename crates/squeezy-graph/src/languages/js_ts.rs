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

/// Incrementally maintained JS/TS module resolver.
///
/// Workspace-wide `tsconfig.json` / `package.json` files contribute path
/// mappings and package definitions. Re-parsing every config on every
/// `rebuild_semantic_edges` is O(n) per file save; the per-file caches
/// below let us reuse the derived state for configs whose `ContentHash`
/// is unchanged and only re-aggregate the flat lookup vectors when an
/// entry was added, removed, or rebuilt.
#[derive(Debug, Clone, Default)]
pub(crate) struct JsTsResolver {
    path_mappings: Vec<JsTsPathMapping>,
    packages: Vec<JsTsPackage>,
    tsconfig_entries: HashMap<FileId, JsTsTsconfigEntry>,
    package_entries: HashMap<FileId, JsTsPackageEntry>,
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

#[derive(Debug, Clone)]
struct JsTsTsconfigEntry {
    hash: ContentHash,
    relative_path: String,
    mappings: Vec<JsTsPathMapping>,
}

#[derive(Debug, Clone)]
struct JsTsPackageEntry {
    hash: ContentHash,
    relative_path: String,
    package: Option<JsTsPackage>,
}

/// Outcome of an incremental [`JsTsResolver::update_from_files`] pass.
///
/// `inserted` and `rebuilt` count freshly parsed configs (cache misses),
/// `reused` counts configs whose `ContentHash` matched the cache and were
/// skipped entirely. `removed` counts entries dropped because the file is
/// no longer in the workspace.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct JsTsResolverUpdate {
    pub inserted: usize,
    pub rebuilt: usize,
    pub reused: usize,
    pub removed: usize,
}

impl JsTsResolverUpdate {
    pub(crate) fn changed(&self) -> bool {
        self.inserted + self.rebuilt + self.removed > 0
    }

    #[allow(dead_code)]
    pub(crate) fn parses(&self) -> usize {
        self.inserted + self.rebuilt
    }
}

impl JsTsResolver {
    /// Incrementally bring the resolver in sync with the workspace.
    ///
    /// Only configs whose `ContentHash` differs from the cached entry
    /// are re-parsed; unchanged configs reuse the previously derived
    /// `JsTsPathMapping` / `JsTsPackage` state. The flat lookup vectors
    /// are re-aggregated only when at least one entry was inserted,
    /// rebuilt, or removed.
    pub(crate) fn update_from_files(
        &mut self,
        files: &HashMap<FileId, FileRecord>,
    ) -> JsTsResolverUpdate {
        let mut update = JsTsResolverUpdate::default();

        let mut tsconfig_ids: HashSet<&FileId> = HashSet::new();
        let mut package_ids: HashSet<&FileId> = HashSet::new();
        for (file_id, file) in files {
            if file.relative_path.ends_with("tsconfig.json") {
                tsconfig_ids.insert(file_id);
            } else if file.relative_path.ends_with("package.json") {
                package_ids.insert(file_id);
            }
        }

        let drop_tsconfigs: Vec<FileId> = self
            .tsconfig_entries
            .keys()
            .filter(|id| !tsconfig_ids.contains(id))
            .cloned()
            .collect();
        for id in drop_tsconfigs {
            self.tsconfig_entries.remove(&id);
            update.removed += 1;
        }
        let drop_packages: Vec<FileId> = self
            .package_entries
            .keys()
            .filter(|id| !package_ids.contains(id))
            .cloned()
            .collect();
        for id in drop_packages {
            self.package_entries.remove(&id);
            update.removed += 1;
        }

        for id in tsconfig_ids {
            let file = &files[id];
            match self.tsconfig_entries.get(id) {
                Some(entry) if entry.hash == file.hash => update.reused += 1,
                Some(_) => {
                    let mappings = parse_tsconfig_mappings(file);
                    self.tsconfig_entries.insert(
                        id.clone(),
                        JsTsTsconfigEntry {
                            hash: file.hash.clone(),
                            relative_path: file.relative_path.clone(),
                            mappings,
                        },
                    );
                    update.rebuilt += 1;
                }
                None => {
                    let mappings = parse_tsconfig_mappings(file);
                    self.tsconfig_entries.insert(
                        id.clone(),
                        JsTsTsconfigEntry {
                            hash: file.hash.clone(),
                            relative_path: file.relative_path.clone(),
                            mappings,
                        },
                    );
                    update.inserted += 1;
                }
            }
        }
        for id in package_ids {
            let file = &files[id];
            match self.package_entries.get(id) {
                Some(entry) if entry.hash == file.hash => update.reused += 1,
                Some(_) => {
                    let package = parse_package_entry(file);
                    self.package_entries.insert(
                        id.clone(),
                        JsTsPackageEntry {
                            hash: file.hash.clone(),
                            relative_path: file.relative_path.clone(),
                            package,
                        },
                    );
                    update.rebuilt += 1;
                }
                None => {
                    let package = parse_package_entry(file);
                    self.package_entries.insert(
                        id.clone(),
                        JsTsPackageEntry {
                            hash: file.hash.clone(),
                            relative_path: file.relative_path.clone(),
                            package,
                        },
                    );
                    update.inserted += 1;
                }
            }
        }

        if update.changed() {
            self.rebuild_flat_views();
        }

        update
    }

    fn rebuild_flat_views(&mut self) {
        let mut tsconfigs: Vec<&JsTsTsconfigEntry> = self.tsconfig_entries.values().collect();
        tsconfigs.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
        self.path_mappings = tsconfigs
            .into_iter()
            .flat_map(|entry| entry.mappings.iter().cloned())
            .collect();

        let mut packages: Vec<&JsTsPackageEntry> = self.package_entries.values().collect();
        packages.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
        self.packages = packages
            .into_iter()
            .filter_map(|entry| entry.package.clone())
            .collect();
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

fn parse_tsconfig_mappings(file: &FileRecord) -> Vec<JsTsPathMapping> {
    let Ok(raw) = std::fs::read_to_string(&file.path) else {
        return Vec::new();
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return Vec::new();
    };
    let Some(options) = json
        .get("compilerOptions")
        .and_then(|value| value.as_object())
    else {
        return Vec::new();
    };
    let config_dir = parent_dir_string(&file.relative_path);
    let base_url = options
        .get("baseUrl")
        .and_then(|value| value.as_str())
        .map(|value| js_ts_normalize_module_path(&join_module_path(&config_dir, value)));
    let mut mappings = Vec::new();
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
                mappings.push(JsTsPathMapping {
                    config_dir: config_dir.clone(),
                    base_url: base_url.clone(),
                    pattern: pattern.clone(),
                    targets,
                });
            }
        }
    }
    if let Some(base_url) = base_url {
        mappings.push(JsTsPathMapping {
            config_dir,
            base_url: None,
            pattern: "*".to_string(),
            targets: vec![format!("{base_url}/*")],
        });
    }
    mappings
}

fn parse_package_entry(file: &FileRecord) -> Option<JsTsPackage> {
    let raw = std::fs::read_to_string(&file.path).ok()?;
    let json = serde_json::from_str::<serde_json::Value>(&raw).ok()?;
    let name = json.get("name").and_then(|value| value.as_str())?;
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
    Some(JsTsPackage {
        root,
        name: name.to_string(),
        exports,
        main_entries,
    })
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
