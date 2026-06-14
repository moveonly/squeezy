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
    layout::{Alignment, Constraint, Direction, Layout},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap},
};
use squeezy_core::{AppConfig, SqueezyError};
use squeezy_store::{
    EventBranchTip, GlobalSessionIndexEntry, SessionMetadata, SessionQuery, SessionStore,
    detect_branches, paths_same,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::modal;

/// Maximum number of sessions shown in the overlay, newest-first. The picker
/// is opt-in (`--resume`) and scrolls, so the user came here deliberately to
/// hunt for a session — show a deep list rather than an arbitrarily short one.
pub(crate) const MAX_PICKER_ENTRIES: usize = 100;

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
    /// Set when branch detection failed because `events.jsonl` could not
    /// be opened or parsed (e.g. a transient file-lock on Windows). The
    /// picker renders the session as linear and marks the row so the user
    /// knows branch data may be missing.
    pub(crate) branch_load_failed: bool,
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
            branch_load_failed: false,
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
            branch_load_failed: false,
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
    StartFresh,
    Resume {
        session_id: String,
        /// When `Some(sequence)`, the user picked a branch tip in a
        /// branched session and the caller should resume at that specific
        /// tip rather than the latest event. `None` for linear sessions
        /// (the common case), where the resume flow is unchanged.
        branch_tip: Option<u64>,
    },
    /// Selected session lives outside the current cwd. The caller re-roots the
    /// workspace at `target_cwd` and resumes in place; only if that directory
    /// is gone does it fall back to surfacing the `squeezy sessions resume
    /// <id>` invocation to run from there.
    CrossProject {
        session_id: String,
        target_cwd: String,
    },
    Back,
    Quit,
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

    /// Construct a picker row pinned to a specific branch tip. Not yet called
    /// by `expand_entries` because branch-aware resume is not yet implemented;
    /// retained here so the schema is ready when that feature lands.
    #[allow(dead_code)]
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
    filter_inner(sessions, now_ms, |meta| paths_same(&meta.cwd, &cwd_str))
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
        .filter(|meta| session_metadata_has_resume_content(meta))
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
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut out: Vec<SessionSummary> =
        Vec::with_capacity((local.len() + global.len()).min(MAX_PICKER_ENTRIES));
    for meta in local {
        if !meta.resume_available {
            continue;
        }
        if !session_metadata_has_resume_content(meta) {
            continue;
        }
        if now_ms.saturating_sub(meta.started_at_ms) > RECENT_WINDOW_MS {
            continue;
        }
        if !seen.insert(meta.session_id.as_str()) {
            continue;
        }
        out.push(SessionSummary::from_metadata(meta));
    }
    for entry in global {
        if !entry.resume_available {
            continue;
        }
        if !global_index_entry_has_resume_content(entry) {
            continue;
        }
        if now_ms.saturating_sub(entry.started_at_ms) > RECENT_WINDOW_MS {
            continue;
        }
        if !seen.insert(entry.session_id.as_str()) {
            continue;
        }
        out.push(SessionSummary::from_global_index_entry(entry));
    }
    out.sort_by_key(|summary| std::cmp::Reverse(summary.started_at_ms));
    out.truncate(MAX_PICKER_ENTRIES);
    out
}

fn nonblank(value: Option<&str>) -> bool {
    value.is_some_and(|value| !value.trim().is_empty())
}

fn session_metadata_has_resume_content(meta: &SessionMetadata) -> bool {
    meta.metrics.turns > 0
        || nonblank(meta.first_user_task.as_deref())
        || nonblank(meta.latest_summary.as_deref())
        || nonblank(meta.display_name.as_deref())
}

fn global_index_entry_has_resume_content(entry: &GlobalSessionIndexEntry) -> bool {
    entry.turn_count > 0
        || nonblank(entry.title.as_deref())
        || nonblank(entry.display_name.as_deref())
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
        self.candidates.len() + 1
    }

    /// Index of the "Start fresh" row — always 0 so the safe default is
    /// pre-selected when the picker opens.
    pub(crate) const fn start_fresh_index(&self) -> usize {
        0
    }

    /// Number of recent sessions that live outside the current cwd — the rows
    /// the Tab toggle would reveal. Counted off `all_sessions` (the unscoped
    /// recency-filtered list) so the scoped view can advertise the toggle even
    /// when it has no rows of its own to show.
    fn other_project_count(&self) -> usize {
        let cwd_str = self.cwd.display().to_string();
        self.all_sessions
            .iter()
            .filter(|summary| !paths_same(&summary.cwd, &cwd_str))
            .count()
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
            return Some(ResumeChoice::StartFresh);
        }
        // candidate rows live at indices 1..=N.
        let entry = self.candidates.get(self.cursor - 1)?;
        let cwd_str = self.cwd.display().to_string();
        if paths_same(&entry.summary.cwd, &cwd_str) {
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
            (KeyCode::Enter, _) => self.select_at_cursor(),
            (KeyCode::Esc, _) | (KeyCode::Char('n'), _) | (KeyCode::Char('N'), _) => {
                Some(ResumeChoice::StartFresh)
            }
            (KeyCode::Char('q'), _) | (KeyCode::Char('Q'), _) => Some(ResumeChoice::Quit),
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => Some(ResumeChoice::Quit),
            _ => None,
        }
    }
}

fn scoped_view(all: &[SessionSummary], cwd_str: &str) -> Vec<SessionSummary> {
    all.iter()
        .filter(|s| paths_same(&s.cwd, cwd_str))
        .cloned()
        .collect()
}

pub(crate) fn has_scoped_candidates(all: &[SessionSummary], cwd: &Path) -> bool {
    let cwd_str = cwd.display().to_string();
    all.iter().any(|summary| paths_same(&summary.cwd, &cwd_str))
}

/// Flatten each summary into selectable rows. All sessions — including those
/// with multiple branch tips — emit a single `PickerEntry` so the picker only
/// shows resumable rows. Branch-aware resume (replaying up to a chosen
/// `parent_event_sequence`) is not yet implemented; showing multiple per-tip
/// rows and ignoring the selected tip would silently resume the wrong state,
/// so branched sessions collapse to a single linear entry until that feature
/// lands.
fn expand_entries(summaries: Vec<SessionSummary>) -> Vec<PickerEntry> {
    summaries.into_iter().map(PickerEntry::linear).collect()
}

/// Pull recent resumable sessions across every cwd. The picker filters
/// down to the current cwd by default but keeps the broader list around
/// so Tab can flip into a cross-project view without a second store read.
/// Per-project entries (richer state) merge over the cross-project index
/// snapshots so sibling-repo sessions surface alongside the local ones.
/// On error we log to stderr and start fresh — the picker is a
/// convenience, not a hard dependency.
/// Build the picker candidate list (scoped + recency-filtered + capped)
/// without the per-candidate event-log reads. Callers that only need the
/// set of recent sessions — e.g. the startup gate deciding whether to offer
/// a resume question — use this to avoid touching `events.jsonl` at all.
pub(crate) fn load_candidate_summaries(config: &AppConfig) -> Vec<SessionSummary> {
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
    merge_candidates_for_picker(&local, &global, now_ms)
}

pub(crate) fn load_candidates(config: &AppConfig) -> Vec<SessionSummary> {
    let store = SessionStore::open(config);
    let mut summaries = load_candidate_summaries(config);
    // Branch detection requires reading each candidate's event log.  The
    // list is already capped so this stays cheap on cold start.  On
    // failure (e.g. a transient file-lock on Windows) we mark the session
    // with `branch_load_failed` so the picker can surface the ambiguity
    // rather than silently rendering as linear.
    for summary in &mut summaries {
        match store.show(&summary.session_id) {
            Ok(record) => summary.branches = detect_branches(&record.events),
            Err(_) => summary.branch_load_failed = true,
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
    // Only skip the picker entirely when there is genuinely nothing to resume.
    // When the scoped view is empty but recent sessions exist in *other*
    // projects, still draw a frame: the "Start fresh" row plus a Tab hint teach
    // the toggle that would surface those sessions, instead of silently
    // dropping the user into a fresh session unaware they exist.
    if state.candidates.is_empty() && state.other_project_count() == 0 {
        return Ok(ResumeChoice::StartFresh);
    }
    let choice = loop {
        terminal.draw(|frame| render_picker(frame, &state))?;
        match event::read()? {
            Event::Key(key) => {
                if let Some(choice) = state.dispatch(key) {
                    break choice;
                }
            }
            Event::Resize(_, _) => continue,
            _ => continue,
        }
    };
    // The loop drew the modal block at least once; clear the shared terminal
    // exactly once on close so the block leaves no ghost rows behind. The
    // early `StartFresh` return above never drew, so it skips this.
    modal::clear_after_close(terminal)?;
    Ok(choice)
}

fn render_picker(frame: &mut ratatui::Frame<'_>, state: &ResumePickerState) {
    let full = frame.area();

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
    // Most of the terminal so long session labels have room, capped so the
    // overlay stays scannable (and centered) on ultra-wide monitors.
    // The resume picker paints before the persisted glyph mode is restored, so
    // it draws with the built-in Unicode chrome.
    let inner = modal::surface(
        frame,
        full,
        160,
        32,
        title,
        crate::glyph_mode::GlyphMode::DEFAULT,
    );

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(1), // question/title
            Constraint::Length(1), // count
            Constraint::Length(1), // spacer
            Constraint::Min(3),    // list
            Constraint::Length(1), // spacer
            Constraint::Length(3), // footer (legend line + two hint lines)
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
            state.candidates.len().to_string(),
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

    let cwd_str = state.cwd.display().to_string();
    // Resolve the viewport first: the list scrolls so the cursor stays on
    // screen, and a scrollbar steals a column when it overflows. Knowing the
    // real content width up front lets candidate labels ellipsise to fit
    // instead of hard-cropping into the scrollbar.
    let list_area = layout[3];
    let visible = list_area.height.max(1) as usize;
    let total = state.candidates.len() + 1; // Start fresh + candidates
    let (body_area, scrollbar_area) = if total > visible {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(list_area);
        (chunks[0], Some(chunks[1]))
    } else {
        (list_area, None)
    };
    let content_width = body_area.width as usize;

    // Start fresh leads the list as the safe default (cursor opens on it),
    // followed by each candidate session at index 1..=N.
    let mut rows: Vec<Line<'_>> = Vec::with_capacity(total);
    rows.push(render_start_fresh_row(
        state.cursor == state.start_fresh_index(),
    ));
    rows.extend(state.candidates.iter().enumerate().map(|(idx, entry)| {
        // candidates start at row 1; active row uses the cursor offset.
        let row_idx = idx + 1;
        let cross_project = !paths_same(&entry.summary.cwd, &cwd_str);
        render_candidate_row(entry, row_idx == state.cursor, cross_project, content_width)
    }));

    let offset = if total <= visible {
        0
    } else {
        state
            .cursor
            .saturating_sub(visible / 2)
            .min(total - visible)
    };
    frame.render_widget(Paragraph::new(rows).scroll((offset as u16, 0)), body_area);
    if let Some(sb_area) = scrollbar_area {
        let mut sb_state = ScrollbarState::new(total).position(state.cursor);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None),
            sb_area,
            &mut sb_state,
        );
    }

    // When the highlighted row is a session from another directory, spell out
    // that confirming will switch into that directory before resuming — the
    // inline `↪ project` marker can be truncated on a narrow terminal, this
    // line is not.
    if let Some(entry) = state
        .cursor
        .checked_sub(1)
        .and_then(|idx| state.candidates.get(idx))
        && !paths_same(&entry.summary.cwd, &cwd_str)
    {
        let hint = Line::from(vec![
            Span::styled(" ↪ ", Style::default().fg(crate::render::theme::accent())),
            Span::styled(
                format!("Enter switches to {} and resumes there", entry.summary.cwd),
                Style::default().fg(crate::render::theme::secondary()),
            ),
        ]);
        frame.render_widget(Paragraph::new(hint), layout[4]);
    } else if state.candidates.is_empty() {
        // Scoped view has no rows of its own. If recent sessions exist in other
        // projects, teach the Tab toggle that would reveal them — otherwise the
        // user never learns the resume path is one keystroke away.
        let other = state.other_project_count();
        if other > 0 {
            let hint = Line::from(vec![
                Span::styled(
                    " Tab ",
                    Style::default().fg(crate::render::theme::secondary()),
                ),
                Span::styled(
                    format!(
                        "— {other} session{} in other projects",
                        if other == 1 { "" } else { "s" }
                    ),
                    Style::default().fg(crate::render::theme::quiet()),
                ),
            ]);
            frame.render_widget(Paragraph::new(hint), layout[4]);
        }
    }

    let tab_hint = if state.show_all_projects {
        "this dir"
    } else {
        "all projects"
    };
    let mut footer_lines = Vec::with_capacity(3);
    // A lone `!` on a row is cryptic, so when any candidate failed to load its
    // branch data (e.g. a transient file-lock) explain what the marker means and
    // that the session still resumes safely at its latest event.
    if state
        .candidates
        .iter()
        .any(|e| e.summary.branch_load_failed)
    {
        footer_lines.push(Line::from(vec![
            Span::styled("! ", Style::default().fg(crate::render::theme::warn())),
            Span::styled(
                "— branch data unavailable; resumes at latest event",
                Style::default().fg(crate::render::theme::quiet()),
            ),
        ]));
    }
    let mut first_line = vec![
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
            "confirm  ",
            Style::default().fg(crate::render::theme::quiet()),
        ),
    ];
    // In the cross-project view the always-visible "confirm" wording hides the
    // real consequence: confirming a foreign row re-roots the workspace into
    // that directory. Spell it out so it isn't discoverable only on hover.
    if state.show_all_projects {
        first_line.push(Span::styled(
            "· cross-project rows change your working directory",
            Style::default().fg(crate::render::theme::quiet()),
        ));
    }
    footer_lines.push(Line::from(first_line));
    let mut second_line = Vec::with_capacity(if state.can_go_back() { 8 } else { 6 });
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
    entry: &PickerEntry,
    active: bool,
    cross_project: bool,
    content_width: usize,
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
    // `expand_entries` currently produces only linear entries, so
    // `branch_tip` is always `None` here. The branch rendering path is
    // preserved so it activates automatically when branch-aware resume
    // populates `branch_tip` in the future.
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
    let label_hint = summary.label_hint();
    let timestamp = format_started_at(summary.started_at_ms);
    let turns = format!("{:>10}", summary.turn_indicator());
    let project_hint = if cross_project {
        summary.project_hint()
    } else {
        String::new()
    };
    // A `!` marker is added when branch data could not be loaded (e.g. a
    // transient file-lock on Windows) so the user sees the ambiguity.
    let branch_warn = if summary.branch_load_failed {
        "  !"
    } else {
        ""
    };
    // Ellipsise the label to the width left after the fixed prefix and the
    // trailing markers, so long session titles never hard-crop into the
    // scrollbar and the ↪ cross-project marker stays visible.
    let branch_len = branch_marker
        .as_ref()
        .map_or(0, |m| UnicodeWidthStr::width(m.as_str()))
        + UnicodeWidthStr::width(branch_warn);
    let hint_len = if label_hint.is_empty() {
        0
    } else {
        2 + UnicodeWidthStr::width(label_hint.as_str())
    };
    let cross_len = if cross_project {
        4 + UnicodeWidthStr::width(project_hint.as_str())
    } else {
        0
    };
    let fixed = prefix.chars().count() + timestamp.chars().count() + 2 + turns.chars().count() + 2;
    let label_budget = content_width
        .saturating_sub(fixed + branch_len + hint_len + cross_len)
        .max(8);
    let label = truncate_label(&label, label_budget);

    let mut spans = Vec::with_capacity(13);
    spans.push(Span::styled(prefix, Style::default().fg(prefix_color)));
    spans.push(Span::styled(timestamp, timestamp_style));
    spans.push(Span::styled("  ", Style::default()));
    spans.push(Span::styled(turns, timestamp_style));
    spans.push(Span::styled("  ", Style::default()));
    spans.push(Span::styled(label, label_style));
    if let Some(marker) = branch_marker {
        spans.push(Span::styled(
            marker,
            Style::default().fg(crate::render::theme::magenta()),
        ));
    }
    if summary.branch_load_failed {
        spans.push(Span::styled(
            branch_warn,
            Style::default().fg(crate::render::theme::warn()),
        ));
    }
    if !label_hint.is_empty() {
        spans.push(Span::styled("  ", Style::default()));
        spans.push(Span::styled(
            label_hint,
            Style::default().fg(crate::render::theme::secondary()),
        ));
    }
    if cross_project {
        // ↪ signals that picking this row switches to another directory,
        // distinguishing it from same-project sessions at a glance.
        spans.push(Span::styled(
            "  ↪ ",
            Style::default().fg(crate::render::theme::accent()),
        ));
        spans.push(Span::styled(
            project_hint,
            Style::default().fg(crate::render::theme::path_hint()),
        ));
    }
    Line::from(spans)
}

/// Truncate a single-line label to `max` display columns, appending an
/// ellipsis when it overflows so the cut is visibly intentional.
fn truncate_label(label: &str, max: usize) -> String {
    if UnicodeWidthStr::width(label) <= max {
        return label.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut width = 0;
    for c in label.chars() {
        let cw = c.width().unwrap_or(0);
        if width + cw > max - 1 {
            break;
        }
        out.push(c);
        width += cw;
    }
    out.push('…');
    out
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
