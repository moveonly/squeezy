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
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
};
use squeezy_core::{AppConfig, SqueezyError};
use squeezy_store::{SessionMetadata, SessionQuery, SessionStore};

use crate::render::palette::{AMBER, GOLD, MODE_PURPLE, QUIET};

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

    pub(crate) fn label(&self) -> String {
        let task = self
            .first_user_task
            .as_deref()
            .or(self.latest_summary.as_deref())
            .unwrap_or("(no prompt recorded)")
            .lines()
            .next()
            .unwrap_or("(no prompt recorded)");
        truncate(task, 80)
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
        truncate(tail, 24)
    }
}

fn truncate(input: &str, limit: usize) -> String {
    if input.chars().count() <= limit {
        return input.to_string();
    }
    let mut out: String = input.chars().take(limit.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResumeChoice {
    StartFresh,
    Resume(String),
    /// Selected session lives outside the current cwd. The TUI exits without
    /// chdir-ing and surfaces the `squeezy sessions resume <id>` invocation
    /// the user should run from `target_cwd` — silently relocating the
    /// process would surprise users juggling sibling repos.
    CrossProject {
        session_id: String,
        target_cwd: String,
    },
    Quit,
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
pub(crate) fn filter_candidates_all_projects(
    sessions: &[SessionMetadata],
    now_ms: u64,
) -> Vec<SessionSummary> {
    filter_inner(sessions, now_ms, |_| true)
}

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

/// State machine driving the picker. Pure — owns no IO.
#[derive(Debug, Clone)]
pub(crate) struct ResumePickerState {
    /// Currently-visible rows, derived from `all_sessions` and the
    /// `show_all_projects` toggle. Recomputed every time the toggle flips.
    pub(crate) candidates: Vec<SessionSummary>,
    /// Full recent list across every cwd; the cwd-scoped view is a filter
    /// over this. Kept on the state so Tab can re-derive `candidates`
    /// without re-reading the session store.
    all_sessions: Vec<SessionSummary>,
    pub(crate) cursor: usize,
    pub(crate) show_all_projects: bool,
    cwd: PathBuf,
}

impl ResumePickerState {
    /// Build a picker over a pre-filtered set of sessions. `all_sessions`
    /// must already be recency-filtered (see `filter_candidates_all_projects`)
    /// so the toggle can flip between scoped and unscoped views without
    /// re-applying the recency filter.
    pub(crate) fn new(all_sessions: Vec<SessionSummary>, cwd: PathBuf) -> Self {
        let cwd_str = cwd.display().to_string();
        let candidates = scoped_view(&all_sessions, &cwd_str);
        Self {
            candidates,
            all_sessions,
            cursor: 0,
            show_all_projects: false,
            cwd,
        }
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

    /// Flip the project scope and re-derive `candidates`. The cursor is
    /// reset to "Start fresh" so the user does not act on a row that
    /// silently moved under them.
    pub(crate) fn toggle_all_projects(&mut self) {
        self.show_all_projects = !self.show_all_projects;
        self.candidates = if self.show_all_projects {
            self.all_sessions.clone()
        } else {
            scoped_view(&self.all_sessions, &self.cwd.display().to_string())
        };
        self.cursor = 0;
    }

    fn select_at_cursor(&self) -> Option<ResumeChoice> {
        if self.cursor == self.start_fresh_index() {
            return Some(ResumeChoice::StartFresh);
        }
        // candidate rows live at indices 1..=N.
        let summary = self.candidates.get(self.cursor - 1)?;
        let cwd_str = self.cwd.display().to_string();
        if summary.cwd == cwd_str {
            Some(ResumeChoice::Resume(summary.session_id.clone()))
        } else {
            Some(ResumeChoice::CrossProject {
                session_id: summary.session_id.clone(),
                target_cwd: summary.cwd.clone(),
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
    all.iter().filter(|s| s.cwd == cwd_str).cloned().collect()
}

/// Pull recent resumable sessions across every cwd. The picker filters
/// down to the current cwd by default but keeps the broader list around
/// so Tab can flip into a cross-project view without a second store read.
/// On error we log to stderr and start fresh — the picker is a convenience,
/// not a hard dependency.
pub(crate) fn load_candidates(config: &AppConfig) -> Vec<SessionSummary> {
    let store = SessionStore::open(config);
    let sessions = match store.list(&SessionQuery::default()) {
        Ok(sessions) => sessions,
        Err(error) => {
            let _: SqueezyError = error;
            eprintln!("squeezy: failed to list sessions for resume picker: {error}");
            return Vec::new();
        }
    };
    let now_ms = current_unix_ms();
    filter_candidates_all_projects(&sessions, now_ms)
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
) -> io::Result<ResumeChoice> {
    let mut state = ResumePickerState::new(all_sessions, cwd);
    if state.candidates.is_empty() && state.all_sessions.is_empty() {
        return Ok(ResumeChoice::StartFresh);
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

    let title = Line::from(vec![
        Span::styled(" ◆ ", Style::default().fg(AMBER)),
        Span::styled(
            "squeezy",
            Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" · ", Style::default().fg(QUIET)),
        Span::styled("resume a recent session", Style::default().fg(Color::White)),
        Span::raw(" "),
    ]);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(AMBER))
        .title(title)
        .title_alignment(Alignment::Left);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(1), // intro
            Constraint::Length(1), // spacer
            Constraint::Min(3),    // list
            Constraint::Length(1), // spacer
            Constraint::Length(1), // footer
        ])
        .split(inner);

    let scope = if state.show_all_projects {
        " recent session{} across all projects"
    } else {
        " recent session{} for this directory"
    };
    let intro = Paragraph::new(Line::from(vec![
        Span::styled(
            format!("{}", state.candidates.len()),
            Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            scope.replace("{}", if state.candidates.len() == 1 { "" } else { "s" }),
            Style::default().fg(QUIET),
        ),
    ]))
    .alignment(Alignment::Left);
    frame.render_widget(intro, layout[0]);

    // Start fresh leads the list as the safe default (cursor opens on it),
    // followed by each candidate session at index 1..=N.
    let cwd_str = state.cwd.display().to_string();
    let mut rows: Vec<Line<'_>> = Vec::with_capacity(state.candidates.len() + 1);
    rows.push(render_start_fresh_row(
        state.cursor == state.start_fresh_index(),
    ));
    rows.extend(state.candidates.iter().enumerate().map(|(idx, summary)| {
        // candidates start at row 1; active row uses the cursor offset.
        let row_idx = idx + 1;
        let cross_project = summary.cwd != cwd_str;
        render_candidate_row(idx, summary, row_idx == state.cursor, cross_project)
    }));

    let body = Paragraph::new(rows);
    frame.render_widget(body, layout[2]);

    let tab_hint = if state.show_all_projects {
        "this dir"
    } else {
        "all projects"
    };
    let footer = Paragraph::new(Line::from(vec![
        Span::styled("↑/↓ ", Style::default().fg(GOLD)),
        Span::styled("move  ", Style::default().fg(QUIET)),
        Span::styled("Enter ", Style::default().fg(GOLD)),
        Span::styled("confirm  ", Style::default().fg(QUIET)),
        Span::styled("Tab ", Style::default().fg(GOLD)),
        Span::styled(format!("{tab_hint}  "), Style::default().fg(QUIET)),
        Span::styled("Esc ", Style::default().fg(GOLD)),
        Span::styled("start fresh  ", Style::default().fg(QUIET)),
        Span::styled("Q ", Style::default().fg(GOLD)),
        Span::styled("quit", Style::default().fg(QUIET)),
    ]))
    .alignment(Alignment::Left);
    frame.render_widget(footer, layout[4]);
}

fn render_candidate_row(
    _idx: usize,
    summary: &SessionSummary,
    active: bool,
    cross_project: bool,
) -> Line<'static> {
    let (prefix_color, label_style) = if active {
        (
            AMBER,
            Style::default().fg(GOLD).add_modifier(Modifier::BOLD),
        )
    } else {
        (QUIET, Style::default().fg(Color::White))
    };
    let prefix = if active { "▸ " } else { "  " };
    let timestamp_style = if active {
        Style::default().fg(AMBER)
    } else {
        Style::default().fg(QUIET)
    };
    let mut spans = vec![
        Span::styled(prefix, Style::default().fg(prefix_color)),
        Span::styled(format_started_at(summary.started_at_ms), timestamp_style),
        Span::styled("  ", Style::default()),
        Span::styled(format!("{:>10}", summary.turn_indicator()), timestamp_style),
        Span::styled("  ", Style::default()),
        Span::styled(summary.label(), label_style),
    ];
    if cross_project {
        spans.push(Span::styled("  · ", Style::default().fg(QUIET)));
        spans.push(Span::styled(
            summary.project_hint(),
            Style::default().fg(MODE_PURPLE),
        ));
    }
    Line::from(spans)
}

fn render_start_fresh_row(active: bool) -> Line<'static> {
    let (prefix_color, label_style, hint_style) = if active {
        (
            MODE_PURPLE,
            Style::default()
                .fg(MODE_PURPLE)
                .add_modifier(Modifier::BOLD),
            Style::default().fg(QUIET),
        )
    } else {
        (
            QUIET,
            Style::default().fg(MODE_PURPLE),
            Style::default().fg(QUIET),
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

/// Center a fixed-size area inside `full` with reasonable bounds for small
/// terminals.
fn centered_area(full: Rect) -> Rect {
    let max_width = 86u16;
    let max_height = 18u16;
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
