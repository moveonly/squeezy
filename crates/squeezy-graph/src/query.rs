use std::collections::HashSet;

use squeezy_core::{Confidence, FileId, Freshness, SourceSpan, SymbolId, SymbolKind};
use squeezy_parse::{BodyHit, BodyHitKind, ParsedReference};

use crate::{GraphEdge, GraphSymbol};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HierarchyNode {
    pub id: SymbolId,
    pub name: String,
    pub kind: SymbolKind,
    pub span: SourceSpan,
    pub freshness: Freshness,
    pub children: Vec<HierarchyNode>,
}

/// Result of `SemanticGraph::compute_impact`: files, symbols, and test symbols
/// reachable from a set of changed files through reverse-import propagation.
#[derive(Debug, Clone, Default)]
pub struct ImpactSet {
    pub affected_files: HashSet<FileId>,
    pub affected_symbols: Vec<GraphSymbol>,
    pub affected_tests: Vec<GraphSymbol>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureQuery {
    pub text: String,
    pub kind: Option<SymbolKind>,
    pub visibility: Option<String>,
    pub attribute: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodySearchQuery {
    pub text: String,
    pub owner_kind: Option<SymbolKind>,
    pub hit_kind: Option<BodyHitKind>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodySearchHit {
    pub owner: Option<GraphSymbol>,
    pub hit: BodyHit,
    pub confidence: Confidence,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferenceHit {
    pub owner: Option<GraphSymbol>,
    pub reference: ParsedReference,
    pub confidence: Confidence,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallEdgeHit {
    pub caller: Option<GraphSymbol>,
    pub callee: Option<GraphSymbol>,
    pub edge: GraphEdge,
}
