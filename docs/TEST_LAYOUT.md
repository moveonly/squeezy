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

Integration tests may still live under crate-level or workspace-level `tests/` directories.

## Language Modules

Language-specific parser and graph code should use the family registry pattern:

```text
crates/squeezy-parse/src/backend.rs
crates/squeezy-graph/src/backend.rs
```

Each supported `LanguageFamily` must have exactly one parse backend and one
graph extension. Registry tests under crate-level `tests/registry.rs` enforce
that adding a new family does not silently skip parser or graph support.

Benchmark code follows the same boundary:

```text
benchmarks/squeezy-graph-bench/src/oracles/<oracle>.rs
benchmarks/squeezy-graph-bench/src/summary/<family>.rs
```

Oracle modules are named after the validator (`rust_analyzer`, `clang`,
`roslyn`, `javac`, `cpython_ast`, `go_types`, `tsc`) because one oracle can own
multiple `LanguageKind` variants.
