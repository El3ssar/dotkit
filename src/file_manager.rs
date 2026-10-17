//! Tracked-file inventory and a staged editor. No filesystem discovery or side effects on open.
use crate::cli::{build_cli, Args};
use crate::core::*;
use crate::tui::{Editor, Outcome};
use crate::util::*;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    layout::Constraint,
    style::{Color, Style, Stylize},
    text::Line,
    widgets::{Block, Cell, Paragraph, Row, Table, TableState},
    Frame,
};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, IsTerminal};

#[derive(Clone, Copy, PartialEq)]
enum Copy {
    Save,
    TakeRepo,
}
#[derive(Default, Clone)]
struct Pending {
    copy: Option<Copy>,
    remove: bool,
    ssh: Option<bool>,
}
impl Pending {
    fn count(&self) -> usize {
        usize::from(self.copy.is_some())
            + usize::from(self.remove)
            + usize::from(self.ssh.is_some())
    }
}
struct FileRow {
    path: String,
    state: String,
    ssh: bool,
    ignored: bool,
    saved: bool,
    home_digest: Option<String>,
    repo_digest: Option<String>,
}
fn clean(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}
fn inventory() -> Vec<FileRow> {
    let synced = load_synced();
    let saved = crate::files::saved_changes();
    managed(None)
        .into_iter()
        .map(|path| {
            let excluded_here = ignored_here(&path);
            let state = if excluded_here {
                "excluded here".into()
            } else {
                match classify(&path, &synced) {
                    Ok(Some(code)) => label(code).into(),
                    Ok(None) => "in sync".into(),
                    Err(e) => e,
                }
            };
            FileRow {
                ssh: !ignored(&path, &["all", "linux", "remote"]),
                saved: saved.iter().any(|(_, p)| p == &format!("home/{path}")),
                home_digest: digest(read_home_entry(&path).as_ref()),
                repo_digest: digest(
                    source_entry(&path, &local_contexts())
                        .ok()
                        .flatten()
                        .as_ref(),
                ),
                path,
                state,
                ignored: excluded_here,
            }
        })
        .collect()
}

pub fn command(args: &Args) {
    let rows = inventory();
    if args.flag("json") || args.subcommand().is_some_and(|(_, sub)| sub.flag("json")) {
        let rows: Vec<_> = rows
            .iter()
            .map(|r| serde_json::json!({"path":r.path,"state":r.state,"ssh":r.ssh,"saved":r.saved}))
            .collect();
        out(&serde_json::to_string_pretty(&rows).unwrap());
    } else if args.subcommand().is_some() || (!args.flag("tui") && !io::stdin().is_terminal()) {
        for r in rows {
            out(&format!(
                "  ~/{}  · {}{} · SSH/Linux: {}",
                clean(&r.path),
                clean(&r.state),
                if r.saved {
                    " · saved, not committed"
                } else {
                    ""
                },
                if r.ssh { "included" } else { "excluded" }
            ));
        }
    } else if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        fail("kit files --tui needs an interactive terminal; use kit files list or --json");
    } else {
        let mut editor = FileEditor::new(rows);
        match crate::tui::run(&mut editor) {
            Ok(true) => editor.apply(),
            Ok(false) => say("file changes cancelled"),
            Err(e) => fail(&format!("file manager: {e}")),
        }
    }
}

struct FileEditor {
    rows: Vec<FileRow>,
    selected: BTreeSet<String>,
    queued: BTreeMap<String, Pending>,
    table: TableState,
    query: String,
    searching: bool,
    message: String,
    page_rows: usize,
    patch: Option<String>,
    overlay: Option<(bool, String)>,
    scroll: u16,
    scroll_x: u16,
    ignore_snapshot: Vec<u8>,
}
impl FileEditor {
    fn new(rows: Vec<FileRow>) -> Self {
        Self { rows, selected: BTreeSet::new(), queued: BTreeMap::new(),
            table: TableState::default().with_selected(0), query: String::new(), searching: false,
            message: "Blue = pending. Enter applies; q cancels. Only explicitly tracked files are listed.".into(),
            page_rows: 5, patch: None, overlay: None, scroll: 0, scroll_x: 0, ignore_snapshot: std::fs::read(&ctx().ignore).unwrap_or_default() }
    }
    fn visible(&self) -> Vec<usize> {
        let query = self.query.to_lowercase();
        self.rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.path.to_lowercase().contains(&query))
            .map(|(i, _)| i)
            .collect()
    }
    fn hidden_selected(&self) -> usize {
        let query = self.query.to_lowercase();
        self.selected
            .iter()
            .filter(|name| !name.to_lowercase().contains(&query))
            .count()
    }
    fn current(&self) -> Option<usize> {
        self.visible()
            .get(self.table.selected().unwrap_or(0))
            .copied()
    }
    fn navigate(&mut self, delta: isize) {
        let max = self.visible().len().saturating_sub(1);
        self.table.select(Some(
            self.table
                .selected()
                .unwrap_or(0)
                .saturating_add_signed(delta)
                .min(max),
        ));
    }
    fn show_diff(&mut self) {
        if let Some(i) = self.current() {
            let r = &self.rows[i];
            let src = source_entry(&r.path, &local_contexts()).ok().flatten();
            let here = read_home_entry(&r.path);
            // Bound expensive diffs. Large/binary files remain manageable without opening contents.
            let size = |e: &Option<Entry>| match e {
                Some(Entry::File(b, _)) => b.len(),
                _ => 0,
            };
            self.patch = Some(if size(&src).max(size(&here)) > 1024 * 1024 {
                "File exceeds 1 MiB; inspect it outside the TUI.".into()
            } else {
                crate::diff::diff_text(
                    &clean(&r.path),
                    src.as_ref(),
                    here.as_ref(),
                    "repo",
                    "this machine",
                    &r.state,
                )
            });
            self.scroll = 0;
            self.scroll_x = 0;
        }
    }
    fn review(&mut self) {
        let lines = self
            .queued
            .iter()
            .map(|(path, p)| {
                let row = self.rows.iter().find(|r| &r.path == path).unwrap();
                let mut actions = vec![];
                match p.copy {
                    Some(Copy::Save) if row.state == "deleted here" => actions
                        .push("SAVE DELETION: remove repo copy; local file is already absent"),
                    Some(Copy::Save) => actions.push("save local version to repo"),
                    Some(Copy::TakeRepo) => {
                        actions.push("replace local with repo version (backed up)")
                    }
                    None => {}
                }
                if let Some(ssh) = p.ssh {
                    actions.push(if ssh {
                        "include in SSH/Linux"
                    } else {
                        "exclude from SSH/Linux"
                    });
                }
                if p.remove {
                    actions.push("untrack; keep local file");
                }
                format!("~/{}: {}", clean(path), actions.join("; "))
            })
            .collect::<Vec<_>>()
            .join("\n");
        self.overlay = Some((true, format!("REVIEW ALL PENDING ACTIONS\n\n{}\n\nEnter applies these actions. Esc / Tab returns to editing. Ctrl-C cancels.", if lines.is_empty() { "Nothing queued." } else { &lines })));
        self.scroll = 0;
    }
    fn apply(self) {
        if self.queued.is_empty() {
            say("no file changes queued");
            return;
        }
        guard_repo();
        if std::fs::read(&ctx().ignore).unwrap_or_default() != self.ignore_snapshot {
            fail(".kitignore changed while the manager was open; reopen kit files before applying");
            return;
        }
        for r in self
            .rows
            .iter()
            .filter(|r| self.queued.contains_key(&r.path))
        {
            if digest(read_home_entry(&r.path).as_ref()) != r.home_digest
                || digest(
                    source_entry(&r.path, &local_contexts())
                        .ok()
                        .flatten()
                        .as_ref(),
                ) != r.repo_digest
            {
                fail(&format!(
                    "~/{} changed while the manager was open; no queued actions applied",
                    clean(&r.path)
                ));
                return;
            }
        }
        for (path, pending) in &self.queued {
            let paths = vec![path.clone()];
            match pending.copy {
                Some(Copy::Save) => {
                    crate::files::re_add(Some(&paths), false);
                }
                Some(Copy::TakeRepo) => {
                    crate::files::apply_files(Some(&paths), true);
                }
                None => {}
            }
            if exit_code() != 0 {
                warn("remaining file actions not applied after a failed copy");
                return;
            }
            if let Some(enabled) = pending.ssh {
                set_remote_scope(path, enabled);
            }
            if pending.remove && managed(None).contains(path) {
                let m = build_cli().get_matches_from(["kit", "rm", &format!("~/{path}")]);
                crate::files::cmd_rm(&Args::new(m.subcommand().unwrap().1.clone()));
            }
        }
        if exit_code() == 0 {
            say("file changes applied; kit status reviews them, kit sync backs up saved changes");
        } else {
            warn("some file actions failed; review the errors and kit status");
        }
    }
}

fn set_remote_scope(path: &str, enabled: bool) {
    let text = std::fs::read_to_string(&ctx().ignore).unwrap_or_default();
    let rule = format!(
        "{}{path}",
        if enabled { "!/" } else { "/" },
        path = glob_escape(path)
    );
    let positive = format!("/{}  # kit files", glob_escape(path));
    let negative = format!("!{positive}");
    let text = text
        .lines()
        .filter(|l| *l != positive && *l != negative)
        .collect::<Vec<_>>()
        .join("\n");
    let marker = format!("\n[remote]\n{rule}  # kit files\n");
    // Appending an exact-path override handles wildcard rules and other sections consistently.
    if let Err(e) = write_atomic(&ctx().ignore, (text + &marker).as_bytes(), 0o644) {
        fail(&format!("cannot update SSH scope: {e}"));
    }
    reset_ignore_cache();
}

impl Editor for FileEditor {
    fn key(&mut self, key: KeyEvent) -> Option<Outcome> {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c' | 'd'))
        {
            return Some(Outcome::Cancel);
        }
        // Modified character chords are not plain action keys (Ctrl-U must not update tools).
        // Arrow/navigation modifiers remain available for terminal key-sequence compatibility.
        if matches!(key.code, KeyCode::Char(_))
            && key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return None;
        }
        if self.page_rows == 0 {
            return match key.code {
                KeyCode::Esc | KeyCode::Char('q') => Some(Outcome::Cancel),
                _ => None,
            };
        }
        if let Some((review, _)) = &self.overlay {
            match key.code {
                KeyCode::Enter if *review => return Some(Outcome::Apply),
                KeyCode::Esc | KeyCode::Char('q' | '?') | KeyCode::Tab => self.overlay = None,
                KeyCode::Down | KeyCode::Char('j') => self.scroll = self.scroll.saturating_add(1),
                KeyCode::Up | KeyCode::Char('k') => self.scroll = self.scroll.saturating_sub(1),
                KeyCode::PageDown => {
                    self.scroll = self.scroll.saturating_add(self.page_rows as u16)
                }
                KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(self.page_rows as u16),
                KeyCode::Home => self.scroll = 0,
                KeyCode::End => self.scroll = u16::MAX,
                _ => {}
            }
            return None;
        }
        if self.patch.is_some() {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q' | 'v') => self.patch = None,
                KeyCode::Down | KeyCode::Char('j') => self.scroll = self.scroll.saturating_add(1),
                KeyCode::Up | KeyCode::Char('k') => self.scroll = self.scroll.saturating_sub(1),
                KeyCode::PageDown => {
                    self.scroll = self.scroll.saturating_add(self.page_rows as u16)
                }
                KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(self.page_rows as u16),
                KeyCode::Home => self.scroll = 0,
                KeyCode::End => self.scroll = u16::MAX,
                KeyCode::Left => self.scroll_x = self.scroll_x.saturating_sub(4),
                KeyCode::Right => self.scroll_x = self.scroll_x.saturating_add(4),
                _ => {}
            }
            return None;
        }
        if self.searching {
            match key.code {
                KeyCode::Esc | KeyCode::Enter => self.searching = false,
                KeyCode::Backspace => {
                    self.query.pop();
                    self.table.select(Some(0));
                }
                KeyCode::Char(c)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    self.query.push(c);
                    self.table.select(Some(0));
                }
                KeyCode::Down => self.navigate(1),
                KeyCode::Up => self.navigate(-1),
                _ => {}
            }
            return None;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Some(Outcome::Cancel),
            KeyCode::Enter => {
                if self.queued.iter().any(|(path, p)| {
                    p.copy == Some(Copy::Save)
                        && self
                            .rows
                            .iter()
                            .any(|r| &r.path == path && r.state == "deleted here")
                }) {
                    self.review();
                } else {
                    return Some(Outcome::Apply);
                }
            }
            KeyCode::Tab => self.review(),
            KeyCode::Char('?') => {
                self.overlay = Some((false, "FILE MANAGER\n\nOnly explicitly tracked files appear. Space toggles selection; a selects all matching rows; A clears selection while keeping queued actions. Actions affect ALL selected files, including hidden matches; without a selection, only the cursor row. c clears selections and pending actions.\n\n/ filters names; Enter finishes search. Clear the filter with / then Enter. Arrows, j/k, Page Up/Down, Home/End navigate.\n\nv compares repo (-) with local (+). Left/Right scroll long lines horizontally; Up/Down/Page Up/Down/Home/End scroll vertically, v/Esc returns. Secret contents are hidden.\n\ns toggles saving the local version to the repo. A tracked file deleted locally queues SAVE DELETION and requires review before applying. Missing local files cannot be saved. Unsafe conflicting edits are refused; kit sync merges them.\n\nr toggles replacing the local version with the repo version; Kit keeps a backup for kit undo. d toggles untracking while keeping the local file. p toggles SSH/Linux inclusion; macOS destinations use their platform rules.\n\nBlue values are pending. Tab reviews ALL actions, including hidden rows. Enter applies; q/Esc cancels the manager. No writes happen until applying.\n\nEsc / ? closes help.".into()));
                self.scroll = 0;
            }
            KeyCode::Down | KeyCode::Char('j') => self.navigate(1),
            KeyCode::Up | KeyCode::Char('k') => self.navigate(-1),
            KeyCode::PageDown => self.navigate(self.page_rows as isize),
            KeyCode::PageUp => self.navigate(-(self.page_rows as isize)),
            KeyCode::Home => self.table.select(Some(0)),
            KeyCode::End => self.navigate(isize::MAX),
            KeyCode::Char('/') => {
                self.query.clear();
                self.searching = true;
                self.table.select(Some(0));
            }
            KeyCode::Char('v') => self.show_diff(),
            KeyCode::Char(' ') => {
                if let Some(i) = self.current() {
                    let p = self.rows[i].path.clone();
                    if !self.selected.remove(&p) {
                        self.selected.insert(p);
                    }
                }
            }
            KeyCode::Char('a') => {
                for i in self.visible() {
                    self.selected.insert(self.rows[i].path.clone());
                }
            }
            KeyCode::Char('A') => self.selected.clear(),
            KeyCode::Char('c') => {
                self.selected.clear();
                self.queued.clear();
                self.message = "Queue and selection cleared.".into();
            }
            KeyCode::Char(c @ ('s' | 'r' | 'd' | 'p')) => {
                let names: Vec<String> = if self.selected.is_empty() {
                    self.current()
                        .map(|i| self.rows[i].path.clone())
                        .into_iter()
                        .collect()
                } else {
                    self.selected.iter().cloned().collect()
                };
                for name in names {
                    let row = self.rows.iter().find(|r| r.path == name).unwrap();
                    if row.ignored && matches!(c, 's' | 'r') {
                        self.message = "File excluded on this OS; edit .kitignore before copying either version.".into();
                        continue;
                    }
                    if c == 's'
                        && matches!(
                            row.state.as_str(),
                            "missing here" | "changed in repo" | "in sync"
                        )
                    {
                        self.message = match row.state.as_str() {
                            "missing here" => "Nothing to save: local file is absent. r takes the repo version; d stops tracking.",
                            "changed in repo" => "Only the repo changed. r takes its version; saving local would overwrite unseen changes.",
                            _ => "Already in sync; no local changes to save.",
                        }.into();
                        continue;
                    }
                    let p = self.queued.entry(name.clone()).or_default();
                    match c {
                        's' | 'r' => {
                            let copy = if c == 's' { Copy::Save } else { Copy::TakeRepo };
                            p.copy = if p.copy == Some(copy) {
                                None
                            } else {
                                Some(copy)
                            };
                        }
                        'd' => p.remove = !p.remove,
                        'p' => {
                            let next = !p.ssh.unwrap_or(row.ssh);
                            p.ssh = (next != row.ssh).then_some(next);
                        }
                        _ => unreachable!(),
                    }
                    if p.count() == 0 {
                        self.queued.remove(&name);
                    }
                    self.message = "Blue = pending. Take repo replaces local contents with a backup. Enter applies; q cancels.".into();
                }
            }
            _ => {}
        }
        None
    }
    fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();
        if area.width < 40 || area.height < 15 {
            self.page_rows = 0;
            frame.render_widget(
                Paragraph::new(
                    "Window too small. Resize to inspect queued actions. q / Esc cancels.",
                ),
                area,
            );
            return;
        }
        let [heading, search, table, details, footer] = crate::tui::panels(area, false);
        let compact = area.width < 90 || area.height < 23;
        self.page_rows = table.height.saturating_sub(3).max(1) as usize;
        if let Some((review, text)) = &self.overlay {
            crate::tui::overlay(
                frame,
                if *review {
                    " Review · ↑↓/PgUp/PgDn scroll · Enter applies · Esc returns "
                } else {
                    " Help · ↑↓/PgUp/PgDn scroll · Esc closes "
                },
                text,
                &mut self.scroll,
            );
            return;
        }
        if let Some(patch) = &self.patch {
            self.page_rows = area.height.saturating_sub(2).max(1) as usize;
            let paragraph = Paragraph::new(patch.as_str());
            let max_scroll = patch
                .lines()
                .count()
                .saturating_sub(self.page_rows)
                .min(u16::MAX as usize) as u16;
            self.scroll = self.scroll.min(max_scroll);
            frame.render_widget(
                paragraph
                    .scroll((self.scroll, 0))
                    .block(Block::bordered().title(
                        " Diff · repo (-), local (+) · arrows/PgUp/PgDn/Home/End · v/Esc closes ",
                    )),
                area,
            );
            return;
        }
        frame.render_widget(
            Paragraph::new(vec![
                Line::from("KIT · Files").cyan().bold(),
                Line::from(format!(
                    "{} tracked · {} selected ({} hidden) · {} pending",
                    self.rows.len(),
                    self.selected.len(),
                    self.hidden_selected(),
                    self.queued.values().map(Pending::count).sum::<usize>()
                )),
            ]),
            heading,
        );
        frame.render_widget(
            Paragraph::new(clean(&self.query))
                .scroll((
                    0,
                    (Line::from(self.query.as_str())
                        .width()
                        .saturating_sub(search.width.saturating_sub(3) as usize))
                    .min(u16::MAX as usize) as u16,
                ))
                .block(Block::bordered().title(if self.searching {
                    " Search · Enter finishes "
                } else {
                    " Search · / to filter "
                })),
            search,
        );
        if self.searching {
            frame.set_cursor_position((
                search.x
                    + 1
                    + (Line::from(self.query.as_str()).width() as u16)
                        .min(search.width.saturating_sub(3)),
                search.y + 1,
            ));
        }
        let rows: Vec<Row> =
            self.visible()
                .iter()
                .map(|&i| {
                    let r = &self.rows[i];
                    let p = self.queued.get(&r.path).cloned().unwrap_or_default();
                    let blue = Style::default().fg(Color::LightBlue);
                    if compact {
                        let mut summary = vec![];
                        match p.copy {
                            Some(Copy::Save) if r.state == "deleted here" => {
                                summary.push("SAVE DELETION")
                            }
                            Some(Copy::Save) => summary.push("save local"),
                            Some(Copy::TakeRepo) => summary.push("take repo"),
                            None => {}
                        }
                        if let Some(ssh) = p.ssh {
                            summary.push(if ssh { "SSH+" } else { "SSH-" });
                        }
                        if p.remove {
                            summary.push("untrack");
                        }
                        return Row::new(vec![
                            Cell::from(if self.selected.contains(&r.path) {
                                "[x]"
                            } else {
                                "[ ]"
                            }),
                            Cell::from(clean(&r.path)),
                            Cell::from(if summary.is_empty() {
                                clean(&r.state)
                            } else {
                                summary.join(" · ")
                            })
                            .style(if p.count() > 0 {
                                blue
                            } else {
                                Style::default()
                            }),
                        ]);
                    }
                    Row::new(vec![
                        Cell::from(if self.selected.contains(&r.path) {
                            "[x]"
                        } else {
                            "[ ]"
                        }),
                        Cell::from(clean(&r.path)),
                        Cell::from(clean(&r.state)).style(if r.state == "in sync" {
                            Style::default().fg(Color::Green)
                        } else {
                            Style::default().fg(Color::Yellow)
                        }),
                        Cell::from(if p.ssh.unwrap_or(r.ssh) {
                            "included"
                        } else {
                            "excluded"
                        })
                        .style(if p.ssh.is_some() {
                            blue
                        } else {
                            Style::default()
                        }),
                        Cell::from(match p.copy {
                            Some(Copy::Save) if r.state == "deleted here" => "save deletion",
                            Some(Copy::Save) => "save local",
                            Some(Copy::TakeRepo) => "take repo",
                            None if r.saved => "saved",
                            None => "—",
                        })
                        .style(if p.copy.is_some() {
                            blue
                        } else {
                            Style::default()
                        }),
                        Cell::from(if p.remove { "untrack" } else { "tracked" })
                            .style(if p.remove { blue } else { Style::default() }),
                    ])
                })
                .collect();
        frame.render_stateful_widget(
            Table::new(
                rows,
                if compact {
                    vec![
                        Constraint::Length(3),
                        Constraint::Percentage(45),
                        Constraint::Min(14),
                    ]
                } else {
                    vec![
                        Constraint::Length(3),
                        Constraint::Min(16),
                        Constraint::Length(22),
                        Constraint::Length(10),
                        Constraint::Length(13),
                        Constraint::Length(8),
                    ]
                },
            )
            .header(
                Row::new(if compact {
                    vec!["", "File", "State / pending"]
                } else {
                    vec!["", "File", "State", "SSH/Linux", "Copy", "Tracking"]
                })
                .bold(),
            )
            .block(Block::bordered().title(" Tracked files "))
            .row_highlight_style(Style::default().bg(Color::DarkGray).bold())
            .highlight_symbol("› "),
            table,
            &mut self.table,
        );
        if let Some(i) = self.current() {
            let r = &self.rows[i];
            let description = if compact {
                vec![Line::from(format!(
                    "~/{} · {} · SSH {}",
                    clean(&r.path),
                    clean(&r.state),
                    if self
                        .queued
                        .get(&r.path)
                        .and_then(|p| p.ssh)
                        .unwrap_or(r.ssh)
                    {
                        "included"
                    } else {
                        "excluded"
                    }
                ))]
            } else {
                vec![Line::from(format!("~/{}", clean(&r.path))), Line::from("s: save local (or deletion) · r: take repo with backup · d: untrack, keep local")]
            };
            frame.render_widget(
                Paragraph::new(description)
                    .block(Block::bordered().title(" Cursor file · v diff · ? help · Tab review ")),
                details,
            );
        }
        if compact {
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from("? help · Tab review · / search · Space select · v diff"),
                    Line::from(self.message.as_str()).cyan(),
                    Line::from("Enter APPLY · q/Esc CANCEL · blue=pending").bold(),
                ]),
                footer,
            );
        } else {
            frame.render_widget(
            Paragraph::new(vec![
                Line::from(self.message.as_str()).cyan(),
                Line::from("↑↓ move · Space select · a select all · / search · v diff · ? help · Tab review"),
                Line::from("s toggle save · r toggle take repo · p toggle SSH · d toggle untrack"),
                Line::from("Enter: APPLY QUEUED ACTIONS     q / Esc: CANCEL     c: clear queue")
                    .bold(),
            ]),
            footer,
        );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};
    fn fixture(count: usize) -> FileEditor {
        FileEditor {
            rows: (0..count)
                .map(|i| FileRow {
                    path: format!(".config/probe-{i:03}"),
                    state: "changed here".into(),
                    ssh: true,
                    ignored: false,
                    saved: false,
                    home_digest: None,
                    repo_digest: None,
                })
                .collect(),
            selected: BTreeSet::new(),
            queued: BTreeMap::new(),
            table: TableState::default().with_selected(0),
            query: String::new(),
            searching: false,
            message: String::new(),
            page_rows: 5,
            patch: None,
            overlay: None,
            scroll: 0,
            scroll_x: 0,
            ignore_snapshot: vec![],
        }
    }
    fn press(app: &mut FileEditor, key: KeyCode) -> Option<Outcome> {
        app.key(KeyEvent::new(key, KeyModifiers::NONE))
    }
    fn screen(t: &Terminal<TestBackend>) -> String {
        let b = t.backend().buffer();
        (0..b.area.height)
            .map(|y| {
                (0..b.area.width)
                    .map(|x| b[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
    #[test]
    fn copy_choices_are_exclusive_but_scope_and_untracking_are_independent() {
        let mut app = fixture(2);
        for c in ['a', 's', 'p', 'd'] {
            press(&mut app, KeyCode::Char(c));
        }
        assert_eq!(app.queued.len(), 2);
        assert_eq!(app.queued[".config/probe-000"].count(), 3);
        press(&mut app, KeyCode::Char('r'));
        for p in app.queued.values() {
            assert!(p.copy == Some(Copy::TakeRepo));
            assert_eq!(p.ssh, Some(false));
            assert!(p.remove);
        }
        for c in ['r', 'p', 'd'] {
            press(&mut app, KeyCode::Char(c));
        }
        assert!(app.queued.is_empty());
        assert_eq!(app.selected.len(), 2);
    }
    #[test]
    fn pending_values_are_blue_and_navigation_handles_empty_filters_and_resize() {
        let mut app = fixture(70);
        let mut terminal = Terminal::new(TestBackend::new(100, 26)).unwrap();
        press(&mut app, KeyCode::Char('p'));
        terminal.draw(|f| app.render(f)).unwrap();
        let buffer = terminal.backend().buffer();
        let line = (0..100)
            .map(|x| buffer[(x, 7)].symbol())
            .collect::<String>();
        let col = line[..line.find("excluded").unwrap()].chars().count() as u16;
        assert_eq!(buffer[(col, 7)].fg, Color::LightBlue);
        for _ in 0..40 {
            press(&mut app, KeyCode::Down);
        }
        terminal.draw(|f| app.render(f)).unwrap();
        assert!(app.table.offset() > 0);
        assert!(screen(&terminal).contains("probe-040"));
        press(&mut app, KeyCode::Char('/'));
        for c in "不存在".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        press(&mut app, KeyCode::Down);
        assert!(app.current().is_none());
        terminal.draw(|f| app.render(f)).unwrap();
        terminal.backend_mut().resize(30, 8);
        terminal
            .resize(ratatui::layout::Rect::new(0, 0, 30, 8))
            .unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        assert!(screen(&terminal).contains("Window too small"));
    }
    #[test]
    fn diff_is_read_only_and_key_releases_do_not_toggle() {
        let mut app = fixture(1);
        app.patch = Some("-repo\n+local\n".into());
        press(&mut app, KeyCode::Char('s'));
        assert!(app.queued.is_empty());
        press(&mut app, KeyCode::Char('q'));
        assert!(app.patch.is_none());
        let mut release = KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        app.key(release);
        assert!(app.queued.is_empty());
        assert!(matches!(
            press(&mut app, KeyCode::Char('q')),
            Some(Outcome::Cancel)
        ));
    }
    #[test]
    fn compact_panes_keep_rows_actions_and_hidden_selection_scope_visible() {
        let mut app = fixture(3);
        press(&mut app, KeyCode::Char(' '));
        press(&mut app, KeyCode::Char('/'));
        for c in "probe-001".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        press(&mut app, KeyCode::Enter);
        let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("probe-001"), "{text}");
        assert!(text.contains("1 hidden"), "{text}");
        assert!(text.contains("Enter APPLY"), "{text}");
        assert!(text.contains("? help"), "{text}");
        press(&mut app, KeyCode::Char('p'));
        press(&mut app, KeyCode::Tab);
        terminal.draw(|f| app.render(f)).unwrap();
        assert!(screen(&terminal).contains("probe-000"));
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Char('?'));
        press(&mut app, KeyCode::Char('s'));
        assert_eq!(app.queued.len(), 1, "help must not stage actions");
    }
    #[test]
    fn local_deletion_requires_explicit_review_and_missing_saves_are_refused() {
        let mut app = fixture(1);
        app.rows[0].state = "deleted here".into();
        press(&mut app, KeyCode::Char('s'));
        assert!(press(&mut app, KeyCode::Enter).is_none());
        assert!(app.overlay.as_ref().unwrap().1.contains("SAVE DELETION"));
        assert!(matches!(
            press(&mut app, KeyCode::Enter),
            Some(Outcome::Apply)
        ));
        let mut missing = fixture(1);
        missing.rows[0].state = "missing here".into();
        press(&mut missing, KeyCode::Char('s'));
        assert!(missing.queued.is_empty());
        assert!(missing.message.contains("absent"));
    }
    #[test]
    fn diff_scrolls_long_lines_end_reaches_tail_and_tiny_panes_cannot_apply() {
        let mut app = fixture(1);
        app.patch = Some(format!(
            "{}\n{}TAIL",
            "x".repeat(600),
            "middle\n".repeat(25)
        ));
        let mut terminal = Terminal::new(TestBackend::new(40, 15)).unwrap();
        press(&mut app, KeyCode::Right);
        assert_eq!(app.scroll_x, 4);
        press(&mut app, KeyCode::Left);
        assert_eq!(app.scroll_x, 0);
        press(&mut app, KeyCode::End);
        terminal.draw(|f| app.render(f)).unwrap();
        assert!(screen(&terminal).contains("TAIL"));
        terminal.backend_mut().resize(30, 8);
        terminal
            .resize(ratatui::layout::Rect::new(0, 0, 30, 8))
            .unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        assert!(press(&mut app, KeyCode::Enter).is_none());
        assert!(matches!(
            press(&mut app, KeyCode::Esc),
            Some(Outcome::Cancel)
        ));
    }
    #[test]
    fn modified_character_chords_do_not_stage_actions() {
        let mut app = fixture(1);
        for modifier in [KeyModifiers::CONTROL, KeyModifiers::ALT] {
            for c in ['s', 'u', 'l', 'p', 'r'] {
                app.key(KeyEvent::new(KeyCode::Char(c), modifier));
            }
        }
        assert!(app.queued.is_empty());
    }
}
