# Test Layout

Squeezy keeps unit tests in separate sibling files so implementation files stay focused.

For a source file:

```text
src/foo.rs
```

write unit tests in:

```text
src/foo_tests.rs
```

and declare the test module from `foo.rs`:

```rust
#[cfg(test)]
#[path = "foo_tests.rs"]
mod tests;
```

Rules enforced by `python3 scripts/check_test_layout.py`:

- no inline `mod tests { ... }` blocks in `src/`;
- no file literally named `tests.rs` in `src/`;
- no file literally named `mod.rs` in `src/`;
- every `*_tests.rs` file has a sibling source file;
- every source file with a sibling `*_tests.rs` declares it with both `#[cfg(test)]` and `#[path = "<module>_tests.rs"]`.

## Unit vs. integration tests

The `src/<module>.rs` + `src/<module>_tests.rs` pair is for **unit tests** that
need access to crate-private items (private functions, fields, constructors,
test seams). Use it only when the source file has real production code; do not
create an empty `<module>.rs` just to satisfy the layout check.

**Integration tests** live under the crate's own `tests/` directory:

```text
crates/<crate>/tests/<scenario>.rs
```

Each `tests/*.rs` file is compiled as a separate binary that depends on the
crate's public API only. Use this layout when the test scenario is naturally
end-to-end (driving the public surface), spans multiple modules, or has no
single source-file owner. There is no requirement to add a sibling source
file under `src/`.

Pick by access:

- The test reads or constructs crate-private items → unit test in
  `src/<module>_tests.rs`.
- The test only uses the crate's public API → integration test in
  `tests/<scenario>.rs`.

Workspace-level `tests/` directories continue to host cross-crate
integration suites.

## Language Modules

Language-specific parser and graph code should use the family registry pattern:

```text
crates/squeezy-parse/src/backend.rs
crates/squeezy-graph/src/backend.rs
```

Each supported `LanguageFamily` must have exactly one parse backend and one
graph extension. Registry tests under crate-level `tests/registry.rs` enforce
that adding a new family does not silently skip parser or graph support.

Semantic-graph benchmark code follows the same ownership boundary:

```text
benchmarks/squeezy-graph-bench/src/oracles.rs
benchmarks/squeezy-graph-bench/src/oracles/<oracle>.rs
benchmarks/squeezy-graph-bench/src/summary.rs
```

Oracle modules are named after the validator (`clang`, `common_scan`,
`cpython_ast`, `dart_oracle`, `go_types`, `javac`, `kotlin_oracle`,
`php_oracle`, `roslyn`, `ruby_oracle`, `rust_analyzer`, `scala_semanticdb`,
`swift_sourcekit`, `tsc`) because one oracle can own multiple `LanguageKind`
variants. Fixture and query-spec data lives outside the
Rust crate under `benchmarks/fixtures/` and `benchmarks/specs/`; agent-driven
graph/no-graph benchmark scenarios live under
`crates/squeezy-eval/fixtures/scenarios/benchmarks/`.
