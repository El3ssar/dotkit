//! Commands that move files between $HOME and the repo.
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
    if cx.source.join(".git").exists() {
        say(&format!("kit repo already at {}", cx.source.display()));
        return;
    }
    if let Some(repo) = a.one("repo") {
        run_check(&["git", "clone", &repo, &path_str(&cx.source)], false);
        say(&format!("cloned {repo} into {} — `kit apply -n` to preview, `kit apply` to set up this machine", cx.source.display()));
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
    for (path, empty) in [(&cx.pkgs, Value::Object(Default::default())), (&cx.rules, Value::Array(vec![])), (&cx.externals, Value::Object(Default::default())), (&cx.dirs, Value::Array(vec![]))] {
        if !path.exists() {
            save_json(path, &empty);
        }
    }
    let _ = fs::create_dir_all(&cx.scripts);
    say(&format!("new kit repo at {}", cx.source.display()));
    note("next: kit add ~/.zshrc ~/.config/nvim … · create an empty PRIVATE repo on GitHub ·");
    note("      kit git remote add origin <its url> · kit save");
}

// --------------------------------------------------------------------------- add / re-add / forget / ignore
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
            fail(&format!("~/{rel} not added: {problem}. Keep secrets out of git: kit ignore it, or kit add --force"));
            return 0;
        }
    }
    if ctx().tree.join(rel).exists() && !rules_for(rel, &local_contexts()).is_empty() {
        warn(&format!("~/{rel} has {} rules; edit it in the repo instead (kit edit ~/{rel})", ctx().platform));
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
    if let Some(d) = digest(Some(&e)) {
        synced.insert(rel.to_string(), d);
    }
    1
}

pub fn cmd_add(a: &Args) {
    guard_repo();
    let cx = ctx();
    let mut synced = load_synced();
    let mut dirs = tracked_dirs();
    let mut count = 0;
    let mut tops: Vec<String> = Vec::new();
    let force = a.flag("force");
    for path in a.many("paths") {
        let rel = rel_of(&path);
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
            note(&format!("~/{rel} was forgotten before; tracking it again"));
        }
        if is_real_dir(&target) {
            if ignored_here(&rel) && !force {
                if let Some((n, pat)) = ignore_match(&rel, &local_contexts()) {
                    fail(&format!("skipped ~/{rel}: matches .kitignore line {n} ('{pat}')"));
                }
                continue;
            }
            if tree_path(&format!("{rel}/x")).is_err() {
                fail(&format!("~/{rel} is a folder here but a symlink in the repo: kit forget ~/{rel} first, then add it"));
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
                warn(&format!("~/{rel} added {n} files — sure that's all config? (kit forget ~/{rel} to undo)"));
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
    if a.flag("no_servers") && !tops.is_empty() {
        add_ignore(&tops, "remote", "");
        note("kept off servers ([remote] in .kitignore)");
    } else if tops.iter().any(|t| !t.starts_with(".config/") && !t.starts_with(".fizsh/")) {
        note("only ~/.config/… and ~/.fizsh/… travel to servers; other files are backed up and restored on your machines");
    }
    say(&format!("tracking {count} file(s){}", if count > 0 { " — `kit save` to back them up" } else { "" }));
}

pub fn re_add(paths: &[String], force: bool) {
    guard_repo();
    let cx = ctx();
    let prefixes = scope(paths);
    let mut synced = load_synced();
    let mut n = 0;
    for (code, rel) in file_changes(prefixes.as_deref(), &synced.clone()) {
        match code {
            "missing" | "unreadable" => continue,
            "type" => {
                warn(&format!("skipped ~/{rel}: a folder here, a file in the repo (kit forget ~/{rel}, then kit add ~/{rel})"));
                continue;
            }
            "repo" if !force => continue, // the repo is newer; that's for `kit apply`
            "both" if !force => {
                warn(&format!(
                    "skipped ~/{rel}: changed here AND in the repo — kit diff ~/{rel}, then kit re-add --force ~/{rel} (keep yours) or kit apply --force ~/{rel} (take the repo's)"
                ));
                continue;
            }
            _ => {}
        }
        if code != "deleted" && !rules_for(&rel, &local_contexts()).is_empty() {
            warn(&format!("skipped ~/{rel}: it has {} rules (edit the repo copy: kit edit ~/{rel})", cx.platform));
            continue;
        }
        if code == "deleted" {
            if let Err(e) = remove_tree(&rel) {
                fail(&e);
                continue;
            }
            synced.remove(&rel);
            out(&format!("  removed {rel} from the repo (deleted here)"));
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
                fail(&format!("~/{rel} not updated: {problem} (kit re-add --force to store it anyway)"));
                continue;
            }
        }
        let Some(e) = e else { continue };
        if let Err(err) = write_tree(&rel, &e) {
            fail(&format!("could not store ~/{rel}: {err}"));
            continue;
        }
        if let Some(d) = digest(Some(&e)) {
            synced.insert(rel.clone(), d);
        }
        out(&format!("  updated {rel}"));
        n += 1;
    }
    refresh_synced(&mut synced, prefixes.as_deref());
    say(&format!("{n} change(s) copied into the repo{}", if n > 0 { " — `kit save` to back them up" } else { "" }));
}

pub fn cmd_re_add(a: &Args) {
    re_add(&a.many("paths"), a.flag("force"));
}

pub fn cmd_forget(a: &Args) {
    guard_repo();
    let cx = ctx();
    let mut synced = load_synced();
    let mut dirs = tracked_dirs();
    for path in a.many("paths") {
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
            synced.remove(v);
        }
        dirs.retain(|d| !under(d, &rel));
        if inside_tracked {
            add_ignore(std::slice::from_ref(&rel), "all", "forgotten");
        }
        say(&format!("forgot ~/{rel} ({} file(s); the files on this machine are untouched)", victims.len()));
        if in_git {
            note("it stays in the repo's git history");
        }
    }
    save_json(&cx.dirs, &Value::from(dirs));
    save_synced(&synced);
}

pub fn cmd_ignore(a: &Args) {
    let section = if a.flag("remote") {
        "remote"
    } else if a.flag("mac") {
        "mac"
    } else if a.flag("linux") {
        "linux"
    } else {
        "all"
    };
    let rels: Vec<String> = a.many("paths").iter().map(|p| rel_of(p)).collect();
    for line in add_ignore(&rels, section, "") {
        out(&format!("  {line}{}", if section != "all" { format!("   [{section}]") } else { String::new() }));
    }
    if section == "remote" {
        say("tracked, but not sent to servers");
    } else {
        say(&format!("ignored{}", if section == "all" { String::new() } else { format!(" on {section}") }));
        let tracked: Vec<String> = rels.iter().filter(|r| !managed(Some(&[(*r).clone()])).is_empty()).map(|r| format!("~/{r}")).collect();
        if !tracked.is_empty() {
            note(&format!("already tracked: {} — kit forget to stop tracking", tracked.join(", ")));
        }
    }
}

pub fn cmd_managed(a: &Args) {
    let prefixes = scope(&a.many("paths"));
    for rel in managed(prefixes.as_deref()) {
        out(&rel);
    }
}

pub fn cmd_unmanaged(a: &Args) {
    let prefixes = scope(&a.many("paths"));
    for rel in new_files(prefixes.as_deref()) {
        out(&rel);
    }
    if prefixes.is_none() {
        let tracked = managed(None);
        let cfg = ctx().home.join(".config");
        if let Ok(rd) = fs::read_dir(&cfg) {
            let mut items: Vec<_> = rd.flatten().collect();
            items.sort_by_key(|e| e.file_name());
            for e in items {
                let rel = format!(".config/{}", e.file_name().to_string_lossy());
                if !tracked.iter().any(|t| under(t, &rel)) && !ignored_here(&rel) {
                    out(&format!("{rel}{}", if e.path().is_dir() { "/" } else { "" }));
                }
            }
        }
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
    (&["here", "new", "deleted"], "keep this machine's version → kit re-add"),
    (&["repo", "missing"], "take the repo's version → kit apply"),
    (&["both", "type"], "changed on both sides → kit diff, then kit re-add --force (yours) or kit apply --force (repo's)"),
    (&["new"], "don't want a new file? → kit ignore <file>"),
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

pub fn cmd_status(a: &Args) {
    let cx = ctx();
    let prefixes = scope(&a.many("paths"));
    let synced = load_synced();
    let changes = file_changes(prefixes.as_deref(), &synced);
    if !changes.is_empty() {
        out("Files (this machine vs the repo):");
        let rows = group_rows(&changes, a.flag("verbose"));
        for (code, shown) in &rows {
            out(&format!("  {} {shown}", c(color_for(code), &format!("{:16}", label(code)))));
        }
        for (codes, hint) in HINTS {
            if changes.iter().any(|(c, _)| codes.contains(c)) {
                out(&c("2", &format!("  {hint}")));
            }
        }
        if rows.len() < changes.len() {
            out(&c("2", "  kit status -v lists every file"));
        }
    }
    if prefixes.is_some() {
        if changes.is_empty() && exit_code() == 0 {
            say("in sync");
        }
        return;
    }
    let mut problems = !changes.is_empty();
    let pk: Vec<String> = all_packages().into_iter().filter(|(n, p)| pkg_spec(p).is_some() && !pkg_installed(n, p)).map(|(n, _)| n).collect();
    if !pk.is_empty() {
        problems = true;
        out(&format!("Packages not installed here: {}{}", pk.join(", "), c("2", "   → kit pkg install")));
    }
    for (rel, _, r#ref, state) in externals_state() {
        if state != "ok" {
            problems = true;
            let what = if state == "missing" { "missing".to_string() } else { format!("not at {}", r#ref.unwrap_or_default()) };
            out(&format!("External ~/{rel}: {what}{}", c("2", "   → kit apply")));
        }
    }
    let pending: Vec<String> = script_status()
        .into_iter()
        .filter(|(_, m, st)| *st == "pending" && m.run != "always")
        .map(|(f, _, _)| f.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    if !pending.is_empty() {
        problems = true;
        out(&format!("Scripts waiting to run: {}{}", pending.join(", "), c("2", "   → kit apply (or kit scripts run)")));
    }
    let hidden = hidden_by_gitignore();
    if !hidden.is_empty() {
        problems = true;
        out(&format!(
            "{}{}",
            c("33", &format!("Not in the backup: {} tracked file(s) hidden by a .gitignore inside home/ (e.g. ~/{})", hidden.len(), hidden[0])),
            c("2", "   → kit save includes them")
        ));
    }
    if cx.source.join(".git").exists() {
        if let Some(busy) = repo_busy() {
            problems = true;
            out(&format!("{}{}", c("31", &format!("Repo: in the middle of a git {busy}")), c("2", "   → kit cd, git status")));
        }
        let dirty = git(&["status", "--porcelain"]).stdout.lines().count();
        if dirty > 0 {
            problems = true;
            out(&format!("Repo: {dirty} unsaved change(s){}", c("2", "   → kit save")));
        }
        if git(&["rev-parse", "--abbrev-ref", "@{u}"]).code == 0 {
            let ab: Vec<String> = git(&["rev-list", "--left-right", "--count", "HEAD...@{u}"]).stdout.split_whitespace().map(String::from).collect();
            if ab.len() == 2 {
                if ab[0] != "0" {
                    problems = true;
                    out(&format!("Repo: {} commit(s) not pushed{}", ab[0], c("2", "   → kit save")));
                }
                if ab[1] != "0" {
                    problems = true;
                    out(&format!("Repo: {} commit(s) to pull{}", ab[1], c("2", "   → kit update")));
                }
            }
        }
    }
    if !problems {
        say(if managed(None).is_empty() { "nothing tracked yet — kit add <file or folder>" } else { "everything in sync" });
    }
}

pub fn cmd_verify(a: &Args) {
    let prefixes = scope(&a.many("paths"));
    let changes: Vec<_> = file_changes(prefixes.as_deref(), &load_synced()).into_iter().filter(|(c, _)| *c != "new").collect();
    for (code, rel) in &changes {
        out(&format!("  {:16} ~/{rel}", label(code)));
    }
    if !changes.is_empty() {
        set_exit(1);
    } else if exit_code() == 0 {
        say("files match the repo");
    }
}

// --------------------------------------------------------------------------- diff
pub fn cmd_diff(a: &Args) {
    let cx = ctx();
    let prefixes = scope(&a.many("paths"));
    let mut parts = String::new();
    for (code, rel) in file_changes(prefixes.as_deref(), &load_synced()) {
        let tgt = read_entry(&cx.home.join(&rel));
        if code == "new" {
            parts += &crate::diff::diff_text(&rel, None, tgt.as_ref(), "repo", "here", "new here, not in the repo (kit re-add adds it)");
            continue;
        }
        let src = source_entry(&rel, &local_contexts()).ok().flatten();
        if a.flag("reverse") {
            parts += &crate::diff::diff_text(&rel, src.as_ref(), tgt.as_ref(), "repo", "here", label(code));
        } else {
            parts += &crate::diff::diff_text(&rel, tgt.as_ref(), src.as_ref(), "this machine", "repo", label(code));
        }
    }
    crate::diff::show_patch(&parts, a.flag("plain"), true);
}

pub fn cmd_cat(a: &Args) {
    let rel = rel_of(&a.one("path").unwrap_or_default());
    let e = if managed(None).contains(&rel) { source_entry(&rel, &local_contexts()).ok().flatten() } else { None };
    match e {
        Some(Entry::Link(t)) => out(&format!("-> {t}")),
        Some(Entry::File(b, _)) => out_raw(&b),
        _ => die(&format!("~/{rel} is not a tracked file")),
    }
}

pub fn cmd_source_path(a: &Args) {
    match a.one("path") {
        Some(p) => match tree_path(&rel_of(&p)) {
            Ok(t) => out(&path_str(&t)),
            Err(e) => die(&e),
        },
        None => out(&path_str(&ctx().source)),
    }
}

// --------------------------------------------------------------------------- apply
pub fn apply_files(prefixes: Option<&[String]>, force: bool, dry_run: bool, verbose: bool) -> (usize, usize) {
    let cx = ctx();
    let mut synced = load_synced();
    let stamp = now_stamp();
    let mut written = 0;
    let mut skipped: Vec<(&str, String)> = Vec::new();
    for (code, rel) in file_changes(prefixes, &synced.clone()) {
        if code == "new" {
            continue;
        }
        if code == "unreadable" {
            fail(&format!("can't read ~/{rel} (permission denied)"));
            continue;
        }
        let risky = matches!(code, "here" | "both" | "deleted" | "type");
        if risky && !force {
            let mut ans = String::new();
            if cx.interactive && !dry_run {
                loop {
                    ans = ask(&format!("~/{rel}: {}. Replace it with the repo's version? [y/N/d=diff]", label(code)));
                    if ans != "d" {
                        break;
                    }
                    let src = source_entry(&rel, &local_contexts()).ok().flatten();
                    crate::diff::show_diff(&rel, read_entry(&cx.home.join(&rel)).as_ref(), src.as_ref(), "here", "repo");
                }
            }
            if ans != "y" {
                skipped.push((code, rel));
                continue;
            }
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
                fail(&format!("~/{rel}: the repo's version has git conflict markers — fix it first (kit edit ~/{rel})"));
                continue;
            }
        }
        if dry_run {
            out(&format!("  would write ~/{rel} ({})", label(code)));
            continue;
        }
        if verbose {
            crate::diff::show_diff(&rel, read_entry(&cx.home.join(&rel)).as_ref(), Some(&src), "here", "repo");
        }
        let tgt = cx.home.join(&rel);
        let result: KResult<()> = (|| {
            if exists_or_link(&tgt) {
                backup(&stamp, &rel);
            }
            if code == "type" {
                // only with --force or a yes, and backed up just above
                if is_real_dir(&tgt) {
                    fs::remove_dir_all(&tgt).map_err(|e| e.to_string())?;
                } else {
                    fs::remove_file(&tgt).map_err(|e| e.to_string())?;
                }
            }
            write_home(&rel, &src)
        })();
        if let Err(e) = result {
            fail(&format!("could not write ~/{rel}: {e}"));
            continue;
        }
        if let Some(d) = digest(Some(&src)) {
            synced.insert(rel.clone(), d);
        }
        out(&format!("  wrote ~/{rel}"));
        written += 1;
    }
    if !dry_run {
        refresh_synced(&mut synced, prefixes);
    }
    if written > 0 {
        let saved = cx.backups.join(&stamp);
        say(&format!(
            "{written} file(s) written{}",
            if saved.exists() { format!(" (replaced versions kept: kit restore {stamp})") } else { String::new() }
        ));
    }
    if !skipped.is_empty() {
        warn(&format!("{} file(s) left alone because they changed on this machine:", skipped.len()));
        for (code, rel) in &skipped {
            err_line(&c("2", &format!("    {:16} ~/{rel}", label(code))));
        }
        err_line(&c("2", "    keep yours: kit re-add · take the repo's: kit apply --force <file> · compare: kit diff"));
    }
    (written, skipped.len())
}

pub fn apply(paths: &[String], force: bool, dry_run: bool, verbose: bool, no_packages: bool, no_scripts: bool) {
    guard_repo();
    let prefixes = scope(paths);
    let (written, skipped) = apply_files(prefixes.as_deref(), force, dry_run, verbose);
    if prefixes.is_none() {
        apply_externals(dry_run);
        if !no_packages {
            if dry_run {
                for (n, p) in all_packages() {
                    if pkg_spec(&p).is_some() && !pkg_installed(&n, &p) {
                        out(&format!("  would install {n}"));
                    }
                }
            } else if ctx().is_mac {
                crate::pkg::install_packages();
            } else {
                crate::remote::linux_environment(false);
            }
        }
        if !no_scripts {
            run_scripts(None, false, dry_run);
        }
    }
    if written == 0 && skipped == 0 && !dry_run && exit_code() == 0 {
        say("files already in sync");
    }
}

pub fn cmd_apply(a: &Args) {
    apply(&a.many("paths"), a.flag("force"), a.flag("dry_run"), a.flag("verbose"), a.flag("no_packages"), a.flag("no_scripts"));
}

pub fn cmd_edit(a: &Args) {
    guard_repo();
    let cx = ctx();
    let rel = rel_of(&a.one("path").unwrap_or_default());
    if !managed(None).contains(&rel) {
        let why = if cx.home.join(&rel).is_dir() { " (it's a folder: name a file in it)" } else { " (kit add it first)" };
        die(&format!("~/{rel} is not a tracked file{why}"));
    }
    let src = tree_path(&rel).unwrap_or_else(|e| die(&e));
    if is_link(&src) {
        die(&format!("~/{rel} is a tracked symlink; nothing to edit"));
    }
    let editor = std::env::var("VISUAL").ok().filter(|s| !s.is_empty()).or_else(|| std::env::var("EDITOR").ok().filter(|s| !s.is_empty())).unwrap_or_else(|| "nvim".into());
    let mut cmd: Vec<String> = editor.split_whitespace().map(String::from).collect();
    cmd.push(path_str(&src));
    if run(&cmd, false).code != 0 {
        die("the editor exited with an error; nothing applied");
    }
    apply_files(Some(&[rel]), false, false, false);
}

fn exec(cmd: &str, args: &[String]) -> ! {
    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new(cmd).args(args).exec();
    die(&format!("could not run {cmd}: {err}"))
}

pub fn cmd_cd(_a: &Args) {
    say(&format!("in {} — exit to return", ctx().source.display()));
    let _ = std::env::set_current_dir(&ctx().source);
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".into());
    exec(&shell, &[]);
}

pub fn cmd_git(a: &Args) {
    let mut args = vec!["-C".to_string(), path_str(&ctx().source)];
    args.extend(a.many("args"));
    exec("git", &args);
}

// --------------------------------------------------------------------------- save / update
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
        warn(&format!(
            "your backup repo {repo} is PUBLIC: anyone can read your configs. Make it private: gh repo edit {repo} --visibility private"
        ));
    } else if r.code == 0 {
        let _ = fs::create_dir_all(&cx.state);
        let _ = fs::write(cx.state.join("visibility-checked"), r.stdout);
    }
}

pub fn cmd_save(a: &Args) {
    let cx = ctx();
    if !cx.source.join(".git").exists() {
        die("no kit repo yet (kit init)");
    }
    guard_repo();
    if a.flag("all") {
        re_add(&[], false);
    }
    let pending: Vec<_> =
        file_changes(None, &load_synced()).into_iter().filter(|(c, _)| matches!(*c, "here" | "new" | "deleted" | "both")).collect();
    if !pending.is_empty() {
        warn(&format!("{} change(s) on this machine are NOT in the repo yet:", pending.len()));
        for (code, shown) in group_rows(&pending, false).into_iter().take(8) {
            err_line(&c("2", &format!("    {:16} {shown}", label(code))));
        }
        err_line(&c("2", "    include them: kit save -a  (or kit re-add first)"));
    }
    git_check(&["add", "-A", "--", "."]);
    if is_real_dir(&cx.tree) {
        git_check(&["add", "-f", "-A", "--", "home"]); // tracked files hidden by a .gitignore inside home/
    }
    let staged: Vec<String> = git(&["diff", "--cached", "--name-only", "-z"]).stdout.split('\0').filter(|s| !s.is_empty()).map(String::from).collect();
    if staged.is_empty() {
        say("nothing new in the repo to save");
    } else {
        if !a.flag("force") {
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
                    "these seem to contain secrets, so nothing was saved:\n    {}\n  take the secret out (or kit forget the file), or kit save --force",
                    leaks.join("\n    ")
                ));
            }
        }
        let msg = a.one("message").unwrap_or_else(|| format!("kit save {} on {}", chrono::Local::now().format("%Y-%m-%d %H:%M"), hostname()));
        git_check(&["commit", "-q", "-m", &msg]);
        say(&format!("committed: {}", msg.lines().next().unwrap_or("")));
    }
    let url = remote_url();
    if url.is_empty() {
        warn("saved on this machine only — not backed up yet. Create an empty PRIVATE repo, then: kit git remote add origin <url> && kit save");
        return;
    }
    let r = git(&["push", "-q", "-u", "origin", "HEAD"]);
    if r.code != 0 {
        if regex::Regex::new(r"rejected|fetch first|non-fast-forward").unwrap().is_match(&r.stderr) {
            die("the backup repo has newer changes from another machine: run `kit update`, then `kit save`");
        }
        die(&format!("push to {url} failed:\n{}", r.stderr.trim()));
    }
    say(&format!("backed up to {url}"));
    warn_if_public(&url);
}

pub fn cmd_update(a: &Args) {
    guard_repo();
    let cx = ctx();
    if remote_url().is_empty() {
        die("no backup repo to update from (kit git remote add origin <url>)");
    }
    let old = git(&["rev-parse", "HEAD"]).stdout.trim().to_string();
    let r = git(&["pull", "-q", "--rebase", "--autostash"]);
    if r.code != 0 {
        let conflicted: Vec<String> = git(&["diff", "--name-only", "--diff-filter=U"]).stdout.split_whitespace().map(String::from).collect();
        if repo_busy().is_some() {
            git(&["rebase", "--abort"]);
        }
        let detail = if conflicted.is_empty() { format!("\n{}", r.stderr.trim()) } else { format!(" Both machines changed: {}.", conflicted.join(", ")) };
        die(&format!(
            "couldn't combine the backup repo's changes with this machine's, so nothing was applied.{detail}\n  Fix it by hand: kit cd, then git pull --rebase, resolve, git rebase --continue"
        ));
    }
    let new = git(&["rev-parse", "HEAD"]).stdout.trim().to_string();
    if new == old {
        say("the repo was already up to date");
    } else {
        say(&format!("pulled {} commit(s)", git(&["rev-list", "--count", &format!("{old}..{new}")]).stdout.trim()));
    }
    let mut no_scripts = a.flag("no_scripts");
    if !old.is_empty() && new != old && !a.flag("yes") && !no_scripts {
        let risky: Vec<String> =
            git(&["diff", "--name-only", &format!("{old}..{new}"), "--", "scripts", "externals.json"]).stdout.split_whitespace().map(String::from).collect();
        let pk_old: serde_json::Map<String, Value> = serde_json::from_str(&git(&["show", &format!("{old}:packages.json")]).stdout).unwrap_or_default();
        let pkgs = load_obj(&cx.pkgs);
        let cmds: Vec<(String, String)> = pkgs
            .iter()
            .filter_map(|(n, p)| {
                let spec = p.get(cx.platform)?.as_str()?;
                let before = pk_old.get(n).and_then(|o| o.get(cx.platform)).and_then(Value::as_str);
                (spec.starts_with("cmd:") && before != Some(spec)).then(|| (n.clone(), spec.to_string()))
            })
            .collect();
        if !risky.is_empty() || !cmds.is_empty() {
            out("The update changes things that run code on this machine:");
            for f in &risky {
                out(&format!("  {f}"));
            }
            for (n, spec) in &cmds {
                out(&format!("  package {n}: {spec}"));
            }
            if cx.interactive {
                if ask("Run them? [y/N]") != "y" {
                    no_scripts = true;
                }
            } else {
                warn("not running new scripts without a terminal; kit update --yes allows it");
                no_scripts = true;
            }
        }
    }
    apply(&[], false, false, false, a.flag("no_packages"), no_scripts);
}

// --------------------------------------------------------------------------- restore / undo
fn files_under(base: &Path) -> Vec<std::path::PathBuf> {
    let mut rels = Vec::new();
    walk_entries(base, "", &mut rels, &mut |_, _| false);
    rels.into_iter().map(|r| base.join(r)).collect()
}

pub fn restore(stamp: Option<String>, yes: bool) {
    let cx = ctx();
    let mut sets: Vec<String> = fs::read_dir(&cx.backups).map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
    sets.sort();
    if sets.is_empty() {
        say("no backups yet (apply keeps the versions it replaces)");
        return;
    }
    let Some(stamp) = stamp else {
        for s in sets.iter().rev().take(15).rev() {
            let base = cx.backups.join(s);
            let files: Vec<String> = files_under(&base).iter().filter_map(|p| rel_to(p, &base)).collect();
            let more = if files.len() > 3 { " …" } else { "" };
            out(&format!("  {s}  {} file(s): {}{more}", files.len(), files.iter().take(3).cloned().collect::<Vec<_>>().join(", ")));
        }
        note("put one back: kit restore <stamp> · the latest: kit undo");
        return;
    };
    let stamp = if stamp == "last" { sets.last().unwrap().clone() } else { stamp };
    let base = cx.backups.join(&stamp);
    if !base.is_dir() {
        die(&format!("no backup called {stamp} (kit restore lists them)"));
    }
    let files = files_under(&base);
    if !yes {
        if !cx.interactive {
            die("add --yes to restore without a terminal");
        }
        if ask(&format!("Put back {} file(s) from {stamp}? [y/N]", files.len())) != "y" {
            return;
        }
    }
    let now = format!("{}-before-restore", now_stamp());
    for p in files {
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
            Ok(()) => out(&format!("  restored ~/{rel}")),
            Err(e) => fail(&format!("could not restore ~/{rel}: {e}")),
        }
    }
    let replaced = cx.backups.join(&now);
    say(&format!("restored {stamp}{}", if replaced.exists() { format!(" (what it replaced: kit restore {now})") } else { String::new() }));
}

pub fn cmd_restore(a: &Args) {
    restore(a.one("stamp"), a.flag("yes"));
}

pub fn cmd_undo(a: &Args) {
    restore(Some("last".into()), a.flag("yes"));
}

// --------------------------------------------------------------------------- externals
pub fn apply_externals(dry_run: bool) {
    let home = &ctx().home;
    for (rel, url, r#ref, st) in externals_state() {
        if st == "ok" {
            continue;
        }
        let dest = path_str(&home.join(&rel));
        if dry_run {
            out(&format!("  would {} ~/{rel} ({url} {})", if st == "missing" { "clone" } else { "update" }, r#ref.clone().unwrap_or_default()));
            continue;
        }
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
