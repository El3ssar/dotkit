//! Tracked-file inventory and a staged editor. No filesystem discovery or side effects on open.
use crate::cli::{build_cli, Args};
use crate::core::*;
use crate::tui::{Editor, Outcome};
use crate::util::*;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    layout::{Constraint, Layout},
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
    Resolved,
}
#[derive(Default, Clone)]
struct Pending {
    copy: Option<Copy>,
    resolved: Option<Entry>,
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

#[derive(Clone, Copy, Default, PartialEq)]
enum View {
    #[default]
    All,
    Changed,
    Conflicts,
    Pending,
}
impl View {
    fn title(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Changed => "Changed",
            Self::Conflicts => "Conflicts",
            Self::Pending => "Pending",
        }
    }
}
fn changed(row: &FileRow) -> bool {
    !matches!(row.state.as_str(), "in sync" | "excluded here") || row.saved
}
fn is_conflict(state: &str) -> bool {
    matches!(
        state,
        "has conflicts" | "changed on both" | "deleted here, changed in repo" | "file vs folder"
    )
}
struct Resolver {
    path: String,
    resolution: crate::conflict::Resolution,
    index: usize,
    mode: u32,
    scroll: u16,
    preview: bool,
    scroll_x: u16,
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
    collapsed: BTreeSet<String>,
    view: View,
    preview: Option<(String, String)>,
    resolver: Option<Resolver>,
}
impl FileEditor {
    fn new(rows: Vec<FileRow>) -> Self {
        let mut app = Self {
            rows,
            selected: BTreeSet::new(),
            queued: BTreeMap::new(),
            table: TableState::default().with_selected(0),
            query: String::new(),
            searching: false,
            message: "Only tracked files. Blue = pending; Enter reviews; q cancels.".into(),
            page_rows: 5,
            patch: None,
            overlay: None,
            scroll: 0,
            scroll_x: 0,
            ignore_snapshot: std::fs::read(&ctx().ignore).unwrap_or_default(),
            collapsed: BTreeSet::new(),
            view: View::All,
            preview: None,
            resolver: None,
        };
        for node in app.nodes() {
            if node.file.is_none()
                && node
                    .members
                    .iter()
                    .all(|&i| app.rows[i].state == "in sync" && !app.rows[i].saved)
            {
                app.collapsed.insert(node.path);
            }
        }
        app
    }
    fn matches(&self, i: usize) -> bool {
        let r = &self.rows[i];
        r.path.to_lowercase().contains(&self.query.to_lowercase())
            && match self.view {
                View::All => true,
                View::Changed => changed(r),
                View::Conflicts => is_conflict(&r.state),
                View::Pending => self.queued.contains_key(&r.path),
            }
    }
    fn nodes(&self) -> Vec<crate::file_tree::Node> {
        crate::file_tree::nodes(
            self.rows
                .iter()
                .enumerate()
                .filter(|(i, _)| self.matches(*i))
                .map(|(i, r)| (i, r.path.as_str())),
            &self.collapsed,
            !self.query.is_empty(),
        )
    }
    fn visible(&self) -> Vec<usize> {
        self.nodes().iter().filter_map(|n| n.file).collect()
    }
    fn hidden_selected(&self) -> usize {
        let visible: BTreeSet<_> = self
            .visible()
            .iter()
            .map(|&i| self.rows[i].path.as_str())
            .collect();
        self.selected
            .iter()
            .filter(|p| !visible.contains(p.as_str()))
            .count()
    }
    fn node(&self) -> Option<crate::file_tree::Node> {
        self.nodes()
            .get(self.table.selected().unwrap_or(0))
            .cloned()
    }
    fn current(&self) -> Option<usize> {
        self.node().and_then(|n| n.file)
    }
    fn targets(&self) -> Vec<String> {
        if self.selected.is_empty() {
            self.node()
                .map(|n| {
                    n.members
                        .iter()
                        .map(|&i| self.rows[i].path.clone())
                        .collect()
                })
                .unwrap_or_default()
        } else {
            self.selected.iter().cloned().collect()
        }
    }
    fn focus_file(&mut self) {
        let i = self
            .nodes()
            .iter()
            .position(|n| n.file.is_some())
            .unwrap_or(0);
        self.table.select(Some(i));
    }
    fn fold(&mut self, expand: bool) {
        if !self.query.is_empty() {
            self.message = "Search reveals matching branches; clear search to fold folders.".into();
            return;
        }
        if let Some(n) = self.node() {
            if n.file.is_none() && expand {
                if !self.collapsed.remove(&n.path) {
                    self.navigate(1);
                }
            } else if n.file.is_none() && !self.collapsed.contains(&n.path) {
                self.collapsed.insert(n.path);
            } else if let Some(i) = self
                .nodes()
                .iter()
                .rposition(|p| p.file.is_none() && n.path.starts_with(&format!("{}/", p.path)))
            {
                self.table.select(Some(i));
            }
        }
    }
    fn next_change(&mut self, reverse: bool) {
        let current = self.current();
        let changes: Vec<_> = (0..self.rows.len())
            .filter(|&i| self.matches(i) && (changed(&self.rows[i])))
            .collect();
        if changes.is_empty() {
            self.message = "No matching changes.".into();
            return;
        }
        let target = if reverse {
            changes
                .iter()
                .rev()
                .find(|&&i| current.is_some_and(|c| i < c))
                .copied()
                .unwrap_or(*changes.last().unwrap())
        } else {
            changes
                .iter()
                .find(|&&i| current.is_some_and(|c| i > c))
                .copied()
                .unwrap_or(changes[0])
        };
        let path = &self.rows[target].path;
        self.collapsed
            .retain(|p| !path.starts_with(&format!("{p}/")));
        if let Some(i) = self.nodes().iter().position(|n| n.file == Some(target)) {
            self.table.select(Some(i));
        }
    }
    fn navigate(&mut self, delta: isize) {
        let max = self.nodes().len().saturating_sub(1);
        self.table.select(Some(
            self.table
                .selected()
                .unwrap_or(0)
                .saturating_add_signed(delta)
                .min(max),
        ));
    }
    fn diff_for(&self, i: usize) -> String {
        let r = &self.rows[i];
        let src = source_entry(&r.path, &local_contexts()).ok().flatten();
        let here = self
            .queued
            .get(&r.path)
            .and_then(|p| p.resolved.clone())
            .or_else(|| read_home_entry(&r.path));
        let size = |e: &Option<Entry>| match e {
            Some(Entry::File(b, _)) => b.len(),
            _ => 0,
        };
        if size(&src).max(size(&here)) > 1024 * 1024 {
            return "File exceeds 1 MiB; inspect it outside the TUI.".into();
        }
        crate::diff::diff_text(
            &clean(&r.path),
            src.as_ref(),
            here.as_ref(),
            "Saved in Kit",
            "Your machine",
            &r.state,
        )
    }
    fn show_diff(&mut self) {
        if let Some(i) = self.current() {
            self.patch = Some(self.diff_for(i));
            self.scroll = 0;
            self.scroll_x = 0;
        } else {
            self.message = "Expand a folder or search for a file to preview its contents.".into();
        }
    }
    fn open_resolver(&mut self) {
        let Some(i) = self.current() else {
            self.message =
                "Select one file to resolve; folders and batch selections are not merged together."
                    .into();
            return;
        };
        let row = &self.rows[i];
        if row.ignored || !rules_for(&row.path, &local_contexts()).is_empty() {
            self.message="This file has platform rules or is excluded here; edit its source copy explicitly.".into();
            return;
        }
        let here = read_home_entry(&row.path);
        let src = source_entry(&row.path, &local_contexts()).ok().flatten();
        if looks_secret_name(&row.path)
            || [&here, &src]
                .iter()
                .any(|e| matches!(e,Some(Entry::File(b,_)) if secret_hit(b).is_some()))
        {
            self.message = "Secret contents are hidden; resolve this file outside the TUI.".into();
            return;
        }
        let (Some(Entry::File(local, mode)), Some(Entry::File(repo, _))) = (here, src) else {
            self.message="Text merging needs two regular files. Use r for the repo copy, or d to stop tracking.".into();
            return;
        };
        if local.len().max(repo.len()) > 1024 * 1024 || is_binary(&local) || is_binary(&repo) {
            self.message="Binary or large file: inspect outside the TUI; r uses the repo copy with a backup.".into();
            return;
        }
        let merged = if has_conflict_markers(&local) {
            local
        } else if let Some(base) = get_base(&row.path) {
            if base.len() > 1024 * 1024 || is_binary(&base) {
                self.message = "The merge base cannot be previewed safely.".into();
                return;
            }
            match crate::files::merge3(&local, &base, &repo) {
                Some((out, _)) => out,
                None => {
                    self.message = "Could not prepare a text merge. No files changed.".into();
                    return;
                }
            }
        } else {
            self.message="No shared ancestor is recorded. Compare with v, then s keeps your whole file or r uses the repo copy.".into();
            return;
        };
        if secret_hit(&merged).is_some() {
            self.message = "Merged result may contain a secret; use an external editor.".into();
            return;
        }
        let Ok(text) = std::str::from_utf8(&merged) else {
            self.message = "Non-UTF-8 text requires an external editor.".into();
            return;
        };
        match crate::conflict::Resolution::parse(text) {
            Ok(resolution) => {
                self.resolver = Some(Resolver {
                    path: row.path.clone(),
                    resolution,
                    index: 0,
                    mode,
                    scroll: 0,
                    preview: false,
                    scroll_x: 0,
                })
            }
            Err(e) => self.message = e,
        }
    }
    fn resolver_key(&mut self, key: KeyCode) {
        use crate::conflict::Choice;
        let resolver = self.resolver.as_mut().unwrap();
        match key {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.resolver = None;
                self.message = "Merge editor closed; no new resolution queued.".into();
            }
            KeyCode::Char('1' | '2' | '3') if !resolver.resolution.hunks.is_empty() => {
                resolver.resolution.hunks[resolver.index].choice = Some(match key {
                    KeyCode::Char('1') => Choice::Local,
                    KeyCode::Char('2') => Choice::Repo,
                    _ => Choice::Both,
                });
                if let Some(next) = resolver
                    .resolution
                    .hunks
                    .iter()
                    .enumerate()
                    .skip(resolver.index + 1)
                    .find(|(_, h)| h.choice.is_none())
                    .map(|(i, _)| i)
                {
                    resolver.index = next;
                    resolver.scroll = 0;
                }
            }
            KeyCode::Char(']' | 'n') => {
                resolver.index =
                    (resolver.index + 1).min(resolver.resolution.hunks.len().saturating_sub(1));
                resolver.scroll = 0;
            }
            KeyCode::Char('[' | 'p') => {
                resolver.index = resolver.index.saturating_sub(1);
                resolver.scroll = 0;
            }
            KeyCode::Char('0') if !resolver.resolution.hunks.is_empty() => {
                resolver.resolution.hunks[resolver.index].choice = None
            }
            KeyCode::Char('v') => {
                resolver.preview = !resolver.preview;
                resolver.scroll = 0;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                resolver.scroll = resolver.scroll.saturating_add(1)
            }
            KeyCode::Up | KeyCode::Char('k') => resolver.scroll = resolver.scroll.saturating_sub(1),
            KeyCode::PageDown => {
                resolver.scroll = resolver.scroll.saturating_add(self.page_rows as u16)
            }
            KeyCode::PageUp => {
                resolver.scroll = resolver.scroll.saturating_sub(self.page_rows as u16)
            }
            KeyCode::Left => resolver.scroll_x = resolver.scroll_x.saturating_sub(4),
            KeyCode::Right => resolver.scroll_x = resolver.scroll_x.saturating_add(4),
            KeyCode::Home => {
                resolver.scroll = 0;
                resolver.scroll_x = 0;
            }
            KeyCode::End => resolver.scroll = u16::MAX,
            KeyCode::Enter => match resolver.resolution.output() {
                Ok(text) => {
                    let pending = self.queued.entry(resolver.path.clone()).or_default();
                    pending.copy = Some(Copy::Resolved);
                    pending.resolved = Some(Entry::File(text.into_bytes(), resolver.mode));
                    self.resolver = None;
                    self.message="Resolution queued in blue. Enter reviews before applying; nothing written yet.".into();
                }
                Err(e) => self.message = e,
            },
            _ => {}
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
                    Some(Copy::Save) => actions.push("keep machine's whole file and save to Kit"),
                    Some(Copy::Resolved) => actions
                        .push("write resolved text locally and save to Kit (original backed up)"),
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
        self.preview = None;
        self.overlay = Some((true, format!("REVIEW ALL PENDING ACTIONS\n\n{}\n\nEnter applies these exact actions. Esc / Tab returns to editing. Ctrl-C cancels everything.", if lines.is_empty() { "Nothing queued." } else { &lines })));
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
        for (path, p) in &self.queued {
            if let Some(entry) = &p.resolved {
                if !rules_for(path, &local_contexts()).is_empty()
                    || secret_problem(path, Some(entry)).is_some()
                    || matches!(entry,Entry::File(b,_) if has_conflict_markers(b))
                {
                    fail(&format!(
                        "~/{path}: resolution cannot be safely saved; no queued actions applied"
                    ));
                    return;
                }
            }
        }
        let stamp = now_stamp();
        for (path, pending) in &self.queued {
            let paths = vec![path.clone()];
            match pending.copy {
                Some(Copy::Save | Copy::Resolved) if pending.resolved.is_some() => {
                    let entry = pending.resolved.as_ref().unwrap();
                    let result: KResult<()> = (|| {
                        backup(&stamp, path)?;
                        write_home(path, entry)?;
                        write_tree(path, entry)?;
                        let mut synced = load_synced();
                        mark_synced(&mut synced, path, entry);
                        save_synced(&synced);
                        Ok(())
                    })();
                    if let Err(e) = result {
                        fail(&format!(
                            "could not save resolved ~/{path}: {e}; original kept in kit undo"
                        ));
                    }
                }
                Some(Copy::Save) => {
                    crate::files::re_add(Some(&paths), false);
                }
                Some(Copy::Resolved) => {
                    fail("missing queued resolution; reopen kit files");
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
        if self.resolver.is_some() {
            self.resolver_key(key.code);
            self.preview = None;
            return None;
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
                KeyCode::Esc | KeyCode::Enter => {
                    self.searching = false;
                    self.focus_file();
                }
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
            KeyCode::Enter => self.review(),
            KeyCode::Tab => self.review(),
            KeyCode::Char('?') => {
                self.overlay = Some((false, "FILE MANAGER\n\nOnly tracked files appear. ←/→ (h/l) fold folders. O expands all; C collapses all. Search temporarily reveals matching branches. 1 All; 2 Changed (including saved changes awaiting backup); 3 Conflicts; 4 Pending. n/N jumps between changes.\n\nSpace selects a file or all matching tracked descendants of a folder. [-] means partly selected. a selects all matching files, including collapsed folders. A clears selection; c clears selection AND pending actions. Actions use ALL selected files, including hidden rows; with no selection they use the cursor file or matching folder descendants. Counts and review always include hidden actions.\n\nv compares Saved in Kit (-) with Your machine (+). Arrow keys scroll; Esc returns. Secrets stay hidden.\n\ns saves the machine copy. For changes on both sides it explicitly keeps the machine's whole file. r uses the repo's whole copy with a backup. d stops tracking and keeps the local file. p toggles SSH/Linux inclusion. These actions stay pending until review and apply.\n\nm opens a single file's text merge. 1 keeps the machine side; 2 keeps the repo side; 3 keeps both in that order. [/ ] changes sections; v previews the completed result. Enter queues only a complete resolution; Esc discards the merge editor. Original files are backed up when applying. Unsupported or secret files require an external editor.\n\nBlue = pending. Enter or Tab reviews every pending action. Enter in the review applies; Esc returns. q/Esc in the main list cancels everything. Applying several files can partially succeed if a later write fails.\n\nEsc / ? closes help.".into()));
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
            KeyCode::Char('1' | '2' | '3' | '4') => {
                self.view = match key.code {
                    KeyCode::Char('2') => View::Changed,
                    KeyCode::Char('3') => View::Conflicts,
                    KeyCode::Char('4') => View::Pending,
                    _ => View::All,
                };
                self.focus_file();
                self.preview = None;
            }
            KeyCode::Left | KeyCode::Char('h') => self.fold(false),
            KeyCode::Right | KeyCode::Char('l') => self.fold(true),
            KeyCode::Char('O') => self.collapsed.clear(),
            KeyCode::Char('C') => {
                self.collapsed = self
                    .nodes()
                    .iter()
                    .filter(|n| n.file.is_none())
                    .map(|n| n.path.clone())
                    .collect();
                self.table.select(Some(0));
            }
            KeyCode::Char('n') => self.next_change(false),
            KeyCode::Char('N') => self.next_change(true),
            KeyCode::Char('m') => self.open_resolver(),
            KeyCode::Char('v') => self.show_diff(),
            KeyCode::Char(' ') => {
                if let Some(node) = self.node() {
                    let all = node
                        .members
                        .iter()
                        .all(|&i| self.selected.contains(&self.rows[i].path));
                    for i in node.members {
                        if all {
                            self.selected.remove(&self.rows[i].path);
                        } else {
                            self.selected.insert(self.rows[i].path.clone());
                        }
                    }
                }
            }
            KeyCode::Char('a') => {
                for i in (0..self.rows.len())
                    .filter(|&i| self.matches(i))
                    .collect::<Vec<_>>()
                {
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
                let names = self.targets();
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
                    let mut reviewed_local = None;
                    if c == 's' && is_conflict(&row.state) {
                        if row.state == "has conflicts" {
                            self.message="Use m to resolve conflict sections first; r takes the whole repo copy.".into();
                            continue;
                        }
                        let local = read_home_entry(&name);
                        if !matches!(local, Some(Entry::File(..)) | Some(Entry::Link(_)))
                            || secret_problem(&name, local.as_ref()).is_some()
                            || !rules_for(&name, &local_contexts()).is_empty()
                        {
                            self.message="Cannot safely save this conflict as a whole file; inspect it explicitly.".into();
                            continue;
                        }
                        reviewed_local = local;
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
                            p.resolved = if p.copy == Some(Copy::Save) {
                                reviewed_local
                            } else {
                                None
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
                    self.message = "Blue = pending. Take repo replaces local contents with a backup. Enter reviews; q cancels.".into();
                }
            }
            _ => {}
        }
        self.preview = None;
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
        self.page_rows = area.height.saturating_sub(10).max(1) as usize;
        if let Some(resolver) = &mut self.resolver {
            render_resolver(resolver, &self.message, frame);
            return;
        }
        if let Some((review, text)) = &self.overlay {
            crate::tui::overlay(
                frame,
                if *review {
                    " Review all actions · Enter applies · Esc returns "
                } else {
                    " Help · ↑↓ scroll · Esc closes "
                },
                text,
                &mut self.scroll,
            );
            return;
        }
        if let Some(patch) = &self.patch {
            self.page_rows = area.height.saturating_sub(2) as usize;
            self.scroll = self.scroll.min(
                patch
                    .lines()
                    .count()
                    .saturating_sub(self.page_rows)
                    .min(u16::MAX as usize) as u16,
            );
            frame.render_widget(Paragraph::new(patch_lines(patch)).scroll((self.scroll,self.scroll_x)).block(Block::bordered().title(" Compare · Saved in Kit (-) / Your machine (+) · arrows scroll · Esc returns ")),area);
            return;
        }
        let [heading, search, body, details, footer] = crate::tui::panels(area, false);
        let wide = area.width >= 115 && area.height >= 23;
        let panes = Layout::horizontal([
            Constraint::Percentage(if wide { 45 } else { 100 }),
            Constraint::Min(0),
        ])
        .split(body);
        let table = panes[0];
        self.page_rows = table.height.saturating_sub(3).max(1) as usize;
        let nodes = self.nodes();
        self.table.select(Some(
            self.table
                .selected()
                .unwrap_or(0)
                .min(nodes.len().saturating_sub(1)),
        ));
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(format!(
                    "KIT · Files · {} tracked · {} selected ({} hidden) · {} pending",
                    self.rows.len(),
                    self.selected.len(),
                    self.hidden_selected(),
                    self.queued.values().map(Pending::count).sum::<usize>()
                ))
                .cyan()
                .bold(),
                Line::from(format!(
                    "1 All   2 Changed   3 Conflicts   4 Pending     [{}]",
                    self.view.title()
                )),
            ]),
            heading,
        );
        frame.render_widget(
            Paragraph::new(clean(&self.query))
                .scroll((
                    0,
                    Line::from(self.query.as_str())
                        .width()
                        .saturating_sub(search.width.saturating_sub(3) as usize)
                        .min(u16::MAX as usize) as u16,
                ))
                .block(Block::bordered().title(if self.searching {
                    " Search · Enter finishes · Esc keeps filter "
                } else {
                    " / Search tracked paths "
                })),
            search,
        );
        if self.searching {
            frame.set_cursor_position((
                search.x
                    + 1
                    + (Line::from(self.query.as_str())
                        .width()
                        .min(search.width.saturating_sub(3) as usize)
                        as u16),
                search.y + 1,
            ));
        }
        let rows: Vec<_> = nodes
            .iter()
            .map(|node| {
                let selected = node
                    .members
                    .iter()
                    .filter(|&&i| self.selected.contains(&self.rows[i].path))
                    .count();
                let check = if selected == 0 {
                    "[ ]"
                } else if selected == node.members.len() {
                    "[x]"
                } else {
                    "[-]"
                };
                let pending = node
                    .members
                    .iter()
                    .filter(|&&i| self.queued.contains_key(&self.rows[i].path))
                    .count();
                let blue = Style::default().fg(Color::LightBlue);
                let prefix = "  ".repeat(node.depth.min(12));
                let name = if node.file.is_some() {
                    format!("{prefix}{}", clean(&node.label))
                } else {
                    format!(
                        "{prefix}{} {}",
                        if self.collapsed.contains(&node.path) && self.query.is_empty() {
                            "▸"
                        } else {
                            "▾"
                        },
                        clean(&node.label)
                    )
                };
                let summary = if let Some(i) = node.file {
                    let r = &self.rows[i];
                    let p = self.queued.get(&r.path);
                    let mut text = match p.and_then(|p| p.copy) {
                        Some(Copy::Save) if r.state == "deleted here" => "save deletion".into(),
                        Some(Copy::Save) => "save machine copy".into(),
                        Some(Copy::TakeRepo) => "use repo copy".into(),
                        Some(Copy::Resolved) => "save resolution".into(),
                        None if r.saved && r.state == "in sync" => "saved · awaiting backup".into(),
                        None if r.saved => format!("{} · saved", clean(&r.state)),
                        None => clean(&r.state),
                    };
                    if let Some(p) = p {
                        if p.remove {
                            text.push_str(" · untrack");
                        }
                        if let Some(ssh) = p.ssh {
                            text.push_str(if ssh {
                                " · SSH included"
                            } else {
                                " · SSH excluded"
                            });
                        }
                    }
                    text
                } else {
                    let changed = node
                        .members
                        .iter()
                        .filter(|&&i| changed(&self.rows[i]))
                        .count();
                    let conflicts = node
                        .members
                        .iter()
                        .filter(|&&i| is_conflict(&self.rows[i].state))
                        .count();
                    format!(
                        "{} files · {} changed{}{}",
                        node.members.len(),
                        changed,
                        if conflicts > 0 {
                            format!(" · {conflicts} conflicts")
                        } else {
                            String::new()
                        },
                        if pending > 0 {
                            format!(" · {pending} pending")
                        } else {
                            String::new()
                        }
                    )
                };
                Row::new([
                    Cell::from(check),
                    Cell::from(name),
                    Cell::from(summary).style(if pending > 0 {
                        blue
                    } else if node
                        .members
                        .iter()
                        .any(|&i| is_conflict(&self.rows[i].state))
                    {
                        Style::default().fg(Color::Yellow)
                    } else {
                        Style::default()
                    }),
                ])
            })
            .collect();
        frame.render_stateful_widget(
            Table::new(
                rows,
                [
                    Constraint::Length(3),
                    Constraint::Percentage(48),
                    Constraint::Min(12),
                ],
            )
            .header(Row::new(["", "Tracked path", "State / pending"]).bold())
            .block(Block::bordered().title(format!(
                " {} · ←→ fold · Space select folder ",
                self.view.title()
            )))
            .row_highlight_style(Style::default().bg(Color::DarkGray).bold())
            .highlight_symbol("› "),
            table,
            &mut self.table,
        );
        if wide {
            let content = if let Some(i) = self.current() {
                let path = self.rows[i].path.clone();
                if self.preview.as_ref().is_none_or(|(p, _)| p != &path) {
                    self.preview = Some((path, self.diff_for(i)));
                }
                self.preview.as_ref().unwrap().1.clone()
            } else {
                "Select a file to compare its contents.\n\n←/→ collapse or expand folders\nO expands all · C collapses all\nn/N next/previous changed file\n\nSpace on a folder selects its matching tracked files. Actions include selected files hidden by folders or filters.\n\nEnter reviews every pending action before applying.".into()
            };
            frame.render_widget(
                Paragraph::new(patch_lines(&content)).block(
                    Block::bordered().title(" Compare · Kit (-) / machine (+) · v full screen "),
                ),
                panes[1],
            );
        }
        let info = if let Some(i) = self.current() {
            let r = &self.rows[i];
            let ssh = self
                .queued
                .get(&r.path)
                .and_then(|p| p.ssh)
                .unwrap_or(r.ssh);
            format!(
                "~/{} · {} · SSH/Linux {}\n{}",
                clean(&r.path),
                r.state,
                if ssh { "included" } else { "excluded" },
                if is_conflict(&r.state) {
                    "m resolve sections · s keep machine's whole file · r use repo copy (backup)"
                } else {
                    "s save machine copy · r use repo copy (backup) · d stop tracking; keep file"
                }
            )
        } else if let Some(n) = self.node() {
            format!("~/{} · {} matching tracked files\nSpace selects descendants · actions use selection, or this folder's matching files",clean(&n.path),n.members.len())
        } else {
            "No matching files. 1 shows all views; / then Enter clears search.".into()
        };
        frame.render_widget(
            Paragraph::new(info).block(Block::bordered().title(" Selected path ")),
            details,
        );
        let compact = area.width < 90 || area.height < 23;
        let mut lines = vec![Line::from(self.message.as_str()).cyan()];
        if !compact {
            lines.push(Line::from("↑↓ move · ←→ fold · O/C expand/collapse all · n/N changes · Space select · / search · v compare"));
        }
        lines.push(Line::from(
            "s save · r use repo · m resolve · p SSH · d untrack · ? help",
        ));
        lines.push(Line::from("Enter REVIEW · q/Esc CANCEL · Tab review · blue=pending").bold());
        frame.render_widget(Paragraph::new(lines), footer);
    }
}

fn patch_lines(text: &str) -> Vec<Line<'_>> {
    text.lines()
        .map(|line| {
            let style = if line.starts_with("---") || line.starts_with("+++") {
                Style::default().fg(Color::Cyan)
            } else if line.starts_with('+') {
                Style::default().fg(Color::Green)
            } else if line.starts_with('-') {
                Style::default().fg(Color::Red)
            } else if line.starts_with("@@") {
                Style::default().fg(Color::LightBlue)
            } else {
                Style::default()
            };
            Line::from(line).style(style)
        })
        .collect()
}
fn display_text(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_control() && c != '\n' && c != '\t' {
                '�'
            } else {
                c
            }
        })
        .collect()
}
fn render_resolver(resolver: &mut Resolver, message: &str, frame: &mut Frame) {
    let area = frame.area();
    let [heading, body, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(4),
        Constraint::Length(4),
    ])
    .areas(area);
    let count = resolver.resolution.hunks.len();
    let remaining = resolver.resolution.remaining();
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(format!("Resolve ~/{}", clean(&resolver.path)))
                .cyan()
                .bold(),
            Line::from(format!(
                "Section {} / {} · {} still need a choice · nothing written",
                if count == 0 { 0 } else { resolver.index + 1 },
                count,
                remaining
            )),
            Line::from(if count == 0 {
                "Changes merge cleanly. v previews the merged file; Enter queues it."
            } else {
                "Choose each section; unchanged and already merged lines are preserved."
            }),
        ]),
        heading,
    );
    if resolver.preview || count == 0 {
        let text = resolver
            .resolution
            .output()
            .unwrap_or_else(|e| format!("{e}\n\nPress v to return to the sections."));
        let safe = display_text(&text);
        resolver.scroll = resolver.scroll.min(
            safe.lines()
                .count()
                .saturating_sub(body.height.saturating_sub(2) as usize)
                .min(u16::MAX as usize) as u16,
        );
        frame.render_widget(
            Paragraph::new(safe)
                .scroll((resolver.scroll, resolver.scroll_x))
                .block(
                    Block::bordered()
                        .title(" Resolved result · Enter queues; no files written yet "),
                ),
            body,
        );
    } else {
        let h = &resolver.resolution.hunks[resolver.index];
        let panes = if area.width >= 90 {
            Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(body)
        } else {
            Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).split(body)
        };
        let height = panes[0].height.min(panes[1].height).saturating_sub(2) as usize;
        resolver.scroll = resolver.scroll.min(
            h.local
                .lines()
                .count()
                .max(h.repo.lines().count())
                .saturating_sub(height)
                .min(u16::MAX as usize) as u16,
        );
        for (pane, text, title, chosen) in [
            (
                panes[0],
                &h.local,
                " 1 Your machine ",
                matches!(
                    h.choice,
                    Some(crate::conflict::Choice::Local | crate::conflict::Choice::Both)
                ),
            ),
            (
                panes[1],
                &h.repo,
                " 2 Saved in Kit ",
                matches!(
                    h.choice,
                    Some(crate::conflict::Choice::Repo | crate::conflict::Choice::Both)
                ),
            ),
        ] {
            frame.render_widget(
                Paragraph::new(display_text(text))
                    .scroll((resolver.scroll, resolver.scroll_x))
                    .block(
                        Block::bordered()
                            .title(format!(
                                "{title}{}",
                                if chosen { "✓ selected " } else { "" }
                            ))
                            .border_style(if chosen {
                                Style::default().fg(Color::LightBlue)
                            } else {
                                Style::default()
                            }),
                    ),
                pane,
            );
        }
    }
    let choice = resolver
        .resolution
        .hunks
        .get(resolver.index)
        .map(|h| match h.choice {
            None => "not chosen",
            Some(crate::conflict::Choice::Local) => "machine",
            Some(crate::conflict::Choice::Repo) => "repo",
            Some(crate::conflict::Choice::Both) => "both (machine then repo)",
        })
        .unwrap_or("automatic merge");
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(format!(
                "Choice: {choice} · 1 machine · 2 repo · 3 both · 0 reset"
            ))
            .cyan(),
            Line::from("[/] sections · arrows/PgUp/PgDn scroll · v result preview"),
            Line::from("Enter QUEUE resolution · Esc cancel merge; returns to file list").bold(),
            Line::from(message),
        ]),
        footer,
    );
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
            table: TableState::default().with_selected(1),
            query: String::new(),
            searching: false,
            message: String::new(),
            page_rows: 5,
            patch: None,
            overlay: None,
            scroll: 0,
            scroll_x: 0,
            ignore_snapshot: vec![],
            collapsed: BTreeSet::new(),
            view: View::All,
            preview: None,
            resolver: None,
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
        assert!(buffer.content.iter().any(|c| c.fg == Color::LightBlue));
        assert!(screen(&terminal).contains("SSH excluded"));
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
        assert!(text.contains("Enter REVIEW"), "{text}");
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
    #[test]
    fn folds_search_filters_and_group_selection_do_not_lose_pending_actions() {
        let mut app = fixture(5);
        app.table.select(Some(0));
        press(&mut app, KeyCode::Left);
        assert_eq!(app.nodes().len(), 1);
        press(&mut app, KeyCode::Char(' '));
        assert_eq!(app.selected.len(), 5);
        assert_eq!(app.hidden_selected(), 5);
        press(&mut app, KeyCode::Char('p'));
        assert_eq!(app.queued.len(), 5);
        app.query = "probe-001".into();
        app.focus_file();
        assert!(app.current().is_some());
        assert_eq!(app.hidden_selected(), 4);
        app.query.clear();
        assert_eq!(app.nodes().len(), 1, "search must preserve folds");
        press(&mut app, KeyCode::Char('4'));
        assert_eq!(app.queued.len(), 5);
        press(&mut app, KeyCode::Char('O'));
        assert_eq!(app.visible().len(), 5);
        app.selected.clear();
        app.table.select(Some(1));
        press(&mut app, KeyCode::Char(' '));
        let node = app.nodes()[0].clone();
        assert_eq!(
            node.members
                .iter()
                .filter(|&&i| app.selected.contains(&app.rows[i].path))
                .count(),
            1
        );
        assert!(press(&mut app, KeyCode::Enter).is_none());
        let text = &app.overlay.as_ref().unwrap().1;
        for row in &app.rows {
            assert!(text.contains(&row.path));
        }
    }
    #[test]
    fn view_filters_and_next_change_skip_unchanged_and_reveal_folders() {
        let mut app = fixture(4);
        app.rows[0].state = "in sync".into();
        app.rows[1].state = "excluded here".into();
        app.rows[2].state = "has conflicts".into();
        press(&mut app, KeyCode::Char('2'));
        assert_eq!(app.visible(), vec![2, 3]);
        press(&mut app, KeyCode::Char('3'));
        assert_eq!(app.visible(), vec![2]);
        press(&mut app, KeyCode::Char('1'));
        press(&mut app, KeyCode::Char('C'));
        press(&mut app, KeyCode::Char('n'));
        assert_eq!(app.current(), Some(2));
        press(&mut app, KeyCode::Char('n'));
        assert_eq!(app.current(), Some(3));
        press(&mut app, KeyCode::Char('n'));
        assert_eq!(app.current(), Some(2));
        press(&mut app, KeyCode::Char('N'));
        assert_eq!(app.current(), Some(3));
    }
    #[test]
    fn resolver_requires_all_choices_and_stages_without_applying() {
        let mut app = fixture(1);
        app.resolver = Some(Resolver {
            path: app.rows[0].path.clone(),
            resolution: crate::conflict::Resolution::parse(
                "head\n<<<<<<< this machine\nlocal\n=======\nrepo\n>>>>>>> repo\ntail\n",
            )
            .unwrap(),
            index: 0,
            mode: 0o600,
            scroll: 0,
            scroll_x: 0,
            preview: false,
        });
        press(&mut app, KeyCode::Enter);
        assert!(app.queued.is_empty());
        assert!(app.message.contains("remaining sections"));
        let mut terminal = Terminal::new(TestBackend::new(120, 28)).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("Your machine") && text.contains("Saved in Kit"));
        assert!(text.contains("remaining sections"));
        press(&mut app, KeyCode::Char('2'));
        press(&mut app, KeyCode::Char('v'));
        terminal.draw(|f| app.render(f)).unwrap();
        assert!(screen(&terminal).contains("Resolved result"));
        assert!(press(&mut app, KeyCode::Enter).is_none());
        assert!(app.resolver.is_none());
        assert!(
            matches!(app.queued[".config/probe-000"].resolved,Some(Entry::File(ref b,0o600)) if b==b"head\nrepo\ntail\n")
        );
        assert!(press(&mut app, KeyCode::Enter).is_none());
        assert!(app
            .overlay
            .as_ref()
            .unwrap()
            .1
            .contains("write resolved text"));
        assert!(matches!(
            press(&mut app, KeyCode::Enter),
            Some(Outcome::Apply)
        ));
    }
    #[test]
    fn conflict_status_is_visible_even_when_repo_has_saved_changes() {
        let mut app = fixture(1);
        app.rows[0].saved = true;
        app.rows[0].state = "has conflicts".into();
        let mut terminal = Terminal::new(TestBackend::new(100, 26)).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        assert!(screen(&terminal).contains("has conflicts · saved"));
    }
}
