//! Startup resume picker.
//!
//! When a user runs `squeezy` with no explicit `--resume <id>` flag and a
//! recent session exists for the current cwd, surface a small selection
//! overlay so they can pick up where they left off.
//!
//! Most of this module is pure logic so the candidate filter and
//! key-dispatch can be tested without touching the terminal.

use std::{
    io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
};
use squeezy_core::{AppConfig, SqueezyError};
use squeezy_store::{
    EventBranchTip, GlobalSessionIndexEntry, SessionMetadata, SessionQuery, SessionStore,
    detect_branches,
};

/// Maximum number of sessions shown in the overlay. Keep small — the user
/// is choosing one of "most recent" and a longer list is just noise.
pub(crate) const MAX_PICKER_ENTRIES: usize = 5;

/// Sessions started within this window of `now_ms` are considered for the
/// resume picker. Older sessions can still be reached via
/// `squeezy sessions list`/`/resume <id>`.
pub(crate) const RECENT_WINDOW_MS: u64 = 7 * 24 * 60 * 60 * 1_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionSummary {
    pub(crate) session_id: String,
    pub(crate) started_at_ms: u64,
    pub(crate) first_user_task: Option<String>,
    pub(crate) latest_summary: Option<String>,
    /// User-turn count for the row's "(N prompts)" indicator. Sourced from
    /// `SessionMetrics::turns` — when 0 we render "new session" so the row
    /// reads naturally for sessions that recorded nothing.
    pub(crate) turn_count: u64,
    /// cwd recorded on the source `SessionMetadata`, kept so the picker can
    /// (a) hide cross-project entries until the user opts in via Tab and
    /// (b) annotate the row with a short directory hint when the entry's
    /// cwd differs from the current process cwd.
    pub(crate) cwd: String,
    /// Optional repo-root label shown alongside cross-project entries so
    /// the user can disambiguate sibling clones with similar prompts.
    pub(crate) repo_root: Option<String>,
    /// User-set name (`/session rename <name>`). When present the picker
    /// uses it as the row's primary title in place of the inferred task
    /// label, so memorable sessions stay easy to spot.
    pub(crate) display_name: Option<String>,
    /// User-set labels (`/session label <name>`). Rendered after the row
    /// title as compact `#labels`. Empty for sessions the user has not
    /// tagged yet.
    pub(crate) labels: Vec<String>,
    /// Branch tips discovered in the session's `events.jsonl`. Empty for
    /// linear sessions (the common case); populated when the session log
    /// contains at least two branches because the user re-prompted from
    /// an earlier turn. Each tip becomes its own row in the picker so the
    /// user can navigate to either path.
    pub(crate) branches: Vec<EventBranchTip>,
}

impl SessionSummary {
    fn from_metadata(metadata: &SessionMetadata) -> Self {
        Self {
            session_id: metadata.session_id.clone(),
            started_at_ms: metadata.started_at_ms,
            first_user_task: metadata.first_user_task.clone(),
            latest_summary: metadata.latest_summary.clone(),
            turn_count: metadata.metrics.turns,
            cwd: metadata.cwd.clone(),
            repo_root: metadata.repo_root.clone(),
            display_name: metadata.display_name.clone(),
            labels: metadata.labels.clone(),
            branches: Vec::new(),
        }
    }

    /// Build a summary from a cross-project index entry. The global index
    /// only persists a single `title` field — surface it as
    /// `first_user_task` so the picker label code (which prefers
    /// `first_user_task` over `latest_summary`) reads naturally. Labels
    /// are not persisted on the index entry: cross-project rows show the
    /// row's `display_name` (when set) and otherwise behave the same as
    /// per-project rows.
    fn from_global_index_entry(entry: &GlobalSessionIndexEntry) -> Self {
        Self {
            session_id: entry.session_id.clone(),
            started_at_ms: entry.started_at_ms,
            first_user_task: entry.title.clone(),
            latest_summary: None,
            turn_count: entry.turn_count,
            cwd: entry.cwd.clone(),
            repo_root: entry.repo_root.clone(),
            display_name: entry.display_name.clone(),
            labels: Vec::new(),
            branches: Vec::new(),
        }
    }

    /// Compact "(N prompt[s])" indicator. Returns an empty string for
    /// sessions that recorded no turns so the row stays uncluttered.
    pub(crate) fn turn_indicator(&self) -> String {
        match self.turn_count {
            0 => "new".to_string(),
            1 => "1 prompt".to_string(),
            n => format!("{n} prompts"),
        }
    }

    /// Primary row title. Prefers the user-set `display_name` from
    /// `/session rename <name>` so memorable sessions stay easy to spot;
    /// falls back to the inferred first-user-task / latest-summary pair
    /// for sessions the user has not renamed yet.
    pub(crate) fn label(&self) -> String {
        if let Some(name) = self.display_name.as_deref() {
            let line = name.lines().next().unwrap_or(name);
            if !line.trim().is_empty() {
                return line.to_string();
            }
        }
        let task = self
            .first_user_task
            .as_deref()
            .or(self.latest_summary.as_deref())
            .unwrap_or("(no prompt recorded)")
            .lines()
            .next()
            .unwrap_or("(no prompt recorded)");
        task.to_string()
    }

    /// Compact `#label1 #label2` hint rendered after the row title when
    /// the user has tagged the session via `/session label <name>`. Empty
    /// for untagged sessions so the picker layout stays unchanged for
    /// the default case.
    pub(crate) fn label_hint(&self) -> String {
        if self.labels.is_empty() {
            return String::new();
        }
        self.labels
            .iter()
            .map(|label| format!("#{label}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Short directory hint shown when the entry lives outside the current
    /// cwd. Prefers the repo-root basename so sibling clones disambiguate,
    /// otherwise falls back to the cwd's tail path component.
    pub(crate) fn project_hint(&self) -> String {
        let raw = self.repo_root.as_deref().unwrap_or(&self.cwd);
        let tail = Path::new(raw)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(raw);
        tail.to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResumeChoice {
    StartFresh {
        suppress: ResumePickerSuppress,
    },
    Resume {
        session_id: String,
        /// When `Some(sequence)`, the user picked a branch tip in a
        /// branched session and the caller should resume at that specific
        /// tip rather than the latest event. `None` for linear sessions
        /// (the common case), where the resume flow is unchanged.
        branch_tip: Option<u64>,
    },
    /// Selected session lives outside the current cwd. The TUI exits without
    /// chdir-ing and surfaces the `squeezy sessions resume <id>` invocation
    /// the user should run from `target_cwd` — silently relocating the
    /// process would surprise users juggling sibling repos.
    CrossProject {
        session_id: String,
        target_cwd: String,
    },
    Back,
    Quit,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ResumePickerSuppress {
    pub(crate) project: bool,
    pub(crate) user: bool,
}

/// One selectable row in the picker. Linear sessions produce a single
/// entry; branched sessions expand into one entry per branch tip so the
/// user can navigate to either path. `summary` is shared across rows
/// belonging to the same session so the row renderer still has access
/// to `cwd`, `repo_root`, etc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PickerEntry {
    pub(crate) summary: SessionSummary,
    /// `Some(tip)` when this row represents one branch of a branched
    /// session; `None` when the session is linear (or contains exactly
    /// one branch tip, which the detector treats as linear).
    pub(crate) branch_tip: Option<EventBranchTip>,
}

impl PickerEntry {
    fn linear(summary: SessionSummary) -> Self {
        Self {
            summary,
            branch_tip: None,
        }
    }

    fn branched(summary: SessionSummary, branch_tip: EventBranchTip) -> Self {
        Self {
            summary,
            branch_tip: Some(branch_tip),
        }
    }

    fn session_id(&self) -> &str {
        &self.summary.session_id
    }
}

/// Pure filter applied to the raw session list. Returns the most-recent
/// resumable sessions whose cwd matches the current working directory and
/// that started within [`RECENT_WINDOW_MS`].
#[cfg(test)]
pub(crate) fn filter_candidates(
    sessions: &[SessionMetadata],
    cwd: &Path,
    now_ms: u64,
) -> Vec<SessionSummary> {
    let cwd_str = cwd.display().to_string();
    filter_inner(sessions, now_ms, |meta| meta.cwd == cwd_str)
}

/// Cross-project view: drop the cwd filter so sessions from sibling repos
/// surface in the picker. The recency and resumable filters still apply.
/// Test-only — production now flows through [`merge_candidates_for_picker`]
/// which fans the per-project metadata together with the cross-project
/// index snapshots.
#[cfg(test)]
pub(crate) fn filter_candidates_all_projects(
    sessions: &[SessionMetadata],
    now_ms: u64,
) -> Vec<SessionSummary> {
    filter_inner(sessions, now_ms, |_| true)
}

#[cfg(test)]
fn filter_inner(
    sessions: &[SessionMetadata],
    now_ms: u64,
    extra: impl Fn(&SessionMetadata) -> bool,
) -> Vec<SessionSummary> {
    let mut out: Vec<SessionSummary> = sessions
        .iter()
        .filter(|meta| meta.resume_available)
        .filter(|meta| now_ms.saturating_sub(meta.started_at_ms) <= RECENT_WINDOW_MS)
        .filter(|meta| extra(meta))
        .map(SessionSummary::from_metadata)
        .collect();
    // `SessionStore::list` already sorts newest-first, but we re-sort here
    // so a caller passing a raw vec still sees the right order.
    out.sort_by_key(|summary| std::cmp::Reverse(summary.started_at_ms));
    out.truncate(MAX_PICKER_ENTRIES);
    out
}

/// Cross-project view over the recency-filtered union of per-project
/// metadata and cross-project index entries. Per-project entries take
/// precedence when the same `session_id` appears in both — they carry
/// richer state (latest_summary, finalised turn count) than the index
/// snapshot. Returns at most [`MAX_PICKER_ENTRIES`] newest-first.
pub(crate) fn merge_candidates_for_picker(
    local: &[SessionMetadata],
    global: &[GlobalSessionIndexEntry],
    now_ms: u64,
) -> Vec<SessionSummary> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out: Vec<SessionSummary> = Vec::new();
    for meta in local {
        if !meta.resume_available {
            continue;
        }
        if now_ms.saturating_sub(meta.started_at_ms) > RECENT_WINDOW_MS {
            continue;
        }
        if !seen.insert(meta.session_id.clone()) {
            continue;
        }
        out.push(SessionSummary::from_metadata(meta));
    }
    for entry in global {
        if !entry.resume_available {
            continue;
        }
        if now_ms.saturating_sub(entry.started_at_ms) > RECENT_WINDOW_MS {
            continue;
        }
        if !seen.insert(entry.session_id.clone()) {
            continue;
        }
        out.push(SessionSummary::from_global_index_entry(entry));
    }
    out.sort_by_key(|summary| std::cmp::Reverse(summary.started_at_ms));
    out.truncate(MAX_PICKER_ENTRIES);
    out
}

/// State machine driving the picker. Pure — owns no IO.
#[derive(Debug, Clone)]
pub(crate) struct ResumePickerState {
    /// Currently-visible rows, derived from `all_sessions` and the
    /// `show_all_projects` toggle. Recomputed every time the toggle flips.
    /// One entry per row — branched sessions expand into multiple rows so
    /// each branch tip is independently selectable.
    pub(crate) candidates: Vec<PickerEntry>,
    /// Full recent list across every cwd; the cwd-scoped view is a filter
    /// over this. Kept on the state so Tab can re-derive `candidates`
    /// without re-reading the session store.
    all_sessions: Vec<SessionSummary>,
    pub(crate) cursor: usize,
    pub(crate) show_all_projects: bool,
    pub(crate) never_project: bool,
    pub(crate) never_user: bool,
    setup_progress: Option<(usize, usize)>,
    cwd: PathBuf,
}

impl ResumePickerState {
    /// Build a picker over a pre-filtered set of sessions. `all_sessions`
    /// must already be recency-filtered (see `filter_candidates_all_projects`)
    /// so the toggle can flip between scoped and unscoped views without
    /// re-applying the recency filter.
    pub(crate) fn new(all_sessions: Vec<SessionSummary>, cwd: PathBuf) -> Self {
        Self::with_setup_progress(all_sessions, cwd, None)
    }

    pub(crate) fn with_setup_progress(
        all_sessions: Vec<SessionSummary>,
        cwd: PathBuf,
        setup_progress: Option<(usize, usize)>,
    ) -> Self {
        let cwd_str = cwd.display().to_string();
        let candidates = expand_entries(scoped_view(&all_sessions, &cwd_str));
        Self {
            candidates,
            all_sessions,
            cursor: 0,
            show_all_projects: false,
            never_project: false,
            never_user: false,
            setup_progress,
            cwd,
        }
    }

    fn can_go_back(&self) -> bool {
        self.setup_progress.is_some()
    }

    /// Number of selectable rows in the list — the leading "Start fresh"
    /// row plus every candidate.
    fn row_count(&self) -> usize {
        self.candidates.len() + 3
    }

    /// Index of the "Start fresh" row — always 0 so the safe default is
    /// pre-selected when the picker opens.
    pub(crate) const fn start_fresh_index(&self) -> usize {
        0
    }

    fn project_checkbox_index(&self) -> usize {
        self.candidates.len() + 1
    }

    fn user_checkbox_index(&self) -> usize {
        self.candidates.len() + 2
    }

    fn cursor_on_checkbox(&self) -> bool {
        self.cursor == self.project_checkbox_index() || self.cursor == self.user_checkbox_index()
    }

    /// Flip the project scope and re-derive `candidates`. The cursor is
    /// reset to "Start fresh" so the user does not act on a row that
    /// silently moved under them.
    pub(crate) fn toggle_all_projects(&mut self) {
        self.show_all_projects = !self.show_all_projects;
        let visible = if self.show_all_projects {
            self.all_sessions.clone()
        } else {
            scoped_view(&self.all_sessions, &self.cwd.display().to_string())
        };
        self.candidates = expand_entries(visible);
        self.cursor = 0;
    }

    fn select_at_cursor(&self) -> Option<ResumeChoice> {
        if self.cursor == self.start_fresh_index() {
            return Some(ResumeChoice::StartFresh {
                suppress: self.suppress_choice(),
            });
        }
        if self.cursor_on_checkbox() {
            return None;
        }
        // candidate rows live at indices 1..=N.
        let entry = self.candidates.get(self.cursor - 1)?;
        let cwd_str = self.cwd.display().to_string();
        if entry.summary.cwd == cwd_str {
            Some(ResumeChoice::Resume {
                session_id: entry.session_id().to_string(),
                branch_tip: entry.branch_tip.as_ref().map(|tip| tip.tip_sequence),
            })
        } else {
            Some(ResumeChoice::CrossProject {
                session_id: entry.session_id().to_string(),
                target_cwd: entry.summary.cwd.clone(),
            })
        }
    }

    fn suppress_choice(&self) -> ResumePickerSuppress {
        ResumePickerSuppress {
            project: self.never_project || self.never_user,
            user: self.never_user,
        }
    }

    fn toggle_at_cursor(&mut self) -> bool {
        if self.cursor == self.project_checkbox_index() {
            self.never_project = !self.never_project;
            if !self.never_project {
                self.never_user = false;
            }
            return true;
        }
        if self.cursor == self.user_checkbox_index() {
            self.never_user = !self.never_user;
            if self.never_user {
                self.never_project = true;
            }
            return true;
        }
        false
    }

    pub(crate) fn dispatch(&mut self, key: KeyEvent) -> Option<ResumeChoice> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Up, _) => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                } else {
                    self.cursor = self.row_count().saturating_sub(1);
                }
                None
            }
            (KeyCode::Down, _) => {
                self.cursor = (self.cursor + 1) % self.row_count().max(1);
                None
            }
            (KeyCode::Tab, _) => {
                self.toggle_all_projects();
                None
            }
            (KeyCode::Left, _) | (KeyCode::Backspace, _) if self.can_go_back() => {
                Some(ResumeChoice::Back)
            }
            (KeyCode::Char(' '), _) => {
                self.toggle_at_cursor();
                None
            }
            (KeyCode::Enter, _) => {
                if self.toggle_at_cursor() {
                    None
                } else {
                    self.select_at_cursor()
                }
            }
            (KeyCode::Esc, _) | (KeyCode::Char('n'), _) | (KeyCode::Char('N'), _) => {
                Some(ResumeChoice::StartFresh {
                    suppress: self.suppress_choice(),
                })
            }
            (KeyCode::Char('q'), _) | (KeyCode::Char('Q'), _) => Some(ResumeChoice::Quit),
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => Some(ResumeChoice::Quit),
            _ => None,
        }
    }
}

fn scoped_view(all: &[SessionSummary], cwd_str: &str) -> Vec<SessionSummary> {
    all.iter().filter(|s| s.cwd == cwd_str).cloned().collect()
}

pub(crate) fn has_scoped_candidates(all: &[SessionSummary], cwd: &Path) -> bool {
    let cwd_str = cwd.display().to_string();
    all.iter().any(|summary| summary.cwd == cwd_str)
}

/// Flatten each summary into selectable rows: linear sessions emit a
/// single `PickerEntry`, branched sessions emit one entry per branch tip
/// so the user can pick either path. Sessions reach this function in
/// newest-first order (set by `filter_inner`), and we preserve that
/// order across branch expansion: tips inside one session stay grouped,
/// with the newest tip first (already enforced by `detect_branches`).
fn expand_entries(summaries: Vec<SessionSummary>) -> Vec<PickerEntry> {
    let mut entries = Vec::with_capacity(summaries.len());
    for summary in summaries {
        if summary.branches.len() < 2 {
            entries.push(PickerEntry::linear(summary));
            continue;
        }
        let tips = summary.branches.clone();
        for tip in tips {
            entries.push(PickerEntry::branched(summary.clone(), tip));
        }
    }
    entries
}

/// Pull recent resumable sessions across every cwd. The picker filters
/// down to the current cwd by default but keeps the broader list around
/// so Tab can flip into a cross-project view without a second store read.
/// Per-project entries (richer state) merge over the cross-project index
/// snapshots so sibling-repo sessions surface alongside the local ones.
/// On error we log to stderr and start fresh — the picker is a
/// convenience, not a hard dependency.
pub(crate) fn load_candidates(config: &AppConfig) -> Vec<SessionSummary> {
    let store = SessionStore::open(config);
    let local = match store.list(&SessionQuery::default()) {
        Ok(sessions) => sessions,
        Err(error) => {
            let _: SqueezyError = error;
            eprintln!("squeezy: failed to list sessions for resume picker: {error}");
            Vec::new()
        }
    };
    let global = SessionStore::list_global_index();
    let now_ms = current_unix_ms();
    let mut summaries = merge_candidates_for_picker(&local, &global, now_ms);
    // Branch detection requires reading each candidate's event log. The
    // list is already capped so this stays cheap on cold start; we
    // silently ignore read errors because the picker is a convenience
    // surface — a session that fails to load simply renders as linear,
    // the same as legacy logs.
    for summary in &mut summaries {
        if let Ok(record) = store.show(&summary.session_id) {
            summary.branches = detect_branches(&record.events);
        }
    }
    summaries
}

fn current_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Drive the resume picker on an existing terminal. Returns the user's
/// choice or `StartFresh` when nothing in `all_sessions` is a viable resume
/// target (either the list is empty or every entry is cross-project — the
/// scoped default view is empty and we want the user to opt in via Tab
/// rather than surprise them with foreign sessions on first run).
pub(crate) fn run_picker<W: io::Write>(
    terminal: &mut Terminal<CrosstermBackend<W>>,
    all_sessions: Vec<SessionSummary>,
    cwd: PathBuf,
    setup_progress: Option<(usize, usize)>,
) -> io::Result<ResumeChoice> {
    let mut state = if setup_progress.is_some() {
        ResumePickerState::with_setup_progress(all_sessions, cwd, setup_progress)
    } else {
        ResumePickerState::new(all_sessions, cwd)
    };
    if state.candidates.is_empty() {
        return Ok(ResumeChoice::StartFresh {
            suppress: ResumePickerSuppress::default(),
        });
    }
    loop {
        terminal.draw(|frame| render_picker(frame, &state))?;
        match event::read()? {
            Event::Key(key) => {
                if let Some(choice) = state.dispatch(key) {
                    return Ok(choice);
                }
            }
            Event::Resize(_, _) => continue,
            _ => continue,
        }
    }
}

fn render_picker(frame: &mut ratatui::Frame<'_>, state: &ResumePickerState) {
    let full = frame.area();
    let area = centered_area(full);

    frame.render_widget(Clear, full);

    let title_suffix = if state.can_go_back() {
        "first run setup"
    } else {
        "resume a recent session"
    };
    let title = Line::from(vec![
        Span::styled(" ◆ ", Style::default().fg(crate::render::theme::accent())),
        Span::styled(
            "squeezy",
            Style::default()
                .fg(crate::render::theme::accent())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" · ", Style::default().fg(crate::render::theme::quiet())),
        Span::styled(
            title_suffix,
            Style::default().fg(crate::render::theme::foreground()),
        ),
        Span::raw(" "),
    ]);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(crate::render::theme::accent()))
        .title(title)
        .title_alignment(Alignment::Left);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(1), // question/title
            Constraint::Length(1), // count
            Constraint::Length(1), // spacer
            Constraint::Min(3),    // list
            Constraint::Length(1), // spacer
            Constraint::Length(2), // footer
        ])
        .split(inner);

    let question = if let Some((current, total)) = state.setup_progress {
        Line::from(vec![
            Span::styled(
                format!("Question {current}/{total} "),
                Style::default()
                    .fg(crate::render::theme::secondary())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "Resume a session",
                Style::default().fg(crate::render::theme::foreground()),
            ),
        ])
    } else {
        Line::from(Span::styled(
            "Resume a recent session",
            Style::default()
                .fg(crate::render::theme::secondary())
                .add_modifier(Modifier::BOLD),
        ))
    };
    frame.render_widget(Paragraph::new(question), layout[0]);

    let scope = if state.show_all_projects {
        " recent session{} across all projects"
    } else {
        " recent session{} for this directory"
    };
    let intro = Paragraph::new(Line::from(vec![
        Span::styled(
            format!("{}", state.candidates.len()),
            Style::default()
                .fg(crate::render::theme::accent())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            scope.replace("{}", if state.candidates.len() == 1 { "" } else { "s" }),
            Style::default().fg(crate::render::theme::quiet()),
        ),
    ]))
    .alignment(Alignment::Left);
    frame.render_widget(intro, layout[1]);

    // Start fresh leads the list as the safe default (cursor opens on it),
    // followed by each candidate session at index 1..=N.
    let cwd_str = state.cwd.display().to_string();
    let mut rows: Vec<Line<'_>> = Vec::with_capacity(state.candidates.len() + 3);
    rows.push(render_start_fresh_row(
        state.cursor == state.start_fresh_index(),
    ));
    rows.extend(state.candidates.iter().enumerate().map(|(idx, entry)| {
        // candidates start at row 1; active row uses the cursor offset.
        let row_idx = idx + 1;
        let cross_project = entry.summary.cwd != cwd_str;
        render_candidate_row(idx, entry, row_idx == state.cursor, cross_project)
    }));
    rows.push(render_checkbox_row(
        "Never ask again for this project",
        state.never_project || state.never_user,
        state.cursor == state.project_checkbox_index(),
    ));
    rows.push(render_checkbox_row(
        "Never ask again for this user",
        state.never_user,
        state.cursor == state.user_checkbox_index(),
    ));

    let body = Paragraph::new(rows).wrap(Wrap { trim: false });
    frame.render_widget(body, layout[3]);

    let tab_hint = if state.show_all_projects {
        "this dir"
    } else {
        "all projects"
    };
    let mut footer_lines = vec![Line::from(vec![
        Span::styled(
            "↑/↓ ",
            Style::default().fg(crate::render::theme::secondary()),
        ),
        Span::styled("move  ", Style::default().fg(crate::render::theme::quiet())),
        Span::styled(
            "Enter ",
            Style::default().fg(crate::render::theme::secondary()),
        ),
        Span::styled(
            "confirm/toggle  ",
            Style::default().fg(crate::render::theme::quiet()),
        ),
        Span::styled(
            "Space ",
            Style::default().fg(crate::render::theme::secondary()),
        ),
        Span::styled(
            "toggle  ",
            Style::default().fg(crate::render::theme::quiet()),
        ),
    ])];
    let mut second_line = Vec::new();
    if state.can_go_back() {
        second_line.push(Span::styled(
            "← ",
            Style::default().fg(crate::render::theme::secondary()),
        ));
        second_line.push(Span::styled(
            "back  ",
            Style::default().fg(crate::render::theme::quiet()),
        ));
    }
    second_line.extend([
        Span::styled(
            "Tab ",
            Style::default().fg(crate::render::theme::secondary()),
        ),
        Span::styled(
            format!("{tab_hint}  "),
            Style::default().fg(crate::render::theme::quiet()),
        ),
        Span::styled(
            "Esc ",
            Style::default().fg(crate::render::theme::secondary()),
        ),
        Span::styled(
            "start fresh  ",
            Style::default().fg(crate::render::theme::quiet()),
        ),
        Span::styled("Q ", Style::default().fg(crate::render::theme::secondary())),
        Span::styled("quit", Style::default().fg(crate::render::theme::quiet())),
    ]);
    footer_lines.push(Line::from(second_line));
    let footer = Paragraph::new(footer_lines)
        .wrap(Wrap { trim: false })
        .alignment(Alignment::Left);
    frame.render_widget(footer, layout[5]);
}

fn render_candidate_row(
    _idx: usize,
    entry: &PickerEntry,
    active: bool,
    cross_project: bool,
) -> Line<'static> {
    let summary = &entry.summary;
    let (prefix_color, label_style) = if active {
        (
            crate::render::theme::accent(),
            Style::default()
                .fg(crate::render::theme::secondary())
                .add_modifier(Modifier::BOLD),
        )
    } else {
        (
            crate::render::theme::quiet(),
            Style::default().fg(crate::render::theme::foreground()),
        )
    };
    let prefix = if active { "▸ " } else { "  " };
    let timestamp_style = if active {
        Style::default().fg(crate::render::theme::accent())
    } else {
        Style::default().fg(crate::render::theme::quiet())
    };
    // Branched rows replace the default session label with the branch's
    // first user message (if any) so the user can disambiguate two paths
    // that came out of the same fork point.
    let (label, branch_marker) = match entry.branch_tip.as_ref() {
        Some(tip) => {
            let branch_label = tip
                .first_message_after_branch
                .as_deref()
                .map(|text| text.lines().next().unwrap_or(text).to_string())
                .unwrap_or_else(|| summary.label());
            let marker = format!("  ⎇ branch @{}", tip.tip_sequence);
            (branch_label, Some(marker))
        }
        None => (summary.label(), None),
    };
    let mut spans = vec![
        Span::styled(prefix, Style::default().fg(prefix_color)),
        Span::styled(format_started_at(summary.started_at_ms), timestamp_style),
        Span::styled("  ", Style::default()),
        Span::styled(format!("{:>10}", summary.turn_indicator()), timestamp_style),
        Span::styled("  ", Style::default()),
        Span::styled(label, label_style),
    ];
    if let Some(marker) = branch_marker {
        spans.push(Span::styled(
            marker,
            Style::default().fg(crate::render::theme::magenta()),
        ));
    }
    let label_hint = summary.label_hint();
    if !label_hint.is_empty() {
        spans.push(Span::styled("  ", Style::default()));
        spans.push(Span::styled(
            label_hint,
            Style::default().fg(crate::render::theme::secondary()),
        ));
    }
    if cross_project {
        spans.push(Span::styled(
            "  · ",
            Style::default().fg(crate::render::theme::quiet()),
        ));
        spans.push(Span::styled(
            summary.project_hint(),
            Style::default().fg(crate::render::theme::path_hint()),
        ));
    }
    Line::from(spans)
}

fn render_start_fresh_row(active: bool) -> Line<'static> {
    let (prefix_color, label_style, hint_style) = if active {
        (
            crate::render::theme::magenta(),
            Style::default()
                .fg(crate::render::theme::magenta())
                .add_modifier(Modifier::BOLD),
            Style::default().fg(crate::render::theme::quiet()),
        )
    } else {
        (
            crate::render::theme::quiet(),
            Style::default().fg(crate::render::theme::magenta()),
            Style::default().fg(crate::render::theme::quiet()),
        )
    };
    let prefix = if active { "▸ " } else { "  " };
    Line::from(vec![
        Span::styled(prefix, Style::default().fg(prefix_color)),
        Span::styled("◇ ", label_style),
        Span::styled("Start fresh", label_style),
        Span::styled("    — new conversation", hint_style),
    ])
}

fn render_checkbox_row(label: &'static str, checked: bool, active: bool) -> Line<'static> {
    let (prefix_color, label_style) = if active {
        (
            crate::render::theme::accent(),
            Style::default()
                .fg(crate::render::theme::secondary())
                .add_modifier(Modifier::BOLD),
        )
    } else {
        (
            crate::render::theme::quiet(),
            Style::default().fg(crate::render::theme::foreground()),
        )
    };
    let prefix = if active { "▸ " } else { "  " };
    let mark = if checked { "[x] " } else { "[ ] " };
    Line::from(vec![
        Span::styled(prefix, Style::default().fg(prefix_color)),
        Span::styled(mark, Style::default().fg(crate::render::theme::accent())),
        Span::styled(label, label_style),
    ])
}

/// Center a fixed-size area inside `full` with reasonable bounds for small
/// terminals.
fn centered_area(full: Rect) -> Rect {
    let max_width = 98u16;
    let max_height = 22u16;
    let width = full.width.min(max_width);
    let height = full.height.min(max_height);
    let x = full.x + full.width.saturating_sub(width) / 2;
    let y = full.y + full.height.saturating_sub(height) / 2;
    Rect {
        x,
        y,
        width,
        height,
    }
}

fn format_started_at(started_at_ms: u64) -> String {
    // Convert epoch milliseconds to a UTC `YYYY-MM-DD HH:MM` label without
    // pulling in `chrono`. Squeezy already targets sessions started in
    // the last 7 days, so leap years are not a concern but they are
    // handled correctly by `days_to_ymd` below.
    let total_secs = started_at_ms / 1_000;
    let days = (total_secs / 86_400) as i64;
    let secs_of_day = total_secs % 86_400;
    let hour = (secs_of_day / 3_600) as u32;
    let minute = ((secs_of_day % 3_600) / 60) as u32;
    let (year, month, day) = days_to_ymd(days);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}Z")
}

/// Convert days since 1970-01-01 to `(year, month, day)`. Adapted from
/// Howard Hinnant's "chrono::date::ymd_from_days" algorithm so it is
/// dependency-free and exact for any positive day count.
fn days_to_ymd(days_since_epoch: i64) -> (i32, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if m <= 2 { y + 1 } else { y };
    (year as i32, m, d)
}

#[cfg(test)]
#[path = "resume_picker_tests.rs"]
mod tests;
