use std::{
    fs,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use squeezy_core::SymbolKind;
use squeezy_graph::{BodySearchQuery, GraphManager, HierarchyNode, RefreshConfig, SignatureQuery};

#[test]
fn semantic_pipeline_indexes_queries_and_refreshes_incrementally() {
    let root = temp_root("semantic-pipeline");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"case\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::write(root.join("README.md"), "# docs\n").unwrap();
    fs::write(
        root.join("src").join("extra.rs"),
        "pub fn extra() -> usize { 1 }\n",
    )
    .unwrap();
    fs::write(root.join("src").join("lib.rs"), initial_source()).unwrap();

    let mut manager = GraphManager::open_with_config(
        &root,
        RefreshConfig {
            debounce: Duration::from_millis(0),
            idle_refresh_interval: Duration::from_millis(0),
            per_tool_refresh_budget: Duration::from_secs(5),
        },
    )
    .unwrap();
    let graph = manager.graph();

    let hierarchy = flatten_hierarchy(&graph.hierarchy(None, 8));
    assert!(hierarchy.contains(&"File:src/lib.rs".to_string()));
    assert!(!hierarchy.contains(&"File:Cargo.toml".to_string()));
    assert!(!hierarchy.contains(&"File:README.md".to_string()));
    assert_eq!(
        graph
            .signature_search(&SignatureQuery {
                text: "pub fn run".to_string(),
                kind: Some(SymbolKind::Method),
                visibility: None,
                attribute: None,
            })
            .len(),
        1
    );
    assert!(
        graph
            .body_search(&BodySearchQuery {
                text: "helper".to_string(),
                owner_kind: Some(SymbolKind::Method),
                hit_kind: None,
            })
            .iter()
            .any(|hit| hit.hit.text == "helper")
    );
    assert!(!graph.reference_search("Runner").is_empty());

    let run = graph.find_symbol_by_name("run").pop().unwrap();
    let helper = graph.find_symbol_by_name("helper").pop().unwrap();
    assert!(graph.call_chain(&run.id, &helper.id, 4).is_some());

    fs::write(root.join("src").join("lib.rs"), updated_source()).unwrap();
    manager.record_changed_path(root.join("src").join("lib.rs"));
    let report = manager.refresh_before_query().unwrap();

    assert!(report.refreshed);
    assert_eq!(report.reparsed_files, 1);
    assert!(manager.graph().find_symbol_by_name("helper").is_empty());
    assert!(!manager.graph().find_symbol_by_name("helper_two").is_empty());
    assert!(!manager.graph().find_symbol_by_name("extra").is_empty());
}

#[test]
fn semantic_pipeline_refreshes_java_incrementally() {
    let root = temp_root("semantic-pipeline-java");
    let java_dir = root.join("src/main/java/com/example/app");
    fs::create_dir_all(&java_dir).unwrap();
    fs::write(root.join("README.md"), "# java case\n").unwrap();
    fs::write(
        root.join("pom.xml"),
        r#"
<project>
  <build>
    <sourceDirectory>src/main/java</sourceDirectory>
    <testSourceDirectory>src/test/java</testSourceDirectory>
  </build>
  <dependencies>
    <dependency>
      <groupId>org.junit.jupiter</groupId>
      <artifactId>junit-jupiter</artifactId>
      <version>5.10.0</version>
      <scope>test</scope>
    </dependency>
  </dependencies>
</project>
"#,
    )
    .unwrap();
    fs::write(
        root.join("build.gradle"),
        r#"
plugins {
    id 'java'
}

dependencies {
    implementation 'com.google.guava:guava:33.0.0-jre'
}

sourceSets {
    main {
        java {
            srcDir 'src/main/java'
        }
    }
    test {
        java {
            srcDir 'src/test/java'
        }
    }
}
"#,
    )
    .unwrap();
    fs::write(java_dir.join("Runner.java"), initial_java_source()).unwrap();

    let mut manager = GraphManager::open_with_config(
        &root,
        RefreshConfig {
            debounce: Duration::from_millis(0),
            idle_refresh_interval: Duration::from_millis(0),
            per_tool_refresh_budget: Duration::from_secs(5),
        },
    )
    .unwrap();

    assert!(
        manager
            .graph()
            .signature_search(&SignatureQuery {
                text: "class Runner".to_string(),
                kind: Some(SymbolKind::Class),
                visibility: Some("public".to_string()),
                attribute: None,
            })
            .iter()
            .any(|symbol| symbol.name == "Runner")
    );
    let facts = manager
        .graph()
        .java_project_facts()
        .iter()
        .map(|fact| format!("{}:{}:{}", fact.provider, fact.kind, fact.value))
        .collect::<Vec<_>>();
    assert!(facts.contains(&"maven:source_root:main:src/main/java".to_string()));
    assert!(facts.contains(&"maven:test_root:test:src/test/java".to_string()));
    assert!(
        facts.contains(&"maven:dependency:test:org.junit.jupiter:junit-jupiter:5.10.0".to_string())
    );
    assert!(facts.contains(&"gradle:source_root:main:src/main/java".to_string()));
    assert!(facts.contains(&"gradle:test_root:test:src/test/java".to_string()));
    assert!(facts.contains(
        &"gradle:dependency:implementation:com.google.guava:guava:33.0.0-jre".to_string()
    ));

    fs::write(java_dir.join("Runner.java"), updated_java_source()).unwrap();
    manager.record_changed_path(java_dir.join("Runner.java"));
    let report = manager.refresh_before_query().unwrap();

    assert!(report.refreshed);
    assert_eq!(report.reparsed_files, 1);
    assert!(manager.graph().find_symbol_by_name("run").is_empty());
    assert!(!manager.graph().find_symbol_by_name("execute").is_empty());
}

fn initial_source() -> &'static str {
    r#"
pub struct Runner;

impl Runner {
    pub fn run(&self) -> usize {
        helper()
    }
}

fn helper() -> usize { 1 }
"#
}

fn updated_source() -> &'static str {
    r#"
pub struct Runner;

impl Runner {
    pub fn run(&self) -> usize {
        helper_two()
    }
}

fn helper_two() -> usize { 2 }
"#
}

fn initial_java_source() -> &'static str {
    r#"
package com.example.app;

public class Runner {
    public void run() {
        helper();
    }

    private void helper() {}
}
"#
}

fn updated_java_source() -> &'static str {
    r#"
package com.example.app;

public class Runner {
    public void execute() {
        helper();
    }

    private void helper() {}
}
"#
}

fn flatten_hierarchy(nodes: &[HierarchyNode]) -> Vec<String> {
    fn visit(node: &HierarchyNode, out: &mut Vec<String>) {
        out.push(format!("{:?}:{}", node.kind, node.name));
        for child in &node.children {
            visit(child, out);
        }
    }

    let mut out = Vec::new();
    for node in nodes {
        visit(node, &mut out);
    }
    out
}

fn temp_root(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("squeezy-{name}-{nonce}"));
    fs::create_dir_all(&root).unwrap();
    root
}
