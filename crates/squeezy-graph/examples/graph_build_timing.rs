//! Standalone timing harness for `GraphManager::open`: builds the semantic
//! graph for a workspace and prints whether the in-memory build completes and
//! how long it took. Used to confirm large hierarchical Dart workspaces
//! (flutter/flutter) build in bounded time instead of blowing up in
//! cross-file call resolution.
//!
//! Usage: cargo run -p squeezy-graph --example graph_build_timing -- <path>
use std::time::Instant;

fn main() {
    let root = std::env::args()
        .nth(1)
        .expect("usage: graph_build_timing <workspace-root>");
    let started = Instant::now();
    let result = squeezy_graph::GraphManager::open(&root);
    let elapsed = started.elapsed();
    match result {
        Ok(manager) => {
            let report = manager.build_report();
            // symbols+edges are a determinism fingerprint: serial vs parallel
            // resolution must produce identical counts.
            println!(
                "OK   build={:.2}s  symbols={} edges={}  root={root}",
                elapsed.as_secs_f64(),
                report.stats.symbols,
                report.stats.edges
            );
        }
        Err(err) => println!(
            "ERR  build={:.2}s  root={root}  error={err}",
            elapsed.as_secs_f64()
        ),
    }
}
