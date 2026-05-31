//! Interactive overlay for `/model`.
//!
//! Today this is the only inline list-of-options picker reachable from
//! a slash command. `/verbosity`, `/tool-verbosity`, and `/permissions`
//! all route through `toggle_config_screen` instead — their former
//! Overlay variants were removed in the 2026-05 slash-command audit
//! cleanup (see `squeezy-h2ab`).

#![allow(dead_code)]

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use squeezy_llm::{MODEL_REGISTRY, ModelInfo};

use crate::render::palette;

#[derive(Debug, Clone)]
pub(crate) enum Overlay {
    Model(SelectOverlay<ModelEntry>),
}

/// Where focus should return to when an overlay closes.
///
/// The TUI does not have a generic focus manager today: the resting
/// input owner is always the composer, and modal overlays / popups
/// borrow keys while they are open. This enum is the contract callers
/// hand [`DialogHandle::open`] to declare what was focused, and
/// [`DialogHandle::restore_focus`] returns that hint at close time so
/// future focusables can be added without changing call sites.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) enum PriorFocus {
    /// The composer (prompt input). The default for slash-command
    /// overlays: closing the overlay returns focus to the composer
    /// because that is the TUI's resting input owner.
    #[default]
    Composer,
    /// No specific holder; `restore_focus` is a no-op hint.
    None,
}

/// Typed handle returned by [`DialogHandle::open`] so callers can manipulate
/// the overlay (`hide`, `set`, `restore_focus`) without poking
/// `app.overlay` and the active-dialog id directly.
///
/// Each `open()` bumps the per-app generation id; `hide`, `set`, and
/// `restore_focus` check that id against the slot's `active_id`, so a
/// stale handle for an already-closed or replaced dialog is a no-op
/// instead of clobbering the newer dialog that now owns the slot.
#[derive(Debug, Clone)]
pub(crate) struct DialogHandle {
    id: u64,
    prior_focus: PriorFocus,
}

impl DialogHandle {
    /// Open `content` as a typed dialog. `next_id` is the per-app
    /// generation counter (incremented in place); `active_id` records
    /// which dialog currently owns the slot. Any previous overlay is
    /// replaced and any outstanding handle to it becomes stale.
    pub(crate) fn open(
        slot: &mut Option<Overlay>,
        next_id: &mut u64,
        active_id: &mut Option<u64>,
        content: Overlay,
        prior_focus: PriorFocus,
    ) -> Self {
        *next_id = next_id.wrapping_add(1);
        let id = *next_id;
        *slot = Some(content);
        *active_id = Some(id);
        DialogHandle { id, prior_focus }
    }

    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    pub(crate) fn prior_focus(&self) -> &PriorFocus {
        &self.prior_focus
    }

    /// Close this dialog if it is still the active one. Returns `true`
    /// when the overlay was hidden, `false` for stale handles (the
    /// caller's dialog is already gone).
    pub(crate) fn hide(&self, slot: &mut Option<Overlay>, active_id: &mut Option<u64>) -> bool {
        if *active_id == Some(self.id) {
            *slot = None;
            *active_id = None;
            true
        } else {
            false
        }
    }

    /// Replace the overlay content while keeping this handle valid.
    /// Returns `false` for stale handles, in which case the slot is
    /// unchanged so a newer dialog cannot be clobbered.
    pub(crate) fn set(
        &self,
        slot: &mut Option<Overlay>,
        active_id: &Option<u64>,
        content: Overlay,
    ) -> bool {
        if *active_id == Some(self.id) {
            *slot = Some(content);
            true
        } else {
            false
        }
    }

    /// Hide this dialog (if still active) and return the focus hint
    /// captured at open time. Today the only restorable target is the
    /// composer, which is implicit focus once no overlay is open; the
    /// hint exists so additional focusables (e.g. an inline picker)
    /// can be wired in without changing the close protocol.
    pub(crate) fn restore_focus(
        &self,
        slot: &mut Option<Overlay>,
        active_id: &mut Option<u64>,
    ) -> PriorFocus {
        let _ = self.hide(slot, active_id);
        self.prior_focus.clone()
    }
}

impl Overlay {
    pub(crate) fn title(&self) -> &'static str {
        match self {
            Overlay::Model(_) => "Select model",
        }
    }

    pub(crate) fn move_up(&mut self) {
        match self {
            Overlay::Model(o) => o.move_up(),
        }
    }

    pub(crate) fn move_down(&mut self) {
        match self {
            Overlay::Model(o) => o.move_down(),
        }
    }

    pub(crate) fn render_lines(&self) -> Vec<Line<'static>> {
        let mut lines = vec![header_line(self.title())];
        match self {
            Overlay::Model(o) => lines.extend(o.render(|e| e.label())),
        }
        lines.push(footer_line());
        lines
    }
}

fn header_line(title: &'static str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            title,
            Style::default()
                .fg(crate::render::theme::secondary())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "  ↑↓ choose · Enter apply · Esc cancel",
            Style::default().fg(crate::render::theme::quiet()),
        ),
    ])
}

fn footer_line() -> Line<'static> {
    Line::from(Span::styled(
        "",
        Style::default().fg(crate::render::theme::quiet()),
    ))
}

#[derive(Debug, Clone)]
pub(crate) struct SelectOverlay<T> {
    pub entries: Vec<T>,
    pub selected: usize,
}

impl<T> SelectOverlay<T> {
    pub(crate) fn new(entries: Vec<T>, default_index: usize) -> Self {
        let selected = default_index.min(entries.len().saturating_sub(1));
        Self { entries, selected }
    }

    pub(crate) fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    pub(crate) fn move_down(&mut self) {
        if self.selected + 1 < self.entries.len() {
            self.selected += 1;
        }
    }

    pub(crate) fn selected(&self) -> Option<&T> {
        self.entries.get(self.selected)
    }

    fn render(&self, label: impl Fn(&T) -> String) -> Vec<Line<'static>> {
        const WINDOW: usize = 5;
        let total = self.entries.len();
        if total == 0 {
            return Vec::new();
        }
        let half = WINDOW / 2;
        let start = self
            .selected
            .saturating_sub(half)
            .min(total.saturating_sub(WINDOW));
        let end = (start + WINDOW).min(total);
        self.entries[start..end]
            .iter()
            .enumerate()
            .map(|(rel, entry)| {
                let index = start + rel;
                let is_selected = index == self.selected;
                let marker = if is_selected { "› " } else { "  " };
                let style = if is_selected {
                    Style::default()
                        .fg(crate::render::theme::secondary())
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(palette::muted_fg())
                };
                Line::from(vec![
                    Span::styled(
                        marker,
                        Style::default().fg(if is_selected {
                            crate::render::theme::secondary()
                        } else {
                            crate::render::theme::quiet()
                        }),
                    ),
                    Span::styled(label(entry), style),
                ])
            })
            .collect()
    }
}

// ---- Model overlay ----

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelEntry {
    pub provider: &'static str,
    pub id: &'static str,
    pub context_window: Option<u64>,
}

impl ModelEntry {
    fn label(&self) -> String {
        let ctx = self
            .context_window
            .map(|c| format!(" · ctx {c}"))
            .unwrap_or_default();
        format!("{}:{}{}", self.provider, self.id, ctx)
    }
}

pub(crate) fn build_model_overlay(current_provider: &str, current_id: &str) -> Overlay {
    let entries: Vec<ModelEntry> = MODEL_REGISTRY
        .iter()
        .map(|model: &ModelInfo| ModelEntry {
            provider: model.provider,
            id: model.id,
            context_window: model.limits.map(|l| l.context_window_tokens),
        })
        .collect();
    let default_index = entries
        .iter()
        .position(|e| e.provider == current_provider && e.id == current_id)
        .unwrap_or(0);
    Overlay::Model(SelectOverlay::new(entries, default_index))
}
