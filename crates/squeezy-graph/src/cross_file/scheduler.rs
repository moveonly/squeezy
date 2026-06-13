//! File-level import graph plus Tarjan SCC + topological levels.
//!
//! The phased resolver needs to process each file *after* every file it
//! imports (so cross-file lookups have something to look at) and to bound
//! the fixpoint iteration *inside* each strongly-connected component (so
//! `pub use` / `export * from` cycles terminate). This module produces
//! that schedule from an [`ImportGraph`] of `Local`-resolved import
//! edges.
//!
//! The schedule is computed and the data structures are ready. The flip
//! that makes the phased resolver consume this schedule (replacing the
//! single-pass [`crate::resolution::SemanticGraph::resolve_call`]) is the
//! next planned step. Until then, this module is pure data with no active
//! call-resolution consumer.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use squeezy_core::FileId;

/// Maximum fixpoint iterations inside one strongly-connected component.
/// `pub use` / `export * from` cycles can expose new bindings on each
/// pass; in practice they converge in 3-5 iterations and a hard cap
/// prevents pathological corner cases from spinning forever.
pub const SCC_FIXPOINT_MAX_ITERATIONS: u32 = 64;

/// Directed graph of resolved file-to-file import edges. Internally
/// `HashMap`/`HashSet` because `squeezy_core::FileId` is not `Ord`;
/// iteration order is normalised to sorted-by-id wherever the output
/// reaches the schedule.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportGraph {
    edges: HashMap<FileId, HashSet<FileId>>,
    nodes: HashSet<FileId>,
}

impl ImportGraph {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_edge(&mut self, from: FileId, to: FileId) {
        self.nodes.insert(from.clone());
        self.nodes.insert(to.clone());
        self.edges.entry(from).or_default().insert(to);
    }

    pub fn add_node(&mut self, node: FileId) {
        self.nodes.insert(node);
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    fn sorted_nodes(&self) -> Vec<FileId> {
        let mut nodes: Vec<FileId> = self.nodes.iter().cloned().collect();
        nodes.sort_by(|left, right| left.0.cmp(&right.0));
        nodes
    }

    fn sorted_successors(&self, node: &FileId) -> Vec<FileId> {
        let mut succs: Vec<FileId> = self
            .edges
            .get(node)
            .into_iter()
            .flat_map(|set| set.iter().cloned())
            .collect();
        succs.sort_by(|left, right| left.0.cmp(&right.0));
        succs
    }
}

/// One strongly-connected component of an [`ImportGraph`]. `files` is
/// sorted; `is_cyclic` is `true` iff the SCC spans more than one file
/// (single-file SCCs with self-loops are intentionally not flagged —
/// they don't change cross-file ordering).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scc {
    pub id: u32,
    pub files: Vec<FileId>,
    pub is_cyclic: bool,
}

/// Tarjan SCC + topological levelling result. The resolver processes
/// `levels[0]` first (leaf SCCs that import nothing within the project),
/// then `levels[1]`, …; SCCs within the same level are independent and
/// can run in parallel.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schedule {
    pub sccs: Vec<Scc>,
    pub levels: Vec<Vec<u32>>,
}

/// Compute the [`Schedule`] for `graph`. Empty graph yields an empty
/// `Schedule`.
pub fn schedule(graph: &ImportGraph) -> Schedule {
    if graph.node_count() == 0 {
        return Schedule::default();
    }
    let (sccs, file_to_scc) = tarjan_sccs(graph);
    let levels = topological_levels(&sccs, &file_to_scc, graph);
    Schedule { sccs, levels }
}

fn tarjan_sccs(graph: &ImportGraph) -> (Vec<Scc>, HashMap<FileId, u32>) {
    let node_count = graph.node_count();
    let mut state = TarjanState {
        index_counter: 0,
        stack: Vec::with_capacity(node_count),
        on_stack: HashSet::with_capacity(node_count),
        indices: HashMap::with_capacity(node_count),
        lowlinks: HashMap::with_capacity(node_count),
        sccs: Vec::with_capacity(node_count),
    };
    for node in graph.sorted_nodes() {
        if !state.indices.contains_key(&node) {
            tarjan_visit(&node, graph, &mut state);
        }
    }
    let mut file_to_scc = HashMap::with_capacity(node_count);
    for scc in &state.sccs {
        for file in &scc.files {
            file_to_scc.insert(file.clone(), scc.id);
        }
    }
    (state.sccs, file_to_scc)
}

struct TarjanState {
    index_counter: u32,
    stack: Vec<FileId>,
    on_stack: HashSet<FileId>,
    indices: HashMap<FileId, u32>,
    lowlinks: HashMap<FileId, u32>,
    sccs: Vec<Scc>,
}

/// One frame of the explicit Tarjan work stack, simulating a single
/// recursive `tarjan_visit` call. `successors` is the (already sorted)
/// successor list captured on entry; `cursor` is the index of the next
/// successor to examine, so the frame can resume after a child frame
/// returns.
struct TarjanFrame {
    node: FileId,
    successors: Vec<FileId>,
    cursor: usize,
}

/// Iterative Tarjan SCC visit. Equivalent to the textbook recursive
/// `tarjan_visit`, but pushes per-call frames onto a heap-allocated
/// work stack instead of recursing, so traversal depth is bounded by
/// available heap rather than the native thread stack. Deep import
/// chains (file N imports file N+1 for large N) therefore cannot
/// overflow the stack. Output ordering is identical because
/// `sorted_successors` already normalises traversal order.
fn tarjan_visit(start: &FileId, graph: &ImportGraph, state: &mut TarjanState) {
    let mut frames: Vec<TarjanFrame> = Vec::new();

    // Push the initial frame (equivalent to entering `tarjan_visit(start)`).
    state.indices.insert(start.clone(), state.index_counter);
    state.lowlinks.insert(start.clone(), state.index_counter);
    state.index_counter += 1;
    state.stack.push(start.clone());
    state.on_stack.insert(start.clone());
    frames.push(TarjanFrame {
        node: start.clone(),
        successors: graph.sorted_successors(start),
        cursor: 0,
    });

    // Indexed (rather than `last_mut`) access into `frames` so the work
    // stack can be pushed/popped without holding a borrow across the
    // mutation.
    while let Some(top) = frames.len().checked_sub(1) {
        let frame = &mut frames[top];
        if frame.cursor < frame.successors.len() {
            let succ = frame.successors[frame.cursor].clone();
            frame.cursor += 1;
            let node = frame.node.clone();
            if !state.indices.contains_key(&succ) {
                // Descend into `succ` (equivalent to the recursive call).
                // The lowlink fold of the child into this parent happens
                // when the child frame returns (see the pop branch below).
                state.indices.insert(succ.clone(), state.index_counter);
                state.lowlinks.insert(succ.clone(), state.index_counter);
                state.index_counter += 1;
                state.stack.push(succ.clone());
                state.on_stack.insert(succ.clone());
                frames.push(TarjanFrame {
                    node: succ.clone(),
                    successors: graph.sorted_successors(&succ),
                    cursor: 0,
                });
            } else if state.on_stack.contains(&succ) {
                let succ_idx = state.indices[&succ];
                let node_low = state.lowlinks[&node];
                state.lowlinks.insert(node, node_low.min(succ_idx));
            }
            continue;
        }

        // All successors of this frame's node have been processed; this
        // is the point where the recursive call would return. Emit an SCC
        // if the node is a root, then fold its lowlink into its parent.
        let node = frame.node.clone();
        if state.lowlinks[&node] == state.indices[&node] {
            let mut files = Vec::new();
            while let Some(top) = state.stack.pop() {
                state.on_stack.remove(&top);
                let done = top == node;
                files.push(top);
                if done {
                    break;
                }
            }
            files.sort_by(|left, right| left.0.cmp(&right.0));
            let id = u32::try_from(state.sccs.len()).unwrap_or(u32::MAX);
            let is_cyclic = files.len() > 1;
            state.sccs.push(Scc {
                id,
                files,
                is_cyclic,
            });
        }
        frames.pop();
        if let Some(parent) = frames.last() {
            let node_low = state.lowlinks[&node];
            let parent_low = state.lowlinks[&parent.node];
            state
                .lowlinks
                .insert(parent.node.clone(), parent_low.min(node_low));
        }
    }
}

fn topological_levels(
    sccs: &[Scc],
    file_to_scc: &HashMap<FileId, u32>,
    graph: &ImportGraph,
) -> Vec<Vec<u32>> {
    let mut out_edges: HashMap<u32, HashSet<u32>> = HashMap::new();
    for scc in sccs {
        out_edges.entry(scc.id).or_default();
    }
    for from in graph.sorted_nodes() {
        let Some(&from_scc) = file_to_scc.get(&from) else {
            continue;
        };
        for to in graph.sorted_successors(&from) {
            let Some(&to_scc) = file_to_scc.get(&to) else {
                continue;
            };
            if from_scc != to_scc {
                out_edges.entry(from_scc).or_default().insert(to_scc);
            }
        }
    }
    // Kahn's algorithm peeling leaves of the *reverse* condensation: a
    // node becomes a leaf once every successor (i.e. every SCC it imports
    // from) has already been emitted. Result: `levels[0]` are the SCCs
    // that depend on nothing in the project; `levels[k]` depend only on
    // earlier levels.
    let mut remaining = out_edges;
    let mut levels: Vec<Vec<u32>> = Vec::new();
    let mut emitted: HashSet<u32> = HashSet::new();
    loop {
        let mut level: Vec<u32> = remaining
            .iter()
            .filter(|(_, succs)| succs.is_empty())
            .map(|(id, _)| *id)
            .filter(|id| !emitted.contains(id))
            .collect();
        if level.is_empty() {
            break;
        }
        level.sort_unstable();
        for id in &level {
            emitted.insert(*id);
            remaining.remove(id);
        }
        for succs in remaining.values_mut() {
            for id in &level {
                succs.remove(id);
            }
        }
        levels.push(level);
    }
    levels
}

#[cfg(test)]
#[path = "scheduler_tests.rs"]
mod tests;
