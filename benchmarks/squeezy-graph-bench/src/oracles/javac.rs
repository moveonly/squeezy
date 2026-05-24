use std::{fs, path::Path, process::Command, time::Instant};

use serde::Deserialize;
use squeezy_core::{Result, SqueezyError};
use squeezy_graph::SemanticGraph;

use crate::{
    accuracy::{compare_symbol_sets, increment_symbol, ratio, symbol_count},
    oracles::common_scan::{collect_squeezy_symbol_scan, default_oracle_exclusions},
    oracles::rust_analyzer::normalize_symbol_name,
    report::{JavaOracleReport, QueryOracleReport, QueryReport, SymbolKey, SymbolScan},
    util::{command_exists, increment, temp_dir},
};

pub(crate) fn time_java_oracle_optional(root: &Path) -> (u128, String) {
    if !command_exists("java") {
        return (0, "skipped: java not found".to_string());
    }
    let started = Instant::now();
    match collect_java_compiler_tree_symbol_scan(root) {
        Ok((_, status)) if status.starts_with("JDK compiler tree oracle succeeded") => {
            (started.elapsed().as_millis(), status)
        }
        Ok((_, status)) => (0, format!("skipped: {status}")),
        Err(err) => (0, format!("skipped: Java oracle failed: {err}")),
    }
}

pub(crate) fn collect_java_oracle_accuracy(
    root: &Path,
    graph: &SemanticGraph,
    queries: &[QueryReport],
) -> Result<JavaOracleReport> {
    if !command_exists("java") {
        return Ok(JavaOracleReport {
            oracle_ms: None,
            status: "skipped: java not found".to_string(),
            symbols: compare_symbol_sets(
                &collect_squeezy_symbol_scan(graph),
                &SymbolScan::default(),
            ),
            navigation: collect_query_oracle_accuracy(queries),
            limitations: java_oracle_limitations(),
        });
    }
    let started = Instant::now();
    match collect_java_compiler_tree_symbol_scan(root) {
        Ok((oracle, status)) if status.starts_with("JDK compiler tree oracle succeeded") => {
            let oracle_ms = started.elapsed().as_millis();
            let squeezy_symbols = collect_squeezy_symbol_scan(graph);
            Ok(JavaOracleReport {
                oracle_ms: Some(oracle_ms),
                status,
                symbols: compare_symbol_sets(&squeezy_symbols, &oracle),
                navigation: collect_query_oracle_accuracy(queries),
                limitations: java_oracle_limitations(),
            })
        }
        Ok((_, status)) => Ok(JavaOracleReport {
            oracle_ms: None,
            status: format!("skipped: {status}"),
            symbols: compare_symbol_sets(
                &collect_squeezy_symbol_scan(graph),
                &SymbolScan::default(),
            ),
            navigation: collect_query_oracle_accuracy(queries),
            limitations: java_oracle_limitations(),
        }),
        Err(err) => Ok(JavaOracleReport {
            oracle_ms: None,
            status: format!("skipped: Java oracle failed: {err}"),
            symbols: compare_symbol_sets(
                &collect_squeezy_symbol_scan(graph),
                &SymbolScan::default(),
            ),
            navigation: collect_query_oracle_accuracy(queries),
            limitations: java_oracle_limitations(),
        }),
    }
}

pub(crate) fn collect_query_oracle_accuracy(queries: &[QueryReport]) -> QueryOracleReport {
    let true_positive = queries
        .iter()
        .map(|query| {
            query
                .expected_contains
                .iter()
                .filter(|expected| query.actual.contains(expected))
                .count()
        })
        .sum::<usize>();
    let false_negative = queries
        .iter()
        .map(|query| query.missing.len())
        .sum::<usize>();
    // Query specs use expected_contains, not an exhaustive expected set, so
    // extra results stay visible on each query but are not counted as oracle FP.
    let false_positive = 0;
    QueryOracleReport {
        status: "fixture query truth (minimum expected_contains oracle)".to_string(),
        query_count: queries.len(),
        true_positive,
        false_positive,
        false_negative,
        precision: ratio(true_positive, true_positive + false_positive),
        recall: ratio(true_positive, true_positive + false_negative),
    }
}

pub(crate) fn java_oracle_limitations() -> Vec<String> {
    vec![
        "The Java oracle uses the JDK compiler tree API for declarations only and does not require successful type attribution.".to_string(),
        "Symbol comparison is file/name/kind based; overload resolution, dispatch, generated sources, annotation processors, and external libraries remain separate navigation-loss areas.".to_string(),
        "If java or a JDK compiler is unavailable, the oracle is skipped while fixture query gates still run.".to_string(),
    ]
}

#[derive(Debug, Deserialize)]
pub(crate) struct JavaOracleOutput {
    rows: Vec<[String; 3]>,
}

pub(crate) fn collect_java_compiler_tree_symbol_scan(root: &Path) -> Result<(SymbolScan, String)> {
    let exclusions = default_oracle_exclusions(root)?;
    let temp = temp_dir("squeezy-java-oracle")?;
    let oracle_path = temp.join("JavaOracle.java");
    fs::write(&oracle_path, JAVA_COMPILER_TREE_ORACLE)?;
    let output = Command::new("java")
        .arg(&oracle_path)
        .arg(root)
        .output()
        .map_err(|err| SqueezyError::Graph(format!("failed to run Java oracle: {err}")))?;
    if !output.status.success() {
        return Ok((
            SymbolScan::default(),
            format!(
                "Java oracle unavailable: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    let output: JavaOracleOutput = serde_json::from_slice(&output.stdout)
        .map_err(|err| SqueezyError::Graph(format!("invalid Java oracle JSON: {err}")))?;
    let mut scan = SymbolScan::default();
    for [file, kind, name] in output.rows {
        scan.raw_total += 1;
        if exclusions.excludes(&file) {
            increment(&mut scan.excluded_by_kind, "ExcludedPath");
            continue;
        }
        increment_symbol(
            &mut scan.counts,
            SymbolKey {
                file,
                kind,
                name: normalize_symbol_name(&name),
            },
        );
    }
    Ok((
        scan.clone(),
        format!(
            "JDK compiler tree oracle succeeded with {} declaration symbols",
            symbol_count(&scan.counts)
        ),
    ))
}

const JAVA_COMPILER_TREE_ORACLE: &str = r#"
import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayDeque;
import java.util.ArrayList;
import java.util.Comparator;
import java.util.List;
import javax.tools.JavaCompiler;
import javax.tools.StandardJavaFileManager;
import javax.tools.ToolProvider;
import com.sun.source.tree.ClassTree;
import com.sun.source.tree.CompilationUnitTree;
import com.sun.source.tree.MethodTree;
import com.sun.source.tree.Tree;
import com.sun.source.util.JavacTask;
import com.sun.source.util.TreeScanner;

public class JavaOracle {
  record Row(String file, String kind, String name) {}

  public static void main(String[] args) throws Exception {
    JavaCompiler compiler = ToolProvider.getSystemJavaCompiler();
    if (compiler == null) {
      System.err.println("JDK compiler is not available");
      System.exit(2);
    }
    Path root = Path.of(args[0]).toAbsolutePath().normalize();
    List<Path> files = Files.walk(root)
      .filter(path -> path.toString().endsWith(".java"))
      .sorted()
      .toList();
    List<Row> rows = new ArrayList<>();
    try (StandardJavaFileManager manager = compiler.getStandardFileManager(null, null, StandardCharsets.UTF_8)) {
      Iterable units = manager.getJavaFileObjectsFromPaths(files);
      JavacTask task = (JavacTask) compiler.getTask(null, manager, null, List.of("-proc:none"), null, units);
      for (CompilationUnitTree unit : task.parse()) {
        String rel = root.relativize(Path.of(unit.getSourceFile().toUri()).toAbsolutePath().normalize()).toString().replace('\\', '/');
        new Scanner(rel, rows).scan(unit, null);
      }
    }
    rows.sort(Comparator.comparing(Row::file).thenComparing(Row::kind).thenComparing(Row::name));
    StringBuilder out = new StringBuilder();
    out.append("{\"rows\":[");
    for (int i = 0; i < rows.size(); i++) {
      Row row = rows.get(i);
      if (i > 0) out.append(',');
      out.append("[\"").append(escape(row.file())).append("\",\"")
        .append(escape(row.kind())).append("\",\"")
        .append(escape(row.name())).append("\"]");
    }
    out.append("]}");
    System.out.println(out);
  }

  static class Scanner extends TreeScanner<Void, Void> {
    private final String file;
    private final List<Row> rows;
    private final ArrayDeque<String> classes = new ArrayDeque<>();

    Scanner(String file, List<Row> rows) {
      this.file = file;
      this.rows = rows;
    }

    @Override
    public Void visitClass(ClassTree node, Void unused) {
      String kind = switch (node.getKind()) {
        case CLASS -> "Class";
        case INTERFACE, ANNOTATION_TYPE -> "Trait";
        case ENUM -> "Enum";
        case RECORD -> "Struct";
        default -> "Class";
      };
      String name = node.getSimpleName().toString();
      if (name.isEmpty()) {
        return super.visitClass(node, unused);
      }
      rows.add(new Row(file, kind, name));
      classes.push(name);
      super.visitClass(node, unused);
      classes.pop();
      return null;
    }

    @Override
    public Void visitMethod(MethodTree node, Void unused) {
      String name = node.getName().toString();
      if ("<init>".equals(name) && !classes.isEmpty()) {
        name = classes.peek();
      }
      rows.add(new Row(file, "Method", name));
      return super.visitMethod(node, unused);
    }
  }

  static String escape(String value) {
    return value.replace("\\", "\\\\").replace("\"", "\\\"");
  }
}
"#;
