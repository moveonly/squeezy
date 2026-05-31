#!/usr/bin/env bash
# Build the squeezy Kotlin oracle into a self-contained fat jar.
#
# Requirements:
#   - JDK 17+ on PATH
#   - kotlinc 1.9+ on PATH (the kotlin-compiler-embeddable that ships with
#     kotlinc satisfies the PSI imports KotlinOracle.kt depends on)
#
# Output: kotlin-oracle.jar next to this script. The Rust harness
# (`benchmarks/squeezy-graph-bench/src/oracles/kotlin_oracle.rs`) looks for
# the jar at `benchmarks/oracle/kotlin/kotlin-oracle.jar` relative to the
# repo root; do not move it without updating that constant.
#
# CI: build-oracle-jar step in `.github/workflows/benchmark-lang.yml`
# invokes this script before the benchmark runs. Locally, run it once after
# editing KotlinOracle.kt.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$HERE"

if ! command -v kotlinc >/dev/null 2>&1; then
    echo "kotlinc not found on PATH" >&2
    echo "Install kotlinc 1.9+ (e.g. https://kotlinlang.org/docs/command-line.html) and retry." >&2
    exit 2
fi

if ! command -v java >/dev/null 2>&1; then
    echo "java not found on PATH" >&2
    exit 2
fi

# `-include-runtime` bundles the Kotlin stdlib. KotlinOracle.kt also depends on
# `org.jetbrains.kotlin.cli.*` / `org.jetbrains.kotlin.psi.*`, which live in
# `kotlin-compiler-embeddable.jar`. kotlinc does NOT add it to the classpath
# automatically when invoked directly, so we resolve it from the kotlinc
# install (one level up from the kotlinc launcher) and pass it explicitly.
KOTLINC_HOME="$(dirname "$(dirname "$(readlink -f "$(command -v kotlinc)")")")"
# The standalone kotlinc distribution ships `kotlin-compiler.jar`; the
# `*-embeddable` variant is published as a separate Maven artifact for tools
# that need the intellij-shaded classes. Either jar exposes the
# `org.jetbrains.kotlin.cli.*` and `.psi.*` symbols KotlinOracle.kt imports,
# so accept whichever is present. Avoid `ls A B | head -1` because under
# `set -euo pipefail`, `ls` exits non-zero when any argument is missing and
# the pipefail propagation aborts the script even though one match exists.
COMPILER_JAR=""
for candidate in \
    "$KOTLINC_HOME/lib/kotlin-compiler-embeddable.jar" \
    "$KOTLINC_HOME/lib/kotlin-compiler.jar"; do
    if [ -f "$candidate" ]; then
        COMPILER_JAR="$candidate"
        break
    fi
done
if [ -z "$COMPILER_JAR" ]; then
    echo "kotlin-compiler[-embeddable].jar not found under $KOTLINC_HOME/lib" >&2
    ls "$KOTLINC_HOME/lib" 2>/dev/null | head -20 >&2 || true
    exit 2
fi

echo "Building Kotlin oracle jar with $(kotlinc -version 2>&1)..."
kotlinc -include-runtime -cp "$COMPILER_JAR" -d kotlin-oracle.jar KotlinOracle.kt

echo "Wrote $(pwd)/kotlin-oracle.jar"
