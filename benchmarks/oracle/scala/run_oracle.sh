#!/usr/bin/env bash
# benchmarks/oracle/scala/run_oracle.sh
#
# Drives `scalac -Xsemanticdb` for the Scala oracle. Invoked from the Rust
# bench harness (`benchmarks/squeezy-graph-bench/src/oracles/scala_semanticdb.rs`)
# with three positional arguments:
#
#   $1  source root (directory containing the Scala fixture/repo)
#   $2  SemanticDB output directory (will be populated with
#       `META-INF/semanticdb/.../*.semanticdb` per source file)
#   $3  scratch classes directory (scalac requires a -d target even though
#       the bench only consumes the protobufs)
#
# The helper prefers `sbt`/`mill` when their build files are present so
# multi-module fixtures resolve their dependencies the same way they do
# under `metals`; otherwise it falls back to a bare `scalac` over every
# `.scala` file under the root. Vendored / generated trees are excluded
# so scalac does not choke on partial sources.

set -euo pipefail

if [[ $# -lt 3 ]]; then
  echo "usage: $0 <source-root> <semanticdb-target> <classes-target>" >&2
  exit 2
fi

ROOT="$1"
SDB_TARGET="$2"
CLASSES_TARGET="$3"

mkdir -p "$SDB_TARGET" "$CLASSES_TARGET"

if [[ -f "$ROOT/build.sbt" ]] && command -v sbt >/dev/null 2>&1; then
  # `sbt semanticdbEnabled := true` is the documented way to surface the
  # protobufs through sbt's incremental layer. We funnel the output into
  # the bench-managed scratch dir so the cache key remains stable.
  cd "$ROOT"
  sbt -Dsbt.log.noformat=true \
    "set semanticdbEnabled := true" \
    "set semanticdbTargetRoot := file(\"$SDB_TARGET\")" \
    compile
  exit $?
fi

if [[ -f "$ROOT/build.sc" ]] && command -v mill >/dev/null 2>&1; then
  cd "$ROOT"
  mill -i __.compile
  # Mill emits `.semanticdb` inside `out/<module>/compile.dest/...` — copy
  # them into the bench scratch dir so the Rust walker finds them at the
  # same path as the scalac fallback below.
  find out -name '*.semanticdb' -type f -print0 \
    | xargs -0 -I{} cp --parents {} "$SDB_TARGET"
  exit $?
fi

# Bare scalac fallback. Collect every `.scala` source under the root,
# skipping vendored / generated trees that may have unsatisfiable imports.
# Sources must be relative to the root so scalac honours -semanticdb-target
# rather than writing siblings next to the source files.
cd "$ROOT"
SOURCES=$(find . \
  -type d \( -name vendor -o -name generated -o -name target -o -name out -o -name build -o -name node_modules \) -prune \
  -o -type f -name '*.scala' -print \
  | sort)

if [[ -z "$SOURCES" ]]; then
  # No Scala sources to compile — emit an empty target directory so the
  # Rust walker terminates with an empty document set.
  exit 0
fi

# shellcheck disable=SC2086
scalac \
  -Xsemanticdb \
  -semanticdb-target "$SDB_TARGET" \
  -d "$CLASSES_TARGET" \
  $SOURCES
