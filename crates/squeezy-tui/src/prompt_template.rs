//! Prompt Templates As Queue Cards (§12.3.6).
//!
//! A *prompt template* is a reusable parameterised prompt with editable slots,
//! e.g. `Review {file}` — pick the template, fill its `{slot}`s, then enqueue or
//! run the resolved text. This module owns only the *pure* model: the parser
//! that splits a template body into literal + slot [`Segment`]s, the deterministic
//! [`resolve`] resolver (substitute filled slots; missing/blank slots BLOCK with
//! inline status), the [`TemplateCard`] editing state (per-slot values + a focused
//! slot the user edits), and the bounded reusable [`TemplateStore`] the picker
//! overlay browses.
//!
//! ## Model, not chrome
//!
//! Like [`crate::snippet_store`] and [`crate::queue_groups`], this module is
//! deliberately side-effect free so every cap, cursor, parse rule, and resolution
//! outcome is unit-testable without standing up a `TuiApp` or a terminal. `lib.rs`
//! owns the side effects: opening/closing the picker, painting it through the one
//! fullscreen `render()`, routing key/mouse to the focused slot, calling
//! `prompt_queue.push_back` on a clean resolution, and writing the status line.
//!
//! ## Deterministic, tiny — NOT a templating engine
//!
//! The spec is explicit: "Avoid a large templating engine; use a small
//! deterministic resolver." So the grammar is intentionally minimal:
//!
//!   - `{name}` is a slot. `name` is a bounded run of `[A-Za-z0-9_-]`.
//!   - `{{` / `}}` are literal `{` / `}` escapes.
//!   - Everything else is literal text, including a lone `{` that does not open a
//!     well-formed slot (it is kept verbatim so a body never silently loses text).
//!
//! Duplicate slot names share one value: filling `{file}` once fills every
//! `{file}` in the body. Resolution is a pure function of (segments, values), so
//! the same card always resolves to the same text.

/// Largest number of reusable templates the store retains. Small on purpose:
/// templates are a "stash a few reusable shapes" affordance, not a database.
/// Saving past this drops the oldest so the store stays bounded and the picker
/// stays a fixed, scannable size.
pub(crate) const MAX_TEMPLATES: usize = 32;

/// Largest number of characters retained in a template's NAME (the one-line
/// handle the picker shows). The body is untouched.
pub(crate) const NAME_CHARS: usize = 48;

/// Largest number of distinct slots a single template body can declare. Past
/// this, extra `{slot}` occurrences are folded into literal text so a pathological
/// body can never blow up the slot list / focus ring. Generous for any real
/// prompt; a hard ceiling so the editor stays a fixed size.
pub(crate) const MAX_SLOTS: usize = 16;

/// Largest number of characters retained in one slot's NAME. A `{...}` run longer
/// than this is treated as literal text (not a slot) so a stray brace can never
/// declare an enormous slot.
pub(crate) const SLOT_NAME_CHARS: usize = 32;

/// Largest number of body characters the picker shows for one template's preview.
pub(crate) const PREVIEW_CHARS: usize = 72;

/// One piece of a parsed template body: either a literal run of text or a named
/// slot to be filled. The whole body is a `Vec<Segment>`; [`resolve`] walks it
/// once, emitting literals verbatim and slot values in place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Segment {
    /// Verbatim text emitted as-is (escapes already decoded).
    Literal(String),
    /// A `{name}` placeholder. The same name may appear in several segments; they
    /// all draw from one shared value (see [`TemplateCard`]).
    Slot(String),
}

/// Parse a template `body` into its literal/slot [`Segment`]s.
///
/// Deterministic and total: every input produces a `Vec<Segment>` and no input
/// is ever rejected. `{{`/`}}` decode to literal braces; a well-formed
/// `{name}` (name a bounded `[A-Za-z0-9_-]` run no longer than
/// [`SLOT_NAME_CHARS`]) becomes a [`Segment::Slot`]; anything else — a lone `{`,
/// an empty `{}`, an over-long or illegal name — stays literal so no body text is
/// ever lost. Adjacent literals are merged so the segment list stays compact.
pub(crate) fn parse(body: &str) -> Vec<Segment> {
    let mut segments: Vec<Segment> = Vec::new();
    let mut literal = String::new();
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < body.len() {
        let ch = body[i..].chars().next().unwrap_or('\0');
        if ch == '{' {
            // `{{` → literal `{`.
            if bytes.get(i + 1) == Some(&b'{') {
                literal.push('{');
                i += 2;
                continue;
            }
            // Try to read a well-formed `{name}` slot.
            if let Some((name, consumed)) = read_slot(&body[i..]) {
                if !literal.is_empty() {
                    segments.push(Segment::Literal(std::mem::take(&mut literal)));
                }
                segments.push(Segment::Slot(name));
                i += consumed;
                continue;
            }
            // A lone/malformed `{` is kept verbatim.
            literal.push('{');
            i += 1;
            continue;
        }
        if ch == '}' && bytes.get(i + 1) == Some(&b'}') {
            // `}}` → literal `}`.
            literal.push('}');
            i += 2;
            continue;
        }
        literal.push(ch);
        i += ch.len_utf8();
    }
    if !literal.is_empty() {
        segments.push(Segment::Literal(literal));
    }
    segments
}

/// Try to read a `{name}` slot off the FRONT of `rest` (which starts at the `{`).
/// Returns `(name, consumed_bytes)` on success, where `consumed_bytes` covers the
/// whole `{name}` including both braces. A name must be a non-empty run of
/// `[A-Za-z0-9_-]` no longer than [`SLOT_NAME_CHARS`] chars and be immediately
/// closed by `}`; anything else returns `None` so the caller keeps the `{`
/// literal.
fn read_slot(rest: &str) -> Option<(String, usize)> {
    let mut chars = rest.char_indices();
    // First char is the opening brace.
    let (_, open) = chars.next()?;
    debug_assert_eq!(open, '{');
    let mut name = String::new();
    for (idx, ch) in chars {
        if ch == '}' {
            if name.is_empty() {
                return None;
            }
            // idx is the byte offset of `}` within `rest`; +1 to consume it.
            return Some((name, idx + 1));
        }
        if !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '-') {
            return None;
        }
        if name.chars().count() >= SLOT_NAME_CHARS {
            return None;
        }
        name.push(ch);
    }
    None
}

/// The distinct slot names declared by `segments`, in first-appearance order, with
/// duplicates removed and capped at [`MAX_SLOTS`]. This is the ordered slot list
/// the editor's focus ring walks; first-appearance order keeps the focus order
/// matching the reading order of the body.
pub(crate) fn slot_names(segments: &[Segment]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for seg in segments {
        if let Segment::Slot(name) = seg
            && !names.iter().any(|n| n == name)
        {
            names.push(name.clone());
            if names.len() >= MAX_SLOTS {
                break;
            }
        }
    }
    names
}

/// Why a template card could not be resolved into a runnable prompt. Returned by
/// [`resolve`] so the caller can paint inline status and BLOCK execution (the
/// spec's "Missing/invalid slots block execution with inline status").
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResolveError {
    /// One or more required slots are still empty (whitespace-only counts as
    /// empty). Holds the offending slot names in body order.
    MissingSlots(Vec<String>),
}

/// Resolve `segments` against the filled `value_of` lookup into the final prompt
/// text. Deterministic: literals pass through verbatim; each slot is replaced by
/// its trimmed value. A slot whose value is absent or blank BLOCKS the whole
/// resolution — [`ResolveError::MissingSlots`] names every such slot in body
/// order, so the caller can refuse to enqueue and tell the user which slots to
/// fill. Never partially resolves: an `Err` means nothing ran.
pub(crate) fn resolve(
    segments: &[Segment],
    mut value_of: impl FnMut(&str) -> Option<String>,
) -> Result<String, ResolveError> {
    let mut missing: Vec<String> = Vec::new();
    let mut out = String::new();
    for seg in segments {
        match seg {
            Segment::Literal(text) => out.push_str(text),
            Segment::Slot(name) => {
                let filled = value_of(name).map(|v| v.trim().to_string());
                match filled {
                    Some(v) if !v.is_empty() => out.push_str(&v),
                    _ => {
                        if !missing.iter().any(|m| m == name) {
                            missing.push(name.clone());
                        }
                    }
                }
            }
        }
    }
    if missing.is_empty() {
        Ok(out)
    } else {
        Err(ResolveError::MissingSlots(missing))
    }
}

/// One saved, reusable template.
///
/// Holds a stable monotonic `id`, a concise human `name` (derived from the first
/// non-empty line by [`derive_name`] when not given), and the raw `body` (with
/// `{slot}` markers intact, so re-instantiating reproduces the template exactly).
/// The parsed segments / slot list are recomputed on demand from `body` rather
/// than stored, so the store holds only the source of truth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PromptTemplate {
    /// Stable, monotonically-increasing id. Never reused within a session, so a
    /// picker selection / delete keyed by id never lands on the wrong template
    /// after a drop shifts the list.
    pub(crate) id: u64,
    /// Concise one-line handle shown in the picker.
    pub(crate) name: String,
    /// The raw template body with `{slot}` markers intact.
    pub(crate) body: String,
}

impl PromptTemplate {
    /// The parsed segments of this template's body.
    pub(crate) fn segments(&self) -> Vec<Segment> {
        parse(&self.body)
    }

    /// The distinct slot names this template declares, in body order.
    pub(crate) fn slot_names(&self) -> Vec<String> {
        slot_names(&self.segments())
    }

    /// Number of distinct slots this template declares.
    pub(crate) fn slot_count(&self) -> usize {
        self.slot_names().len()
    }

    /// A bounded single-line preview of the body for the picker row: the first
    /// [`PREVIEW_CHARS`] characters with interior newlines/tabs flattened to a
    /// single space and a trailing `…` when clipped. Slot markers are kept (so the
    /// user sees `Review {file}`). Pure presentation; the full `body` is untouched.
    pub(crate) fn preview(&self) -> String {
        flatten_one_line(&self.body, PREVIEW_CHARS)
    }
}

/// Derive a concise template name from `body`: the first non-empty line,
/// whitespace-flattened and clipped to [`NAME_CHARS`] with a trailing `…`. Falls
/// back to `"(empty template)"` when `body` has no non-whitespace content.
pub(crate) fn derive_name(body: &str) -> String {
    let first = body
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    if first.is_empty() {
        return "(empty template)".to_string();
    }
    flatten_one_line(first, NAME_CHARS)
}

/// Flatten `text` to a single line — interior `\n`/`\r`/`\t` runs collapse to one
/// space — then clip to `max_chars` with a trailing `…` when over. Shared by the
/// name derivation and the preview so the two never drift in their flattening.
fn flatten_one_line(text: &str, max_chars: usize) -> String {
    let mut flattened = String::with_capacity(text.len().min(max_chars * 2));
    let mut last_was_space = false;
    for ch in text.chars() {
        let c = if ch == '\n' || ch == '\r' || ch == '\t' {
            ' '
        } else {
            ch
        };
        if c == ' ' {
            if last_was_space {
                continue;
            }
            last_was_space = true;
        } else {
            last_was_space = false;
        }
        flattened.push(c);
    }
    let flattened = flattened.trim();
    let count = flattened.chars().count();
    if count <= max_chars {
        return flattened.to_string();
    }
    let keep = max_chars.saturating_sub(1);
    let mut clipped: String = flattened.chars().take(keep).collect();
    clipped.push('…');
    clipped
}

/// The live editing state for one instantiated template card: the template's
/// segments, the ordered slot names, the per-slot values being filled, and the
/// focused slot the user is editing.
///
/// Created by [`TemplateStore::instantiate`]; the picker overlay paints it, routes
/// arrow/Tab focus moves and character input to the focused slot, and asks
/// [`TemplateCard::resolved`] whether the card can run yet. The card holds the
/// template's `id` so a save-back / re-resolution stays tied to its source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TemplateCard {
    /// Stable id of the template this card was instantiated from.
    pub(crate) template_id: u64,
    /// The template's display name (carried so the card can label itself without
    /// re-reading the store, which a concurrent delete may have emptied).
    pub(crate) name: String,
    /// The parsed body segments. Resolution walks these.
    segments: Vec<Segment>,
    /// The distinct slot names in body order — the focus ring.
    slots: Vec<String>,
    /// Per-slot current value, index-aligned with `slots`. Starts all-empty.
    values: Vec<String>,
    /// Index into `slots`/`values` of the focused slot the user is editing.
    /// Always in range when there is at least one slot; meaningless (0) when the
    /// template has no slots (a slot-less card is runnable immediately).
    focused: usize,
}

impl TemplateCard {
    /// Build an editing card from a template's `id`, `name`, and `body`. The body
    /// is parsed once; every slot starts empty with focus on the first slot.
    pub(crate) fn new(template_id: u64, name: String, body: &str) -> Self {
        let segments = parse(body);
        let slots = slot_names(&segments);
        let values = vec![String::new(); slots.len()];
        Self {
            template_id,
            name,
            segments,
            slots,
            values,
            focused: 0,
        }
    }

    /// The ordered slot names (the focus ring).
    pub(crate) fn slots(&self) -> &[String] {
        &self.slots
    }

    /// Number of slots this card has.
    pub(crate) fn slot_count(&self) -> usize {
        self.slots.len()
    }

    /// Whether this card has no slots — it is runnable immediately (the resolved
    /// text is just the body's literal text).
    pub(crate) fn has_no_slots(&self) -> bool {
        self.slots.is_empty()
    }

    /// Index of the focused slot. Meaningless when [`has_no_slots`](Self::has_no_slots).
    pub(crate) fn focused_index(&self) -> usize {
        self.focused
    }

    /// The focused slot's name, or `None` for a slot-less card.
    pub(crate) fn focused_slot(&self) -> Option<&str> {
        self.slots.get(self.focused).map(String::as_str)
    }

    /// The current value of the slot at `index`, or `""` when out of range.
    pub(crate) fn value_at(&self, index: usize) -> &str {
        self.values.get(index).map_or("", String::as_str)
    }

    /// Move focus to the next slot, wrapping at the end. A no-op on a slot-less
    /// card. Wrapping keeps Tab cycling forever without dead-ending.
    pub(crate) fn focus_next(&mut self) {
        if self.slots.is_empty() {
            return;
        }
        self.focused = (self.focused + 1) % self.slots.len();
    }

    /// Move focus to the previous slot, wrapping at the start. A no-op on a
    /// slot-less card.
    pub(crate) fn focus_prev(&mut self) {
        if self.slots.is_empty() {
            return;
        }
        self.focused = (self.focused + self.slots.len() - 1) % self.slots.len();
    }

    /// Point focus at the slot at `index`, returning `true` when it exists. Used
    /// by the mouse path: a click resolves a row to its slot index, then focuses
    /// it. Out-of-range indices are ignored.
    pub(crate) fn focus_index(&mut self, index: usize) -> bool {
        if index < self.slots.len() {
            self.focused = index;
            true
        } else {
            false
        }
    }

    /// Append `ch` to the focused slot's value. A no-op on a slot-less card.
    pub(crate) fn insert_char(&mut self, ch: char) {
        if let Some(value) = self.values.get_mut(self.focused) {
            value.push(ch);
        }
    }

    /// Delete the last char of the focused slot's value. A no-op on a slot-less
    /// card or an empty value.
    pub(crate) fn delete_back(&mut self) {
        if let Some(value) = self.values.get_mut(self.focused) {
            value.pop();
        }
    }

    /// Clear the focused slot's value. A no-op on a slot-less card.
    pub(crate) fn clear_focused(&mut self) {
        if let Some(value) = self.values.get_mut(self.focused) {
            value.clear();
        }
    }

    /// Resolve the card into its final prompt text, or report which slots are
    /// still missing. Deterministic over the current slot values (see [`resolve`]).
    pub(crate) fn resolved(&self) -> Result<String, ResolveError> {
        resolve(&self.segments, |name| {
            self.slots
                .iter()
                .position(|n| n == name)
                .and_then(|i| self.values.get(i))
                .cloned()
        })
    }

    /// The slot names still missing a (non-blank) value, in body order. Empty
    /// when the card is fully filled / has no slots. Drives the inline "fill
    /// these" status without re-running the whole resolution at the call site.
    pub(crate) fn missing_slots(&self) -> Vec<String> {
        match self.resolved() {
            Ok(_) => Vec::new(),
            Err(ResolveError::MissingSlots(missing)) => missing,
        }
    }
}

/// Bounded, newest-first ring of reusable templates plus the picker's selection
/// cursor.
///
/// Entries are stored with index 0 = newest. [`MAX_TEMPLATES`] is enforced on
/// every [`save`](Self::save) by dropping the oldest. The `selected` cursor is
/// the picker's highlighted row, kept in range as the list shrinks/grows.
#[derive(Debug, Clone, Default)]
pub(crate) struct TemplateStore {
    templates: Vec<PromptTemplate>,
    /// Monotonic id source. Never reset within a session.
    next_id: u64,
    /// The picker's highlighted index (into `templates`, newest-first). Clamped to
    /// a valid row whenever the list changes; meaningless when empty.
    selected: usize,
}

impl TemplateStore {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Seed the store with a couple of small, broadly-useful starter templates so
    /// the picker is never empty on first open (the feature is discoverable
    /// without first knowing how to author one). Ids stay monotonic from here.
    pub(crate) fn with_starters() -> Self {
        let mut store = Self::new();
        // Newest-first: push the more general one first so "Review {file}" ends up
        // on top (the freshest, pre-selected row).
        store.save(
            Some("Summarise {topic}"),
            "Summarise {topic} in a few bullet points.",
        );
        store.save(
            Some("Review {file}"),
            "Review {file} and list any bugs or improvements.",
        );
        store
    }

    /// Save a template with raw `body` and an optional explicit `name` (when
    /// `None`, the name is derived from the first non-empty body line). Returns the
    /// new template's id, or `None` when `body` has no non-whitespace content —
    /// there is nothing to save. Newest-first: inserted at the front, cursor
    /// follows. Enforces [`MAX_TEMPLATES`] by dropping the oldest.
    pub(crate) fn save(&mut self, name: Option<&str>, body: &str) -> Option<u64> {
        if body.trim().is_empty() {
            return None;
        }
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        let name = match name {
            Some(n) if !n.trim().is_empty() => flatten_one_line(n.trim(), NAME_CHARS),
            _ => derive_name(body),
        };
        let template = PromptTemplate {
            id,
            name,
            body: body.to_string(),
        };
        self.templates.insert(0, template);
        self.selected = 0;
        self.enforce_cap();
        Some(id)
    }

    /// Drop oldest templates until at most [`MAX_TEMPLATES`] remain, then re-clamp
    /// the cursor.
    fn enforce_cap(&mut self) {
        while self.templates.len() > MAX_TEMPLATES {
            self.templates.pop();
        }
        self.clamp_selection();
    }

    /// Number of templates currently held.
    pub(crate) fn len(&self) -> usize {
        self.templates.len()
    }

    /// Whether the store is empty (the picker shows an empty-state line).
    pub(crate) fn is_empty(&self) -> bool {
        self.templates.is_empty()
    }

    /// Read-only view of the templates, newest first.
    pub(crate) fn templates(&self) -> &[PromptTemplate] {
        &self.templates
    }

    /// The picker's currently-selected index (newest-first). Meaningless when
    /// empty; callers gate on [`is_empty`](Self::is_empty) first.
    pub(crate) fn selected_index(&self) -> usize {
        self.selected
    }

    /// The currently-selected template, or `None` when the store is empty.
    pub(crate) fn selected_template(&self) -> Option<&PromptTemplate> {
        self.templates.get(self.selected)
    }

    /// Move the picker cursor up one row (toward the newest). Saturates at the
    /// top; a no-op on an empty list.
    pub(crate) fn select_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    /// Move the picker cursor down one row (toward the oldest). Saturates at the
    /// last row; a no-op on an empty list.
    pub(crate) fn select_down(&mut self) {
        if self.selected + 1 < self.templates.len() {
            self.selected += 1;
        }
    }

    /// Point the cursor at the template with `id`, returning `true` when it
    /// exists. Used by the mouse path: a click resolves a row to its stable id,
    /// then selects it — so a concurrent drop can never select the wrong row.
    pub(crate) fn select_id(&mut self, id: u64) -> bool {
        if let Some(pos) = self.templates.iter().position(|t| t.id == id) {
            self.selected = pos;
            true
        } else {
            false
        }
    }

    /// Build a fresh [`TemplateCard`] from the template with `id` for editing, or
    /// `None` when no such template exists (it was dropped/deleted meanwhile).
    pub(crate) fn instantiate(&self, id: u64) -> Option<TemplateCard> {
        let template = self.templates.iter().find(|t| t.id == id)?;
        Some(TemplateCard::new(
            template.id,
            template.name.clone(),
            &template.body,
        ))
    }

    /// Delete the template with `id`, returning `true` when one was removed. Keeps
    /// the selection cursor on a valid row.
    pub(crate) fn delete(&mut self, id: u64) -> bool {
        if let Some(pos) = self.templates.iter().position(|t| t.id == id) {
            self.templates.remove(pos);
            self.clamp_selection();
            true
        } else {
            false
        }
    }

    /// Drop every template and reset the cursor.
    pub(crate) fn clear(&mut self) {
        self.templates.clear();
        self.selected = 0;
    }

    /// Keep `selected` within `[0, len)`; clamp to the last row when the list
    /// shrank past it, and to 0 when it emptied.
    fn clamp_selection(&mut self) {
        if self.templates.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.templates.len() {
            self.selected = self.templates.len() - 1;
        }
    }
}

#[cfg(test)]
#[path = "prompt_template_tests.rs"]
mod tests;
