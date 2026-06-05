# Scala SemanticDB oracle

Reads `.semanticdb` protobufs emitted by `scalac -Xsemanticdb` and surfaces
the `(file, kind, name)` rows the squeezy bench oracle compares against the
tree-sitter extractor.

## Pieces

| Path | Role |
|------|------|
| `run_oracle.sh` | Spawns scalac (or sbt / mill when their build files exist) and writes `.semanticdb` files into a scratch directory. Invoked from the Rust harness in [`../../squeezy-graph-bench/src/oracles/scala_semanticdb.rs`](../../squeezy-graph-bench/src/oracles/scala_semanticdb.rs). |
| `scala-oracle.sc` | Optional scala-cli driver retained as a reference. The bench no longer depends on it because the Rust harness parses the protobufs directly, so JVM-side scala-cli is not required. |

## Modes

The Rust harness picks one of two modes per run:

1. **SemanticDB mode** (preferred). Runs when `scalac` is on `$PATH`. The
   harness invokes `run_oracle.sh`, walks the SemanticDB scratch directory
   for `*.semanticdb` files, and decodes the `TextDocuments` protobuf in
   pure Rust (no `protoc` / `prost-build` dependency). Precision and recall
   are gated at P>=0.90, R>=0.75.
2. **Scan-only fallback**. Runs when `scalac` is missing or its invocation
   fails. The oracle returns an empty SemanticDB scan, the gate check is
   suppressed (status starts with `scan-only-fallback`), and fixture query
   gates continue to run unaffected.

## Installing scalac locally

- Coursier (preferred, matches CI):
  ```bash
  curl -fL https://github.com/coursier/launchers/raw/master/cs-x86_64-apple-darwin.gz \
    | gunzip > cs
  chmod +x cs
  sudo mv cs /usr/local/bin/cs
  cs install scala3-compiler
  ```
- Homebrew (macOS only, slower toolchain refresh):
  ```bash
  brew install scala
  ```

After install, run the smoke bench to verify the SemanticDB path is active:

```bash
cargo run --release --manifest-path benchmarks/squeezy-graph-bench/Cargo.toml -- \
  --corpus benchmarks/corpus.json --family scala --tier smoke
```

The resulting report has `scala_oracle.status` beginning with
`SemanticDB oracle succeeded` when the protobuf reader fired.

## Limitations

See [`docs/internal/lang-specs/scala.md`](../../../docs/internal/lang-specs/scala.md)
for the Scala graph contract. Key oracle comparison exclusions:

- Implicit-conversion injection at call sites
- `given` / `using` resolution at call sites
- Macro-expanded synthetic members and anonymous classes (`$anon`, `$$anonfun`)
- Local `val` / `var` (LOCAL kind), parameters, type parameters, bare fields
- Path-dependent type references (`a.B`) — references only, no resolution edge
