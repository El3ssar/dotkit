//! packages.json: { name: {"mac": "brew:bat"|"cask:kitty"|"cargo:eza"|"cargo-git:<url>"|"cmd:<shell>"|"mise:<spec>",
//!                         "linux": "aqua:sharkdp/bat@0.26.1"   (a mise tool spec, Linux machines + servers),
//!                         "linux_fallback": "cargo:...@1.0"   (built from source when no download runs),
//!                         "bin": "bat", "remote": false  (not sent to servers)} }
use crate::cli::Args;
use crate::util::*;
use regex::Regex;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::OnceLock;

pub type Pkg = Map<String, Value>;
const NOT_MISE: &[&str] = &["brew", "cask", "cmd", "cargo-git", "mise"]; // Mac-only recipe kinds

/// kit's own tools: always installed with kit, even if packages.json doesn't list them
/// (delta shows `kit diff`; bat builds the syntax themes delta uses). packages.json entries win.
fn core_packages() -> BTreeMap<String, Pkg> {
    let mk = |mac: &str, linux: &str, bin: &str| {
        let mut m = Pkg::new();
        m.insert("mac".into(), mac.into());
        m.insert("linux".into(), linux.into());
        m.insert("bin".into(), bin.into());
        m
    };
    BTreeMap::from([
        ("delta".to_string(), mk("brew:git-delta", "aqua:dandavison/delta@0.20.1", "delta")),
        ("bat".to_string(), mk("brew:bat", "aqua:sharkdp/bat@0.26.1", "bat")),
    ])
}

pub fn load_pkgs() -> BTreeMap<String, Pkg> {
    load_obj(&ctx().pkgs).into_iter().map(|(k, v)| (k, v.as_object().cloned().unwrap_or_default())).collect()
}

pub fn save_pkgs(p: &BTreeMap<String, Pkg>) {
    save_json(&ctx().pkgs, &serde_json::to_value(p).unwrap());
}

pub fn all_packages() -> BTreeMap<String, Pkg> {
    let pkgs = load_pkgs();
    let bins: Vec<String> = pkgs.iter().map(|(n, p)| s(p, "bin").unwrap_or(n.as_str()).to_string()).collect();
    let mut all: BTreeMap<String, Pkg> =
        core_packages().into_iter().filter(|(n, p)| !pkgs.contains_key(n) && !bins.iter().any(|b| Some(b.as_str()) == s(p, "bin"))).collect();
    all.extend(pkgs);
    all
}

pub fn s<'a>(p: &'a Pkg, k: &str) -> Option<&'a str> {
    p.get(k).and_then(Value::as_str).filter(|v| !v.is_empty())
}

pub fn pkg_spec(p: &Pkg) -> Option<&str> {
    s(p, ctx().platform)
}

fn sends_to_servers(p: &Pkg) -> bool {
    s(p, "linux").is_some() && p.get("remote").and_then(Value::as_bool).unwrap_or(true)
}

/// "aqua:sharkdp/bat@0.26.1" -> ("aqua:sharkdp/bat", Some("0.26.1")); npm:@scope/pkg keeps its @.
pub fn split_spec(spec: &str) -> (String, Option<String>) {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r#"^(.*[^:/@])@([^@/\s"]+)$"#).unwrap());
    match re.captures(spec) {
        Some(m) => (m[1].to_string(), Some(m[2].to_string())),
        None => (spec.to_string(), None),
    }
}

pub fn is_mise_spec(spec: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r#"^[\w-]+:[^\s"']+$"#).unwrap());
    re.is_match(spec) && !NOT_MISE.contains(&spec.split(':').next().unwrap_or(""))
}

pub fn pkg_key(name: &str) -> String {
    let (base, _) = split_spec(name);
    let base = Regex::new(r"\[.*\]$").unwrap().replace(&base, "").into_owned();
    let last = base.trim_end_matches('/').rsplit('/').next().unwrap_or("").to_string();
    last.rsplit(':').next().unwrap_or("").to_string()
}

fn brew_prefix() -> Option<PathBuf> {
    let b = which("brew")?;
    let real = std::fs::canonicalize(&b).unwrap_or(b);
    Some(real.parent()?.parent()?.to_path_buf())
}

pub fn pkg_installed(name: &str, p: &Pkg) -> bool {
    let spec = pkg_spec(p).unwrap_or("");
    let (kind, what) = spec.split_once(':').unwrap_or((spec, ""));
    let what = what.rsplit('/').next().unwrap_or(what); // tap/name -> name
    let prefix = if kind == "brew" || kind == "cask" { brew_prefix() } else { None };
    if kind == "cask" {
        if let Some(app) = s(p, "app") {
            if PathBuf::from("/Applications").join(app).exists() {
                return true;
            }
        }
        // Homebrew keeps each installed cask/formula in a folder: much faster than asking `brew list`
        return prefix.is_some_and(|pre| pre.join("Caskroom").join(what).is_dir());
    }
    if kind == "brew" && prefix.is_some_and(|pre| pre.join("Cellar").join(what).is_dir()) {
        return true;
    }
    have(s(p, "bin").unwrap_or(name))
}

pub fn install_package(name: &str, p: &Pkg) -> bool {
    let Some(spec) = pkg_spec(p) else { return true };
    let (kind, what) = spec.split_once(':').unwrap_or((spec, ""));
    let cmd: Vec<String> = match kind {
        "brew" => vec!["brew".into(), "install".into(), what.into()],
        "cask" => vec!["brew".into(), "install".into(), "--cask".into(), what.into()],
        "cargo" => vec!["cargo".into(), "install".into(), "--locked".into(), what.into()],
        "cargo-git" => vec!["cargo".into(), "install".into(), "--locked".into(), "--git".into(), what.into()],
        "cmd" => vec!["sh".into(), "-c".into(), what.into()],
        "mise" => vec!["mise".into(), "use".into(), "-g".into(), what.into()],
        _ => vec!["mise".into(), "use".into(), "-g".into(), spec.into()],
    };
    if cmd[0] != "sh" && !have(&cmd[0]) {
        fail(&format!("{name}: needs {}, which isn't installed (kit doctor)", cmd[0]));
        return false;
    }
    say(&format!("installing {name} ({spec})"));
    if run(&cmd, false).code != 0 {
        fail(&format!("{name}: install failed"));
        return false;
    }
    true
}

pub fn install_packages() {
    for (n, p) in all_packages() {
        if pkg_spec(&p).is_some() && !pkg_installed(&n, &p) {
            install_package(&n, &p);
        }
    }
}

fn mise_backend(name: &str) -> Option<String> {
    if name.contains(':') {
        return is_mise_spec(name).then(|| name.to_string());
    }
    if Regex::new(r"^[\w.-]+/[\w.-]+$").unwrap().is_match(name) {
        return Some(format!("github:{name}"));
    }
    if !have("mise") {
        return None;
    }
    let r = run(&["mise", "registry", name], true);
    let words: Vec<&str> = r.stdout.split_whitespace().collect();
    if r.code != 0 || words.is_empty() {
        return None;
    }
    let backends: Vec<&str> = if words[0] == name { words[1..].to_vec() } else { words };
    for pref in ["core:", "aqua:", "github:", "ubi:"] {
        if let Some(b) = backends.iter().find(|b| b.starts_with(pref)) {
            return Some(b.to_string());
        }
    }
    backends.first().map(|b| b.to_string())
}

fn brew_formula(name: &str) -> Option<String> {
    if !have("brew") || name.contains('/') || name.contains(':') {
        return None;
    }
    let r = run(&["brew", "info", "--json=v2", name], true);
    if r.code != 0 {
        return None;
    }
    let info: Value = serde_json::from_str(&r.stdout).ok()?;
    if let Some(f) = info.get("formulae").and_then(Value::as_array).and_then(|a| a.first()) {
        return Some(format!("brew:{}", f.get("name")?.as_str()?));
    }
    if let Some(c) = info.get("casks").and_then(Value::as_array).and_then(|a| a.first()) {
        return Some(format!("cask:{}", c.get("token")?.as_str()?));
    }
    None
}

fn mise_latest(backend: &str) -> Option<String> {
    if !have("mise") {
        return None;
    }
    let r = run(&["mise", "latest", backend], true);
    let v = r.stdout.trim().to_string();
    (r.code == 0 && !v.is_empty() && !v.contains(char::is_whitespace)).then_some(v)
}

fn describe(name: &str, p: &Pkg) -> String {
    let mut parts = vec![format!("mac: {}", s(p, "mac").unwrap_or("-")), format!("linux: {}", s(p, "linux").unwrap_or("-"))];
    if let Some(fb) = s(p, "linux_fallback") {
        parts.push(format!("fallback: {fb}"));
    }
    parts.push(format!("servers: {}", if sends_to_servers(p) { "yes" } else { "no" }));
    format!("{name}  {}", parts.join(" · "))
}

fn pinned(spec: &str) -> String {
    let (base, ver) = split_spec(spec);
    let ver = ver.or_else(|| mise_latest(&base)).unwrap_or_else(|| "latest".into());
    format!("{base}@{ver}")
}

/// The same recipe at the newest version mise knows (or unchanged when it can't tell).
fn bumped(spec: &str) -> String {
    let (base, old) = split_spec(spec);
    match mise_latest(&base) {
        Some(v) => format!("{base}@{v}"),
        None => old.map(|o| format!("{base}@{o}")).unwrap_or_else(|| format!("{base}@latest")),
    }
}

/// `kit add <tool>`: work out how to install it on a Mac and on Linux, record it, install it here.
pub fn add_tools(names: &[String], a: &Args) {
    let mut pkgs = load_pkgs();
    let mut changed = false;
    for name in names {
        let name = name.clone();
        let key = pkg_key(&name);
        let old = pkgs.get(&key).cloned();
        let mut entry = old.clone().unwrap_or_default();
        let kind = name.split_once(':').map(|(k, _)| k.to_string());
        // how to install it on a Mac
        if let Some(m) = a.one("mac") {
            entry.insert("mac".into(), m.into());
        } else if matches!(kind.as_deref(), Some("brew" | "cask" | "cargo" | "cargo-git" | "cmd")) {
            entry.insert("mac".into(), name.clone().into()); // e.g. kit add cask:kitty
        }
        // how to install it on Linux and servers (adding a tracked tool again pins the latest version)
        {
            let wanted = a.one("linux").or_else(|| (kind.is_some() && is_mise_spec(&name)).then(|| name.clone()));
            match wanted {
                Some(spec) if is_mise_spec(&spec) => {
                    entry.insert("linux".into(), pinned(&spec).into());
                }
                Some(spec) => fail(&format!("{spec} isn't a Linux recipe (use a mise spec like aqua:owner/repo or github:owner/repo)")),
                None => match s(&entry, "linux").map(String::from) {
                    Some(cur) => {
                        entry.insert("linux".into(), bumped(&cur).into());
                    }
                    None => {
                        if let Some(b) = mise_backend(&name) {
                            entry.insert("linux".into(), pinned(&b).into());
                        }
                    }
                },
            }
            if a.one("fallback").is_none() {
                if let Some(fb) = s(&entry, "linux_fallback").map(String::from) {
                    if old.is_some() {
                        entry.insert("linux_fallback".into(), bumped(&fb).into());
                    }
                }
            }
        }
        if s(&entry, "mac").is_none() {
            let mac = brew_formula(&name).or_else(|| s(&entry, "linux").map(|l| format!("mise:{l}")));
            if let Some(m) = mac {
                entry.insert("mac".into(), m.into());
            }
        }
        if let Some(b) = a.one("bin") {
            entry.insert("bin".into(), b.into());
        }
        if let Some(fb) = a.one("fallback") {
            if is_mise_spec(&fb) {
                entry.insert("linux_fallback".into(), pinned(&fb).into());
            } else {
                fail(&format!("--fallback {fb}: use a mise spec, e.g. cargo:<crate>@<version>"));
            }
        }
        if a.flag("no_servers") {
            entry.insert("remote".into(), false.into());
        }
        if a.flag("servers") {
            entry.remove("remote");
        }
        entry.retain(|_, v| !v.is_null() && v.as_str() != Some(""));
        if s(&entry, "mac").is_none() && s(&entry, "linux").is_none() {
            fail(&format!("{name}: not a file here, and not a tool Homebrew or mise knows — for a tool give its GitHub repo (owner/repo) or --mac/--linux"));
            continue;
        }
        if Some(&entry) == old.as_ref() {
            out(&format!("  already tracked, nothing new: {}", describe(&key, &entry)));
        } else {
            pkgs.insert(key.clone(), entry.clone());
            save_pkgs(&pkgs);
            changed = true;
            out(&format!("  {}: {}", if old.is_some() { "updated" } else { "added" }, describe(&key, &entry)));
        }
        if pkg_spec(&entry).is_some() && !pkg_installed(&key, &entry) {
            install_package(&key, &entry);
        }
    }
    if changed {
        say("kit sync to back it up · kit push to put it on your servers");
    }
}

/// `kit rm <tool>`: stop tracking it (it stays installed here).
pub fn rm_tools(names: &[String]) {
    let mut pkgs = load_pkgs();
    for name in names {
        let key = pkg_key(name);
        if pkgs.remove(&key).is_some() {
            say(&format!("{key} is no longer tracked (still installed here; servers drop it on the next kit push)"));
        } else {
            fail(&format!("{name} is not tracked"));
        }
    }
    save_pkgs(&pkgs);
}

/// The table of tracked tools (kit status -v).
pub fn print_tools() {
    let pkgs = all_packages();
    let w = pkgs.keys().map(|n| n.chars().count()).max().unwrap_or(4).max(4);
    out(&format!("Tools:\n  {:w$}  {:4}  {:7}  {:28}  linux (fallback)", "name", "here", "servers", "mac"));
    for (n, p) in &pkgs {
        let here = if pkg_spec(p).is_some() {
            if pkg_installed(n, p) { "yes ".to_string() } else { c("33", "no  ") }
        } else {
            "-   ".to_string()
        };
        let srv = if sends_to_servers(p) { "yes" } else { "no" };
        let mut mac = s(p, "mac").unwrap_or("-").to_string();
        if mac.chars().count() > 28 {
            mac = mac.chars().take(27).collect::<String>() + "…";
        }
        let fb = s(p, "linux_fallback").map(|f| format!("  ({f})")).unwrap_or_default();
        out(&format!("  {n:w$}  {here}  {srv:7}  {mac:28}  {}{fb}", s(p, "linux").unwrap_or("-")));
    }
    note(&format!("  here: installed on this machine ('-' = no recipe for {}) · servers: installed by kit push", ctx().platform));
}

