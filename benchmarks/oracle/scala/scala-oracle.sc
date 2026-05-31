//> using scala 3.5.0
//> using dep org.scalameta:semanticdb-shared_3:4.9.9

// Scala SemanticDB oracle driver — invoked by
// `benchmarks/squeezy-graph-bench/src/oracles/scala_semanticdb.rs` once
// scalac has emitted `.semanticdb` protobufs alongside compiled classes.
// Reads every `.semanticdb` file under the first argument and prints a JSON
// object `{"rows": [["<rel_path>", "<kind>", "<displayName>"], ...]}` that
// the Rust oracle parses into the shared `SymbolScan` shape.
//
// NOTE(scala): the Rust-side runner currently skips the oracle until the
// `scalac -Xsemanticdb` + scala-cli invocation is wired through. Until then
// this script is informational — see docs/internal/lang-specs/scala.md §9
// for the full plan, including the proto deserialisation and the symbol
// kind mapping.

import scala.meta.internal.semanticdb._
import java.nio.file._
import scala.jdk.CollectionConverters._

@main def run(sdbDir: String, rootDir: String): Unit = {
  val root = Paths.get(rootDir).toAbsolutePath.normalize
  val rows = scala.collection.mutable.ArrayBuffer.empty[(String, String, String)]
  Files.walk(Paths.get(sdbDir)).iterator.asScala
    .filter(_.toString.endsWith(".semanticdb"))
    .foreach { p =>
      val doc = TextDocuments.parseFrom(Files.readAllBytes(p)).documents.head
      val relSource = root.relativize(Paths.get(doc.uri)).toString.replace('\\', '/')
      doc.symbols.foreach { si =>
        val kind = si.kind match {
          case SymbolInformation.Kind.CLASS  => "Class"
          case SymbolInformation.Kind.TRAIT  => "Trait"
          case SymbolInformation.Kind.OBJECT => "Class"
          case SymbolInformation.Kind.METHOD => "Method"
          case SymbolInformation.Kind.MACRO  => "Method"
          case SymbolInformation.Kind.TYPE   => "TypeAlias"
          case SymbolInformation.Kind.FIELD  => "Field"
          case SymbolInformation.Kind.LOCAL  => "_skip"
          case _                              => "_skip"
        }
        if (kind != "_skip") rows += ((relSource, kind, si.displayName))
      }
    }
  print(rows.distinct.map { case (f, k, n) =>
    s"""["$f","$k","${n.replace("\\", "\\\\").replace("\"", "\\\"")}"]"""
  }.mkString("""{"rows":[""", ",", "]}"))
}
