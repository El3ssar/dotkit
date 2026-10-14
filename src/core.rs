//! The model: tracked entries, ignore rules, per-platform rules, secrets and sync state.
use crate::util::*;
use regex::bytes::Regex as BRegex;
use regex::Regex;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

pub type KResult<T> = Result<T, String>;

/// Repository metadata is input too: never allow it to escape a managed root.
pub fn validate_rel(rel: &str) -> KResult<()> {
    if rel.is_empty()
        || rel.chars().any(char::is_control)
        || Path::new(rel)
            .components()
            .any(|p| !matches!(p, std::path::Component::Normal(_)))
    {
        return Err(format!("invalid home-relative path: {rel:?}"));
    }
    Ok(())
}

// --------------------------------------------------------------------------- user paths
/// $HOME-relative path for a user-given path (absolute, ~/..., or relative to cwd or $HOME).
pub fn rel_of(path: &str) -> String {
    rel_of_opt(path, false)
}

pub fn rel_of_opt(path: &str, allow_home: bool) -> String {
    let cx = ctx();
    let raw = expanduser(path, &cx.home);
    let cwd = std::env::current_dir().unwrap_or_else(|_| cx.home.clone());
    let cands: Vec<PathBuf> = if raw.is_absolute() {
        vec![raw.clone()]
    } else {
        vec![cwd.join(&raw), cx.home.join(&raw)]
    };
    let home_real = fs::canonicalize(&cx.home).unwrap_or_else(|_| cx.home.clone());
    let mut rels: Vec<String> = Vec::new();
    for cand in cands {
        let p = PathBuf::from(norm(&path_str(&cand)));
        let parent_real = p
            .parent()
            .map(|pp| fs::canonicalize(pp).unwrap_or_else(|_| pp.to_path_buf()))
            .unwrap_or_default()
            .join(p.file_name().unwrap_or_default());
        for (base, q) in [(&cx.home, &p), (&home_real, &p), (&home_real, &parent_real)] {
            if let Some(r) = rel_to(q, base) {
                rels.push(norm(&r));
                break;
            }
        }
    }
    if rels.is_empty() {
        die(&format!("{path} is not inside your home folder"));
    }
    let tracked = managed(None);
    let mut rel = rels[0].clone();
    for r in &rels {
        if exists_or_link(&cx.home.join(r)) || tracked.iter().any(|t| under(t, r)) {
            rel = r.clone();
            break;
        }
    }
    if rel.is_empty() && !allow_home {
        die("that's your whole home folder; name the files or folders to use");
    }
    if rel.starts_with("..") {
        die(&format!("{path} is not inside your home folder"));
    }
    rel
}

/// kit's own folders, relative to $HOME: the repo, the server install dir, kit's state.
pub fn kit_dirs() -> Vec<String> {
    let cx = ctx();
    let mut own = vec![REMOTE_KIT.to_string(), ".local/state/kit".to_string()];
    let home_real = fs::canonicalize(&cx.home).unwrap_or_else(|_| cx.home.clone());
    for p in [&cx.source, &cx.state] {
        let real = fs::canonicalize(p).unwrap_or_else(|_| p.clone());
        if let Some(r) = rel_to(&real, &home_real) {
            own.push(r);
        } else if let Some(r) = rel_to(p, &cx.home) {
            own.push(r);
        }
    }
    own
}

pub fn never_track(rel: &str) -> bool {
    kit_dirs().iter().any(|o| under(rel, o))
}

pub fn contains_kit(rel: &str) -> bool {
    kit_dirs().iter().any(|o| under(o, rel))
}

/// Path inside home/ for rel. Refuses to go through a tracked symlink (which would leave the repo).
pub fn tree_path(rel: &str) -> KResult<PathBuf> {
    validate_rel(rel)?;
    let tree = &ctx().tree;
    let parts: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();
    let mut p = tree.clone();
    for (i, part) in parts.iter().enumerate().take(parts.len().saturating_sub(1)) {
        p = p.join(part);
        if is_link(&p) || p.is_file() {
            let kind = if is_link(&p) { "symlink" } else { "file" };
            return Err(format!(
                "~/{rel} is inside the tracked {kind} ~/{}; use that path instead (kit rm it before replacing it with a folder)",
                parts[..=i].join("/")
            ));
        }
    }
    Ok(tree.join(rel))
}

// --------------------------------------------------------------------------- ignore rules
/// (line number, section, pattern)
type IgnoreLine = (usize, String, String);
static IGNORE_CACHE: OnceLock<Mutex<Option<Vec<IgnoreLine>>>> = OnceLock::new();

fn ignore_cache() -> &'static Mutex<Option<Vec<IgnoreLine>>> {
    IGNORE_CACHE.get_or_init(|| Mutex::new(None))
}

pub fn reset_ignore_cache() {
    *ignore_cache().lock().unwrap() = None;
}

/// [(lineno, section, pattern)] from .kitignore. Patterns before any [section] apply everywhere.
pub fn ignore_lines() -> Vec<(usize, String, String)> {
    let mut guard = ignore_cache().lock().unwrap();
    if let Some(v) = guard.as_ref() {
        return v.clone();
    }
    static COMMENT: OnceLock<Regex> = OnceLock::new();
    static SECTION: OnceLock<Regex> = OnceLock::new();
    let comment = COMMENT.get_or_init(|| Regex::new(r"\s+#(\s.*)?$").unwrap());
    let section_re = SECTION.get_or_init(|| Regex::new(r"^\[(\w+)\]$").unwrap());
    let mut out = Vec::new();
    let mut section = "all".to_string();
    if let Ok(text) = fs::read_to_string(&ctx().ignore) {
        for (n, raw) in text.lines().enumerate() {
            let line = comment.replace(raw, "").trim().to_string();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(m) = section_re.captures(&line) {
                section = m[1].to_string();
                continue;
            }
            out.push((n + 1, section.clone(), line));
        }
    }
    *guard = Some(out.clone());
    out
}

pub fn pattern_match(rel: &str, pattern: &str) -> bool {
    let mut pattern = pattern.trim_end_matches('/');
    if let Some(p) = pattern.strip_suffix("/**") {
        pattern = p;
    }
    let parts: Vec<&str> = rel.split('/').collect();
    if !pattern.contains('/') {
        return parts.iter().any(|p| fnmatch(p, pattern));
    }
    let pattern = pattern.trim_start_matches('/');
    if let Some(rest) = pattern.strip_prefix("**/") {
        return (0..parts.len()).any(|i| pattern_match(&parts[i..].join("/"), rest));
    }
    (0..parts.len()).any(|i| pathmatch(&parts[..=i].join("/"), pattern))
}

/// The .kitignore line that decides rel (gitignore-style: the last match wins, !pattern un-ignores).
pub fn ignore_match(rel: &str, contexts: &[&str]) -> Option<(usize, String)> {
    let mut hit = None;
    let lines = ignore_lines();
    // Specific platform/remote sections override global rules, regardless of file order.
    for context in contexts {
        for (n, section, pat) in &lines {
            if section != context {
                continue;
            }
            let (neg, p) = match pat.strip_prefix('!') {
                Some(p) => (true, p),
                None => (false, pat.as_str()),
            };
            if pattern_match(rel, p) {
                hit = if neg { None } else { Some((*n, pat.clone())) };
            }
        }
    }
    hit
}

pub fn ignored(rel: &str, contexts: &[&str]) -> bool {
    never_track(rel) || ignore_match(rel, contexts).is_some()
}

pub fn local_contexts() -> [&'static str; 2] {
    ["all", ctx().platform]
}

pub fn ignored_here(rel: &str) -> bool {
    ignored(rel, &local_contexts())
}

/// Append exact-path patterns to a .kitignore section. Returns the lines written.
pub fn add_ignore(rels: &[String], section: &str, comment: &str) -> Vec<String> {
    let path = &ctx().ignore;
    let text = fs::read_to_string(path).unwrap_or_default();
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    let mut new: Vec<String> = Vec::new();
    for rel in rels {
        if rel.contains('\n') || rel.contains(" #") {
            fail(&format!(
                "can't ignore ~/{rel:?}: its name has a newline or ' #'; edit .kitignore by hand"
            ));
            continue;
        }
        let pat = glob_escape(rel)
            + &if comment.is_empty() {
                String::new()
            } else {
                format!("  # {comment}")
            };
        if !lines.contains(&pat) && !new.contains(&pat) {
            new.push(pat);
        }
    }
    if new.is_empty() {
        return new;
    }
    let header = Regex::new(r"^\[(\w+)\]$").unwrap();
    let headers: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| header.is_match(l.trim()))
        .map(|(i, _)| i)
        .collect();
    let at = if section == "all" {
        let mut at = headers.first().copied().unwrap_or(lines.len());
        while at > 0 && lines[at - 1].trim().is_empty() {
            at -= 1;
        }
        at
    } else {
        let want = format!("[{section}]");
        match headers.iter().find(|&&i| lines[i].trim() == want) {
            Some(&mine) => {
                let mut at = headers
                    .iter()
                    .find(|&&i| i > mine)
                    .copied()
                    .unwrap_or(lines.len());
                while at > mine + 1 && lines[at - 1].trim().is_empty() {
                    at -= 1;
                }
                at
            }
            None => {
                lines.push(String::new());
                lines.push(want);
                lines.len()
            }
        }
    };
    for (k, l) in new.iter().enumerate() {
        lines.insert(at + k, l.clone());
    }
    let _ = fs::write(path, lines.join("\n") + "\n");
    reset_ignore_cache();
    new
}

/// `kit add` of something forgotten earlier: remove its '# forgotten' line.
pub fn drop_forgotten(rel: &str) -> bool {
    let path = &ctx().ignore;
    let Ok(text) = fs::read_to_string(path) else {
        return false;
    };
    let pat = glob_escape(rel) + "  # forgotten";
    if !text.lines().any(|l| l == pat) {
        return false;
    }
    let kept: Vec<&str> = text.lines().filter(|l| *l != pat).collect();
    let _ = fs::write(path, kept.join("\n") + "\n");
    reset_ignore_cache();
    true
}

/// A forced addition is explicitly usable on other machines, without overriding remote exclusions.
pub fn unignore_exact(rel: &str) {
    if ignore_match(rel, &local_contexts()).is_none() {
        return;
    }
    let mut text = fs::read_to_string(&ctx().ignore).unwrap_or_default();
    let pattern = format!("!/{}", glob_escape(rel));
    text += &format!("\n[all]\n{pattern}\n[{}]\n{pattern}\n", ctx().platform);
    write_atomic(&ctx().ignore, text.as_bytes(), 0o644)
        .unwrap_or_else(|e| die(&format!("cannot update ignore override: {e}")));
    reset_ignore_cache();
}

// --------------------------------------------------------------------------- rules (per-platform edits)
fn check_rule_regex(pattern: &str, path: &str) {
    Regex::new(pattern)
        .unwrap_or_else(|e| die(&format!("rules.json: invalid regex for {path}: {e}")));
}

pub fn load_rules() -> Vec<serde_json::Map<String, Value>> {
    let bad = || -> ! {
        die("rules.json must be a list of {\"path\": ..., \"on\": [...], ...} objects (see README)")
    };
    match load_json(&ctx().rules, Value::Array(vec![])) {
        Value::Array(items) => items
            .into_iter()
            .map(|r| match r {
                Value::Object(m)
                    if m.get("path").and_then(Value::as_str).is_some()
                        && m.get("on").and_then(Value::as_array).is_some() =>
                {
                    let path = m["path"].as_str().unwrap();
                    validate_rel(path).unwrap_or_else(|e| die(&format!("rules.json: {e}")));
                    for v in m
                        .get("delete_lines")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                    {
                        let pat = v.as_str().unwrap_or_else(|| {
                            die("rules.json: delete_lines must contain strings")
                        });
                        check_rule_regex(pat, path);
                    }
                    for (pat, _) in pairs(m.get("regex")) {
                        check_rule_regex(&pat, path);
                    }
                    m
                }
                _ => bad(),
            })
            .collect(),
        _ => bad(),
    }
}

pub fn rules_for(rel: &str, contexts: &[&str]) -> Vec<serde_json::Map<String, Value>> {
    load_rules()
        .into_iter()
        .filter(|r| {
            r.get("path").and_then(Value::as_str) == Some(rel)
                && r.get("on").and_then(Value::as_array).is_some_and(|on| {
                    on.iter()
                        .any(|o| o.as_str().is_some_and(|o| contexts.contains(&o)))
                })
        })
        .collect()
}

fn pairs(v: Option<&Value>) -> Vec<(String, String)> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|p| {
                    let p = p.as_array()?;
                    Some((
                        p.first()?.as_str()?.to_string(),
                        p.get(1)?.as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Python-style replacement (\1, \g<1>, \g<name>) to the regex crate's ${1}.
fn py_repl(repl: &str) -> String {
    let re = Regex::new(r"\\(\d+)|\\g<(\w+)>").unwrap();
    let escaped = repl.replace('$', "$$");
    re.replace_all(&escaped, |c: &regex::Captures| {
        format!("${{{}}}", c.get(1).or(c.get(2)).unwrap().as_str())
    })
    .into_owned()
}

pub fn render(rel: &str, data: &[u8], contexts: &[&str]) -> Vec<u8> {
    let rules = rules_for(rel, contexts);
    if rules.is_empty() {
        return data.to_vec();
    }
    let Ok(mut text) = String::from_utf8(data.to_vec()) else {
        return data.to_vec();
    };
    for r in rules {
        for pat in r
            .get("delete_lines")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            if let Ok(re) = Regex::new(pat) {
                text = text
                    .split_inclusive('\n')
                    .filter(|l| !re.is_match(l.trim_end_matches('\n')))
                    .collect();
            }
        }
        for (old, new) in pairs(r.get("replace")) {
            text = text.replace(&old, &new);
        }
        for (pat, repl) in pairs(r.get("regex")) {
            if let Ok(re) = Regex::new(&format!("(?m){pat}")) {
                text = re.replace_all(&text, py_repl(&repl).as_str()).into_owned();
            }
        }
    }
    text.into_bytes()
}

// --------------------------------------------------------------------------- entries
#[derive(Clone, Debug, PartialEq)]
pub enum Entry {
    File(Vec<u8>, u32),
    Link(String),
    Dir,
    Other,
    Unreadable,
}

impl Entry {
    pub fn kind(&self) -> &'static str {
        match self {
            Entry::File(..) => "file",
            Entry::Link(_) => "link",
            Entry::Dir => "dir",
            Entry::Other => "other",
            Entry::Unreadable => "unreadable",
        }
    }
}

pub fn read_entry(path: &Path) -> Option<Entry> {
    let md = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return Some(Entry::Unreadable)
        }
        Err(_) => return None,
    };
    let ft = md.file_type();
    if ft.is_symlink() {
        return Some(match fs::read_link(path) {
            Ok(t) => Entry::Link(path_str(&t)),
            Err(_) => Entry::Unreadable,
        });
    }
    if ft.is_dir() {
        return Some(Entry::Dir);
    }
    if ft.is_file() {
        return Some(match fs::read(path) {
            Ok(b) => Entry::File(b, md.mode() & 0o777),
            Err(_) => Entry::Unreadable,
        });
    }
    Some(Entry::Other)
}

/// Login activation is generated locally and must never be saved into the dotfile repo.
pub fn read_home_entry(rel: &str) -> Option<Entry> {
    let mut entry = read_entry(&ctx().home.join(rel));
    if matches!(
        rel,
        ".bashrc"
            | ".bash_profile"
            | ".bash_login"
            | ".profile"
            | ".zshrc"
            | ".zprofile"
            | ".zshenv"
            | ".fizsh/.zshrc"
            | ".config/fish/config.fish"
    ) {
        if let Some(Entry::File(data, _)) = &mut entry {
            static HOOK: OnceLock<BRegex> = OnceLock::new();
            let re = HOOK.get_or_init(|| {
                BRegex::new(r"(?ms)^# >>> kit: [^\n]*\n.*?^# <<< kit <<<[^\n]*\n?").unwrap()
            });
            *data = re.replace_all(data, &b""[..]).into_owned();
        }
    }
    entry
}

pub fn digest(e: Option<&Entry>) -> Option<String> {
    match e? {
        Entry::File(b, m) => {
            let h = hex::encode(Sha256::digest(b));
            Some(format!(
                "{}{}",
                &h[..20],
                if m & 0o111 != 0 { "x" } else { "" }
            ))
        }
        Entry::Link(t) => Some(format!("link:{t}")),
        _ => None,
    }
}

pub fn same(a: Option<&Entry>, b: Option<&Entry>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(Entry::File(x, mx)), Some(Entry::File(y, my))) => {
            x == y && ((mx & 0o111 != 0) == (my & 0o111 != 0))
        }
        (Some(Entry::Link(x)), Some(Entry::Link(y))) => x == y,
        _ => false,
    }
}

/// Walk like os.walk without following symlinks: symlinked folders count as entries.
pub fn walk_entries(
    base: &Path,
    rel_base: &str,
    out: &mut Vec<String>,
    skip_dir: &mut dyn FnMut(&str, &Path) -> bool,
) {
    let Ok(rd) = fs::read_dir(base) else { return };
    let mut items: Vec<_> = rd.flatten().collect();
    items.sort_by_key(|e| e.file_name());
    for ent in items {
        let name = ent.file_name().to_string_lossy().into_owned();
        let rel = if rel_base.is_empty() {
            name.clone()
        } else {
            format!("{rel_base}/{name}")
        };
        let p = ent.path();
        let Ok(md) = fs::symlink_metadata(&p) else {
            continue;
        };
        if md.is_dir() {
            if !skip_dir(&rel, &p) {
                walk_entries(&p, &rel, out, skip_dir);
            }
        } else {
            out.push(rel);
        }
    }
}

/// Every tracked file and symlink (relative to $HOME), sorted.
pub fn managed(prefixes: Option<&[String]>) -> Vec<String> {
    let tree = &ctx().tree;
    if !is_real_dir(tree) {
        return vec![];
    }
    let mut all = Vec::new();
    walk_entries(tree, "", &mut all, &mut |_, _| false);
    let mut out: Vec<String> = all
        .into_iter()
        .filter(|r| prefixes.is_none_or(|ps| ps.iter().any(|p| under(r, p))))
        .collect();
    out.sort();
    out
}

pub fn source_entry(rel: &str, contexts: &[&str]) -> KResult<Option<Entry>> {
    let e = read_entry(&tree_path(rel)?);
    Ok(match e {
        Some(Entry::File(b, m)) => Some(Entry::File(render(rel, &b, contexts), m)),
        other => other,
    })
}

pub fn private(rel: &str) -> bool {
    rel.starts_with(".ssh/") || rel.starts_with(".gnupg/") || looks_secret_name(rel)
}

/// Write a tracked entry into $HOME. Never deletes a folder.
pub fn write_home(rel: &str, entry: &Entry) -> KResult<()> {
    validate_rel(rel)?;
    let home = &ctx().home;
    let parts: Vec<&str> = rel.split('/').collect();
    let mut p = home.clone();
    for part in &parts[..parts.len() - 1] {
        p = p.join(part);
        match read_entry(&p) {
            None => {
                fs::create_dir(&p).map_err(|e| {
                    format!(
                        "could not create ~/{}: {e}",
                        rel_to(&p, home).unwrap_or_default()
                    )
                })?;
                let mode = if *part == ".ssh" || *part == ".gnupg" {
                    0o700
                } else {
                    0o755
                };
                let _ = fs::set_permissions(&p, fs::Permissions::from_mode(mode));
            }
            Some(Entry::Dir) => {}
            Some(Entry::Link(_)) if p.is_dir() => {} // a symlinked folder here: write through it
            Some(_) => {
                return Err(format!(
                    "~/{} is a file here but a folder in the repo",
                    rel_to(&p, home).unwrap_or_default()
                ))
            }
        }
    }
    let dest = home.join(rel);
    let old = read_entry(&dest);
    match &old {
        Some(Entry::Dir) => return Err(format!("~/{rel} is a folder here")),
        Some(Entry::Other) => return Err(format!("~/{rel} is a special file here")),
        _ => {}
    }
    match entry {
        Entry::Link(t) => link_atomic(&dest, t).map_err(|e| e.to_string()),
        Entry::File(data, emode) => {
            let exe = emode & 0o111 != 0;
            let mode = match &old {
                Some(Entry::File(_, om)) => {
                    let mut m = om & !0o111;
                    if exe {
                        m |= (om & 0o444) >> 2;
                    }
                    m
                }
                _ if private(rel) => {
                    if exe {
                        0o700
                    } else {
                        0o600
                    }
                }
                _ => {
                    if exe {
                        0o755
                    } else {
                        0o644
                    }
                }
            };
            write_atomic(&dest, data, mode).map_err(|e| e.to_string())
        }
        _ => Err(format!("~/{rel}: nothing to write")),
    }
}

/// Store an entry in the repo's home/ (replacing whatever the repo had at that path).
pub fn write_tree(rel: &str, entry: &Entry) -> KResult<()> {
    tree_path(rel)?;
    let tree = &ctx().tree;
    fs::create_dir_all(tree).map_err(|e| e.to_string())?;
    let parts: Vec<&str> = rel.split('/').collect();
    let mut p = tree.clone();
    for part in &parts[..parts.len() - 1] {
        p = p.join(part);
        if is_link(&p) || p.is_file() {
            return Err(format!(
                "cannot replace tracked parent {} with a folder; kit rm it first",
                p.display()
            ));
        }
        if !p.is_dir() {
            fs::create_dir(&p).map_err(|e| e.to_string())?;
        }
    }
    let dest = tree.join(rel);
    if is_real_dir(&dest) {
        fs::remove_dir_all(&dest).map_err(|e| e.to_string())?; // inside the repo only
    }
    match entry {
        Entry::Link(t) => link_atomic(&dest, t).map_err(|e| e.to_string()),
        Entry::File(data, m) => {
            write_atomic(&dest, data, if m & 0o111 != 0 { 0o755 } else { 0o644 })
                .map_err(|e| e.to_string())
        }
        _ => Err(format!("~/{rel}: can't store a {}", entry.kind())),
    }
}

pub fn remove_tree(rel: &str) -> KResult<()> {
    let tree = &ctx().tree;
    let p = tree_path(rel)?;
    if is_link(&p) || p.is_file() {
        fs::remove_file(&p).map_err(|e| e.to_string())?;
    }
    let mut parent = p.parent().map(Path::to_path_buf);
    while let Some(d) = parent {
        if &d == tree || !d.starts_with(tree) {
            break;
        }
        if fs::remove_dir(&d).is_err() {
            break;
        }
        parent = d.parent().map(Path::to_path_buf);
    }
    Ok(())
}

fn copy_tree(src: &Path, dest: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dest)?;
    for ent in fs::read_dir(src)? {
        let ent = ent?;
        let s = ent.path();
        let d = dest.join(ent.file_name());
        let md = fs::symlink_metadata(&s)?;
        if md.file_type().is_symlink() {
            std::os::unix::fs::symlink(fs::read_link(&s)?, &d)?;
        } else if md.is_dir() {
            copy_tree(&s, &d)?;
        } else if md.is_file() {
            fs::copy(&s, &d)?;
        }
    }
    Ok(())
}

pub fn copy_dir(src: &Path, dest: &Path) -> std::io::Result<()> {
    copy_tree(src, dest)
}

pub fn backup(stamp: &str, rel: &str) -> KResult<()> {
    validate_rel(rel)?;
    let cx = ctx();
    let src = cx.home.join(rel);
    let dest = cx.backups.join(stamp).join(rel);
    let e = read_entry(&src);
    if e.is_none() {
        return Ok(());
    }
    if matches!(e, Some(Entry::Other) | Some(Entry::Unreadable)) {
        return Err(format!(
            "cannot back up ~/{rel}: unreadable or special file"
        ));
    }
    if let Some(p) = dest.parent() {
        fs::create_dir_all(p).map_err(|e| format!("cannot create backup: {e}"))?;
    }
    match e.unwrap() {
        Entry::Dir => {
            copy_tree(&src, &dest).map_err(|e| e.to_string())?;
        }
        Entry::Link(t) => {
            std::os::unix::fs::symlink(t, &dest).map_err(|e| e.to_string())?;
        }
        Entry::File(b, m) => {
            write_atomic(&dest, &b, m).map_err(|e| e.to_string())?;
        }
        _ => {}
    }
    Ok(())
}

// --------------------------------------------------------------------------- secrets
const SECRET_NAMES: &[&str] = &[
    "*.pem",
    "*.key",
    "*.p12",
    "*.pfx",
    "id_rsa*",
    "id_ed25519*",
    "id_ecdsa*",
    "id_dsa*",
    ".netrc",
    ".git-credentials",
    "*token*",
    "*secret*",
    "credentials*",
    "auth.db*",
    ".sgptrc",
    "*.kdbx",
    ".vault-*",
    ".npmrc",
    ".pypirc",
    "*.keystore",
];
const SECRET_PATHS: &[&str] = &[
    ".config/gh/hosts.yml",
    ".docker/config.json",
    ".kube/config",
    ".config/rclone/rclone.conf",
    ".config/github-copilot/apps.json",
    ".config/github-copilot/hosts.json",
    ".aws/credentials",
];

fn secret_re() -> &'static BRegex {
    static RE: OnceLock<BRegex> = OnceLock::new();
    RE.get_or_init(|| {
        BRegex::new(concat!(
            r"(?i-u)(-----BEGIN [A-Z ]*PRIVATE KEY-----|\bgh[pousr]_[A-Za-z0-9]{30,}|\bgithub_pat_\w{20,}",
            r"|\bxox[abpr]-[\w-]{10,}|\bAKIA[0-9A-Z]{16}\b|\bsk-[A-Za-z0-9_-]{20,}|\bglpat-[\w-]{20,}",
            r#"|(oauth_token|_authToken|aws_secret_access_key|api[_-]?key|password|secret)["']?\s*[:=]\s*["']?[A-Za-z0-9_\-+/=.]{16,})"#
        ))
        .unwrap()
    })
}

pub fn looks_secret_name(rel: &str) -> bool {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    SECRET_PATHS.contains(&rel) || SECRET_NAMES.iter().any(|p| fnmatch(name, p))
}

/// Where a file seems to hold a secret ("line N"), never the secret itself.
pub fn secret_hit(data: &[u8]) -> Option<String> {
    let m = secret_re().find(data)?;
    let line = data[..m.start()].iter().filter(|&&b| b == b'\n').count() + 1;
    Some(format!("line {line}"))
}

pub fn secret_problem(rel: &str, entry: Option<&Entry>) -> Option<String> {
    if looks_secret_name(rel) {
        return Some("its name looks like a secret".into());
    }
    if let Some(Entry::File(b, _)) = entry {
        if let Some(hit) = secret_hit(b) {
            return Some(format!("it seems to contain a secret ({hit})"));
        }
    }
    None
}

// --------------------------------------------------------------------------- sync state
pub type Synced = BTreeMap<String, String>;

pub fn load_synced() -> Synced {
    let cx = ctx();
    if cx.synced.exists() {
        let obj = load_obj(&cx.synced);
        for k in obj.keys() {
            validate_rel(k).unwrap_or_else(|e| die(&format!("synced.json: {e}")));
        }
        return obj
            .into_iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_string())))
            .collect();
    }
    // first run on this machine (or after upgrading kit): files already identical count as synced
    let mut synced = Synced::new();
    for rel in managed(None) {
        let Ok(src) = source_entry(&rel, &local_contexts()) else {
            continue;
        };
        let tgt = read_home_entry(&rel);
        if let (Some(_), Some(t)) = (&src, &tgt) {
            if same(src.as_ref(), Some(t)) {
                mark_synced(&mut synced, &rel, t);
            }
        }
    }
    save_synced(&synced);
    synced
}

pub fn save_synced(s: &Synced) {
    save_json(&ctx().synced, &serde_json::to_value(s).unwrap());
}

/// The last-synced copy of each file lives in <state>/base: the common ancestor for merges.
fn base_path(rel: &str) -> PathBuf {
    ctx().state.join("base").join(rel)
}

pub fn get_base(rel: &str) -> Option<Vec<u8>> {
    fs::read(base_path(rel)).ok()
}

/// Remember `e` as the version this machine and the repo last agreed on.
pub fn mark_synced(synced: &mut Synced, rel: &str, e: &Entry) {
    validate_rel(rel).unwrap_or_else(|e| die(&e));
    let Some(d) = digest(Some(e)) else { return };
    let p = base_path(rel);
    let fresh = synced.get(rel) != Some(&d) || !exists_or_link(&p);
    if !fresh {
        synced.insert(rel.to_string(), d);
        return;
    }
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent)
            .unwrap_or_else(|e| die(&format!("cannot record merge base: {e}")));
    }
    match e {
        Entry::File(b, _) => {
            write_atomic(&p, b, 0o600)
                .unwrap_or_else(|e| die(&format!("cannot record merge base: {e}")));
        }
        _ => {
            let _ = fs::remove_file(&p);
        }
    }
    synced.insert(rel.to_string(), d);
}

pub fn unmark_synced(synced: &mut Synced, rel: &str) {
    synced.remove(rel);
    let _ = fs::remove_file(base_path(rel));
}

pub fn tracked_dirs() -> Vec<String> {
    let dirs = load_str_list(&ctx().dirs);
    for d in &dirs {
        validate_rel(d).unwrap_or_else(|e| die(&format!("dirs.json: {e}")));
        if never_track(d) || contains_kit(d) {
            die(&format!("dirs.json: ~/{d} contains kit's own folders"));
        }
    }
    dirs
}

pub fn label(code: &str) -> &'static str {
    match code {
        "here" => "changed here",
        "repo" => "changed in repo",
        "both" => "changed on both",
        "missing" => "missing here",
        "deleted" => "deleted here",
        "type" => "file vs folder",
        "conflict" => "has conflicts",
        "delete-conflict" => "deleted here, changed in repo",
        _ => "unreadable",
    }
}

pub fn classify(rel: &str, synced: &Synced) -> KResult<Option<&'static str>> {
    let src = source_entry(rel, &local_contexts())?;
    let tgt = read_home_entry(rel);
    if !matches!(src, Some(Entry::File(..)) | Some(Entry::Link(_))) {
        return Ok(None);
    }
    Ok(match &tgt {
        None => Some(if let Some(last) = synced.get(rel) {
            if digest(src.as_ref()).as_ref() != Some(last) {
                "delete-conflict"
            } else {
                "deleted"
            }
        } else {
            "missing"
        }),
        Some(Entry::Dir) | Some(Entry::Other) => Some("type"),
        Some(Entry::Unreadable) => Some("unreadable"),
        _ if same(src.as_ref(), tgt.as_ref()) => None,
        Some(Entry::File(b, _)) if has_conflict_markers(b) => Some("conflict"),
        _ => match synced.get(rel) {
            None => Some("both"),
            Some(last) => {
                let here = digest(tgt.as_ref()).as_ref() != Some(last);
                let repo = digest(src.as_ref()).as_ref() != Some(last);
                Some(if here && repo {
                    "both"
                } else if here {
                    "here"
                } else {
                    "repo"
                })
            }
        },
    })
}

/// [(code, rel)] for everything that differs between this machine and the repo.
pub fn file_changes(prefixes: Option<&[String]>, synced: &Synced) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    for rel in managed(prefixes) {
        if ignored_here(&rel) {
            continue;
        }
        match classify(&rel, synced) {
            Ok(Some(code)) => out.push((code, rel)),
            Ok(None) => {}
            Err(e) => fail(&e),
        }
    }
    // Folder metadata groups tracked files; it never enrolls new files implicitly.
    let _ = tracked_dirs();
    out
}

pub fn refresh_synced(synced: &mut Synced, prefixes: Option<&[String]>) {
    let cx = ctx();
    for rel in managed(prefixes) {
        let Ok(src) = source_entry(&rel, &local_contexts()) else {
            continue;
        };
        let tgt = read_home_entry(&rel);
        if let (Some(_), Some(t)) = (&src, &tgt) {
            if same(src.as_ref(), Some(t)) {
                mark_synced(synced, &rel, t);
            }
        }
    }
    let gone: Vec<String> = synced
        .keys()
        .filter(|rel| !exists_or_link(&cx.tree.join(rel)))
        .cloned()
        .collect();
    for rel in gone {
        unmark_synced(synced, &rel);
    }
    save_synced(synced);
}

/// Prefixes for path arguments; flags ones that match nothing tracked.
pub fn scope(paths: &[String]) -> Option<Vec<String>> {
    if paths.is_empty() {
        return None;
    }
    let prefixes: Vec<String> = paths.iter().map(|p| rel_of(p)).collect();
    let tracked = managed(None);
    let dirs = tracked_dirs();
    for p in &prefixes {
        if !tracked.iter().any(|t| under(t, p)) && !dirs.iter().any(|d| p == d) {
            fail(&format!("~/{p} is not tracked (kit add it first)"));
        }
    }
    Some(prefixes)
}

// --------------------------------------------------------------------------- repo state
pub fn repo_busy() -> Option<&'static str> {
    let g = ctx().source.join(".git");
    for (marker, what) in [
        ("rebase-merge", "rebase"),
        ("rebase-apply", "rebase"),
        ("MERGE_HEAD", "merge"),
        ("CHERRY_PICK_HEAD", "cherry-pick"),
    ] {
        if g.join(marker).exists() {
            return Some(what);
        }
    }
    None
}

pub fn guard_repo() {
    if let Some(busy) = repo_busy() {
        die(&format!(
            "the repo is in the middle of a git {busy}. Finish it (kit cd, then git status) or cancel it (kit git {busy} --abort), then try again."
        ));
    }
}

/// Tracked files that git won't back up because of a .gitignore inside home/.
pub fn hidden_by_gitignore() -> Vec<String> {
    let cx = ctx();
    if !cx.source.join(".git").exists() || !is_real_dir(&cx.tree) {
        return vec![];
    }
    let r = git(&[
        "ls-files",
        "-z",
        "--others",
        "--ignored",
        "--exclude-standard",
        "--",
        "home",
    ]);
    r.stdout
        .split('\0')
        .filter_map(|l| l.strip_prefix("home/").map(String::from))
        .collect()
}

pub fn has_conflict_markers(data: &[u8]) -> bool {
    static RE: OnceLock<BRegex> = OnceLock::new();
    RE.get_or_init(|| BRegex::new(r"(?m)^(<{7} |>{7} )").unwrap())
        .is_match(data)
}

pub fn is_binary(data: &[u8]) -> bool {
    data[..data.len().min(8192)].contains(&0)
}
