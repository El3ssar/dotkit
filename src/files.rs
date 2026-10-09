//! The commands: init, add, rm, status, save, sync, undo (push lives in remote.rs).
use crate::cli::Args;
use crate::core::*;
use crate::pkg::{all_packages, pkg_installed, pkg_spec};
use crate::scripts::{run_scripts, script_status};
use crate::util::*;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

// --------------------------------------------------------------------------- init
pub fn cmd_init(a: &Args) {
    let cx = ctx();
    let url = a.one("repo");
    if cx.source.join(".git").exists() {
        match url {
            Some(u) => {
                let set = if remote_url().is_empty() { git(&["remote", "add", "origin", &u]) } else { git(&["remote", "set-url", "origin", &u]) };
                if set.code != 0 {
                    die(&format!("could not set the backup repo: {}", set.stderr.trim()));
                }
                say(&format!("backup: {u} — kit sync to back up"));
            }
            None => say(&format!("kit repo already at {} — kit init <url> sets where it's backed up", cx.source.display())),
        }
        return;
    }
    if let Some(repo) = url {
        run_check(&["git", "clone", &repo, &path_str(&cx.source)], false);
        say(&format!("cloned {repo} — kit status to see what it would change here, kit sync to set this machine up"));
        return;
    }
    let _ = fs::create_dir_all(&cx.source);
    git_check(&["init", "-q"]);
    let _ = fs::write(cx.source.join(".gitignore"), ".kit/\n__pycache__/\n");
    if !cx.ignore.exists() {
        let _ = fs::write(
            &cx.ignore,
            "# Never tracked (gitignore-style patterns, relative to $HOME)\n.DS_Store\n.git\n*.swp\n*.bak\n__pycache__\n\n\
             # Tracked, but not sent to servers by `kit push`\n[remote]\n",
        );
    }
    for (path, empty) in [
        (&cx.pkgs, Value::Object(Default::default())),
        (&cx.rules, Value::Array(vec![])),
        (&cx.externals, Value::Object(Default::default())),
        (&cx.dirs, Value::Array(vec![])),
    ] {
        if !path.exists() {
            save_json(path, &empty);
        }
    }
    let _ = fs::create_dir_all(&cx.scripts);
    say(&format!("new kit repo at {}", cx.source.display()));
    note("next: kit add ~/.zshrc ~/.config/nvim … · make an empty PRIVATE repo on GitHub · kit init <its url> · kit sync");
}

// --------------------------------------------------------------------------- add / rm
/// Is this argument a file (or folder) rather than a tool name?
fn is_path_arg(arg: &str) -> bool {
    let home = &ctx().home;
    let p = expanduser(arg, home);
    if arg.starts_with('~') || arg.starts_with('/') || arg.starts_with('.') {
        return true;
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| home.clone());
    exists_or_link(&cwd.join(&p)) || exists_or_link(&home.join(&p))
}

pub fn add_one(rel: &str, force: bool, synced: &mut Synced, quiet_ignored: bool) -> usize {
    if never_track(rel) {
        return 0;
    }
    if ignored_here(rel) && !force {
        if !quiet_ignored {
            if let Some((n, pat)) = ignore_match(rel, &local_contexts()) {
                fail(&format!("skipped ~/{rel}: matches .kitignore line {n} ('{pat}') — remove that line, or kit add --force"));
            }
        }
        return 0;
    }
    if let Err(e) = tree_path(rel) {
        fail(&e);
        return 0;
    }
    let e = read_entry(&ctx().home.join(rel));
    match &e {
        None | Some(Entry::Dir) => return 0,
        Some(Entry::Other) => {
            warn(&format!("skipped ~/{rel}: not a regular file (pipe/socket/device)"));
            return 0;
        }
        Some(Entry::Unreadable) => {
            fail(&format!("can't read ~/{rel} (permission denied)"));
            return 0;
        }
        _ => {}
    }
    if let Some(problem) = secret_problem(rel, e.as_ref()) {
        if !force {
            fail(&format!("~/{rel} not added: {problem}. Keep secrets out of git (kit rm ~/{rel} hides it), or kit add --force"));
            return 0;
        }
    }
    if ctx().tree.join(rel).exists() && !rules_for(rel, &local_contexts()).is_empty() {
        warn(&format!("~/{rel} has {} rules; edit the repo's copy instead (~/.local/share/kit/home)", ctx().platform));
        return 0;
    }
    if let Some(Entry::Link(t)) = &e {
        if t.starts_with('/') {
            warn(&format!("~/{rel} is a symlink to an absolute path ({t}); it may not exist on other machines"));
        }
    }
    let e = e.unwrap();
    if let Err(err) = write_tree(rel, &e) {
        fail(&format!("could not store ~/{rel}: {err}"));
        return 0;
    }
    mark_synced(synced, rel, &e);
    1
}

fn add_files(paths: &[String], force: bool, no_servers: bool) {
    let cx = ctx();
    let mut synced = load_synced();
    let mut dirs = tracked_dirs();
    let mut count = 0;
    let mut tops: Vec<String> = Vec::new();
    for path in paths {
        let rel = rel_of(path);
        let target = cx.home.join(&rel);
        if !exists_or_link(&target) {
            fail(&format!("~/{rel} does not exist"));
            continue;
        }
        if never_track(&rel) || contains_kit(&rel) {
            fail(&format!(
                "~/{rel} {} kit's own folders; track the files inside it you need instead",
                if never_track(&rel) { "is" } else { "contains" }
            ));
            continue;
        }
        if drop_forgotten(&rel) {
            note(&format!("~/{rel} was removed before; tracking it again"));
        }
        if is_real_dir(&target) {
            if ignored_here(&rel) && !force {
                if let Some((n, pat)) = ignore_match(&rel, &local_contexts()) {
                    fail(&format!("skipped ~/{rel}: matches .kitignore line {n} ('{pat}')"));
                }
                continue;
            }
            if tree_path(&format!("{rel}/x")).is_err() {
                fail(&format!("~/{rel} is a folder here but a symlink in the repo: kit rm ~/{rel} first, then add it"));
                continue;
            }
            if !dirs.contains(&rel) {
                dirs.push(rel.clone());
            }
            let mut entries = Vec::new();
            walk_entries(&target, &rel, &mut entries, &mut |drel, _| ignored_here(drel) || contains_kit(drel));
            let mut n = 0;
            for e in entries {
                n += add_one(&e, force, &mut synced, true);
            }
            if n > 500 {
                warn(&format!("~/{rel} added {n} files — sure that's all config? (kit rm ~/{rel} to undo)"));
            }
            count += n;
        } else {
            count += add_one(&rel, force, &mut synced, false);
        }
        tops.push(rel);
    }
    dirs.sort();
    dirs.dedup();
    save_json(&cx.dirs, &Value::from(dirs));
    save_synced(&synced);
    if no_servers && !tops.is_empty() {
        add_ignore(&tops, "remote", "");
        note("kept off servers ([remote] in .kitignore)");
    } else if tops.iter().any(|t| !t.starts_with(".config/") && !t.starts_with(".fizsh/")) {
        note("only ~/.config/… and ~/.fizsh/… travel to servers; other files are backed up and restored on your machines");
    }
    if !tops.is_empty() {
        say(&format!("tracking {count} file(s){}", if count > 0 { " — kit sync to back them up" } else { "" }));
    }
}

pub fn cmd_add(a: &Args) {
    guard_repo();
    let (files, tools): (Vec<String>, Vec<String>) = a.many("paths").into_iter().partition(|p| is_path_arg(p));
    if !files.is_empty() {
        add_files(&files, a.flag("force"), a.flag("no_servers"));
    }
    if !tools.is_empty() {
        crate::pkg::add_tools(&tools, a);
    }
}

pub fn cmd_rm(a: &Args) {
    guard_repo();
    let cx = ctx();
    let mut synced = load_synced();
    let mut dirs = tracked_dirs();
    let pkgs = crate::pkg::load_pkgs();
    let mut tools = Vec::new();
    for path in a.many("paths") {
        if !is_path_arg(&path) && pkgs.contains_key(&crate::pkg::pkg_key(&path)) {
            tools.push(path);
            continue;
        }
        let rel = rel_of(&path);
        if let Err(e) = tree_path(&rel) {
            fail(&e);
            continue;
        }
        let victims = managed(Some(std::slice::from_ref(&rel)));
        let inside_tracked = dirs.iter().any(|d| under(&rel, d) && &rel != d);
        if victims.is_empty() {
            if inside_tracked {
                add_ignore(std::slice::from_ref(&rel), "all", "forgotten");
                say(&format!("~/{rel} won't show up as new anymore"));
            } else {
                fail(&format!("~/{rel} is not tracked"));
            }
            continue;
        }
        let in_git = cx.source.join(".git").exists() && !git(&["ls-files", "--", &format!("home/{rel}")]).stdout.trim().is_empty();
        for v in &victims {
            if let Err(e) = remove_tree(v) {
                fail(&e);
            }
            unmark_synced(&mut synced, v);
        }
        dirs.retain(|d| !under(d, &rel));
        if inside_tracked {
            add_ignore(std::slice::from_ref(&rel), "all", "forgotten");
        }
        say(&format!("stopped tracking ~/{rel} ({} file(s); the files on this machine are untouched)", victims.len()));
        if in_git {
            note("it stays in the repo's git history");
        }
    }
    save_json(&cx.dirs, &Value::from(dirs));
    save_synced(&synced);
    if !tools.is_empty() {
        crate::pkg::rm_tools(&tools);
    }
}

// --------------------------------------------------------------------------- status
/// Collapse many entries of one tracked folder into one line, unless verbose.
pub fn group_rows(rows: &[(&'static str, String)], verbose: bool) -> Vec<(&'static str, String)> {
    if verbose {
        return rows.iter().map(|(c, r)| (*c, format!("~/{r}"))).collect();
    }
    let mut dirs = tracked_dirs();
    dirs.sort_by_key(|d| std::cmp::Reverse(d.len()));
    let mut order: Vec<(&'static str, String)> = Vec::new();
    let mut groups: BTreeMap<(&'static str, String), Vec<String>> = BTreeMap::new();
    for (code, rel) in rows {
        let d = dirs.iter().find(|d| under(rel, d) && rel != *d).cloned();
        let key = (*code, d.unwrap_or_else(|| rel.clone()));
        if !groups.contains_key(&key) {
            order.push(key.clone());
        }
        groups.entry(key).or_default().push(rel.clone());
    }
    let mut out = Vec::new();
    for key in order {
        let items = &groups[&key];
        if items.len() > 3 {
            out.push((key.0, format!("~/{}/ ({} files)", key.1, items.len())));
        } else {
            out.extend(items.iter().map(|r| (key.0, format!("~/{r}"))));
        }
    }
    out
}

const HINTS: &[(&[&str], &str)] = &[
    (&["here", "new", "deleted"], "keep this machine's version → kit save"),
    (&["repo", "missing"], "get the repo's version → kit sync"),
    (&["both"], "changed on both → kit sync merges them (or: kit save --force keeps yours, kit undo <file> takes the repo's)"),
    (&["conflict"], "conflicts → edit the file, keep what you want between <<<<<<< and >>>>>>>, then kit save (or kit undo <file>)"),
    (&["type"], "file vs folder → kit rm it, then kit add it again"),
    (&["new"], "don't want a new file? → kit rm <file>"),
];

/// [(rel, git url, ref, state)] state: ok | missing | stale
pub fn externals_state() -> Vec<(String, String, Option<String>, &'static str)> {
    let cx = ctx();
    let mut out = Vec::new();
    for (rel, spec) in load_obj(&cx.externals) {
        let rel = norm(&rel);
        if rel.starts_with("..") || rel.starts_with('/') {
            continue;
        }
        let url = spec.get("git").and_then(Value::as_str).unwrap_or("").to_string();
        let r#ref = spec.get("ref").and_then(Value::as_str).map(String::from);
        let dest = cx.home.join(&rel);
        if !dest.exists() {
            out.push((rel, url, r#ref, "missing"));
            continue;
        }
        let mut state = "ok";
        if let Some(r) = &r#ref {
            if dest.join(".git").exists() {
                let d = path_str(&dest);
                let g = |args: &[&str]| {
                    let mut cmd = vec!["git", "-C", &d];
                    cmd.extend_from_slice(args);
                    run(&cmd, true)
                };
                let tag = g(&["describe", "--tags", "--exact-match", "HEAD"]).stdout.trim().to_string();
                let head = g(&["rev-parse", "HEAD"]).stdout.trim().to_string();
                let branch = g(&["rev-parse", "--abbrev-ref", "HEAD"]).stdout.trim().to_string();
                let newer = g(&["merge-base", "--is-ancestor", r, "HEAD"]).code == 0;
                if r != &tag && r != &branch && !head.starts_with(r.as_str()) && !newer {
                    state = "stale";
                }
            }
        }
        out.push((rel, url, r#ref, state));
    }
    out
}

fn color_for(code: &str) -> &'static str {
    match code {
        "here" | "deleted" => "33",
        "new" => "35",
        "repo" | "missing" => "36",
        _ => "31",
    }
}

/// Tools kit itself needs on this machine, with how to get them.
fn missing_tools() -> Vec<String> {
    let cx = ctx();
    let mut out = Vec::new();
    if !have("git") {
        out.push("git (everything)".into());
    }
    if cx.is_mac && !have("brew") {
        out.push("brew (Mac tools: https://brew.sh)".into());
    }
    if !cx.is_mac && !have("mise") {
        out.push("mise (Linux tools: curl https://mise.run | sh)".into());
    }
    if crate::diff::delta_cmd().is_none() {
        out.push("delta (nicer diffs: kit sync installs it)".into());
    }
    out
}

pub fn cmd_status(a: &Args) {
    let cx = ctx();
    let prefixes = scope(&a.many("paths"));
    let synced = load_synced();
    let changes = file_changes(prefixes.as_deref(), &synced);
    if !changes.is_empty() {
        out("Files (this machine vs the repo):");
        let rows = group_rows(&changes, a.flag("verbose") || prefixes.is_some());
        for (code, shown) in &rows {
            out(&format!("  {} {shown}", c(color_for(code), &format!("{:16}", label(code)))));
        }
        for (codes, hint) in HINTS {
            if changes.iter().any(|(c, _)| codes.contains(c)) {
                out(&c("2", &format!("  {hint}")));
            }
        }
        if rows.len() < changes.len() {
            out(&c("2", "  kit status -v lists every file · kit status <file> shows its diff"));
        }
    }
    if prefixes.is_some() {
        if changes.is_empty() {
            if exit_code() == 0 {
                say("in sync");
            }
            return;
        }
        let mut patch = String::new();
        for (code, rel) in &changes {
            let tgt = read_entry(&cx.home.join(rel));
            let src = if *code == "new" { None } else { source_entry(rel, &local_contexts()).ok().flatten() };
            patch += &crate::diff::diff_text(rel, src.as_ref(), tgt.as_ref(), "repo", "this machine", label(code));
        }
        out("");
        crate::diff::show_patch(&patch, false, true);
        return;
    }
    let mut problems = !changes.is_empty();
    let missing = missing_tools();
    if !missing.is_empty() {
        problems = true;
        out(&c("33", &format!("Missing: {}", missing.join(" · "))));
    }
    let pk: Vec<String> = all_packages().into_iter().filter(|(n, p)| pkg_spec(p).is_some() && !pkg_installed(n, p)).map(|(n, _)| n).collect();
    if !pk.is_empty() {
        problems = true;
        out(&format!("Tools not installed here: {}{}", pk.join(", "), c("2", "   → kit sync")));
    }
    for (rel, _, r#ref, state) in externals_state() {
        if state != "ok" {
            problems = true;
            let what = if state == "missing" { "missing".to_string() } else { format!("not at {}", r#ref.unwrap_or_default()) };
            out(&format!("External ~/{rel}: {what}{}", c("2", "   → kit sync")));
        }
    }
    let pending: Vec<String> = script_status()
        .into_iter()
        .filter(|(_, m, st)| *st == "pending" && m.run != "always")
        .map(|(f, _, _)| f.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    if !pending.is_empty() {
        problems = true;
        out(&format!("Scripts waiting to run: {}{}", pending.join(", "), c("2", "   → kit sync")));
    }
    for (f, meta, _) in script_status() {
        for e in &meta.errors {
            problems = true;
            out(&c("33", &format!("scripts/{}: {e}", f.file_name().unwrap().to_string_lossy())));
        }
    }
    if cx.source.join(".git").exists() {
        if let Some(busy) = repo_busy() {
            problems = true;
            out(&c("31", &format!("Repo: in the middle of a git {busy} (finish or abort it in ~/.local/share/kit)")));
        }
        let dirty = git(&["status", "--porcelain"]).stdout.lines().count();
        if dirty > 0 || !hidden_by_gitignore().is_empty() {
            problems = true;
            out(&format!("Saved, not backed up yet: {} change(s){}", dirty.max(1), c("2", "   → kit sync")));
        }
        if remote_url().is_empty() {
            problems = true;
            out(&format!("No backup repo yet{}", c("2", "   → make an empty private repo, then kit init <url>")));
        } else if git(&["rev-parse", "--abbrev-ref", "@{u}"]).code == 0 {
            let ab: Vec<String> = git(&["rev-list", "--left-right", "--count", "HEAD...@{u}"]).stdout.split_whitespace().map(String::from).collect();
            if ab.len() == 2 && (ab[0] != "0" || ab[1] != "0") {
                problems = true;
                out(&format!("Repo: {} commit(s) to send, {} to get{}", ab[0], ab[1], c("2", "   → kit sync")));
            }
        }
    }
    if a.flag("verbose") {
        crate::pkg::print_tools();
    }
    if !problems {
        say(if managed(None).is_empty() { "nothing tracked yet — kit add <file, folder or tool>" } else { "everything in sync" });
    }
}

// --------------------------------------------------------------------------- save
/// Copy this machine's changes (changed, new and deleted files) into the repo. Returns how many.
pub fn re_add(prefixes: Option<&[String]>, force: bool) -> usize {
    let cx = ctx();
    let mut synced = load_synced();
    let mut n = 0;
    for (code, rel) in file_changes(prefixes, &synced.clone()) {
        match code {
            "missing" | "unreadable" | "repo" => continue, // nothing new here; the repo's version comes with kit sync
            "type" => {
                warn(&format!("not saved ~/{rel}: a folder here, a file in the repo (kit rm ~/{rel}, then kit add ~/{rel})"));
                continue;
            }
            "both" if !force => {
                fail(&format!("not saved ~/{rel}: the repo changed too — kit sync merges both (or kit save --force ~/{rel} keeps yours)"));
                continue;
            }
            "conflict" if !force => {
                fail(&format!("not saved ~/{rel}: it still has conflict markers (<<<<<<< … >>>>>>>); fix them, or kit save --force ~/{rel}"));
                continue;
            }
            _ => {}
        }
        if code != "deleted" && !rules_for(&rel, &local_contexts()).is_empty() {
            warn(&format!("not saved ~/{rel}: it has {} rules, so edit the repo's copy (~/.local/share/kit/home)", cx.platform));
            continue;
        }
        if code == "deleted" {
            if let Err(e) = remove_tree(&rel) {
                fail(&e);
                continue;
            }
            unmark_synced(&mut synced, &rel);
            out(&format!("  removed {rel} (deleted here)"));
            n += 1;
            continue;
        }
        if code == "new" {
            if add_one(&rel, force, &mut synced, true) > 0 {
                out(&format!("  added   {rel}"));
                n += 1;
            }
            continue;
        }
        let e = read_entry(&cx.home.join(&rel));
        if let Some(problem) = secret_problem(&rel, e.as_ref()) {
            if !force {
                fail(&format!("~/{rel} not saved: {problem} (kit save --force to store it anyway)"));
                continue;
            }
        }
        let Some(e) = e else { continue };
        if let Err(err) = write_tree(&rel, &e) {
            fail(&format!("could not store ~/{rel}: {err}"));
            continue;
        }
        mark_synced(&mut synced, &rel, &e);
        out(&format!("  saved   {rel}"));
        n += 1;
    }
    refresh_synced(&mut synced, prefixes);
    n
}

pub fn cmd_save(a: &Args) {
    guard_repo();
    let prefixes = scope(&a.many("paths"));
    let n = re_add(prefixes.as_deref(), a.flag("force"));
    if n > 0 {
        say(&format!("{n} change(s) saved — kit sync to back them up"));
    } else if exit_code() == 0 {
        say("nothing to save");
    }
}

// --------------------------------------------------------------------------- writing the repo's version here
/// 3-way merge with git: (merged text, number of conflicts) or None if git can't merge it.
fn merge3(here: &[u8], base: &[u8], repo: &[u8]) -> Option<(Vec<u8>, i32)> {
    let dir = ctx().state.join("merge");
    let _ = fs::create_dir_all(&dir);
    let (h, b, r) = (dir.join("here"), dir.join("base"), dir.join("repo"));
    fs::write(&h, here).ok()?;
    fs::write(&b, base).ok()?;
    fs::write(&r, repo).ok()?;
    let out = std::process::Command::new("git")
        .args(["merge-file", "-p", "-L", "this machine", "-L", "last sync", "-L", "repo"])
        .args([&h, &b, &r])
        .output()
        .ok()?;
    let _ = fs::remove_dir_all(&dir);
    let code = out.status.code()?;
    (0..=127).contains(&code).then_some((out.stdout, code))
}

/// Write the repo's version of files here. Files changed only on this machine are left alone,
/// files changed on both are merged. force: take the repo's version even over local changes.
pub fn apply_files(prefixes: Option<&[String]>, force: bool) -> usize {
    let cx = ctx();
    let mut synced = load_synced();
    let stamp = now_stamp();
    let mut written = 0;
    let (mut local, mut merged, mut conflicts, mut stuck) = (0, Vec::new(), Vec::new(), Vec::new());
    for (code, rel) in file_changes(prefixes, &synced.clone()) {
        match code {
            "new" => continue,
            "unreadable" => {
                fail(&format!("can't read ~/{rel} (permission denied)"));
                continue;
            }
            "here" | "deleted" if !force => {
                local += 1;
                continue;
            }
            "conflict" | "type" if !force => {
                stuck.push((code, rel));
                continue;
            }
            _ => {}
        }
        let src = match source_entry(&rel, &local_contexts()) {
            Ok(Some(s)) => s,
            Ok(None) => continue,
            Err(e) => {
                fail(&e);
                continue;
            }
        };
        if let Entry::File(b, _) = &src {
            if has_conflict_markers(b) {
                fail(&format!("~/{rel}: the repo's version has git conflict markers — fix it in ~/.local/share/kit/home first"));
                continue;
            }
        }
        let tgt = cx.home.join(&rel);
        let mut content = src.clone();
        let mut had_conflicts = false;
        if code == "both" && !force {
            let here = read_entry(&tgt);
            let (Some(Entry::File(hb, _)), Entry::File(rb, rmode), Some(base)) = (&here, &src, get_base(&rel)) else {
                stuck.push((code, rel));
                continue;
            };
            if is_binary(hb) || is_binary(rb) || is_binary(&base) {
                stuck.push((code, rel));
                continue;
            }
            match merge3(hb, &base, rb) {
                Some((text, n)) => {
                    had_conflicts = n > 0;
                    content = Entry::File(text, *rmode);
                }
                None => {
                    stuck.push((code, rel));
                    continue;
                }
            }
        }
        let result: KResult<()> = (|| {
            if exists_or_link(&tgt) {
                backup(&stamp, &rel);
            }
            if code == "type" {
                // only with force (kit undo <file>), and backed up just above
                if is_real_dir(&tgt) {
                    fs::remove_dir_all(&tgt).map_err(|e| e.to_string())?;
                } else {
                    fs::remove_file(&tgt).map_err(|e| e.to_string())?;
                }
            }
            write_home(&rel, &content)
        })();
        if let Err(e) = result {
            fail(&format!("could not write ~/{rel}: {e}"));
            continue;
        }
        mark_synced(&mut synced, &rel, &src); // the repo's version is now the common ancestor
        if code == "both" && !force {
            if had_conflicts {
                conflicts.push(rel);
            } else {
                out(&format!("  merged ~/{rel} (both machines' changes; yours aren't saved yet)"));
                merged.push(rel);
            }
        } else {
            out(&format!("  wrote ~/{rel}"));
        }
        written += 1;
    }
    refresh_synced(&mut synced, prefixes);
    release_stamp(&stamp);
    if written > 0 && cx.backups.join(&stamp).exists() {
        note("what was replaced is kept: kit undo puts it back");
    }
    if local > 0 && prefixes.is_none() {
        note(&format!("{local} file(s) changed here aren't saved yet (kit save, or kit status to see them)"));
    }
    for (code, rel) in &stuck {
        let why = match *code {
            "conflict" => "still has conflict markers from an earlier merge; fix them, then kit save",
            "type" => "is a folder on one side and a file on the other; kit rm it, then kit add it again",
            _ => "changed on both, and kit can't merge it (binary, or no common version); kit status ~/… to compare, kit save --force keeps yours, kit undo takes the repo's",
        };
        warn(&format!("~/{rel} {why}"));
    }
    if !conflicts.is_empty() {
        warn(&format!("{} file(s) changed in the same place on both machines:", conflicts.len()));
        for rel in &conflicts {
            err_line(&format!("    ~/{rel}"));
        }
        err_line(&c("2", "    open each one and keep what you want between <<<<<<< this machine and >>>>>>> repo, then kit save"));
        err_line(&c("2", "    or: kit undo <file> takes the repo's version · kit undo puts back yours as it was before this sync"));
        set_exit(1);
    }
    written
}

/// Write the repo's version here: files, then externals, packages and scripts.
fn apply_all(run_new_scripts: bool) {
    let written = apply_files(None, false);
    apply_externals();
    if ctx().is_mac {
        crate::pkg::install_packages();
    } else {
        crate::remote::linux_environment();
    }
    if run_new_scripts {
        run_scripts(None, false, false);
    } else if script_status().iter().any(|(_, _, st)| *st == "pending") {
        note("scripts not run (run kit sync in a terminal to review them)");
    }
    if written == 0 && exit_code() == 0 {
        say("files already up to date");
    }
}

// --------------------------------------------------------------------------- sync
pub fn remote_url() -> String {
    let r = git(&["remote", "get-url", "origin"]);
    if r.code == 0 {
        r.stdout.trim().to_string()
    } else {
        String::new()
    }
}

fn warn_if_public(url: &str) {
    let cx = ctx();
    let re = regex::Regex::new(r"github\.com[:/]([^/]+/[^/]+?)(\.git)?$").unwrap();
    let Some(m) = re.captures(url) else { return };
    if !have("gh") || cx.state.join("visibility-checked").exists() {
        return;
    }
    let repo = m[1].to_string();
    let r = run(&["gh", "repo", "view", &repo, "--json", "visibility", "--jq", ".visibility"], true);
    if r.code == 0 && r.stdout.trim().eq_ignore_ascii_case("public") {
        warn(&format!("your backup repo {repo} is PUBLIC: anyone can read your configs. Make it private: gh repo edit {repo} --visibility private"));
    } else if r.code == 0 {
        let _ = fs::create_dir_all(&cx.state);
        let _ = fs::write(cx.state.join("visibility-checked"), r.stdout);
    }
}

/// Commit everything saved in the repo. Refuses (and commits nothing) if a file holds a secret.
fn commit_saved() {
    let cx = ctx();
    git_check(&["add", "-A", "--", "."]);
    if is_real_dir(&cx.tree) {
        git_check(&["add", "-f", "-A", "--", "home"]); // tracked files hidden by a .gitignore inside home/
    }
    let staged: Vec<String> = git(&["diff", "--cached", "--name-only", "-z"]).stdout.split('\0').filter(|s| !s.is_empty()).map(String::from).collect();
    if staged.is_empty() {
        return;
    }
    let mut leaks = Vec::new();
    for f in &staged {
        let p = cx.source.join(f);
        if let Some(rest) = f.strip_prefix("home/") {
            if !is_link(&p) {
                if let Ok(b) = fs::read(&p) {
                    if let Some(hit) = secret_hit(&b) {
                        leaks.push(format!("~/{rest} ({hit})"));
                    }
                }
            }
        }
    }
    if !leaks.is_empty() {
        git(&["reset", "-q"]);
        die(&format!(
            "these seem to contain secrets, so nothing was synced:\n    {}\n  take the secret out (or kit rm the file); kit save --force <file> stores it anyway",
            leaks.join("\n    ")
        ));
    }
    let msg = format!("kit sync {} on {}", chrono::Local::now().format("%Y-%m-%d %H:%M"), hostname());
    git_check(&["commit", "-q", "-m", &msg]);
}

/// After both machines saved the same files: drop this machine's unpushed commits (their changes
/// are still in $HOME) and let the home-level merge combine them with the other machine's.
fn reconcile_with_remote() {
    let cx = ctx();
    let mb = git(&["merge-base", "HEAD", "@{u}"]).stdout.trim().to_string();
    if mb.is_empty() {
        die("this machine's repo and the backup repo have no history in common; fix it by hand in ~/.local/share/kit");
    }
    let changed: Vec<String> = git(&["diff", "--name-only", &mb, "HEAD"]).stdout.lines().map(String::from).collect();
    let mut synced = load_synced();
    let mut lost = Vec::new();
    for f in &changed {
        if let Some(rel) = f.strip_prefix("home/") {
            // the last version both machines had is the merge base
            let base = git(&["show", &format!("{mb}:{f}")]);
            if base.code == 0 {
                let data = render(rel, base.stdout.as_bytes(), &local_contexts());
                mark_synced(&mut synced, rel, &Entry::File(data, 0o644));
            } else {
                unmark_synced(&mut synced, rel);
            }
        } else {
            // repo files (packages.json, …): merge them in the repo the same way
            let get = |rev: &str| {
                let r = git(&["show", &format!("{rev}:{f}")]);
                (r.code == 0).then_some(r.stdout.into_bytes())
            };
            match (get("HEAD"), get(&mb), get("@{u}")) {
                (Some(ours), Some(base), Some(theirs)) => match merge3(&ours, &base, &theirs) {
                    Some((text, 0)) => lost.push((f.clone(), Some(text))),
                    _ => lost.push((f.clone(), None)),
                },
                (Some(ours), None, None) => lost.push((f.clone(), Some(ours))), // new here only
                _ => lost.push((f.clone(), None)),
            }
        }
    }
    save_synced(&synced);
    git_check(&["reset", "-q", "--hard", "@{u}"]);
    for (f, text) in lost {
        match text {
            Some(t) => {
                let p = cx.source.join(&f);
                if let Some(parent) = p.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                let _ = fs::write(p, t);
            }
            None => warn(&format!("{f}: both machines changed it in the same place; kept the other machine's version (redo your change)")),
        }
    }
    warn("this machine and another one saved changes to the same files; combining them here (check them, then kit save and kit sync)");
}

/// Get what other machines saved. Returns false when new commits change scripts or `cmd:` tools
/// and the user didn't agree to run them.
fn pull() -> bool {
    let cx = ctx();
    let old = git(&["rev-parse", "HEAD"]).stdout.trim().to_string();
    let fetch = git(&["fetch", "-q", "origin"]);
    if fetch.code != 0 {
        die(&format!("couldn't reach the backup repo:\n{}", fetch.stderr.trim()));
    }
    if git(&["rev-parse", "--abbrev-ref", "@{u}"]).code != 0 {
        let branch = git(&["rev-parse", "--abbrev-ref", "HEAD"]).stdout.trim().to_string();
        if git(&["rev-parse", "--verify", "-q", &format!("origin/{branch}")]).code != 0 {
            return true; // empty backup repo: nothing to get yet
        }
        git(&["branch", "-q", "--set-upstream-to", &format!("origin/{branch}")]);
    }
    let r = git(&["rebase", "-q", "@{u}"]);
    if r.code != 0 {
        if repo_busy().is_some() {
            git(&["rebase", "--abort"]);
        }
        reconcile_with_remote();
    }
    let new = git(&["rev-parse", "HEAD"]).stdout.trim().to_string();
    if new == old || old.is_empty() {
        return true;
    }
    let base = git(&["merge-base", &old, &new]).stdout.trim().to_string();
    let got = git(&["rev-list", "--count", &format!("{base}..{new}")]).stdout.trim().to_string();
    if got != "0" {
        say(&format!("got {got} change set(s) from other machines"));
    }
    let risky: Vec<String> = git(&["diff", "--name-only", &format!("{base}..{new}"), "--", "scripts", "externals.json"]).stdout.split_whitespace().map(String::from).collect();
    let pk_old: serde_json::Map<String, Value> = serde_json::from_str(&git(&["show", &format!("{base}:packages.json")]).stdout).unwrap_or_default();
    let cmds: Vec<(String, String)> = load_obj(&cx.pkgs)
        .iter()
        .filter_map(|(n, p)| {
            let spec = p.get(cx.platform)?.as_str()?;
            let before = pk_old.get(n).and_then(|o| o.get(cx.platform)).and_then(Value::as_str);
            (spec.starts_with("cmd:") && before != Some(spec)).then(|| (n.clone(), spec.to_string()))
        })
        .collect();
    if risky.is_empty() && cmds.is_empty() {
        return true;
    }
    out("Other machines changed things that run code here:");
    for f in &risky {
        out(&format!("  {f}"));
    }
    for (n, spec) in &cmds {
        out(&format!("  tool {n}: {spec}"));
    }
    if cx.interactive {
        ask("Run them? [y/N]") == "y"
    } else {
        warn("not running them without a terminal; run kit sync in a terminal to review and allow them");
        false
    }
}

pub fn cmd_sync(_a: &Args) {
    let cx = ctx();
    if !cx.source.join(".git").exists() {
        die("no kit repo yet (kit init)");
    }
    guard_repo();
    commit_saved();
    let url = remote_url();
    let allowed = if url.is_empty() { true } else { pull() };
    apply_all(allowed);
    commit_saved(); // repo files combined during the pull
    if url.is_empty() {
        warn("not backed up yet: make an empty PRIVATE repo, then kit init <its url> and kit sync");
        return;
    }
    let ahead = git(&["rev-list", "--count", "@{u}..HEAD"]);
    if ahead.code == 0 && ahead.stdout.trim() == "0" {
        return;
    }
    let r = git(&["push", "-q", "-u", "origin", "HEAD"]);
    if r.code != 0 {
        die(&format!("couldn't send your changes to {url}:\n{}", r.stderr.trim()));
    }
    say(&format!("backed up to {url}"));
    warn_if_public(&url);
}

// --------------------------------------------------------------------------- undo
fn files_under(base: &Path) -> Vec<std::path::PathBuf> {
    let mut rels = Vec::new();
    walk_entries(base, "", &mut rels, &mut |_, _| false);
    rels.into_iter().map(|r| base.join(r)).collect()
}

fn backup_sets() -> Vec<String> {
    let mut sets: Vec<String> = fs::read_dir(&ctx().backups).map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
    sets.sort();
    sets
}

/// Put a backup set back (what a sync or undo replaced). Itself undoable.
fn restore(stamp: &str) {
    let cx = ctx();
    let base = cx.backups.join(stamp);
    let now = now_stamp();
    for p in files_under(&base) {
        let rel = rel_to(&p, &base).unwrap_or_default();
        let result: KResult<()> = (|| {
            let mut parent = cx.home.clone();
            let parts: Vec<&str> = rel.split('/').collect();
            for part in &parts[..parts.len() - 1] {
                // a symlink/file now sits where the backup had a folder
                parent = parent.join(part);
                if is_link(&parent) || parent.is_file() {
                    backup(&now, &rel_to(&parent, &cx.home).unwrap_or_default());
                    fs::remove_file(&parent).map_err(|e| e.to_string())?;
                }
            }
            backup(&now, &rel);
            let e = read_entry(&p).ok_or("backup file vanished")?;
            write_home(&rel, &e)
        })();
        match result {
            Ok(()) => out(&format!("  put back ~/{rel}")),
            Err(e) => fail(&format!("could not put back ~/{rel}: {e}")),
        }
    }
    let _ = fs::remove_dir_all(&base); // used up; what it replaced is now the newest backup
    release_stamp(&now);
    say(&format!("done{}", if cx.backups.join(&now).exists() { " (kit undo again to reverse this)" } else { "" }));
}

pub fn cmd_undo(a: &Args) {
    let args = a.many("paths");
    let sets = backup_sets();
    if args.is_empty() {
        match sets.last() {
            Some(last) => restore(last),
            None => say("nothing to undo"),
        }
        return;
    }
    let (stamps, files): (Vec<String>, Vec<String>) = args.into_iter().partition(|s| sets.contains(s));
    for s in &stamps {
        restore(s);
    }
    if !files.is_empty() {
        guard_repo();
        let prefixes = scope(&files);
        if exit_code() != 0 {
            return;
        }
        if apply_files(prefixes.as_deref(), true) == 0 && exit_code() == 0 {
            say("already the same as the repo");
        }
    }
}

// --------------------------------------------------------------------------- externals
pub fn apply_externals() {
    let home = &ctx().home;
    for (rel, url, r#ref, st) in externals_state() {
        if st == "ok" {
            continue;
        }
        let dest = path_str(&home.join(&rel));
        let ok = if st == "missing" {
            say(&format!("cloning {url} into ~/{rel}"));
            let mut args: Vec<String> = ["git", "-c", "advice.detachedHead=false", "clone", "-q", "--depth", "1"].iter().map(|s| s.to_string()).collect();
            if let Some(r) = &r#ref {
                args.push("--branch".into());
                args.push(r.clone());
            }
            args.push(url.clone());
            args.push(dest);
            run(&args, false).code == 0
        } else {
            let r = r#ref.clone().unwrap_or_default();
            say(&format!("updating ~/{rel} to {r}"));
            let g = |extra: &[&str], capture: bool| {
                let mut cmd = vec!["git", "-c", "advice.detachedHead=false", "-C", &dest];
                cmd.extend_from_slice(extra);
                run(&cmd, capture).code == 0
            };
            let tagref = format!("refs/tags/{r}:refs/tags/{r}");
            if g(&["fetch", "-q", "--depth", "1", "origin", &tagref], true) {
                g(&["checkout", "-q", &r], false)
            } else {
                g(&["fetch", "-q", "--depth", "1", "origin", &r], true) && g(&["checkout", "-q", "FETCH_HEAD"], false)
            }
        };
        if !ok {
            fail(&format!("could not set up ~/{rel} from {url}"));
        }
    }
}

/// kit < 2.1 kept per-machine state in <repo>/.kit.
pub fn migrate_state() {
    let cx = ctx();
    let old = cx.source.join(".kit");
    if !old.is_dir() {
        return;
    }
    let _ = fs::create_dir_all(&cx.state);
    if old.join("scripts.json").exists() && !cx.script_state.exists() {
        let _ = fs::rename(old.join("scripts.json"), &cx.script_state);
    }
    if let Ok(rd) = fs::read_dir(old.join("backups")) {
        let _ = fs::create_dir_all(&cx.backups);
        for s in rd.flatten() {
            let dest = cx.backups.join(s.file_name());
            if !dest.exists() {
                let _ = fs::rename(s.path(), dest);
            }
        }
    }
    let _ = fs::remove_dir_all(&old);
}
