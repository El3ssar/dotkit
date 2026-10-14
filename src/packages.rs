//! Package inventory and an interactive editor for the sync manifest.
use crate::cli::Args;
use crate::core::guard_repo;
use crate::pkg::{all_packages, load_pkgs, pkg_available, s, save_pkgs, Pkg};
use crate::util::*;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState},
    Frame,
};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, IsTerminal};

#[derive(Clone, Copy)]
enum Action {
    Update,
    Remove,
    Resolve,
    Servers,
    Local,
}
#[derive(Clone, Default)]
struct Pending {
    update: bool,
    remove: bool,
    resolve: bool,
    ssh: Option<bool>,
}
impl Pending {
    fn count(&self) -> usize {
        usize::from(self.update)
            + usize::from(self.remove)
            + usize::from(self.resolve)
            + usize::from(self.ssh.is_some())
    }
}
impl From<Action> for Pending {
    fn from(action: Action) -> Self {
        let mut pending = Self::default();
        match action {
            Action::Update => pending.update = true,
            Action::Remove => pending.remove = true,
            Action::Resolve => pending.resolve = true,
            Action::Servers => pending.ssh = Some(true),
            Action::Local => pending.ssh = Some(false),
        }
        pending
    }
}
fn ssh_enabled(p: &Pkg) -> bool {
    p.get("remote").and_then(|v| v.as_bool()) != Some(false)
}

pub fn command(args: &Args) {
    if let Some((action, sub)) = args.subcommand() {
        if action == "list" {
            inventory();
            return;
        }
        let action = match action.as_str() {
            "update" => Action::Update,
            "remove" => Action::Remove,
            "resolve" => Action::Resolve,
            "servers" => Action::Servers,
            "local" => Action::Local,
            _ => unreachable!(),
        };
        let names = sub.many("names");
        let tracked = load_pkgs();
        let names = if names.is_empty() {
            tracked.keys().cloned().collect()
        } else {
            names
        };
        for name in &names {
            if !tracked.contains_key(name) {
                fail(&format!("{name} is not a tracked package (kit's built-in tools cannot be removed or edited here)"));
            }
        }
        if exit_code() == 0 {
            if matches!(action, Action::Update) {
                crate::updates::update_tools(&names, sub.flag("check"));
                return;
            }
            apply(names.into_iter().map(|n| (n, action.into())).collect());
        }
        return;
    }
    if args.flag("json") {
        let tracked = load_pkgs();
        let rows: Vec<_> = all_packages().into_iter().map(|(name, recipe)| {
            serde_json::json!({"installed":pkg_available(&name, &recipe), "builtin":!tracked.contains_key(&name), "name":name, "recipe":recipe})
        }).collect();
        out(&serde_json::to_string_pretty(&rows).unwrap());
    } else if args.flag("tui") || (io::stdin().is_terminal() && io::stdout().is_terminal()) {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            fail(
                "kit packages --tui needs an interactive terminal; use kit packages list or --json",
            );
            return;
        }
        match editor() {
            Ok(plan) if !plan.is_empty() => apply(plan),
            Ok(_) => say("package manifest unchanged"),
            Err(e) => fail(&format!("package manager: {e}")),
        }
    } else {
        inventory();
    }
}

fn inventory() {
    let tracked = load_pkgs();
    out("Packages tracked for sync (Mac/Linux recipes; servers = kit push):");
    for (name, p) in all_packages() {
        let origin = if tracked.contains_key(&name) {
            "tracked"
        } else {
            "kit built-in"
        };
        let here = if pkg_available(&name, &p) {
            "installed"
        } else {
            "missing"
        };
        let servers = if p.get("remote").and_then(|v| v.as_bool()) == Some(false) {
            "no"
        } else if s(&p, "linux").is_some() {
            "yes"
        } else {
            "no recipe"
        };
        out(&format!("  {name}  [{origin}]  {here}  servers: {servers}"));
        out(&format!(
            "    mac: {}\n    linux: {}",
            recipe(s(&p, "mac")),
            recipe(s(&p, "linux"))
        ));
        if let Some(fallback) = s(&p, "linux_fallback") {
            out(&format!("    fallback: {}", recipe(Some(fallback))));
        }
    }
    note("kit packages opens the interactive manager · kit packages resolve generates missing Linux recipes explicitly");
}

fn recipe(spec: Option<&str>) -> String {
    match spec {
        Some(v) if v.starts_with("cmd:") => "custom command (see packages.json)".into(),
        Some(v) => clean(v),
        None => "— missing —".into(),
    }
}
fn clean(value: &str) -> String {
    value.chars().filter(|c| !c.is_control()).collect()
}

fn apply(plan: BTreeMap<String, Pending>) {
    guard_repo();
    let mut pkgs = load_pkgs();
    // Validate the entire batch before changing any entry.
    for name in plan.keys() {
        if !pkgs.contains_key(name) {
            fail(&format!("{name} is not a tracked package"));
            return;
        }
    }
    for (name, pending) in &plan {
        if let Some(enabled) = pending.ssh {
            let p = pkgs.get_mut(name).unwrap();
            if enabled {
                p.remove("remote");
            } else {
                p.insert("remote".into(), false.into());
            }
            say(&format!(
                "{name}: {} in kit push",
                if enabled { "included" } else { "excluded" }
            ));
        }
    }
    save_pkgs(&pkgs);
    let resolve: Vec<String> = plan
        .iter()
        .filter(|(_, p)| p.resolve)
        .map(|(n, _)| n.clone())
        .collect();
    if !resolve.is_empty() {
        crate::pkg::resolve_tools(&resolve);
    }
    let updates: Vec<String> = plan
        .iter()
        .filter(|(_, p)| p.update)
        .map(|(n, _)| n.clone())
        .collect();
    if !updates.is_empty() {
        crate::updates::update_tools(&updates, false);
    }
    // Updates and resolution can write recipes. Reload before removing entries, and
    // remove last so every queued action can run while the package is still tracked.
    let mut pkgs = load_pkgs();
    for (name, pending) in &plan {
        if pending.remove {
            pkgs.remove(name);
            say(&format!(
                "stopped syncing {name} (installed programs are untouched)"
            ));
        }
    }
    save_pkgs(&pkgs);
    if exit_code() == 0 {
        say("review with kit status; kit sync publishes the package changes");
    } else {
        warn("some actions failed; review the errors above and kit status before syncing");
    }
}

// Ratatui installs a panic hook; this guard also restores the terminal on normal/error returns.
struct RestoreTerminal;
impl Drop for RestoreTerminal {
    fn drop(&mut self) {
        ratatui::restore();
    }
}

struct PackageEditor {
    tracked: BTreeSet<String>,
    entries: Vec<(String, Pkg, bool)>,
    selected: BTreeSet<String>,
    queued: BTreeMap<String, Pending>,
    table: TableState,
    query: String,
    searching: bool,
    message: String,
    page_rows: usize,
}

enum Outcome {
    Apply,
    Cancel,
}

impl PackageEditor {
    fn load() -> Self {
        let tracked = load_pkgs().into_keys().collect();
        let entries = all_packages()
            .into_iter()
            .map(|(name, p)| {
                let installed = pkg_available(&name, &p);
                (name, p, installed)
            })
            .collect();
        Self {
            tracked,
            entries,
            selected: BTreeSet::new(),
            queued: BTreeMap::new(),
            table: TableState::default().with_selected(0),
            query: String::new(),
            searching: false,
            message: "Nothing changes until Apply. Updates check selected tools through their installed provider.".into(),
            page_rows: 5,
        }
    }

    fn visible(&self) -> Vec<usize> {
        let query = self.query.to_lowercase();
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, (n, _, _))| n.to_lowercase().contains(&query))
            .map(|(i, _)| i)
            .collect()
    }

    fn current(&self) -> Option<&str> {
        self.visible()
            .get(self.table.selected().unwrap_or(0))
            .map(|i| self.entries[*i].0.as_str())
    }

    fn navigate(&mut self, delta: isize) {
        let len = self.visible().len();
        self.table.select(if len == 0 {
            None
        } else {
            Some(
                self.table
                    .selected()
                    .unwrap_or(0)
                    .saturating_add_signed(delta)
                    .min(len - 1),
            )
        });
    }

    fn key(&mut self, key: KeyEvent) -> Option<Outcome> {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c' | 'd'))
        {
            return Some(Outcome::Cancel);
        }
        // Let Crossterm decode normal, application-mode and modified arrow sequences.
        match key.code {
            KeyCode::Down => {
                self.navigate(1);
                return None;
            }
            KeyCode::Up => {
                self.navigate(-1);
                return None;
            }
            KeyCode::PageDown => {
                self.navigate(self.page_rows as isize);
                return None;
            }
            KeyCode::PageUp => {
                self.navigate(-(self.page_rows as isize));
                return None;
            }
            KeyCode::Home => {
                self.table.select(Some(0));
                return None;
            }
            KeyCode::End => {
                self.navigate(isize::MAX);
                return None;
            }
            _ => {}
        }
        if self.searching {
            match key.code {
                KeyCode::Enter | KeyCode::Esc => self.searching = false,
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
                _ => {}
            }
            return None;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Some(Outcome::Cancel),
            KeyCode::Enter => return Some(Outcome::Apply),
            KeyCode::Char('/') => {
                self.searching = true;
                self.query.clear();
                self.table.select(Some(0));
            }
            KeyCode::Char('j') => self.navigate(1),
            KeyCode::Char('k') => self.navigate(-1),
            KeyCode::Char(' ') => {
                if let Some(name) = self.current().map(String::from) {
                    if self.tracked.contains(&name) && !self.selected.remove(&name) {
                        self.selected.insert(name);
                    }
                }
            }
            KeyCode::Char('a') => {
                for i in self.visible() {
                    let name = &self.entries[i].0;
                    if self.tracked.contains(name) {
                        self.selected.insert(name.clone());
                    }
                }
            }
            KeyCode::Char('c') => {
                self.queued.clear();
                self.selected.clear();
                self.message = "Queued actions cleared.".into();
            }
            KeyCode::Char(c @ ('u' | 'd' | 'l' | 's')) => {
                let names = if self.selected.is_empty() {
                    self.current().map(String::from).into_iter().collect()
                } else {
                    self.selected.iter().cloned().collect::<Vec<_>>()
                };
                let mut count = 0;
                for name in names {
                    if !self.tracked.contains(&name) {
                        continue;
                    }
                    let original = self
                        .entries
                        .iter()
                        .find(|(n, _, _)| n == &name)
                        .map(|(_, p, _)| ssh_enabled(p))
                        .unwrap();
                    let pending = self.queued.entry(name.clone()).or_default();
                    match c {
                        'u' => pending.update = !pending.update,
                        'd' => pending.remove = !pending.remove,
                        'l' => pending.resolve = !pending.resolve,
                        's' => {
                            let enabled = !pending.ssh.unwrap_or(original);
                            pending.ssh = (enabled != original).then_some(enabled);
                        }
                        _ => unreachable!(),
                    }
                    if pending.count() == 0 {
                        self.queued.remove(&name);
                    }
                    count += 1;
                }
                self.message = if count == 0 {
                    "Kit's built-in dependencies are read-only. Choose a tracked package.".into()
                } else {
                    format!(
                        "Toggled for {count} package(s). Blue = pending. Enter applies; q cancels."
                    )
                };
            }
            _ => {}
        }
        None
    }

    fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();
        if area.width < 35 || area.height < 12 {
            frame.render_widget(Paragraph::new("Window too small. Resize to browse packages.\nq / Esc: cancel   Enter: apply queued actions"), area);
            return;
        }
        let [heading, search, table_area, details, footer] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(6),
            Constraint::Length(5),
        ])
        .areas(area);
        self.page_rows = table_area.height.saturating_sub(3).max(1) as usize;
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    "KIT · Packages",
                    Style::default().fg(Color::Cyan).bold(),
                )),
                Line::from(format!(
                    "{} tracked · {} built-in · {} selected · {} queued",
                    self.tracked.len(),
                    self.entries.len() - self.tracked.len(),
                    self.selected.len(),
                    self.queued.values().map(Pending::count).sum::<usize>()
                )),
            ]),
            heading,
        );
        let search_block = Block::bordered()
            .title(if self.searching {
                " Search · Enter finishes typing "
            } else {
                " Search · / to filter "
            })
            .border_style(Style::default().fg(if self.searching {
                Color::Cyan
            } else {
                Color::DarkGray
            }));
        frame.render_widget(
            Paragraph::new(self.query.as_str()).block(search_block),
            search,
        );
        if self.searching {
            let width = Line::from(self.query.as_str()).width() as u16;
            frame.set_cursor_position((
                search.x + 1 + width.min(search.width.saturating_sub(3)),
                search.y + 1,
            ));
        }
        let visible = self.visible();
        let rows: Vec<Row> = visible
            .iter()
            .map(|i| {
                let (name, p, _) = &self.entries[*i];
                let builtin = !self.tracked.contains(name);
                let pending = self.queued.get(name).cloned().unwrap_or_default();
                let enabled = pending.ssh.unwrap_or_else(|| ssh_enabled(p));
                let blue = Style::default().fg(Color::LightBlue);
                let linux = s(p, "linux")
                    .map(|v| v.split(':').next().unwrap_or(v))
                    .unwrap_or("missing");
                Row::new(vec![
                    Cell::from(if self.selected.contains(name) {
                        "[x]"
                    } else if builtin {
                        " — "
                    } else {
                        "[ ]"
                    }),
                    Cell::from(if builtin {
                        format!("{name} [kit]")
                    } else {
                        name.clone()
                    }),
                    Cell::from(if enabled { "included" } else { "excluded" }).style(
                        if pending.ssh.is_some() {
                            blue
                        } else {
                            Style::default()
                        },
                    ),
                    Cell::from(if pending.update { "[x]" } else { "[ ]" }).style(
                        if pending.update {
                            blue
                        } else {
                            Style::default()
                        },
                    ),
                    Cell::from(if pending.resolve { "resolve" } else { linux }).style(
                        if pending.resolve {
                            blue
                        } else if linux == "missing" {
                            Style::default().fg(Color::Yellow)
                        } else {
                            Style::default()
                        },
                    ),
                    Cell::from(if pending.remove {
                        "stop sync"
                    } else if builtin {
                        "built-in"
                    } else {
                        "tracked"
                    })
                    .style(if pending.remove {
                        blue
                    } else {
                        Style::default()
                    }),
                ])
            })
            .collect();
        let table = Table::new(
            rows,
            [
                Constraint::Length(3),
                Constraint::Min(12),
                Constraint::Length(14),
                Constraint::Length(7),
                Constraint::Length(12),
                Constraint::Length(9),
            ],
        )
        .header(
            Row::new([
                "",
                "Package",
                "SSH deployment",
                "Update",
                "Linux recipe",
                "Tracking",
            ])
            .style(Style::default().bold()),
        )
        .block(
            Block::bordered()
                .title(" Packages ")
                .border_style(Style::default().fg(Color::DarkGray)),
        )
        .row_highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("› ");
        frame.render_stateful_widget(table, table_area, &mut self.table);
        if let Some(i) = visible.get(self.table.selected().unwrap_or(0)) {
            let (name, p, installed) = &self.entries[*i];
            let pending = self.queued.get(name).cloned().unwrap_or_default();
            let ssh = if pending.ssh.unwrap_or_else(|| ssh_enabled(p)) {
                "included in kit push"
            } else {
                "excluded from kit push"
            };
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(format!("macOS: {}", recipe(s(p, "mac")))),
                    Line::from(format!("Linux: {}", recipe(s(p, "linux")))),
                    Line::from(format!(
                        "Fallback: {}",
                        s(p, "linux_fallback")
                            .map(|v| recipe(Some(v)))
                            .unwrap_or_else(|| "none".into())
                    )),
                    Line::from(format!(
                        "SSH deployment: {ssh}. This setting affects kit push only."
                    ))
                    .style(if pending.ssh.is_some() {
                        Style::default().fg(Color::LightBlue)
                    } else {
                        Style::default()
                    }),
                ])
                .block(Block::bordered().title(format!(
                    " {name} · {} ",
                    if *installed {
                        "installed"
                    } else {
                        "missing here"
                    }
                ))),
                details,
            );
        } else {
            frame.render_widget(
                Paragraph::new("No matching packages").block(Block::bordered().title(" Recipes ")),
                details,
            );
        }
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(self.message.as_str()).style(Style::default().fg(Color::Cyan)),
                Line::from("↑↓ move · Space select · a select all · / search"),
                Line::from("u toggle update · d toggle tracking · l toggle resolve · s toggle SSH"),
                Line::from("Enter: APPLY QUEUED ACTIONS     q / Esc: CANCEL     c: clear queue")
                    .bold(),
            ])
            .block(Block::default().borders(Borders::TOP)),
            footer,
        );
    }
}

fn editor() -> io::Result<BTreeMap<String, Pending>> {
    // Load metadata and inspect installed tools before switching the terminal to raw mode.
    let mut app = PackageEditor::load();
    let _restore = RestoreTerminal;
    let mut terminal = ratatui::try_init()?;
    loop {
        terminal.draw(|frame| app.render(frame))?;
        loop {
            match event::read()? {
                Event::Key(key) => {
                    match app.key(key) {
                        Some(Outcome::Apply) => return Ok(app.queued),
                        Some(Outcome::Cancel) => return Ok(BTreeMap::new()),
                        None => {}
                    }
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                        break;
                    }
                }
                Event::Resize(_, _) => break,
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    fn fixture(count: usize) -> PackageEditor {
        let entries: Vec<_> = (0..count)
            .map(|i| {
                let mut p = Pkg::new();
                p.insert("mac".into(), "cargo:probe".into());
                p.insert("linux".into(), "cargo:probe@1".into());
                (format!("probe-{i:03}"), p, true)
            })
            .collect();
        PackageEditor {
            tracked: entries.iter().map(|(n, _, _)| n.clone()).collect(),
            entries,
            selected: BTreeSet::new(),
            queued: BTreeMap::new(),
            table: TableState::default().with_selected(0),
            query: String::new(),
            searching: false,
            message: "Nothing changes until Apply.".into(),
            page_rows: 5,
        }
    }
    fn press(app: &mut PackageEditor, code: KeyCode) -> Option<Outcome> {
        app.key(KeyEvent::new(code, KeyModifiers::NONE))
    }
    fn text(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn navigation_scrolls_the_table_and_handles_resizes() {
        let mut app = fixture(80);
        let mut terminal = Terminal::new(TestBackend::new(100, 26)).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        for _ in 0..35 {
            press(&mut app, KeyCode::Down);
        }
        terminal.draw(|f| app.render(f)).unwrap();
        assert_eq!(app.current(), Some("probe-035"));
        assert!(app.table.offset() > 0);
        let screen = text(&terminal);
        assert!(screen.contains("probe-035"));
        assert!(screen.contains("SSH deployment"));
        assert!(screen.contains("Linux recipe"));
        assert!(screen.contains("kit push only"));
        press(&mut app, KeyCode::End);
        assert_eq!(app.current(), Some("probe-079"));
        press(&mut app, KeyCode::Home);
        assert_eq!(app.current(), Some("probe-000"));
        terminal.backend_mut().resize(80, 24);
        terminal
            .resize(ratatui::layout::Rect::new(0, 0, 80, 24))
            .unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        assert!(text(&terminal).contains("CANCEL"));
        terminal.backend_mut().resize(30, 8);
        terminal
            .resize(ratatui::layout::Rect::new(0, 0, 30, 8))
            .unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        assert!(text(&terminal).contains("Window too small"));
    }

    #[test]
    fn toggles_are_independent_reversible_and_blue() {
        let mut app = fixture(2);
        app.entries[0].1.insert("remote".into(), false.into());
        press(&mut app, KeyCode::Char('s'));
        press(&mut app, KeyCode::Char('u'));
        press(&mut app, KeyCode::Char('l'));
        press(&mut app, KeyCode::Char('d'));
        let p = &app.queued["probe-000"];
        assert_eq!(p.ssh, Some(true));
        assert_eq!(p.count(), 4);
        let mut terminal = Terminal::new(TestBackend::new(100, 26)).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        let buffer = terminal.backend().buffer();
        let line = (0..100)
            .map(|x| buffer[(x, 7)].symbol())
            .collect::<String>();
        assert!(line.contains("included"), "{line}");
        let col = line.find("included").unwrap();
        // All symbols before this column occupy one terminal cell.
        let col = line[..col].chars().count() as u16;
        assert_eq!(buffer[(col, 7)].fg, Color::LightBlue);
        for c in ['s', 'u', 'l', 'd'] {
            press(&mut app, KeyCode::Char(c));
        }
        assert!(app.queued.is_empty());
    }

    #[test]
    fn batch_toggles_keep_selection_and_each_original_scope() {
        let mut app = fixture(2);
        app.entries[0].1.insert("remote".into(), false.into());
        press(&mut app, KeyCode::Char('a'));
        press(&mut app, KeyCode::Char('s'));
        press(&mut app, KeyCode::Char('u'));
        assert_eq!(app.selected.len(), 2);
        assert_eq!(app.queued["probe-000"].ssh, Some(true));
        assert_eq!(app.queued["probe-001"].ssh, Some(false));
        press(&mut app, KeyCode::Char('s'));
        for p in app.queued.values() {
            assert_eq!(p.ssh, None);
            assert!(p.update);
            assert_eq!(p.count(), 1);
        }
        press(&mut app, KeyCode::Char('u'));
        assert!(app.queued.is_empty());
        press(&mut app, KeyCode::Char('x'));
        assert!(app.queued.is_empty());
    }

    #[test]
    fn filters_empty_results_and_key_release_events_are_safe() {
        let mut app = fixture(3);
        press(&mut app, KeyCode::Char('/'));
        for c in "不存在".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        assert!(app.visible().is_empty());
        press(&mut app, KeyCode::Down);
        assert!(app.current().is_none());
        let mut terminal = Terminal::new(TestBackend::new(100, 26)).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        assert!(text(&terminal).contains("No matching packages"));
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Char('/'));
        press(&mut app, KeyCode::Enter);
        let mut release = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        app.key(release);
        assert_eq!(app.current(), Some("probe-000"));
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Char('d'));
        assert!(app.queued["probe-001"].remove);
        assert!(matches!(
            press(&mut app, KeyCode::Char('q')),
            Some(Outcome::Cancel)
        ));
    }
}
