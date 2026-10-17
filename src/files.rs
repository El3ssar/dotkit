//! The commands: init, add, rm, status, save, sync, undo (push lives in remote.rs).
use crate::cli::Args;
use crate::core::*;
use crate::pkg::{all_packages, pkg_available, pkg_spec};
use crate::scripts::{run_scripts, script_status};
use crate::util::*;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

// --------------------------------------------------------------------------- init
pub fn cmd_init(a: &Args) {
    let cx = ctx();
    let url = a.one("repo");
    if cx.source.join(".git").exists() {
        match url {
            Some(u) => {
                let set = if remote_url().is_empty() {
                    git(&["remote", "add", "origin", &u])
                } else {
                    git(&["remote", "set-url", "origin", &u])
                };
                if set.code != 0 {
                    die(&format!(
                        "could not set the backup repo: {}",
                        set.stderr.trim()
                    ));
                }
                say(&format!("backup: {u} — kit sync to back up"));
            }
            None => say(&format!(
                "kit repo already at {} — kit init <url> sets where it's backed up",
                tilde(&cx.source)
            )),
        }
        return;
    }
    if let Some(repo) = url {
        run_check(&["git", "clone", "-q", &repo, &path_str(&cx.source)], false);
        queue_cloned_code();
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
    say(&format!("new kit repo at {}", tilde(&cx.source)));
    note("next: kit add <files or tools>, then kit init <url of an empty private repo>");
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
    let e = read_home_entry(rel);
    match &e {
        None | Some(Entry::Dir) => return 0,
        Some(Entry::Other) => {
            warn(&format!(
                "skipped ~/{rel}: not a regular file (pipe/socket/device)"
            ));
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
        warn(&format!(
            "~/{rel} has {} rules; edit the repo's copy instead (~/.local/share/kit/home)",
            ctx().platform
        ));
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
    if force {
        allow_secret(rel, &e);
        unignore_exact(rel);
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
                    fail(&format!(
                        "skipped ~/{rel}: matches .kitignore line {n} ('{pat}')"
                    ));
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
            walk_entries(&target, &rel, &mut entries, &mut |drel, _| {
                (!force && ignored_here(drel)) || contains_kit(drel)
            });
            let mut n = 0;
            for e in entries {
                n += add_one(&e, force, &mut synced, true);
            }
            if n > 500 {
                warn(&format!(
                    "~/{rel} added {n} files — sure that's all config? (kit rm ~/{rel} to undo)"
                ));
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
    }
    if !tops.is_empty() {
        say(&format!(
            "tracking {count} file(s){}",
            if count > 0 {
                " — kit sync to back them up"
            } else {
                ""
            }
        ));
    }
}

pub fn cmd_add(a: &Args) {
    guard_repo();
    let (files, tools): (Vec<String>, Vec<String>) =
        a.many("paths").into_iter().partition(|p| is_path_arg(p));
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
            fail(&format!("~/{rel} is not tracked"));
            continue;
        }
        let in_git = cx.source.join(".git").exists()
            && !git(&["ls-files", "--", &format!("home/{rel}")])
                .stdout
                .trim()
                .is_empty();
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
        say(&format!(
            "stopped tracking ~/{rel} ({} file(s); the files on this machine are untouched)",
            victims.len()
        ));
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
    (&["here", "deleted"], "keep this machine's version → kit save"),
    (&["repo", "missing"], "get the repo's version → kit sync"),
    (&["both"], "changed on both → kit sync merges them (or: kit save --force keeps yours, kit undo <file> takes the repo's)"),
    (&["conflict"], "conflicts → edit the file, keep what you want between <<<<<<< and >>>>>>>, then kit save (or kit undo <file>)"),
    (&["type"], "file vs folder → kit rm it, then kit add it again"),
];

/// [(rel, git url, ref, state)] state: ok | missing | stale | ahead | diverged
pub fn externals_state() -> Vec<(String, String, Option<String>, &'static str)> {
    let cx = ctx();
    let mut out = Vec::new();
    for (rel, spec) in load_obj(&cx.externals) {
        validate_rel(&rel).unwrap_or_else(|e| die(&format!("externals.json: {e}")));
        if never_track(&rel) {
            die("externals.json cannot target kit's own folders");
        }
        let url = spec
            .get("git")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let r#ref = spec.get("ref").and_then(Value::as_str).map(String::from);
        if url.is_empty()
            || url.starts_with('-')
            || url.contains(['\n', '\r', '\t'])
            || r#ref.as_ref().is_some_and(|r| {
                r.is_empty() || r.starts_with('-') || r.chars().any(char::is_control)
            })
        {
            fail(&format!("external ~/{rel}: invalid URL or ref"));
            continue;
        }
        if let Err(e) = safe_home_directory(&rel) {
            fail(&e);
            continue;
        }
        let dest = cx.home.join(&rel);
        if !dest.exists() {
            out.push((rel, url, r#ref, "missing"));
            continue;
        }
        let origin = run(
            &[
                "git",
                "-C",
                &path_str(&dest),
                "config",
                "--get",
                "remote.origin.url",
            ],
            true,
        );
        if origin.code == 0
            && origin
                .stdout
                .trim()
                .trim_end_matches('/')
                .trim_end_matches(".git")
                != url.trim_end_matches('/').trim_end_matches(".git")
        {
            fail(&format!(
                "external ~/{rel} has a different origin; left untouched"
            ));
            continue;
        }
        if !dest.join(".git").exists() {
            fail(&format!(
                "external ~/{rel} exists but is not a git checkout"
            ));
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
                let head = g(&["rev-parse", "HEAD"]).stdout.trim().to_string();
                let wanted = g(&["rev-parse", "--verify", &format!("{r}^{{commit}}")]);
                let exact = wanted.code == 0 && wanted.stdout.trim() == head;
                if !exact {
                    state = if wanted.code != 0 {
                        "stale"
                    } else if g(&["merge-base", "--is-ancestor", r, "HEAD"]).code == 0 {
                        "ahead"
                    } else if g(&["merge-base", "--is-ancestor", "HEAD", r]).code == 0 {
                        "stale"
                    } else {
                        "diverged"
                    };
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
        "repo" | "missing" => "36",
        _ => "31",
    }
}

/// Tools kit itself needs on this machine, with how to get them.
fn missing_tools() -> Vec<String> {
    let mut out = Vec::new();
    if !have("git") {
        out.push("git (everything)".into());
    }
    if !have("brew")
        && all_packages().iter().any(|(name, p)| {
            crate::pkg::pkg_spec(p)
                .is_some_and(|s| s.starts_with("brew:") || s.starts_with("cask:"))
                && !crate::pkg::pkg_available(name, p)
        })
    {
        out.push("brew (required by a missing tracked package: https://brew.sh)".into());
    }
    if crate::diff::delta_cmd().is_none() {
        out.push("delta (nicer diffs: kit sync installs it)".into());
    }
    out
}

/// Git paths waiting to be committed, without Git's quoting or directory collapsing.
pub(crate) fn saved_changes() -> Vec<(String, String)> {
    let status = git(&["status", "--porcelain=v1", "-z", "--untracked-files=all"]);
    let mut changes = saved_changes_from_status(&status.stdout);
    changes.extend(
        hidden_by_gitignore()
            .into_iter()
            .map(|p| ("added".into(), format!("home/{p}"))),
    );
    changes.sort();
    changes.dedup();
    changes
}

/// Decode NUL-delimited Git porcelain without invoking Git or changing its index.
pub(crate) fn saved_changes_from_status(text: &str) -> Vec<(String, String)> {
    let mut records = text.split('\0').filter(|s| !s.is_empty());
    let mut changes = Vec::new();
    while let Some(record) = records.next() {
        if record.len() < 4 {
            continue;
        }
        let code = &record[..2];
        let path = &record[3..];
        if code.contains('R') || code.contains('C') {
            if let Some(old) = records.next() {
                if code.contains('R') {
                    changes.push(("deleted".into(), old.into()));
                }
            }
            changes.push(("added".into(), path.into()));
            continue;
        }
        let action = if code.contains('U') || code == "AA" || code == "DD" {
            "conflict"
        } else if code.contains('D') {
            "deleted"
        } else if code.contains('A') || code == "??" {
            "added"
        } else {
            "changed"
        };
        changes.push((action.into(), path.into()));
    }
    changes.sort();
    changes.dedup();
    changes
}

fn revision_packages(revision: &str) -> serde_json::Map<String, Value> {
    let result = git(&["show", &format!("{revision}:packages.json")]);
    serde_json::from_str::<Value>(&result.stdout)
        .ok()
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

fn pending_rows(
    changes: &[(String, String)],
    before: &str,
    after: Option<&str>,
    prefixes: Option<&[String]>,
) -> Vec<String> {
    let mut rows = Vec::new();
    for (action, path) in changes {
        if let Some(ps) = prefixes {
            if !path
                .strip_prefix("home/")
                .is_some_and(|rel| ps.iter().any(|p| under(rel, p)))
            {
                continue;
            }
        }
        if path == "packages.json" {
            let old = revision_packages(before);
            let new = after
                .map(revision_packages)
                .unwrap_or_else(|| load_obj(&ctx().pkgs));
            let mut names: Vec<&String> = old.keys().chain(new.keys()).collect();
            names.sort();
            names.dedup();
            let initial = rows.len();
            for name in names {
                if old.get(name) == new.get(name) {
                    continue;
                }
                let action = if !old.contains_key(name) {
                    "added"
                } else if !new.contains_key(name) {
                    "removed"
                } else {
                    "updated"
                };
                let fields: Vec<&str> = [
                    "mac",
                    "linux",
                    "linux_fallback",
                    "bin",
                    "linux_bin",
                    "remote",
                ]
                .into_iter()
                .filter(|field| {
                    old.get(name).and_then(|p| p.get(*field))
                        != new.get(name).and_then(|p| p.get(*field))
                })
                .collect();
                let detail = if action == "updated" && !fields.is_empty() {
                    format!(" ({})", fields.join(", "))
                } else {
                    String::new()
                };
                rows.push(format!("{action:16} package {name}{detail}"));
            }
            if rows.len() != initial {
                continue;
            }
        }
        let shown = path
            .strip_prefix("home/")
            .map(|p| format!("~/{p}"))
            .unwrap_or_else(|| path.clone());
        let shown = if shown.chars().any(char::is_control) {
            format!("{shown:?}")
        } else {
            shown
        };
        rows.push(format!("{action:16} {shown}"));
    }
    rows
}

fn committed_changes(before: &str, after: &str) -> Vec<(String, String)> {
    let diff = git(&["diff", "--name-status", "--no-renames", "-z", before, after]);
    let mut fields = diff.stdout.split('\0').filter(|s| !s.is_empty());
    let mut changes = Vec::new();
    while let (Some(code), Some(path)) = (fields.next(), fields.next()) {
        changes.push((
            match code {
                "A" => "added",
                "D" => "deleted",
                _ => "changed",
            }
            .into(),
            path.into(),
        ));
    }
    changes
}

/// Show saved work and both directions of changes since the last fetched upstream state.
fn show_pending_changes(prefixes: Option<&[String]>) -> bool {
    if !ctx().source.join(".git").exists() {
        return false;
    }
    let rows = pending_rows(&saved_changes(), "HEAD", None, prefixes);
    let mut pending = !rows.is_empty();
    if pending {
        out(&format!(
            "Saved, not backed up yet:{}",
            c("2", "   → kit sync")
        ));
        for row in rows {
            out(&format!("  {row}"));
        }
    }
    if git(&["rev-parse", "--verify", "@{u}"]).code == 0 {
        let result = git(&["rev-list", "--left-right", "--count", "HEAD...@{u}"]);
        let counts: Vec<&str> = result.stdout.split_whitespace().collect();
        if counts.len() == 2 && (counts[0] != "0" || counts[1] != "0") {
            let base = git(&["merge-base", "HEAD", "@{u}"]);
            let base = if base.code == 0 {
                base.stdout.trim()
            } else {
                "@{u}"
            };
            let outgoing = if counts[0] != "0" {
                pending_rows(
                    &committed_changes(base, "HEAD"),
                    base,
                    Some("HEAD"),
                    prefixes,
                )
            } else {
                vec![]
            };
            let incoming = if counts[1] != "0" {
                pending_rows(
                    &committed_changes(base, "@{u}"),
                    base,
                    Some("@{u}"),
                    prefixes,
                )
            } else {
                vec![]
            };
            if prefixes.is_none() || !outgoing.is_empty() || !incoming.is_empty() {
                pending = true;
                out(&format!(
                    "Repo: {} commit(s) to send, {} to get{}",
                    counts[0],
                    counts[1],
                    c("2", "   → kit sync")
                ));
                for (heading, rows) in
                    [("To send:", outgoing), ("To get (last fetched):", incoming)]
                {
                    if !rows.is_empty() {
                        out(&format!("  {heading}"));
                        for row in rows {
                            out(&format!("    {row}"));
                        }
                    }
                }
            }
        }
    }
    pending
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
            out(&format!(
                "  {} {shown}",
                c(color_for(code), &format!("{:16}", label(code)))
            ));
        }
        for (codes, hint) in HINTS {
            if changes.iter().any(|(c, _)| codes.contains(c)) {
                out(&c("2", &format!("  {hint}")));
            }
        }
        if rows.len() < changes.len() {
            out(&c(
                "2",
                "  kit status -v lists every file · kit status <file> shows its diff",
            ));
        }
    }
    if prefixes.is_some() {
        let pending = show_pending_changes(prefixes.as_deref());
        if changes.is_empty() {
            if exit_code() == 0 && !pending {
                say("in sync");
            }
            return;
        }
        let mut patch = String::new();
        for (code, rel) in &changes {
            let tgt = read_entry(&cx.home.join(rel));
            let src = source_entry(rel, &local_contexts()).ok().flatten();
            patch += &crate::diff::diff_text(
                rel,
                src.as_ref(),
                tgt.as_ref(),
                "repo",
                "this machine",
                label(code),
            );
        }
        out("");
        crate::diff::show_patch(&patch, false, true);
        return;
    }
    let mut problems = !changes.is_empty();
    let mut recipes = Vec::new();
    for (name, p) in all_packages() {
        for platform in ["mac", "linux"] {
            if let Some(spec) = crate::pkg::s(&p, platform) {
                if let Some(why) = crate::recipe::problem(spec) {
                    problems = true;
                    recipes.push((name.clone(), platform, why));
                }
            }
        }
    }
    if !recipes.is_empty() {
        out(&format!(
            "Package recipes need attention ({}){}",
            recipes.len(),
            if recipes.len() <= 3 {
                format!(
                    ": {}",
                    recipes
                        .iter()
                        .map(|(name, platform, _)| format!("{name} ({platform})"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            } else {
                String::new()
            }
        ));
        out("  Installed tools are kept. Repair recipes for future installs: kit packages resolve --both");
        if a.flag("verbose") {
            for (name, platform, why) in recipes {
                out(&format!("  {name} ({platform}): {why}"));
            }
        } else {
            out("  kit status -v shows the reasons; unsupported sources need an explicit supported recipe");
        }
    }
    let missing = missing_tools();
    if !missing.is_empty() {
        problems = true;
        out(&c("33", &format!("Missing: {}", missing.join(" · "))));
    }
    {
        let missing: Vec<String> = all_packages()
            .into_iter()
            .filter(|(_, p)| {
                p.get("linux").is_none()
                    && p.get("mac")
                        .and_then(Value::as_str)
                        .is_some_and(|s| !s.starts_with("cask:"))
            })
            .map(|(n, _)| n)
            .collect();
        if !missing.is_empty() {
            problems = true;
            out(&format!("Tools without Linux recipes: {} (kit packages resolve generates them; kit add <tool> --linux <recipe> overrides)", missing.join(", ")));
        }
    }
    let pk: Vec<String> = all_packages()
        .into_iter()
        .filter(|(n, p)| pkg_spec(p).is_some() && !pkg_available(n, p))
        .map(|(n, _)| n)
        .collect();
    if !pk.is_empty() {
        problems = true;
        out(&format!(
            "Tools not installed here: {}{}",
            pk.join(", "),
            c("2", "   → kit sync")
        ));
    }
    for (rel, _, r#ref, state) in externals_state() {
        if state != "ok" {
            problems = true;
            let reference = r#ref.unwrap_or_default();
            let what = match state {
                "missing" => "missing".to_string(),
                "ahead" => format!("newer than pin {reference}; sync will not downgrade it — review the ref in externals.json"),
                "diverged" => format!("diverged from pin {reference}; sync will preserve local commits — review the checkout and externals.json"),
                _ => format!("not at {reference}"),
            };
            out(&format!(
                "External ~/{rel}: {what}{}",
                if matches!(state, "ahead" | "diverged") {
                    String::new()
                } else {
                    c("2", "   → kit sync")
                }
            ));
        }
    }
    let pending: Vec<String> = script_status()
        .into_iter()
        .filter(|(_, m, st)| *st == "pending" && m.run != "always")
        .map(|(f, _, _)| f.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    if !pending.is_empty() {
        problems = true;
        out(&format!(
            "Scripts waiting to run: {}{}",
            pending.join(", "),
            c("2", "   → kit sync")
        ));
    }
    for (f, meta, _) in script_status() {
        for e in &meta.errors {
            problems = true;
            out(&c(
                "33",
                &format!("scripts/{}: {e}", f.file_name().unwrap().to_string_lossy()),
            ));
        }
    }
    if cx.source.join(".git").exists() {
        if let Some(busy) = repo_busy() {
            problems = true;
            out(&c("31", &format!("Repo: in the middle of a git {busy} (finish or abort it in ~/.local/share/kit)")));
        }
        problems |= show_pending_changes(None);
        if remote_url().is_empty() {
            problems = true;
            out(&format!(
                "No backup repo yet{}",
                c("2", "   → make an empty private repo, then kit init <url>")
            ));
        }
    }
    if a.flag("verbose") {
        crate::pkg::print_tools();
    }
    if !problems {
        say(if managed(None).is_empty() {
            "nothing tracked yet — kit add <file, folder or tool>"
        } else {
            "everything in sync"
        });
    }
}

// --------------------------------------------------------------------------- save
/// Copy this machine's changes to tracked files into the repo. Returns how many.
pub fn re_add(prefixes: Option<&[String]>, force: bool) -> usize {
    let cx = ctx();
    let mut synced = load_synced();
    let mut n = 0;
    for (code, rel) in file_changes(prefixes, &synced.clone()) {
        match code {
            "unreadable" => {
                fail(&format!("cannot read ~/{rel}"));
                continue;
            }
            "missing" | "repo" => continue, // nothing new here; the repo's version comes with kit sync
            "type" => {
                fail(&format!("not saved ~/{rel}: a folder here, a file in the repo (kit rm ~/{rel}, then kit add ~/{rel})"));
                continue;
            }
            "delete-conflict" if !force => {
                fail(&format!("not saved ~/{rel}: deleted here but changed in repo; kit undo takes the repo's or kit save --force keeps the deletion"));
                continue;
            }
            "delete-conflict" => {
                remove_tree(&rel).unwrap_or_else(|e| die(&e));
                unmark_synced(&mut synced, &rel);
                n += 1;
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
            fail(&format!("not saved ~/{rel}: it has {} rules, so edit the repo's copy (~/.local/share/kit/home)", cx.platform));
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
        let e = read_home_entry(&rel);
        if let Some(problem) = secret_problem(&rel, e.as_ref()) {
            if !force {
                fail(&format!(
                    "~/{rel} not saved: {problem} (kit save --force to store it anyway)"
                ));
                continue;
            }
        }
        let Some(e) = e else { continue };
        if let Err(err) = write_tree(&rel, &e) {
            fail(&format!("could not store ~/{rel}: {err}"));
            continue;
        }
        if force {
            allow_secret(&rel, &e);
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
    if exit_code() != 0 {
        return;
    }
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
        .args([
            "merge-file",
            "-p",
            "-L",
            "this machine",
            "-L",
            "last sync",
            "-L",
            "repo",
        ])
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
            "unreadable" => {
                fail(&format!("can't read ~/{rel} (permission denied)"));
                continue;
            }
            "here" | "deleted" if !force => {
                local += 1;
                continue;
            }
            "conflict" | "type" | "delete-conflict" if !force => {
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
            let (Some(Entry::File(hb, _)), Entry::File(rb, rmode), Some(base)) =
                (&here, &src, get_base(&rel))
            else {
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
                backup(&stamp, &rel)?;
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
                out(&format!(
                    "  merged ~/{rel} (both machines' changes; yours aren't saved yet)"
                ));
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
        note(&format!(
            "{local} file(s) changed here aren't saved yet (kit save, or kit status to see them)"
        ));
    }
    if !stuck.is_empty() {
        set_exit(1);
    }
    for (code, rel) in &stuck {
        let why = match *code {
            "conflict" => {
                "still has conflict markers from an earlier merge; fix them, then kit save"
            }
            "type" => {
                "is a folder on one side and a file on the other; kit rm it, then kit add it again"
            }
            _ if get_base(rel).is_none() => {
                "already exists here and differs from the repo, so kit left it alone"
            }
            _ => "changed on both, and kit can't merge it (not a text file)",
        };
        warn(&format!("~/{rel} {why} — kit status ~/{rel} compares them, kit undo ~/{rel} takes the repo's, kit save --force ~/{rel} keeps this one"));
    }
    if !conflicts.is_empty() {
        warn("both machines changed the same lines in:");
        for rel in &conflicts {
            err_line(&format!("    ~/{rel}"));
        }
        err_line(&c(
            "2",
            "    keep what you want between <<<<<<< and >>>>>>>, then kit save",
        ));
        err_line(&c(
            "2",
            "    or: kit undo <file> takes the repo's · kit undo goes back to yours",
        ));
        set_exit(1);
    }
    written
}

/// Write the repo's version here: files, then externals, packages and scripts.
fn apply_all(run_new_scripts: bool) {
    let written = apply_files(None, false);
    if exit_code() != 0 {
        warn("setup skipped until file conflicts are resolved; use kit files to review them");
        return;
    }
    if !run_new_scripts {
        fail("setup is incomplete: repository code needs approval (kit sync --trust, or review in a terminal)");
        return;
    }
    apply_externals();
    if exit_code() != 0 {
        warn("package setup skipped after external checkout errors");
        return;
    }
    if ctx().is_mac {
        crate::pkg::install_packages();
    } else {
        crate::remote::linux_environment();
    }
    if exit_code() != 0 {
        warn("repository hooks skipped after package setup errors");
        return;
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
    let r = git(&["config", "--get", "remote.origin.url"]); // as you typed it (no url rewrites)
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
    let r = run(
        &[
            "gh",
            "repo",
            "view",
            &repo,
            "--json",
            "visibility",
            "--jq",
            ".visibility",
        ],
        true,
    );
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
    let staged: Vec<String> = git(&["diff", "--cached", "--name-only", "-z"])
        .stdout
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    if staged.is_empty() {
        return;
    }
    let mut leaks = Vec::new();
    for f in &staged {
        let p = cx.source.join(f);
        if let Some(rest) = f.strip_prefix("home/") {
            if !is_link(&p) {
                if let Ok(b) = fs::read(&p) {
                    if let Some(hit) = secret_hit(&b).filter(|_| {
                        load_obj(&cx.state.join("secret-overrides.json"))
                            .get(rest)
                            .and_then(Value::as_str)
                            != Some(content_hash(&b).as_str())
                    }) {
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
    let msg = format!(
        "kit sync {} on {}",
        chrono::Local::now().format("%Y-%m-%d %H:%M"),
        hostname()
    );
    git_check(&["commit", "-q", "-m", &msg]);
}

/// Read git blobs without converting binary data or losing executable/symlink modes.
fn git_entry(rev: &str, file: &str) -> Option<Entry> {
    let mode = git(&["ls-tree", rev, "--", file])
        .stdout
        .split_whitespace()
        .next()?
        .to_string();
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(&ctx().source)
        .args(["show", &format!("{rev}:{file}")])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    if mode == "120000" {
        Some(Entry::Link(String::from_utf8(output.stdout).ok()?))
    } else {
        Some(Entry::File(
            output.stdout,
            if mode == "100755" { 0o755 } else { 0o644 },
        ))
    }
}

/// JSON metadata merges by keys, so two machines adding different tools do not conflict.
fn merge_metadata(file: &str, ours: &[u8], base: &[u8], theirs: &[u8]) -> Option<(Vec<u8>, i32)> {
    if file.ends_with(".json") {
        if let (Ok(o), Ok(b), Ok(t)) = (
            serde_json::from_slice::<Value>(ours),
            serde_json::from_slice::<Value>(base),
            serde_json::from_slice::<Value>(theirs),
        ) {
            if file == "dirs.json" {
                if let (Some(o), Some(b), Some(t)) = (o.as_array(), b.as_array(), t.as_array()) {
                    let mut out = Vec::new();
                    for v in o.iter().chain(t.iter()) {
                        if !out.contains(v) && (!b.contains(v) || (o.contains(v) && t.contains(v)))
                        {
                            out.push(v.clone());
                        }
                    }
                    return Some((serde_json::to_vec_pretty(&out).ok()?, 0));
                }
            }
            if let Ok(Some(v)) = merge_json(Some(&b), Some(&o), Some(&t)) {
                return Some((serde_json::to_vec_pretty(&v).ok()?, 0));
            }
            return None;
        }
    }
    merge3(ours, base, theirs)
}

fn merge_json(
    base: Option<&Value>,
    ours: Option<&Value>,
    theirs: Option<&Value>,
) -> Result<Option<Value>, ()> {
    if ours == theirs {
        return Ok(ours.cloned());
    }
    if ours == base {
        return Ok(theirs.cloned());
    }
    if theirs == base {
        return Ok(ours.cloned());
    }
    if let (Some(Value::Object(o)), Some(Value::Object(t))) = (ours, theirs) {
        let b = base.and_then(Value::as_object);
        let keys: std::collections::BTreeSet<_> = o
            .keys()
            .chain(t.keys())
            .chain(b.into_iter().flat_map(|b| b.keys()))
            .collect();
        let mut out = serde_json::Map::new();
        for k in keys {
            if let Some(v) = merge_json(b.and_then(|b| b.get(k)), o.get(k), t.get(k))? {
                out.insert(k.clone(), v);
            }
        }
        return Ok(Some(Value::Object(out)));
    }
    Err(())
}

/// After both machines saved the same files: drop this machine's unpushed commits (their changes
/// are still in $HOME) and let the home-level merge combine them with the other machine's.
fn reconcile_with_remote() {
    let cx = ctx();
    let mb = git(&["merge-base", "HEAD", "@{u}"])
        .stdout
        .trim()
        .to_string();
    if mb.is_empty() {
        die("this machine's repo and the backup repo have no history in common; fix it by hand in ~/.local/share/kit");
    }
    let changed: Vec<String> = git(&["diff", "--name-only", "-z", &mb, "HEAD"])
        .stdout
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    let mut synced = load_synced();
    let stamp = now_stamp();
    release_stamp(&stamp);
    let recovery = format!("refs/kit/recovery/{stamp}");
    git_check(&["update-ref", &recovery, "HEAD"]);
    let mut additions = Vec::new();
    let mut deletions = Vec::new();
    let mut lost = Vec::new();
    for f in &changed {
        if let Some(rel) = f.strip_prefix("home/") {
            // the last version both machines had is the merge base
            let base = git_entry(&mb, f);
            let ours = git_entry("HEAD", f);
            let theirs = git_entry("@{u}", f);
            if ours.is_none() && !same(base.as_ref(), theirs.as_ref()) {
                die(&format!("~/{rel}: deleted here but changed remotely; repo kept intact, recovery: {recovery}"));
            }
            if theirs.is_none() && base.is_some() && !same(ours.as_ref(), base.as_ref()) {
                die(&format!("~/{rel}: changed here but deleted remotely; repo kept intact, recovery: {recovery}"));
            }
            if same(base.as_ref(), theirs.as_ref()) {
                if let Some(e) = ours {
                    additions.push((rel.to_string(), e));
                } else {
                    deletions.push(rel.to_string());
                }
            }
            if let Some(mut e) = base {
                if let Entry::File(b, _) = &mut e {
                    *b = render(rel, b, &local_contexts());
                }
                mark_synced(&mut synced, rel, &e);
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
                (Some(ours), Some(base), Some(theirs)) => match merge_metadata(f, &ours, &base, &theirs) {
                    Some((text, 0)) => lost.push((f.clone(), Some(text))),
                    _ => die(&format!("{f}: conflicting repository changes; repo kept intact, recovery: {recovery}")),
                },
                (Some(ours), None, None) => lost.push((f.clone(), Some(ours))), // new here only
                (None, Some(base), Some(theirs)) if base == theirs => lost.push((f.clone(), None)),
                (Some(ours), Some(base), None) if ours == base => {},
                _ => die(&format!("{f}: conflicting repository changes; repo kept intact, recovery: {recovery}")),
            }
        }
    }
    save_synced(&synced);
    git_check(&["reset", "-q", "--hard", "@{u}"]);
    for (rel, e) in additions {
        write_tree(&rel, &e).unwrap_or_else(|e| die(&e));
        mark_synced(&mut synced, &rel, &e);
    }
    for rel in deletions {
        remove_tree(&rel).unwrap_or_else(|e| die(&e));
        unmark_synced(&mut synced, &rel);
    }
    save_synced(&synced);
    for (f, text) in lost {
        match text {
            Some(t) => {
                let p = cx.source.join(&f);
                if let Some(parent) = p.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                write_atomic(&p, &t, 0o644).unwrap_or_else(|e| {
                    die(&format!("cannot restore {f}: {e}; recovery: {recovery}"))
                });
            }
            None => {
                let p = cx.source.join(&f);
                if p.is_file() {
                    fs::remove_file(p).unwrap_or_else(|e| die(&e.to_string()));
                }
            }
        }
    }
    warn("this machine and another one saved changes to the same files; combining them here (check them, then kit save and kit sync)");
}

fn allow_secret(rel: &str, e: &Entry) {
    if let Entry::File(b, _) = e {
        let path = ctx().state.join("secret-overrides.json");
        let mut allowed = load_obj(&path);
        allowed.insert(rel.into(), Value::from(content_hash(b)));
        save_json(&path, &Value::Object(allowed));
    }
}

fn content_hash(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(data))
}

fn queue_cloned_code() {
    let cx = ctx();
    let mut pending = serde_json::Map::new();
    for (f, meta, _) in script_status() {
        if !meta.errors.is_empty() {
            for error in meta.errors {
                fail(&error);
            }
            continue;
        }
        let name = format!("scripts/{}", f.file_name().unwrap().to_string_lossy());
        if let Ok(data) = crate::scripts::read_script(&f) {
            pending.insert(name, Value::from(content_hash(&data)));
        }
    }
    if !load_obj(&cx.externals).is_empty() {
        pending.insert(
            "externals.json".into(),
            Value::from(content_hash(&fs::read(&cx.externals).unwrap_or_default())),
        );
    }
    for (n, p) in load_obj(&cx.pkgs) {
        for platform in ["mac", "linux"] {
            if let Some(spec) = p
                .get(platform)
                .and_then(Value::as_str)
                .filter(|s| s.starts_with("cmd:"))
            {
                pending.insert(
                    format!("tool:{n}:{platform}"),
                    Value::from(content_hash(spec.as_bytes())),
                );
            }
        }
    }
    save_json(&cx.state.join("pending-code.json"), &Value::Object(pending));
}

fn review_pending_code(trust: bool) -> bool {
    let cx = ctx();
    let path = cx.state.join("pending-code.json");
    let mut pending = load_obj(&path);
    pending.retain(|name, hash| {
        let current = if let Some(tool) = name.strip_prefix("tool:") {
            tool.rsplit_once(':').and_then(|(n, platform)| {
                load_obj(&cx.pkgs)
                    .get(n)
                    .and_then(|p| p.get(platform))
                    .and_then(Value::as_str)
                    .map(|s| content_hash(s.as_bytes()))
            })
        } else {
            crate::scripts::read_script(&cx.source.join(name))
                .ok()
                .map(|b| content_hash(&b))
        };
        if let Some(current) = current {
            *hash = Value::from(current);
            true
        } else {
            false
        }
    });
    if pending.is_empty() {
        save_json(&path, &Value::Object(pending));
        return true;
    }
    out("Repository code waiting for review:");
    for name in pending.keys() {
        out(&format!("  {name}"));
    }
    if trust || (cx.interactive && ask("Run this repository code? [y/N]") == "y") {
        save_json(&path, &serde_json::json!({}));
        true
    } else {
        save_json(&path, &Value::Object(pending));
        warn("not running repository code without approval; inspect it, then kit sync --trust");
        false
    }
}

/// Get what other machines saved. Returns false when new commits change scripts or `cmd:` tools
/// and the user didn't agree to run them.
fn pull() -> bool {
    let cx = ctx();
    let old = git(&["rev-parse", "HEAD"]).stdout.trim().to_string();
    let fetch = git(&["fetch", "-q", "origin"]);
    if fetch.code != 0 {
        die(&format!(
            "couldn't reach the backup repo:\n{}",
            fetch.stderr.trim()
        ));
    }
    if git(&["rev-parse", "--abbrev-ref", "@{u}"]).code != 0 {
        let branch = git(&["rev-parse", "--abbrev-ref", "HEAD"])
            .stdout
            .trim()
            .to_string();
        if git(&["rev-parse", "--verify", "-q", &format!("origin/{branch}")]).code != 0 {
            return true; // empty backup repo: nothing to get yet
        }
        git(&[
            "branch",
            "-q",
            "--set-upstream-to",
            &format!("origin/{branch}"),
        ]);
    }
    let r = git(&["rebase", "-q", "@{u}"]);
    if r.code != 0 {
        if repo_busy().is_some() {
            git(&["rebase", "--abort"]);
        }
        reconcile_with_remote();
    }
    let new = git(&["rev-parse", "HEAD"]).stdout.trim().to_string();
    if new != old && !old.is_empty() {
        let base = git(&["merge-base", &old, &new]).stdout.trim().to_string();
        let mut pending = load_obj(&cx.state.join("pending-code.json"));
        for f in git(&[
            "diff",
            "--name-only",
            "-z",
            &format!("{base}..{new}"),
            "--",
            "scripts",
            "externals.json",
        ])
        .stdout
        .split('\0')
        .filter(|f| !f.is_empty())
        {
            if let Ok(data) = crate::scripts::read_script(&cx.source.join(f)) {
                pending.insert(f.into(), Value::from(content_hash(&data)));
            }
        }
        let before: serde_json::Map<String, Value> =
            serde_json::from_str(&git(&["show", &format!("{base}:packages.json")]).stdout)
                .unwrap_or_default();
        for (n, p) in load_obj(&cx.pkgs) {
            for platform in ["mac", "linux"] {
                if let Some(spec) = p.get(platform).and_then(Value::as_str) {
                    if spec.starts_with("cmd:")
                        && before
                            .get(&n)
                            .and_then(|p| p.get(platform))
                            .and_then(Value::as_str)
                            != Some(spec)
                    {
                        pending.insert(
                            format!("tool:{n}:{platform}"),
                            Value::from(content_hash(spec.as_bytes())),
                        );
                    }
                }
            }
        }
        save_json(&cx.state.join("pending-code.json"), &Value::Object(pending));
    }
    true
}

pub fn cmd_sync(a: &Args) {
    let cx = ctx();
    if !cx.source.join(".git").exists() {
        die("no kit repo yet (kit init)");
    }
    guard_repo();
    commit_saved();
    let url = remote_url();
    let pulled = if url.is_empty() { true } else { pull() };
    if !pulled {
        fail("sync stopped: could not reconcile the backup repository; no setup actions run");
        return;
    }
    let allowed = if a.flag("trust") {
        review_pending_code(true)
    } else {
        pulled && review_pending_code(false)
    };
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
        die(&format!(
            "couldn't send your changes to {url}:\n{}",
            r.stderr.trim()
        ));
    }
    if exit_code() == 0 {
        say(&format!("backed up to {url}"));
    } else {
        warn(&format!(
            "repository backed up to {url}; local setup is incomplete (errors above)"
        ));
    }
    warn_if_public(&url);
}

// --------------------------------------------------------------------------- undo
fn files_under(base: &Path) -> Vec<std::path::PathBuf> {
    fn empty_dirs(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        let entries: Vec<_> = entries.flatten().collect();
        if entries.is_empty() {
            out.push(dir.to_path_buf());
        } else {
            for entry in entries {
                if is_real_dir(&entry.path()) {
                    empty_dirs(&entry.path(), out);
                }
            }
        }
    }
    let mut rels = Vec::new();
    walk_entries(base, "", &mut rels, &mut |_, _| false);
    let mut entries: Vec<_> = rels.into_iter().map(|r| base.join(r)).collect();
    if let Ok(dirs) = fs::read_dir(base) {
        for dir in dirs.flatten() {
            if is_real_dir(&dir.path()) {
                empty_dirs(&dir.path(), &mut entries);
            }
        }
    }
    entries.sort();
    entries
}

fn backup_sets() -> Vec<String> {
    let mut sets: Vec<String> = fs::read_dir(&ctx().backups)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    sets.sort();
    sets
}

/// Put a backup set back (what a sync or undo replaced). Itself undoable.
fn restore(stamp: &str) {
    fn directory_modes(base: &Path, relative: &Path) -> KResult<()> {
        let source = base.join(relative);
        for entry in fs::read_dir(&source).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            if !is_real_dir(&entry.path()) {
                continue;
            }
            let rel = relative.join(entry.file_name());
            directory_modes(base, &rel)?;
            let destination = ctx().home.join(&rel);
            if !is_real_dir(&destination) {
                return Err(format!(
                    "{} was not restored as a directory",
                    destination.display()
                ));
            }
            // Backup parents may have been generated for an individual file. They
            // must never widen permissions on an existing private home directory.
            let saved = entry
                .metadata()
                .map_err(|e| e.to_string())?
                .permissions()
                .mode();
            let current = fs::metadata(&destination)
                .map_err(|e| e.to_string())?
                .permissions()
                .mode();
            fs::set_permissions(
                &destination,
                fs::Permissions::from_mode(saved & current & 0o777),
            )
            .map_err(|e| e.to_string())?;
        }
        Ok(())
    }
    let cx = ctx();
    let base = cx.backups.join(stamp);
    let now = now_stamp();
    let mut failed = false;
    for p in files_under(&base) {
        let rel = rel_to(&p, &base).unwrap_or_default();
        let result: KResult<()> = (|| {
            let mut parent = cx.home.clone();
            let parts: Vec<&str> = rel.split('/').collect();
            for part in &parts[..parts.len() - 1] {
                // a symlink/file now sits where the backup had a folder
                parent = parent.join(part);
                if is_link(&parent) || parent.is_file() {
                    backup(&now, &rel_to(&parent, &cx.home).unwrap_or_default())?;
                    fs::remove_file(&parent).map_err(|e| e.to_string())?;
                }
            }
            backup(&now, &rel)?;
            let e = read_entry(&p).ok_or("backup file vanished")?;
            if is_real_dir(&cx.home.join(&rel)) {
                fs::remove_dir_all(cx.home.join(&rel)).map_err(|e| e.to_string())?;
            }
            if matches!(e, Entry::Dir) {
                let dest = cx.home.join(&rel);
                if exists_or_link(&dest) {
                    fs::remove_file(&dest).map_err(|e| e.to_string())?;
                }
                fs::create_dir_all(&dest).map_err(|e| e.to_string())?;
                let permissions = fs::metadata(&p).map_err(|e| e.to_string())?.permissions();
                fs::set_permissions(&dest, permissions).map_err(|e| e.to_string())
            } else {
                write_home(&rel, &e)?;
                if let Entry::File(_, mode) = &e {
                    // Recovery restores the snapshot's permissions, rather than
                    // inheriting a replacement file's permissions as sync does.
                    fs::set_permissions(cx.home.join(&rel), fs::Permissions::from_mode(*mode))
                        .map_err(|e| e.to_string())?;
                }
                Ok(())
            }
        })();
        match result {
            Ok(()) => out(&format!("  put back ~/{rel}")),
            Err(e) => {
                failed = true;
                fail(&format!("could not put back ~/{rel}: {e}"));
            }
        }
    }
    if let Err(e) = directory_modes(&base, Path::new("")) {
        failed = true;
        fail(&format!("could not restore directory permissions: {e}"));
    }
    if !failed {
        let _ = fs::remove_dir_all(&base);
    }
    release_stamp(&now);
    if failed {
        warn(&format!("backup {stamp} kept; retry with kit undo {stamp}"));
    } else {
        say(&format!(
            "done{}",
            if cx.backups.join(&now).exists() {
                " (kit undo again to reverse this)"
            } else {
                ""
            }
        ));
    }
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
    let (stamps, files): (Vec<String>, Vec<String>) =
        args.into_iter().partition(|s| sets.contains(s));
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
    let git_env = vec![
        ("GIT_TERMINAL_PROMPT".into(), "0".into()),
        (
            "GIT_SSH_COMMAND".into(),
            format!(
                "{} -o BatchMode=yes",
                std::env::var("GIT_SSH_COMMAND").unwrap_or_else(|_| "ssh".into())
            ),
        ),
    ];
    for (rel, url, r#ref, st) in externals_state() {
        if st == "ok" {
            continue;
        }
        let dest = path_str(&home.join(&rel));
        let ok = if st == "missing" {
            say(&format!("cloning {url} into ~/{rel}"));
            let mut args: Vec<String> = [
                "git",
                "-c",
                "advice.detachedHead=false",
                "clone",
                "-q",
                "--depth",
                "1",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect();
            if let Some(r) = &r#ref {
                args.push("--branch".into());
                args.push(r.clone());
            }
            args.push("--".into());
            args.push(url.clone());
            args.push(dest);
            run_install(&format!("external-{rel}"), &args, None, Some(&git_env)).code == 0
        } else {
            let r = r#ref.clone().unwrap_or_default();
            say(&format!("updating ~/{rel} to {r}"));
            let g = |extra: &[&str], _capture: bool| {
                let mut cmd = vec!["git", "-c", "advice.detachedHead=false", "-C", &dest];
                cmd.extend_from_slice(extra);
                query_full(&cmd, 120, None, Some(&git_env)).code == 0
            };
            let checkout = |revision: &str| {
                // A changed pin must not silently abandon commits in a user's checkout.
                if !g(&["diff", "--quiet"], true) || !g(&["diff", "--cached", "--quiet"], true) {
                    fail(&format!("external ~/{rel}: local changes are present; left untouched (review it with git -C ~/{rel})"));
                    return false;
                }
                if !g(&["merge-base", "--is-ancestor", "HEAD", revision], true) {
                    fail(&format!("external ~/{rel}: requested ref {r} differs from this checkout and would abandon commits; left untouched (review it with git -C ~/{rel})"));
                    return false;
                }
                g(&["merge", "--ff-only", "-q", revision], false)
            };
            let tagref = format!("refs/tags/{r}:refs/tags/{r}");
            if g(&["fetch", "-q", "origin", &tagref], true) {
                checkout(&r)
            } else {
                g(&["fetch", "-q", "origin", &r], true) && checkout("FETCH_HEAD")
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
        fs::rename(old.join("scripts.json"), &cx.script_state)
            .unwrap_or_else(|e| die(&format!("state migration failed; old state kept: {e}")));
    }
    if let Ok(rd) = fs::read_dir(old.join("backups")) {
        let _ = fs::create_dir_all(&cx.backups);
        for s in rd.flatten() {
            let dest = cx.backups.join(s.file_name());
            if !dest.exists() {
                fs::rename(s.path(), dest).unwrap_or_else(|e| {
                    die(&format!("backup migration failed; old state kept: {e}"))
                });
            } else {
                die("backup migration collision; old state kept for recovery");
            }
        }
    }
    let _ = fs::remove_dir_all(&old);
}
