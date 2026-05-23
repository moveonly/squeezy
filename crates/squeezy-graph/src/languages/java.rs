use crate::*;

impl SemanticGraph {
    pub(crate) fn java_package_for_file(&self, file_id: &FileId) -> Option<Vec<String>> {
        self.java_package_by_file.get(file_id).cloned()
    }

    pub(crate) fn java_import_matches_symbol(
        &self,
        import: &ParsedImport,
        symbol: &GraphSymbol,
    ) -> bool {
        let mut import_segments = path_segments(&import.path);
        let last_segment_is_glob = import_segments
            .last()
            .map(|segment| segment == "*")
            .unwrap_or(false);
        if last_segment_is_glob {
            import_segments.pop();
        }
        let Some(package) = self.java_package_for_file(&symbol.file_id) else {
            return false;
        };

        // Static glob import (e.g. `import static a.b.C.*;`).
        // After popping `*` the path is `a.b.C`. The symbol must be a member
        // of that class (or a member of one of its enclosing classes if
        // `import_segments` matches farther up the chain).
        if import.is_glob && import.is_static {
            return self.java_symbol_member_of_path(symbol, &import_segments, &package);
        }

        // Regular glob import (e.g. `import a.b.*;`). Matches every top-level
        // type whose package equals `import_segments`, or any nested type
        // whose enclosing class chain begins below `import_segments`.
        if import.is_glob {
            return import_segments == package && symbol_is_top_level_for_imports(symbol)
                || self.java_symbol_owner_path(symbol) == import_segments;
        }

        // Static member import (e.g. `import static a.b.C.method;`).
        // After popping `method` the path is `a.b.C`. The symbol must be a
        // member of class `C`.
        if import.is_static {
            if import_segments.is_empty() {
                return false;
            }
            // Member name must equal the symbol's name.
            if import_segments.last().map(String::as_str) != Some(symbol.name.as_str()) {
                return false;
            }
            import_segments.pop();
            return self.java_symbol_member_of_path(symbol, &import_segments, &package);
        }

        // Plain type import (e.g. `import a.b.C;` or `import a.b.C.Nested;`).
        // After popping the type leaf, `import_segments` is either the package
        // (top-level type) or `package + class chain` (nested type).
        if last_path_segment(&import.path) != symbol.name {
            return false;
        }
        import_segments.pop();
        let owner_path = self.java_symbol_owner_path(symbol);
        owner_path == import_segments
    }

    pub(crate) fn java_symbol_owner_path(&self, symbol: &GraphSymbol) -> Vec<String> {
        let mut path = self
            .java_package_for_file(&symbol.file_id)
            .unwrap_or_default();
        let mut chain = Vec::new();
        let mut parent_id = symbol.parent_id.as_ref();
        while let Some(id) = parent_id {
            let Some(parent) = self.symbols.get(id) else {
                break;
            };
            if matches!(
                parent.kind,
                SymbolKind::Class | SymbolKind::Trait | SymbolKind::Enum | SymbolKind::Struct
            ) {
                chain.push(parent.name.clone());
            }
            parent_id = parent.parent_id.as_ref();
        }
        chain.reverse();
        path.extend(chain);
        path
    }

    pub(crate) fn java_symbol_member_of_path(
        &self,
        symbol: &GraphSymbol,
        path: &[String],
        package: &[String],
    ) -> bool {
        if path.is_empty() || path.len() < package.len() {
            return false;
        }
        if !path.starts_with(package) {
            return false;
        }
        let class_chain = &path[package.len()..];
        if class_chain.is_empty() {
            return false;
        }
        let mut owner_classes = Vec::new();
        let mut parent_id = symbol.parent_id.as_ref();
        while let Some(id) = parent_id {
            let Some(parent) = self.symbols.get(id) else {
                break;
            };
            if matches!(
                parent.kind,
                SymbolKind::Class | SymbolKind::Trait | SymbolKind::Enum | SymbolKind::Struct
            ) {
                owner_classes.push(parent.name.clone());
            }
            parent_id = parent.parent_id.as_ref();
        }
        owner_classes.reverse();
        owner_classes == class_chain
    }

    pub(crate) fn java_static_imported_method(
        &self,
        candidates: &[SymbolId],
        caller_id: &SymbolId,
        call: &ParsedCall,
    ) -> Option<SymbolId> {
        if call.receiver.is_some() {
            return None;
        }
        let caller = self.symbols.get(caller_id)?;
        let caller_file = self.files.get(&caller.file_id)?;
        if caller_file.language != LanguageKind::Java {
            return None;
        }
        single_symbol(
            candidates
                .iter()
                .filter_map(|id| self.symbols.get(id))
                .filter(|symbol| symbol.kind == SymbolKind::Method)
                .filter(|symbol| {
                    self.imports_for_file(&caller.file_id)
                        .filter(|import| import.is_static)
                        .any(|import| self.java_import_matches_symbol(import, symbol))
                })
                .map(|symbol| symbol.id.clone()),
        )
    }

    pub(crate) fn java_receiver_field_method(
        &self,
        caller_id: &SymbolId,
        call: &ParsedCall,
    ) -> Option<SymbolId> {
        let receiver = call.receiver.as_deref()?;
        if matches!(receiver, "this" | "super") || receiver.contains(' ') || receiver.contains('(')
        {
            return None;
        }
        let class_id = self.java_class_for_caller(caller_id)?;
        let field = self
            .children_by_parent
            .get(&class_id)?
            .iter()
            .find_map(|child_id| {
                self.symbols
                    .get(child_id)
                    .filter(|symbol| symbol.kind == SymbolKind::Field && symbol.name == receiver)
            })?;
        let type_name = field
            .attributes
            .iter()
            .find_map(|attribute| attribute.strip_prefix("type:"))?;
        let class_id = self
            .java_class_candidates_for_name_in_file(&field.file_id, type_name)
            .first()?
            .clone();
        self.java_method_on_class(&class_id, &call.name)
    }

    pub(crate) fn java_class_for_caller(&self, caller_id: &SymbolId) -> Option<SymbolId> {
        let caller = self.symbols.get(caller_id)?;
        if matches!(
            caller.kind,
            SymbolKind::Class | SymbolKind::Struct | SymbolKind::Enum | SymbolKind::Trait
        ) {
            return Some(caller.id.clone());
        }
        let mut current = caller.parent_id.clone();
        while let Some(id) = current {
            let symbol = self.symbols.get(&id)?;
            if matches!(
                symbol.kind,
                SymbolKind::Class | SymbolKind::Struct | SymbolKind::Enum | SymbolKind::Trait
            ) {
                return Some(symbol.id.clone());
            }
            current = symbol.parent_id.clone();
        }
        None
    }

    pub(crate) fn java_class_candidates_for_name_in_file(
        &self,
        file_id: &FileId,
        name: &str,
    ) -> Vec<SymbolId> {
        let direct_name = last_path_segment(name);
        let mut class_ids = self
            .symbols_by_name_or_scan(&direct_name)
            .into_iter()
            .filter_map(|id| self.symbols.get(&id))
            .filter(|symbol| {
                matches!(
                    symbol.kind,
                    SymbolKind::Class | SymbolKind::Struct | SymbolKind::Enum | SymbolKind::Trait
                )
            })
            .filter(|symbol| {
                symbol.file_id == *file_id
                    || self.java_package_for_file(&symbol.file_id)
                        == self.java_package_for_file(file_id)
                    || self
                        .imports_for_file(file_id)
                        .any(|import| self.import_matches_symbol(import, symbol))
            })
            .map(|symbol| symbol.id.clone())
            .collect::<Vec<_>>();
        class_ids.sort_by(|left, right| left.0.cmp(&right.0));
        class_ids.dedup();
        class_ids
    }

    pub(crate) fn java_method_on_class(
        &self,
        class_id: &SymbolId,
        method_name: &str,
    ) -> Option<SymbolId> {
        single_symbol(
            self.children_by_parent
                .get(class_id)?
                .iter()
                .filter_map(|child_id| self.symbols.get(child_id))
                .filter(|symbol| symbol.kind == SymbolKind::Method && symbol.name == method_name)
                .map(|symbol| symbol.id.clone()),
        )
    }
}

pub(crate) fn java_build_metadata_provider(file: &FileRecord) -> Option<&'static str> {
    match file.relative_path.as_str() {
        "pom.xml" => Some("maven"),
        "build.gradle" | "build.gradle.kts" | "settings.gradle" | "settings.gradle.kts" => {
            Some("gradle")
        }
        _ => None,
    }
}

pub(crate) fn symbol_is_top_level_for_imports(symbol: &GraphSymbol) -> bool {
    symbol
        .parent_id
        .as_ref()
        .map(|id| id.0.starts_with("file:"))
        .unwrap_or(true)
}

pub(crate) fn java_paths_signature(paths: &[String]) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x00000100000001b3;
    let mut hash = FNV_OFFSET;
    for path in paths {
        for byte in path.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
        hash ^= 0x00;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

pub(crate) fn java_source_root_facts(
    provider: &str,
    java_paths: &[&str],
) -> Vec<(&'static str, String, &'static str)> {
    let mut roots = BTreeSet::new();
    for path in java_paths {
        let segments = path.split('/').collect::<Vec<_>>();
        if segments.len() >= 4 && segments[0] == "src" && segments[2] == "java" {
            let source_set = segments[1];
            roots.insert((source_set.to_string(), format!("src/{source_set}/java")));
        }
        if let Some(root) = generated_source_root(path) {
            roots.insert(("generated".to_string(), root));
        }
    }

    let mut facts = Vec::new();
    for (source_set, root) in roots {
        let kind = if source_set == "generated" {
            "generated_exclusion"
        } else if source_set.to_ascii_lowercase().contains("test") {
            "test_root"
        } else {
            "source_root"
        };
        let reason = if provider == "maven" {
            "Maven Java source layout"
        } else {
            "Gradle Java source layout"
        };
        facts.push((kind, format!("{source_set}:{root}"), reason));
    }
    facts
}

pub(crate) fn java_configured_source_facts(
    provider: &str,
    source: &str,
) -> Vec<(&'static str, String, &'static str)> {
    match provider {
        "maven" => maven_configured_source_facts(source),
        "gradle" => gradle_configured_source_facts(source),
        _ => Vec::new(),
    }
}

pub(crate) fn maven_configured_source_facts(
    source: &str,
) -> Vec<(&'static str, String, &'static str)> {
    let mut facts = Vec::new();
    for value in tag_values(source, "sourceDirectory") {
        facts.push((
            "source_root",
            format!("main:{value}"),
            "Maven configured sourceDirectory",
        ));
    }
    for value in tag_values(source, "testSourceDirectory") {
        facts.push((
            "test_root",
            format!("test:{value}"),
            "Maven configured testSourceDirectory",
        ));
    }
    facts
}

pub(crate) fn gradle_configured_source_facts(
    source: &str,
) -> Vec<(&'static str, String, &'static str)> {
    let mut facts = Vec::new();
    for line in source.lines().map(str::trim) {
        if line.starts_with("//") || !line.contains("srcDir") {
            continue;
        }
        let Some(path) = first_quoted_value(line) else {
            continue;
        };
        let source_set = path
            .strip_prefix("src/")
            .and_then(|rest| rest.split_once('/'))
            .map(|(source_set, _)| source_set)
            .unwrap_or("main");
        let kind = if source_set.to_ascii_lowercase().contains("test") {
            "test_root"
        } else {
            "source_root"
        };
        facts.push((
            kind,
            format!("{source_set}:{path}"),
            "Gradle configured srcDir",
        ));
    }
    facts
}

pub(crate) fn java_dependency_facts(provider: &str, source: &str) -> Vec<String> {
    match provider {
        "maven" => maven_dependency_facts(source),
        "gradle" => gradle_dependency_facts(source),
        _ => Vec::new(),
    }
}

pub(crate) fn maven_dependency_facts(source: &str) -> Vec<String> {
    let scrubbed = strip_maven_meta_blocks(source);
    let mut facts = Vec::new();
    let mut rest = scrubbed.as_str();
    while let Some(start) = rest.find("<dependency>") {
        rest = &rest[start + "<dependency>".len()..];
        let Some(end) = rest.find("</dependency>") else {
            break;
        };
        let block = &rest[..end];
        rest = &rest[end + "</dependency>".len()..];

        let Some(group_id) = first_tag_value(block, "groupId") else {
            continue;
        };
        let Some(artifact_id) = first_tag_value(block, "artifactId") else {
            continue;
        };
        let version = first_tag_value(block, "version").unwrap_or_else(|| "?".to_string());
        let scope = first_tag_value(block, "scope").unwrap_or_else(|| "compile".to_string());
        facts.push(format!("{scope}:{group_id}:{artifact_id}:{version}"));
    }
    facts
}

pub(crate) fn strip_maven_meta_blocks(source: &str) -> String {
    // `<dependencyManagement>` declares versions but not real edges, and
    // `<plugins>` blocks can contain plugin dependencies that should not be
    // surfaced as project dependencies. Strip those subtrees before scanning
    // for `<dependency>` blocks.
    let mut out = String::with_capacity(source.len());
    let mut rest = source;
    loop {
        let next_open = ["<dependencyManagement", "<plugins>", "<pluginManagement"]
            .into_iter()
            .filter_map(|tag| rest.find(tag).map(|index| (index, tag)))
            .min_by_key(|(index, _)| *index);
        let Some((open_index, open_tag)) = next_open else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..open_index]);
        rest = &rest[open_index + open_tag.len()..];
        let close_tag = match open_tag {
            "<dependencyManagement" => "</dependencyManagement>",
            "<plugins>" => "</plugins>",
            "<pluginManagement" => "</pluginManagement>",
            _ => break,
        };
        let Some(close_index) = rest.find(close_tag) else {
            break;
        };
        rest = &rest[close_index + close_tag.len()..];
    }
    out
}

pub(crate) fn gradle_dependency_facts(source: &str) -> Vec<String> {
    let mut facts = Vec::new();
    for line in source.lines().map(str::trim) {
        if line.starts_with("//") {
            continue;
        }
        let Some(coordinate) = first_quoted_value(line) else {
            continue;
        };
        if coordinate.matches(':').count() < 2 {
            continue;
        }
        let config = line
            .split(|ch: char| ch.is_whitespace() || ch == '(')
            .next()
            .unwrap_or_default()
            .trim();
        if config.is_empty() {
            continue;
        }
        facts.push(format!("{config}:{coordinate}"));
    }
    facts
}

pub(crate) fn tag_values(source: &str, tag: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut rest = source;
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    while let Some(start) = rest.find(&open) {
        let value_start = start + open.len();
        let Some(value_len) = rest[value_start..].find(&close) else {
            break;
        };
        let value_end = value_start + value_len;
        let value = rest[value_start..value_end].trim();
        if !value.is_empty() {
            values.push(value.to_string());
        }
        rest = &rest[value_end + close.len()..];
    }
    values
}

pub(crate) fn first_tag_value(source: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = source.find(&open)? + open.len();
    let end = source[start..].find(&close)? + start;
    Some(source[start..end].trim().to_string()).filter(|value| !value.is_empty())
}

pub(crate) fn first_quoted_value(line: &str) -> Option<String> {
    let quote_start = line.find(['"', '\''])?;
    let quote = line.as_bytes()[quote_start] as char;
    let rest = &line[quote_start + 1..];
    let quote_end = rest.find(quote)?;
    Some(rest[..quote_end].trim().to_string()).filter(|value| !value.is_empty())
}

pub(crate) fn generated_source_root(path: &str) -> Option<String> {
    for marker in [
        "target/generated-sources/",
        "build/generated/",
        "generated-src/",
        "src/generated/java/",
    ] {
        if path.starts_with(marker) {
            return Some(marker.trim_end_matches('/').to_string());
        }
    }
    None
}
