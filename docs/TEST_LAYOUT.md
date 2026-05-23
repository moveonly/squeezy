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
