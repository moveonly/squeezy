use std::collections::BTreeMap;

use crate::*;

impl SemanticGraph {
    pub(crate) fn add_csharp_type_edges(&mut self) {
        self.add_csharp_base_edges();
        self.add_csharp_partial_edges();
    }

    fn add_csharp_base_edges(&mut self) {
        let mut edges = Vec::new();
        for symbol in self
            .symbols
            .values()
            .filter(|symbol| self.symbol_is_csharp_type(symbol))
        {
            for base in symbol
                .attributes
                .iter()
                .filter_map(|attribute| attribute.strip_prefix("base:"))
            {
                let candidates =
                    self.csharp_type_candidates_for_name_in_file(&symbol.file_id, base);
                let (to, confidence, edge_candidates) = match candidates.as_slice() {
                    [only] => (Some(only.clone()), Confidence::Heuristic, Vec::new()),
                    [] => (None, Confidence::External, Vec::new()),
                    _ => (
                        None,
                        Confidence::CandidateSet,
                        candidates
                            .iter()
                            .take(MAX_EDGE_CANDIDATES)
                            .cloned()
                            .collect(),
                    ),
                };
                let kind = to
                    .as_ref()
                    .and_then(|id| self.symbols.get(id))
                    .map(|target| {
                        if target.kind == SymbolKind::Interface {
                            EdgeKind::Implements
                        } else {
                            EdgeKind::Extends
                        }
                    })
                    .unwrap_or(EdgeKind::Extends);
                edges.push(GraphEdge {
                    from: symbol.id.clone(),
                    to,
                    target_text: base.to_string(),
                    kind,
                    span: Some(symbol.span),
                    confidence,
                    freshness: Freshness::Fresh,
                    provenance: Provenance::new("tree-sitter-c-sharp", "base type edge"),
                    candidates: edge_candidates,
                });
            }
        }
        self.edges.extend(edges);
    }

    fn add_csharp_partial_edges(&mut self) {
        let edges = {
            let mut groups: BTreeMap<&str, Vec<&GraphSymbol>> = BTreeMap::new();
            for symbol in self.symbols.values() {
                if !self.symbol_is_csharp_type(symbol)
                    || !symbol
                        .attributes
                        .iter()
                        .any(|attribute| attribute == "csharp:partial")
                {
                    continue;
                }
                let Some(identity) = symbol.language_identity.as_deref() else {
                    continue;
                };
                groups.entry(identity).or_default().push(symbol);
            }
            let mut edges = Vec::new();
            for (_identity, mut symbols) in groups {
                if symbols.len() < 2 {
                    continue;
                }
                symbols.sort_by(|left, right| left.id.0.cmp(&right.id.0));
                let canonical = symbols[0].id.clone();
                for symbol in symbols.into_iter().skip(1) {
                    edges.push(GraphEdge {
                        from: symbol.id.clone(),
                        to: Some(canonical.clone()),
                        target_text: "partial".to_string(),
                        kind: EdgeKind::PartialOf,
                        span: Some(symbol.span),
                        confidence: Confidence::ExactSyntax,
                        freshness: Freshness::Fresh,
                        provenance: Provenance::new("tree-sitter-c-sharp", "partial type identity"),
                        candidates: Vec::new(),
                    });
                }
            }
            edges
        };
        self.edges.extend(edges);
    }

    fn symbol_is_csharp_type(&self, symbol: &GraphSymbol) -> bool {
        self.files
            .get(&symbol.file_id)
            .map(|file| file.language == LanguageKind::CSharp)
            .unwrap_or(false)
            && matches!(
                symbol.kind,
                SymbolKind::Class
                    | SymbolKind::Struct
                    | SymbolKind::Interface
                    | SymbolKind::Enum
                    | SymbolKind::TypeAlias
            )
    }

    fn csharp_type_candidates_for_name_in_file(
        &self,
        file_id: &FileId,
        name: &str,
    ) -> Vec<SymbolId> {
        let direct_name = last_path_segment(name);
        let caller_namespace = self.packages.get(file_id);
        let mut ids = self
            .symbols_by_name_or_scan(&direct_name)
            .into_iter()
            .filter_map(|id| self.symbols.get(&id))
            .filter(|symbol| self.symbol_is_csharp_type(symbol))
            .filter(|symbol| {
                symbol.file_id == *file_id
                    || self.packages.get(&symbol.file_id) == caller_namespace
                    || self
                        .imports_for_file(file_id)
                        .any(|import| csharp_import_matches_symbol(import, symbol))
            })
            .map(|symbol| symbol.id.clone())
            .collect::<Vec<_>>();
        ids.sort_by(|left, right| left.0.cmp(&right.0));
        ids.dedup();
        ids
    }
}

pub(crate) fn csharp_import_matches_symbol(import: &ParsedImport, symbol: &GraphSymbol) -> bool {
    if import.alias.as_deref() == Some(symbol.name.as_str()) {
        return true;
    }
    let Some(identity) = symbol.language_identity.as_deref() else {
        return false;
    };
    let full_type_path = identity.strip_prefix("T:").unwrap_or(identity);
    let namespace = full_type_path
        .strip_suffix(symbol.name.as_str())
        .and_then(|prefix| prefix.strip_suffix('.'))
        .unwrap_or(full_type_path);
    import.path == namespace || import.path == full_type_path
}

pub(crate) fn dotnet_project_metadata_provider(file: &FileRecord) -> Option<&'static str> {
    let filename = file
        .relative_path
        .rsplit('/')
        .next()
        .unwrap_or(&file.relative_path);
    // Match case-insensitively: Windows MSBuild conventions allow any casing
    // for `.csproj`, `.sln`, `Directory.Build.props`, etc.
    let lower = filename.to_ascii_lowercase();
    match lower.as_str() {
        "directory.build.props" => Some("directory-build-props"),
        "directory.build.targets" => Some("directory-build-targets"),
        "global.json" => Some("global-json"),
        "packages.lock.json" => Some("packages-lock"),
        name if name.ends_with(".csproj") => Some("csproj"),
        name if name.ends_with(".sln") => Some("sln"),
        name if name.ends_with(".slnx") => Some("slnx"),
        _ => None,
    }
}

pub(crate) fn dotnet_source_root_facts(
    _provider: &str,
    csharp_paths: &[&str],
) -> Vec<(&'static str, String, &'static str)> {
    let mut roots = BTreeSet::new();
    for path in csharp_paths {
        let mut segments = path.split('/');
        let first = segments.next().unwrap_or_default();
        let second = segments.next();
        let has_third = segments.next().is_some();
        let lower = path.to_ascii_lowercase();
        let root = if has_third && matches!(first, "src" | "test" | "tests") {
            format!("{}/{}", first, second.unwrap_or_default())
        } else if second.is_some()
            && (lower.contains("/test") || matches!(first, "src" | "test" | "tests"))
        {
            first.to_string()
        } else {
            continue;
        };
        let kind =
            if lower.contains("/test") || lower.starts_with("test/") || lower.starts_with("tests/")
            {
                "test_root"
            } else {
                "source_root"
            };
        roots.insert((kind, root));
    }
    roots
        .into_iter()
        .map(|(kind, root)| (kind, root, ".NET C# source layout"))
        .collect()
}

pub(crate) fn dotnet_target_facts(
    provider: &str,
    source: &str,
) -> Vec<(&'static str, String, &'static str)> {
    match provider {
        "csproj" | "directory-build-props" | "directory-build-targets" => {
            let mut facts = Vec::new();
            for value in tag_values(source, "TargetFramework") {
                facts.push(("target_framework", value, "MSBuild TargetFramework"));
            }
            for value in tag_values(source, "TargetFrameworks") {
                for framework in value
                    .split(';')
                    .map(str::trim)
                    .filter(|item| !item.is_empty())
                {
                    facts.push((
                        "target_framework",
                        framework.to_string(),
                        "MSBuild TargetFrameworks",
                    ));
                }
            }
            facts
        }
        "global-json" => json_string_field(source, "version")
            .into_iter()
            .map(|version| ("sdk", version, "global.json SDK version"))
            .collect(),
        "sln" | "slnx" => solution_project_paths(source)
            .into_iter()
            .map(|path| ("project_reference", path, ".NET solution project reference"))
            .collect(),
        _ => Vec::new(),
    }
}

pub(crate) fn dotnet_dependency_facts(provider: &str, source: &str) -> Vec<String> {
    match provider {
        "csproj" | "directory-build-props" | "directory-build-targets" => {
            package_references(source)
        }
        "packages-lock" => packages_lock_dependencies(source),
        _ => Vec::new(),
    }
}

pub(crate) fn dotnet_configured_source_facts(
    provider: &str,
    source: &str,
) -> Vec<(&'static str, String, &'static str)> {
    match provider {
        "csproj" | "directory-build-props" | "directory-build-targets" => {
            let mut facts = Vec::new();
            for value in tag_values(source, "Compile") {
                if value.ends_with(".cs") || value.to_ascii_lowercase().ends_with(".cs") {
                    // Normalize backslashes so Windows-style `src\Program.cs`
                    // produces the same path form as `.sln` project paths.
                    let normalized = if value.contains('\\') {
                        value.replace('\\', "/")
                    } else {
                        value
                    };
                    facts.push(("configured_source", normalized, "MSBuild Compile item"));
                }
            }
            for value in tag_values(source, "ProjectReference") {
                let normalized = if value.contains('\\') {
                    value.replace('\\', "/")
                } else {
                    value
                };
                facts.push((
                    "project_reference",
                    normalized,
                    "MSBuild ProjectReference item",
                ));
            }
            facts
        }
        _ => Vec::new(),
    }
}

fn tag_values(source: &str, tag: &str) -> Vec<String> {
    let mut values = Vec::new();
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut rest = source;
    while let Some(start) = rest.find(&open) {
        rest = &rest[start + open.len()..];
        let Some(end) = rest.find(&close) else {
            break;
        };
        let value = rest[..end].trim();
        if !value.is_empty() {
            values.push(value.to_string());
        }
        rest = &rest[end + close.len()..];
    }
    values.extend(attribute_values(source, tag, "Include"));
    values.sort();
    values.dedup();
    values
}

fn package_references(source: &str) -> Vec<String> {
    let mut refs = Vec::new();
    for block in element_blocks(source, "PackageReference") {
        let Some(id) = attribute_value(block, "Include")
            .or_else(|| attribute_value(block, "Update"))
            .or_else(|| first_tag_value(block, "Include"))
        else {
            continue;
        };
        let version = attribute_value(block, "Version")
            .or_else(|| first_tag_value(block, "Version"))
            .unwrap_or_else(|| "?".to_string());
        refs.push(format!("{id}:{version}"));
    }
    refs.sort();
    refs.dedup();
    refs
}

fn packages_lock_dependencies(source: &str) -> Vec<String> {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(source) else {
        return Vec::new();
    };
    let mut deps = Vec::new();
    collect_lock_deps(&json, &mut deps);
    deps.sort();
    deps.dedup();
    deps
}

fn collect_lock_deps(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(dependencies) = map.get("dependencies").and_then(|item| item.as_object()) {
                collect_lock_dependency_map(dependencies, out);
            }
            for value in map.values() {
                collect_lock_deps(value, out);
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                collect_lock_deps(value, out);
            }
        }
        _ => {}
    }
}

fn collect_lock_dependency_map(
    map: &serde_json::Map<String, serde_json::Value>,
    out: &mut Vec<String>,
) {
    for (name, value) in map {
        if value.get("type").is_some()
            || value.get("resolved").is_some()
            || value.get("version").is_some()
        {
            let version = value
                .get("resolved")
                .or_else(|| value.get("version"))
                .and_then(|item| item.as_str())
                .unwrap_or("?");
            out.push(format!("{name}:{version}"));
        } else if let Some(nested) = value.as_object() {
            collect_lock_dependency_map(nested, out);
        }
    }
}

fn solution_project_paths(source: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for line in source.lines() {
        if !line.contains(".csproj") {
            continue;
        }
        for part in line.split('"') {
            if part.ends_with(".csproj") {
                paths.push(part.replace('\\', "/"));
            }
        }
    }
    // Basic .slnx support: project path="src/App/App.csproj"
    for value in attribute_values(source, "Project", "Path")
        .into_iter()
        .chain(attribute_values(source, "Project", "path"))
    {
        if value.ends_with(".csproj") {
            paths.push(value.replace('\\', "/"));
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

fn element_blocks<'a>(source: &'a str, tag: &str) -> Vec<&'a str> {
    let mut blocks = Vec::new();
    let start_tag = format!("<{tag}");
    let end_tag = format!("</{tag}>");
    let mut rest = source;
    while let Some(start) = rest.find(&start_tag) {
        rest = &rest[start..];
        let Some(close) = rest.find('>') else {
            break;
        };
        if rest[..=close].ends_with("/>") {
            blocks.push(&rest[..=close]);
            rest = &rest[close + 1..];
            continue;
        }
        let Some(end) = rest.find(&end_tag) else {
            break;
        };
        blocks.push(&rest[..end + end_tag.len()]);
        rest = &rest[end + end_tag.len()..];
    }
    blocks
}

fn attribute_values(source: &str, tag: &str, attribute: &str) -> Vec<String> {
    element_blocks(source, tag)
        .into_iter()
        .filter_map(|block| attribute_value(block, attribute))
        .collect()
}

fn attribute_value(block: &str, attribute: &str) -> Option<String> {
    for quote in ['"', '\''] {
        let prefix = format!("{attribute}={quote}");
        let Some(start) = block.find(&prefix) else {
            continue;
        };
        let rest = &block[start + prefix.len()..];
        let Some(end) = rest.find(quote) else {
            continue;
        };
        let value = rest[..end].trim();
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

fn first_tag_value(source: &str, tag: &str) -> Option<String> {
    tag_values(source, tag).into_iter().next()
}

fn json_string_field(source: &str, field: &str) -> Option<String> {
    let json = serde_json::from_str::<serde_json::Value>(source).ok()?;
    find_json_string_field(&json, field)
}

fn find_json_string_field(value: &serde_json::Value, field: &str) -> Option<String> {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(value) = map.get(field).and_then(|item| item.as_str()) {
                return Some(value.to_string());
            }
            map.values()
                .find_map(|value| find_json_string_field(value, field))
        }
        serde_json::Value::Array(values) => values
            .iter()
            .find_map(|value| find_json_string_field(value, field)),
        _ => None,
    }
}

#[cfg(test)]
#[path = "csharp_tests.rs"]
mod tests;
