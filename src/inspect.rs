//! Offline diagnostics and a non-mutating preview of the next sync.
use crate::cli::Args;
use crate::core::*;
use crate::pkg::{all_packages, pkg_available, s};
use crate::util::*;
use serde_json::{json, Value};
use std::fs;

fn clean(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { '\u{fffd}' } else { c })
        .collect()
}
fn emit(report: Value, json_mode: bool) {
    if json_mode {
        out(&serde_json::to_string_pretty(&report).unwrap());
        return;
    }
    out(report["title"].as_str().unwrap_or("Kit inspection"));
    for row in report["checks"].as_array().into_iter().flatten() {
        let level = row["level"].as_str().unwrap_or("info");
        let mark = match level {
            "error" => "ERROR",
            "warning" => "WARN",
            "ok" => "OK",
            _ => "INFO",
        };
        out(&format!(
            "  {mark:5} {}",
            clean(row["message"].as_str().unwrap_or(""))
        ));
        if let Some(remedy) = row["remedy"].as_str().filter(|s| !s.is_empty()) {
            out(&format!("        {}", clean(remedy)));
        }
    }
    for key in ["saved", "files", "packages", "scripts", "externals"] {
        let rows = report[key].as_array().cloned().unwrap_or_default();
        if rows.is_empty() {
            continue;
        }
        out(&format!("\n{key}:"));
        for row in rows {
            out(&format!(
                "  {}  {}",
                clean(row["action"].as_str().unwrap_or("")),
                clean(row["name"].as_str().unwrap_or(""))
            ));
        }
    }
    if let Some(note) = report["note"].as_str() {
        out(&format!("\n{note}"));
    }
}
fn check(rows: &mut Vec<Value>, id: &str, level: &str, message: String, remedy: &str) {
    rows.push(json!({"id":id,"level":level,"message":message,"remedy":remedy}));
}
/// Validate containers before calling model helpers; malformed metadata is a diagnostic,
/// not a panic or a reason for the dry run to initialize replacement files.
fn metadata(rows: &mut Vec<Value>) -> bool {
    let cx = ctx();
    let mut valid = true;
    let pending = cx.state.join("pending-code.json");
    for (path, array) in [
        (&cx.pkgs, false),
        (&cx.rules, true),
        (&cx.externals, false),
        (&cx.dirs, true),
        (&cx.remotes, false),
        (&cx.synced, false),
        (&cx.script_state, false),
        (&pending, false),
    ] {
        if !path.exists() && !is_link(path) {
            continue;
        }
        if is_link(path)
            || !path.is_file()
            || fs::metadata(path).is_ok_and(|m| m.len() > 16 * 1024 * 1024)
        {
            valid = false;
            check(
                rows,
                "metadata",
                "error",
                format!(
                    "{} must be a regular JSON file of at most 16 MiB",
                    tilde(path)
                ),
                "Replace symlink/special metadata with a regular JSON file.",
            );
            continue;
        }
        let parsed = fs::read(path)
            .map_err(|e| e.to_string())
            .and_then(|b| serde_json::from_slice::<Value>(&b).map_err(|e| e.to_string()));
        match parsed {
            Ok(v) if if array { v.is_array() } else { v.is_object() } => {
                if let Err(error) = validate_records(path, &v) {
                    valid = false;
                    check(
                        rows,
                        "metadata",
                        "error",
                        format!("{}: {error}", tilde(path)),
                        "Repair this metadata before sync.",
                    );
                }
            }
            Ok(_) => {
                valid = false;
                check(
                    rows,
                    "metadata",
                    "error",
                    format!("{} has the wrong JSON shape", tilde(path)),
                    "Repair this file before running sync.",
                );
            }
            Err(e) => {
                valid = false;
                check(
                    rows,
                    "metadata",
                    "error",
                    format!("{}: {e}", tilde(path)),
                    "Repair this file before running sync.",
                );
            }
        }
    }
    if is_link(&cx.tree) || (cx.tree.exists() && !cx.tree.is_dir()) {
        valid = false;
        check(
            rows,
            "source-tree",
            "error",
            "The repository home/ entry must be a real directory, not a symlink or file.".into(),
            "Move the invalid entry aside and restore the tracked home/ directory from Git.",
        );
    }
    valid
}

fn validate_records(path: &std::path::Path, value: &Value) -> Result<(), String> {
    let cx = ctx();
    let valid_name = regex::Regex::new(r"^[A-Za-z0-9_.+-]+$").unwrap();
    if path == cx.pkgs {
        for (name, entry) in value.as_object().unwrap() {
            if !valid_name.is_match(name) || name.starts_with('-') {
                return Err(format!("invalid package name {name:?}"));
            }
            let pkg = entry
                .as_object()
                .ok_or_else(|| format!("package {name:?} must be an object"))?;
            for field in ["mac", "linux", "linux_fallback", "bin", "linux_bin"] {
                if let Some(v) = pkg.get(field) {
                    let text = v
                        .as_str()
                        .ok_or_else(|| format!("{name}.{field} must be a string"))?;
                    if text.chars().any(char::is_control) {
                        return Err(format!("{name}.{field} contains a control character"));
                    }
                    if let Some(system) = text.strip_prefix("system:") {
                        if !valid_name.is_match(system) || system.starts_with('-') {
                            return Err(format!("{name}.{field}: invalid system package name"));
                        }
                    }
                    if matches!(field, "bin" | "linux_bin") && !valid_name.is_match(text) {
                        return Err(format!("{name}.{field} is not a command name"));
                    }
                }
            }
            if pkg.get("remote").is_some_and(|v| !v.is_boolean()) {
                return Err(format!("{name}.remote must be boolean"));
            }
        }
    } else if path == cx.rules {
        for (i, rule) in value.as_array().unwrap().iter().enumerate() {
            let r = rule
                .as_object()
                .ok_or_else(|| format!("rule {i} must be an object"))?;
            validate_rel(
                r.get("path")
                    .and_then(Value::as_str)
                    .ok_or("rule needs a path string")?,
            )?;
            let on = r
                .get("on")
                .and_then(Value::as_array)
                .ok_or("rule needs an on array")?;
            if on.iter().any(|v| {
                !v.as_str()
                    .is_some_and(|s| matches!(s, "all" | "mac" | "linux" | "remote"))
            }) {
                return Err("rule on entries must be all, mac, linux or remote".into());
            }
            if let Some(patterns) = r.get("delete_lines") {
                for p in patterns.as_array().ok_or("delete_lines must be an array")? {
                    regex::Regex::new(p.as_str().ok_or("delete_lines entries must be strings")?)
                        .map_err(|e| e.to_string())?;
                }
            }
            for field in ["regex", "replace"] {
                if let Some(pairs) = r.get(field) {
                    for p in pairs
                        .as_array()
                        .ok_or("rule replacements must be an array")?
                    {
                        let pair = p
                            .as_array()
                            .filter(|a| a.len() == 2 && a.iter().all(Value::is_string))
                            .ok_or("replacement entries must be two strings")?;
                        if field == "regex" {
                            regex::Regex::new(pair[0].as_str().unwrap())
                                .map_err(|e| e.to_string())?;
                        }
                    }
                }
            }
        }
    } else if path == cx.dirs {
        for v in value.as_array().unwrap() {
            validate_rel(v.as_str().ok_or("directory entries must be strings")?)?;
        }
    } else if path == cx.synced {
        for (key, v) in value.as_object().unwrap() {
            validate_rel(key)?;
            if !v.is_string() {
                return Err("sync hashes must be strings".into());
            }
        }
    } else if path == cx.externals {
        for (key, v) in value.as_object().unwrap() {
            validate_rel(key)?;
            let e = v.as_object().ok_or("external entries must be objects")?;
            if !e.get("git").is_some_and(Value::is_string)
                || e.get("ref").is_some_and(|v| !v.is_string())
            {
                return Err("external needs a git URL and optional ref string".into());
            }
        }
    }
    Ok(())
}

pub fn doctor(args: &Args) {
    let cx = ctx();
    let mut rows = vec![];
    check(
        &mut rows,
        "platform",
        "info",
        format!(
            "{} / {} · source {} · state {}",
            cx.platform,
            std::env::consts::ARCH,
            tilde(&cx.source),
            tilde(&cx.state)
        ),
        "",
    );
    for cmd in ["git", "bash", "tar"] {
        check(
            &mut rows,
            cmd,
            if have(cmd) { "ok" } else { "error" },
            format!("{cmd}: {}", if have(cmd) { "available" } else { "missing" }),
            if have(cmd) {
                ""
            } else {
                "Install this prerequisite on the destination, then retry kit doctor."
            },
        );
    }
    if !have("curl") && !have("wget") {
        check(
            &mut rows,
            "downloads",
            "warning",
            "Neither curl nor wget is available; fresh tool bootstrapping needs one.".into(),
            "Install curl or wget and CA certificates.",
        );
    }
    if !cx.source.exists() || !cx.source.join(".git").exists() {
        check(
            &mut rows,
            "repository",
            "error",
            format!("No Kit Git repository at {}", tilde(&cx.source)),
            "Run kit init, or kit init <your-backup-repository>.",
        );
    } else if metadata(&mut rows) {
        if let Some(busy) = repo_busy() {
            check(
                &mut rows,
                "repository",
                "error",
                format!("Repository has an unfinished {busy}"),
                "Finish or abort the Git operation in the source directory.",
            );
        }
        // Parse package entries independently so malformed records are reported in JSON too.
        let manifest: Value = fs::read(&cx.pkgs)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or(json!({}));
        for (name, entry) in manifest.as_object().into_iter().flatten() {
            let Some(pkg) = entry.as_object() else {
                check(
                    &mut rows,
                    "package",
                    "error",
                    format!("Package {name:?} must be an object"),
                    "Repair packages.json.",
                );
                continue;
            };
            for field in ["mac", "linux", "linux_fallback", "bin", "linux_bin"] {
                if pkg.get(field).is_some_and(|v| !v.is_string()) {
                    check(
                        &mut rows,
                        "package",
                        "error",
                        format!("Package {name:?}: {field} must be a string"),
                        "Repair packages.json.",
                    );
                }
            }
            for os in ["mac", "linux"] {
                if let Some(spec) = s(pkg, os) {
                    if let Some(problem) = crate::recipe::problem(spec) {
                        check(&mut rows,"recipe",if os==cx.platform{"error"}else{"warning"},format!("{name} ({os}): {problem}"),"Use kit packages resolve --both or record an explicit supported recipe.");
                    } else if spec.starts_with("cask:") && os == "linux" {
                        check(
                            &mut rows,
                            "recipe",
                            "error",
                            format!("{name}: a Homebrew cask cannot be a Linux recipe"),
                            "Choose a Linux CLI recipe or remove the Linux entry.",
                        );
                    }
                } else if !(os == "linux" && s(pkg, "mac").is_some_and(|s| s.starts_with("cask:")))
                {
                    check(
                        &mut rows,
                        "recipe",
                        "warning",
                        format!("{name}: no {os} recipe"),
                        "Record a recipe for that operating system before deploying there.",
                    );
                }
            }
            if crate::capability::summary(pkg).starts_with("source build") {
                check(&mut rows,"coverage","warning",format!("{name}: source build portability requires testing on each destination"),"kit packages check validates published artifacts; it cannot certify arbitrary source builds.");
            }
        }
        for (path, meta, state) in crate::scripts::script_status() {
            for error in meta.errors {
                check(
                    &mut rows,
                    "script",
                    "error",
                    format!("{}: {error}", tilde(&path)),
                    "Fix the hook metadata before sync.",
                );
            }
            if state == "pending" {
                check(
                    &mut rows,
                    "script",
                    "info",
                    format!("Pending hook: {}", tilde(&path)),
                    "Review repository hooks before approving them.",
                );
            }
        }
        if !load_obj(&cx.state.join("pending-code.json")).is_empty() {
            check(
                &mut rows,
                "trust",
                "warning",
                "Repository code is waiting for review.".into(),
                "Review it in kit sync; use --trust only after you approve that code.",
            );
        }
    }
    if !rows.iter().any(|r| r["level"] == "error") {
        check(&mut rows,"summary","ok","No blocking configuration problems found. This is an offline check, not a portability certificate.".into(),"Use kit sync --dry-run to preview files and missing tools.");
    }
    let failed = rows.iter().any(|r| r["level"] == "error");
    emit(
        json!({"title":"Kit doctor (offline; no setup actions)","platform":cx.platform,"architecture":std::env::consts::ARCH,"checks":rows}),
        args.flag("json"),
    );
    if failed {
        set_exit(1);
    }
}

fn preview_git_status() -> Out {
    let source = path_str(&ctx().source);
    let mut command = vec![
        "git".to_string(),
        "--no-optional-locks".into(),
        "-c".into(),
        "core.fsmonitor=false".into(),
        "-c".into(),
        "core.hooksPath=/dev/null".into(),
        "-C".into(),
        source,
    ];
    let mut config = command.clone();
    config.extend([
        "config".into(),
        "--name-only".into(),
        "--get-regexp".into(),
        r"^filter\..*\.(clean|process|required)$".into(),
    ]);
    let filters = query(&config, 15);
    if !matches!(filters.code, 0 | 1) {
        return filters;
    }
    for key in filters.stdout.lines().filter(|s| !s.is_empty()) {
        command.extend([
            "-c".into(),
            format!(
                "{key}={}",
                if key.ends_with(".required") {
                    "false"
                } else {
                    ""
                }
            ),
        ]);
    }
    command.extend([
        "status".into(),
        "--porcelain=v1".into(),
        "-z".into(),
        "--untracked-files=all".into(),
        "--ignore-submodules=dirty".into(),
    ]);
    query(&command, 15)
}

pub fn plan(args: &Args) {
    let cx = ctx();
    let mut checks = vec![];
    if !metadata(&mut checks) {
        emit(
            json!({"title":"Sync preview","checks":checks}),
            args.flag("json"),
        );
        set_exit(1);
        return;
    }
    if let Some(busy) = repo_busy() {
        check(
            &mut checks,
            "repository",
            "error",
            format!("Unfinished Git {busy}; sync will stop."),
            "Finish or abort it first.",
        );
    }
    let synced = peek_synced();
    let files: Vec<_> = file_changes(None, &synced)
        .into_iter()
        .map(|(state, path)| {
            let mergeable = match (
                read_home_entry(&path),
                source_entry(&path, &local_contexts()).ok().flatten(),
                get_base(&path),
            ) {
                (Some(Entry::File(h, _)), Some(Entry::File(r, _)), Some(base)) => {
                    !is_binary(&h) && !is_binary(&r) && !is_binary(&base)
                }
                _ => false,
            };
            let has_repo_conflicts=matches!(source_entry(&path,&local_contexts()),Ok(Some(Entry::File(ref bytes,_))) if has_conflict_markers(bytes));
            let (action, blocked) = if has_repo_conflicts {("blocked: repository file has conflict markers",true)}else{file_action(state, mergeable)};
            if blocked {
                check(
                    &mut checks,
                    "file",
                    "error",
                    format!("~/{path}: {action}"),
                    "Use kit files to compare versions and choose how to resolve this.",
                );
            }
            json!({"name":format!("~/{path}"),"state":state,"action":action,"blocked":blocked})
        })
        .collect();
    let packages: Vec<_> = all_packages()
        .into_iter()
        .filter_map(|(name, p)| {
            let spec = s(&p, cx.platform);
            if cx.platform == "linux"
                && spec.is_none()
                && s(&p, "mac").is_some_and(|s| s.starts_with("cask:"))
            {
                return None;
            }
            let forbidden = spec.is_some_and(|s| cx.platform == "linux" && s.starts_with("cask:"));
            let action = if forbidden {
                check(
                    &mut checks,
                    "package",
                    "error",
                    format!("{name}: Homebrew casks are macOS-only"),
                    "Choose a Linux CLI recipe.",
                );
                "blocked: macOS-only cask".into()
            } else if cx.platform == "linux"
                && spec.is_some_and(|s| crate::recipe::problem(s).is_some())
            {
                check(
                    &mut checks,
                    "package",
                    "error",
                    format!("{name}: unsupported Linux recipe"),
                    "Resolve or replace the recipe before deploying.",
                );
                "blocked: unsupported recipe".into()
            } else if pkg_available(&name, &p) {
                "keep existing installation".to_string()
            } else if let Some(spec) = spec {
                if let Some(why) = crate::recipe::problem(spec) {
                    check(
                        &mut checks,
                        "package",
                        "error",
                        format!("{name}: {why}"),
                        "Choose a supported recipe before sync.",
                    );
                    "blocked: unsupported recipe".into()
                } else {
                    format!("install {spec}")
                }
            } else {
                check(
                    &mut checks,
                    "package",
                    "error",
                    format!("{name}: missing {} recipe", cx.platform),
                    "Record a recipe before sync.",
                );
                "blocked: missing recipe".into()
            };
            Some(json!({"name":name,"action":action}))
        })
        .collect();
    let scripts:Vec<_>=crate::scripts::script_status().into_iter().filter(|(_,_,state)|*state=="pending").map(|(path,meta,_)| {
        for error in &meta.errors {check(&mut checks,"script","error",format!("{}: {error}",tilde(&path)),"Fix hook metadata before sync.");}
        json!({"name":tilde(&path),"action":if meta.errors.is_empty(){"run after successful setup, subject to code review"}else{"blocked: invalid metadata"}})
    }).collect();
    let externals:Vec<_>=load_obj(&cx.externals).keys().map(|name|json!({"name":format!("~/{name}"),"action":"check repository origin/ref; clone or update only after review"})).collect();
    let status = preview_git_status();
    let saved_rows = crate::files::saved_changes_from_status(&status.stdout);
    let saved = saved_rows.len();
    if status.code != 0 {
        check(
            &mut checks,
            "repository",
            "error",
            "Could not inspect the repository status.".into(),
            "Run kit doctor and git status in the source directory.",
        );
    }
    check(
        &mut checks,
        "saved",
        "info",
        format!("{saved} saved repository change(s) would be committed before reconciliation."),
        "",
    );
    check(
        &mut checks,
        "network",
        "info",
        "Sync would fetch/reconcile the backup repository, then push saved changes if configured."
            .into(),
        "",
    );
    let saved: Vec<_> = saved_rows
        .into_iter()
        .map(|(action, name)| json!({"name":name,"action":format!("commit {action}")}))
        .collect();
    let blocked = checks.iter().any(|r| r["level"] == "error");
    emit(
        json!({"title":"Sync preview (offline; nothing applied)","checks":checks,"files":files,"packages":packages,"scripts":scripts,"externals":externals,"saved":saved,"note":"No commits, fetches, installs, hooks, or file writes were performed. Remote changes are not fetched; the actual sync may discover additional changes. If a file is blocked, sync skips package setup and hooks."}),
        args.flag("json"),
    );
    if blocked || exit_code() != 0 {
        set_exit(1);
    }
}
fn file_action(state: &str, mergeable: bool) -> (&'static str, bool) {
    match state {
        "here" => ("keep unsaved local edits", false),
        "repo" | "missing" => ("write repository version (back up an existing copy)", false),
        "deleted" => (
            "preserve local deletion; kit save records it, kit undo restores it",
            false,
        ),
        "both" if mergeable => (
            "merge both versions; overlapping edits may need resolution",
            false,
        ),
        "both" => (
            "blocked: differing files have no usable text merge base; choose a version",
            true,
        ),
        "conflict" => ("blocked: unresolved conflict markers", true),
        "delete-conflict" => (
            "blocked: local deletion conflicts with repository edits",
            true,
        ),
        "type" => ("blocked: file/directory conflict", true),
        _ => ("blocked: file cannot be read", true),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preview_does_not_promise_to_overwrite_local_edits() {
        assert!(!file_action("here", true).1);
        assert!(file_action("both", false).1);
        assert!(!file_action("both", true).1);
        assert!(file_action("type", true).1);
    }
}
