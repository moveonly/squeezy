use std::path::Path;

use serde::Deserialize;
use squeezy_core::{Confidence, Provenance, SourceSpan};

use crate::{ToolCall, ToolResult};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReadSliceArgs {
    pub(crate) path: Option<String>,
    pub(crate) symbol_id: Option<String>,
    pub(crate) span_kind: Option<ReadSliceSpanKind>,
    pub(crate) read_mode: Option<ReadSliceReadMode>,
    pub(crate) diff_baseline: Option<DiffReadBaseline>,
    pub(crate) max_ranges: Option<usize>,
    pub(crate) start_byte: Option<usize>,
    pub(crate) end_byte: Option<usize>,
    pub(crate) start_line: Option<u32>,
    pub(crate) end_line: Option<u32>,
    pub(crate) context_lines: Option<u32>,
    pub(crate) offset: Option<usize>,
    pub(crate) limit: Option<usize>,
    pub(crate) diff_only: Option<bool>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReadSliceReadMode {
    #[default]
    Slice,
    Diff,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DiffReadBaseline {
    #[default]
    Worktree,
    #[serde(alias = "branch")]
    BranchBase,
    Index,
    LastReceipt,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReadSliceSpanKind {
    #[default]
    Signature,
    Body,
}

pub(crate) fn diff_read_baseline_str(baseline: DiffReadBaseline) -> &'static str {
    match baseline {
        DiffReadBaseline::Worktree => "worktree",
        DiffReadBaseline::BranchBase => "branch_base",
        DiffReadBaseline::Index => "index",
        DiffReadBaseline::LastReceipt => "last_receipt",
    }
}

pub(crate) enum LastReceiptDiffOutcome {
    Result(Box<ToolResult>),
    Fallback(&'static str),
}

pub(crate) struct ReadSliceDiffCtx<'a> {
    pub(crate) call: &'a ToolCall,
    pub(crate) args: &'a ReadSliceArgs,
    pub(crate) path: &'a Path,
    pub(crate) rel: &'a str,
    pub(crate) graph_available: bool,
    pub(crate) graph_status: &'static str,
    pub(crate) confidence: Confidence,
    pub(crate) provenance: Vec<Provenance>,
    pub(crate) span: Option<SourceSpan>,
    pub(crate) ignored_reason: Option<&'static str>,
}
