//! scripts/*.sh run after `kit sync` writes files. Header lines (in the comment block at the top):
//!   # kit: on=mac               only on this platform (mac|linux)
//!   # kit: run=onchange         onchange (default): again when the script or a watched path changes
//!                               once: only the first time · always: every apply
//!   # kit: watch=<paths>        files/folders (relative to $HOME) whose changes re-run the script
use crate::core::walk_entries;
use crate::util::*;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

pub struct Meta {
    pub on: Option<String>,
    pub run: String,
    pub watch: Vec<String>,
    pub errors: Vec<String>,
}

/// Repository hooks must be ordinary files. Nonblocking, no-follow open keeps
/// even a concurrently replaced FIFO/symlink from hanging metadata inspection.
pub(crate) fn read_script(path: &Path) -> Result<Vec<u8>, String> {
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| format!("{}: cannot read regular hook file: {e}", path.display()))?;
    if !file.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err(format!(
            "{}: hooks must be regular files, not directories, symlinks or special files",
            path.display()
        ));
    }
    let mut data = Vec::new();
    file.read_to_end(&mut data).map_err(|e| e.to_string())?;
    Ok(data)
}

pub fn script_meta(path: &Path) -> Meta {
    let mut m = Meta {
        on: None,
        run: "onchange".into(),
        watch: vec![],
        errors: vec![],
    };
    let bytes = match read_script(path) {
        Ok(bytes) => bytes,
        Err(error) => {
            m.errors.push(error);
            return m;
        }
    };
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let header = regex::Regex::new(r"^#\s*kit:\s*(\w+)\s*=\s*(.+)").unwrap();
    let trailing = regex::Regex::new(r"\s+#.*$").unwrap();
    for line in text.lines() {
        let s = line.trim();
        if !s.is_empty() && !s.starts_with('#') {
            break; // only the comment block at the top
        }
        let Some(caps) = header.captures(s) else {
            continue;
        };
        let key = caps[1].to_lowercase();
        let val = trailing.replace(&caps[2], "").trim().to_string();
        match key.as_str() {
            "watch" => {
                for rel in val.split_whitespace() {
                    if let Err(e) = crate::core::validate_rel(rel) {
                        m.errors.push(format!("invalid watched path: {e}"));
                    } else {
                        m.watch.push(rel.into());
                    }
                }
            }
            "on" => {
                let v = val.to_lowercase();
                if v != "mac" && v != "linux" {
                    m.errors.push(format!("on={val} (use mac or linux)"));
                }
                m.on = Some(v);
            }
            "run" => {
                let v = val.to_lowercase();
                if ["onchange", "once", "always"].contains(&v.as_str()) {
                    m.run = v;
                } else {
                    m.errors
                        .push(format!("run={val} (use onchange, once or always)"));
                }
            }
            _ => m
                .errors
                .push(format!("unknown header '{key}' (use on, run or watch)")),
        }
    }
    m
}

pub fn script_hash(path: &Path, meta: &Meta) -> String {
    let home = &ctx().home;
    let mut h = Sha256::new();
    h.update(read_script(path).unwrap_or_default());
    for rel in &meta.watch {
        let f = home.join(rel);
        if f.is_dir() {
            let mut entries = Vec::new();
            walk_entries(&f, rel, &mut entries, &mut |_, _| false);
            for e in entries {
                let p = home.join(&e);
                if p.is_file() {
                    h.update(e.as_bytes());
                    h.update(fs::read(&p).unwrap_or_default());
                }
            }
        } else {
            h.update(rel.as_bytes());
            h.update(if f.is_file() {
                fs::read(&f).unwrap_or_default()
            } else {
                b"<missing>".to_vec()
            });
        }
    }
    hex::encode(h.finalize())[..16].to_string()
}

fn done_map() -> serde_json::Map<String, Value> {
    load_obj(&ctx().script_state)
}

/// [(path, meta, state)] state: pending | done | skipped
pub fn script_status() -> Vec<(PathBuf, Meta, &'static str)> {
    let cx = ctx();
    if is_link(&cx.scripts) || (cx.scripts.exists() && !is_real_dir(&cx.scripts)) {
        fail("the repository's scripts/ must be a real directory; move the invalid entry aside before sync");
        return vec![];
    }
    let done = done_map();
    let mut files: Vec<PathBuf> = fs::read_dir(&cx.scripts)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "sh"))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files
        .into_iter()
        .map(|f| {
            let meta = script_meta(&f);
            let name = f.file_name().unwrap().to_string_lossy().into_owned();
            let state = if !meta.errors.is_empty() {
                "pending"
            } else if meta.on.as_deref().is_some_and(|on| on != cx.platform) {
                "skipped"
            } else if meta.run == "always" {
                "pending"
            } else if meta.run == "once" {
                if done.contains_key(&name) {
                    "done"
                } else {
                    "pending"
                }
            } else if done.get(&name).and_then(Value::as_str)
                == Some(script_hash(&f, &meta).as_str())
            {
                "done"
            } else {
                "pending"
            };
            (f, meta, state)
        })
        .collect()
}

fn stem(p: &Path) -> String {
    p.file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

fn name(p: &Path) -> String {
    p.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

pub fn run_scripts(names: Option<&[String]>, force: bool, dry_run: bool) {
    let cx = ctx();
    let mut done = done_map();
    let rows = script_status();
    if let Some(names) = names {
        for n in names {
            if !rows.iter().any(|(f, _, _)| &stem(f) == n || &name(f) == n) {
                fail(&format!("no script called {n}"));
            }
        }
    }
    for (f, meta, state) in rows {
        if let Some(names) = names {
            if !names.contains(&stem(&f)) && !names.contains(&name(&f)) {
                continue;
            }
        }
        for e in &meta.errors {
            fail(&format!("scripts/{}: {e}", name(&f)));
        }
        if !meta.errors.is_empty() {
            continue;
        }
        if state == "skipped" || (state == "done" && !force) {
            continue;
        }
        if dry_run {
            out(&format!("  would run script {}", name(&f)));
            continue;
        }
        say(&format!("running script {}", name(&f)));
        let mut env = vec![("KIT_SOURCE".to_string(), path_str(&cx.source))];
        if let Ok(timeout) = std::env::var("KIT_SCRIPT_TIMEOUT") {
            env.push(("KIT_INSTALL_TIMEOUT".into(), timeout));
        }
        let fstr = path_str(&f);
        // on Linux, scripts get the kit's tools (bat, delta, …) the same way your kit shell does
        let kit_env = cx.home.join(REMOTE_KIT).join("bin/kit-env");
        let cmd: Vec<String> = if !cx.is_mac && kit_env.is_file() {
            vec![
                "bash".into(),
                "-c".into(),
                ". \"$1\" && exec bash \"$2\"".into(),
                "kit".into(),
                path_str(&kit_env),
                fstr.clone(),
            ]
        } else {
            vec!["bash".into(), fstr.clone()]
        };
        if run_install(
            &format!("script-{}", name(&f)),
            &cmd,
            Some(&cx.home),
            Some(&env),
        )
        .code
            == 0
        {
            done.insert(name(&f), Value::from(script_hash(&f, &meta)));
            save_json(&cx.script_state, &Value::Object(done.clone()));
        } else {
            fail(&format!(
                "script {} failed; it runs again on the next `kit sync`",
                name(&f)
            ));
        }
    }
}
