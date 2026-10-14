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
const NOT_MISE: &[&str] = &[
    "brew",
    "cask",
    "cmd",
    "cargo-git",
    "mise",
    "system",
    "builtin",
]; // Mac-only recipe kinds

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
        (
            "delta".to_string(),
            mk("brew:git-delta", "aqua:dandavison/delta@0.20.1", "delta"),
        ),
        (
            "bat".to_string(),
            mk("brew:bat", "aqua:sharkdp/bat@0.26.1", "bat"),
        ),
        (
            "neovim".to_string(),
            mk("brew:neovim", "aqua:neovim/neovim@0.12.6", "nvim"),
        ),
    ])
}

pub fn load_pkgs() -> BTreeMap<String, Pkg> {
    let valid_name = Regex::new(r"^[A-Za-z0-9_.+-]+$").unwrap();
    load_obj(&ctx().pkgs)
        .into_iter()
        .map(|(k, v)| {
            if !valid_name.is_match(&k) {
                die(&format!("invalid package name: {k:?}"));
            }
            let p = v
                .as_object()
                .cloned()
                .unwrap_or_else(|| die(&format!("packages.json: {k} must be an object")));
            for field in ["mac", "linux", "linux_fallback", "bin", "linux_bin"] {
                if let Some(v) = p.get(field) {
                    let spec = v.as_str().unwrap_or_else(|| {
                        die(&format!("packages.json: {k}.{field} must be a string"))
                    });
                    if let Some(pkg) = spec.strip_prefix("system:") {
                        if !valid_name.is_match(pkg) || pkg.starts_with('-') {
                            die(&format!("invalid system package for {k}"));
                        }
                    }
                    if spec.contains(['\n', '\r', '\0', '\t']) {
                        die(&format!(
                            "packages.json: {k}.{field} contains a control character"
                        ));
                    }
                    if matches!(field, "bin" | "linux_bin") && (!valid_name.is_match(spec)) {
                        die(&format!("invalid binary name for {k}"));
                    }
                }
            }
            (k, p)
        })
        .collect()
}

pub fn save_pkgs(p: &BTreeMap<String, Pkg>) {
    save_json(&ctx().pkgs, &serde_json::to_value(p).unwrap());
}

pub fn all_packages() -> BTreeMap<String, Pkg> {
    let pkgs = load_pkgs();
    let bins: Vec<String> = pkgs
        .iter()
        .map(|(n, p)| package_bin(n, p).to_string())
        .collect();
    let mut all: BTreeMap<String, Pkg> = core_packages()
        .into_iter()
        .filter(|(n, p)| {
            !pkgs.contains_key(n) && !bins.iter().any(|b| Some(b.as_str()) == s(p, "bin"))
        })
        .collect();
    all.extend(pkgs);
    all
}

pub fn s<'a>(p: &'a Pkg, k: &str) -> Option<&'a str> {
    p.get(k).and_then(Value::as_str).filter(|v| !v.is_empty())
}

pub fn pkg_spec(p: &Pkg) -> Option<&str> {
    let spec = s(p, ctx().platform);
    if ctx().is_mac && !have("brew") && spec.is_some_and(|s| s.starts_with("brew:")) {
        if let Some(portable) = s(p, "linux").filter(|s| is_mise_spec(s) || *s == "builtin:fizsh") {
            return Some(portable);
        }
    }
    spec
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
    let base = Regex::new(r"\[.*\]$")
        .unwrap()
        .replace(&base, "")
        .into_owned();
    let last = base
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_string();
    last.rsplit(':').next().unwrap_or("").to_string()
}

pub(crate) fn brew_prefix() -> Option<PathBuf> {
    static PREFIX: OnceLock<Option<PathBuf>> = OnceLock::new();
    PREFIX
        .get_or_init(|| {
            let b = which("brew")?;
            let result = run(&["brew", "--prefix"], true);
            if result.code == 0 && !result.stdout.trim().is_empty() {
                return Some(PathBuf::from(result.stdout.trim()));
            }
            // Homebrew's bin/brew is a symlink into Library/Homebrew; do not use that target's parent.
            Some(b.parent()?.parent()?.to_path_buf())
        })
        .clone()
}

pub fn pkg_installed(name: &str, p: &Pkg) -> bool {
    let spec = pkg_spec(p).unwrap_or("");
    let installed = load_obj(&ctx().state.join("packages-installed.json"));
    if installed
        .get(name)
        .and_then(Value::as_str)
        .is_some_and(|old| old != spec)
    {
        return false;
    }
    pkg_present(name, p, true)
}

/// Status describes availability, independently of which recipe kit would install or update.
pub fn pkg_available(name: &str, p: &Pkg) -> bool {
    pkg_present(name, p, true)
}

fn package_bin<'a>(name: &'a str, p: &'a Pkg) -> &'a str {
    package_bin_for(name, p, ctx().platform)
}

pub(crate) fn package_bin_for<'a>(name: &'a str, p: &'a Pkg, platform: &str) -> &'a str {
    s(
        p,
        if platform == "mac" {
            "bin"
        } else {
            "linux_bin"
        },
    )
    .or_else(|| s(p, "bin"))
    .unwrap_or(match name {
        "git-delta" => "delta",
        "ripgrep" => "rg",
        "skim" => "sk",
        "neovim" => "nvim",
        _ => name,
    })
}

fn pkg_present(name: &str, p: &Pkg, any_provider: bool) -> bool {
    let spec = pkg_spec(p).unwrap_or("");
    let (kind, what) = spec.split_once(':').unwrap_or((spec, ""));
    if spec == "builtin:fizsh" {
        return is_executable(&ctx().home.join(REMOTE_KIT).join("opt/fizsh/bin/fizsh"));
    }
    let bin = package_bin(name, p);
    if kind != "cask" && any_provider && have(bin) {
        return true;
    }
    let what = what.rsplit('/').next().unwrap_or(what); // tap/name -> name
    let prefix = if kind == "brew" || kind == "cask" {
        brew_prefix()
    } else {
        None
    };
    if kind == "cask" {
        if let Some(app) = s(p, "app") {
            if PathBuf::from("/Applications").join(app).exists() {
                return true;
            }
        }
        // Homebrew keeps each installed cask/formula in a folder: much faster than asking `brew list`
        return prefix.is_some_and(|pre| pre.join("Caskroom").join(what).is_dir());
    }
    if kind == "brew" {
        return prefix.is_some_and(|pre| pre.join("Cellar").join(what).is_dir());
    }
    if !ctx().is_mac {
        // on Linux, tools live in ~/.local/kit (on PATH inside your kit shell, not necessarily here)
        let kit = ctx().home.join(REMOTE_KIT);
        if ["mise/shims", "bin", "opt/nvim/bin", "opt/zsh/bin"]
            .iter()
            .any(|d| is_executable(&kit.join(d).join(bin)))
        {
            return true;
        }
    }
    have(bin)
}

pub fn install_package(name: &str, p: &Pkg) -> bool {
    let Some(spec) = pkg_spec(p) else { return true };
    let (kind, what) = spec.split_once(':').unwrap_or((spec, ""));
    let cmd: Vec<String> = match kind {
        "builtin" if what == "fizsh" => {
            let script = ctx().state.join("install-fizsh.sh");
            write_atomic(
                &script,
                include_bytes!("../assets/remote/install-fizsh.sh"),
                0o700,
            )
            .unwrap_or_else(|e| die(&format!("cannot prepare fizsh installer: {e}")));
            vec!["bash".into(), path_str(&script)]
        }
        "brew" => vec!["brew".into(), "install".into(), what.into()],
        "cask" => vec![
            "brew".into(),
            "install".into(),
            "--cask".into(),
            what.into(),
        ],
        "cargo" => vec![
            "cargo".into(),
            "install".into(),
            "--locked".into(),
            what.into(),
        ],
        "cargo-git" => vec![
            "cargo".into(),
            "install".into(),
            "--locked".into(),
            "--git".into(),
            what.into(),
        ],
        "cmd" => vec!["sh".into(), "-c".into(), what.into()],
        "mise" => vec!["mise".into(), "use".into(), "-g".into(), what.into()],
        _ => vec!["mise".into(), "use".into(), "-g".into(), spec.into()],
    };
    if cmd[0] != "sh" && !have(&cmd[0]) {
        fail(&format!(
            "{name}: needs {}, which isn't installed; install it and retry",
            cmd[0]
        ));
        return false;
    }
    say(&format!("installing {name} ({spec})"));
    let env = if cmd[0] == "mise" {
        let kit = ctx().home.join(REMOTE_KIT);
        vec![
            (
                "MISE_GLOBAL_CONFIG_FILE".into(),
                path_str(&kit.join("mise.toml")),
            ),
            ("MISE_DATA_DIR".into(), path_str(&kit.join("mise"))),
            ("MISE_CONFIG_DIR".into(), path_str(&kit.join("config/mise"))),
            ("MISE_YES".into(), "1".into()),
            ("MISE_TRUSTED_CONFIG_PATHS".into(), path_str(&kit)),
        ]
    } else {
        vec![]
    };
    if cmd[0] == "mise" {
        let target = cmd.last().unwrap();
        let dependency = if target.starts_with("cargo:") {
            Some("core:rust@stable")
        } else if target.starts_with("npm:") {
            Some("core:node@lts")
        } else if target.starts_with("go:") {
            Some("core:go@latest")
        } else {
            None
        };
        if let Some(dep) = dependency {
            if run_full(
                &["mise", "use", "-g", dep],
                false,
                Some(&ctx().home),
                Some(&env),
                None,
            )
            .code
                != 0
            {
                fail(&format!("{name}: failed to install runtime {dep}"));
                return false;
            }
        }
        crate::remote::install_activation();
    }
    if run_full(&cmd, false, Some(&ctx().home), Some(&env), None).code != 0 {
        fail(&format!("{name}: install failed"));
        return false;
    }
    record_install(name, spec);
    true
}

pub(crate) fn record_install(name: &str, spec: &str) {
    let path = ctx().state.join("packages-installed.json");
    let mut installed = load_obj(&path);
    installed.insert(name.into(), spec.into());
    save_json(&path, &Value::Object(installed));
}

pub fn install_packages() {
    for (n, p) in all_packages() {
        if pkg_spec(&p).is_some() && !pkg_installed(&n, &p) {
            install_package(&n, &p);
        }
    }
}

fn registry_entry(name: &str) -> Option<Value> {
    static REGISTRY: OnceLock<Vec<Value>> = OnceLock::new();
    let registry = REGISTRY.get_or_init(|| {
        if !have("mise") {
            return vec![];
        }
        let r = run(&["mise", "registry", "--json"], true);
        serde_json::from_str::<Vec<Value>>(&r.stdout).unwrap_or_default()
    });
    let (name, _) = split_spec(name);
    registry
        .iter()
        .find(|e| {
            e.get("short").and_then(Value::as_str) == Some(&name)
                || e.get("backends")
                    .and_then(Value::as_array)
                    .is_some_and(|bs| {
                        bs.iter().any(|b| {
                            b.as_str().is_some_and(|b| {
                                b == name || b.split_once(':').is_some_and(|(_, repo)| repo == name)
                            })
                        })
                    })
        })
        .cloned()
}

fn inferred_bin(name: &str, spec: &str) -> Option<String> {
    let entry = registry_entry(name).or_else(|| registry_entry(spec))?;
    let bins = entry.get("bins")?.as_array()?;
    if bins.iter().any(|b| b.as_str() == Some(name)) {
        return None;
    }
    bins.first()?
        .as_str()
        .filter(|b| *b != name)
        .map(String::from)
}

/// Upgrade legacy Mac-only CLI entries and record the portable recipe once.
pub fn resolve_tools(names: &[String]) {
    let mut pkgs = load_pkgs();
    let mut changed = false;
    for (name, p) in &mut pkgs {
        if !names.is_empty() && !names.contains(name) {
            continue;
        }
        if name == "fizsh" && s(p, "linux") == Some("builtin:zsh") {
            p.insert("linux".into(), "builtin:fizsh".into());
            changed = true;
        }
        if s(p, "linux").is_some() {
            note(&format!(
                "{name}: Linux recipe already set; kept existing recipe"
            ));
            continue;
        }
        let Some(mac) = s(p, "mac").map(String::from) else {
            fail(&format!("{name}: cannot resolve a Linux recipe without a macOS recipe; kit add {name} --linux <recipe>"));
            continue;
        };
        if let Some(recipe) = portable_recipe(name, &mac) {
            if s(p, "bin").is_none() {
                if let Some(bin) = inferred_bin(name, &recipe) {
                    p.insert("bin".into(), bin.into());
                }
            }
            p.insert("linux".into(), pinned(&recipe).into());
            changed = true;
            note(&format!("{name}: added Linux recipe"));
        } else {
            fail(&format!("{name}: could not resolve a Linux recipe from {mac}; kit add {name} --linux <recipe>"));
        }
    }
    if changed {
        save_pkgs(&pkgs);
    }
}

fn portable_recipe(name: &str, mac: &str) -> Option<String> {
    match mac.split_once(':') {
        Some(("cargo", what)) => Some(format!("cargo:{what}")),
        Some(("cargo-git", url)) => Some(format!("cargo:{url}")),
        Some(("cmd", command)) if name == "rustup" && command.contains("https://sh.rustup.rs") => {
            Some("core:rust@stable".into())
        }
        Some(("cmd", command))
            if ![
                "brew ",
                "osascript",
                "/Applications/",
                "pbcopy",
                "launchctl",
                "defaults write",
            ]
            .iter()
            .any(|token| command.contains(token)) =>
        {
            Some(mac.into())
        }
        Some(("cask", "gcloud-cli" | "google-cloud-sdk")) => mise_backend("gcloud"),
        Some(("cask" | "brew", "kitty")) => Some("system:kitty".into()),
        Some(("mise", what)) => Some(what.into()),
        Some(("brew", what)) => match what {
            "fizsh" => Some("builtin:fizsh".into()),
            "zsh" => Some("builtin:zsh".into()),
            "neovim" => Some("builtin:nvim".into()),
            "mise" => Some("builtin:mise".into()),
            "coreutils" | "graphviz" | "pkgconf" | "cbonsai" => Some(format!("system:{what}")),
            "sevenzip" => Some("aqua:ip7z/7zip".into()),
            "fnm" => Some("aqua:Schniz/fnm".into()),
            _ => mise_backend(what).or_else(|| mise_backend(name)),
        },
        _ => None,
    }
}

fn mise_backend(name: &str) -> Option<String> {
    if name == "fizsh" {
        return Some("builtin:fizsh".into());
    }
    if name.contains(':') {
        return is_mise_spec(name).then(|| name.to_string());
    }
    if Regex::new(r"^[\w.-]+/[\w.-]+$").unwrap().is_match(name) {
        return Some(format!("github:{name}"));
    }
    if let Some(entry) = registry_entry(name) {
        if let Some(bs) = entry.get("backends").and_then(Value::as_array) {
            for pref in [
                "core:", "aqua:", "github:", "cargo:", "go:", "npm:", "pypi:",
            ] {
                if let Some(b) = bs
                    .iter()
                    .filter_map(Value::as_str)
                    .find(|b| b.starts_with(pref))
                {
                    return Some(b.into());
                }
            }
            return bs.first().and_then(Value::as_str).map(String::from);
        }
    }
    if !have("mise") {
        return None;
    }
    let r = run(&["mise", "registry", name], true);
    let words: Vec<&str> = r.stdout.split_whitespace().collect();
    if r.code != 0 || words.is_empty() {
        return None;
    }
    let backends: Vec<&str> = if words[0] == name {
        words[1..].to_vec()
    } else {
        words
    };
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
    if let Some(f) = info
        .get("formulae")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
    {
        return Some(format!("brew:{}", f.get("name")?.as_str()?));
    }
    if let Some(c) = info
        .get("casks")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
    {
        return Some(format!("cask:{}", c.get("token")?.as_str()?));
    }
    None
}

pub(crate) fn mise_latest(backend: &str) -> Option<String> {
    if !have("mise") {
        return None;
    }
    let r = run(&["mise", "latest", backend], true);
    let v = r.stdout.trim().to_string();
    (r.code == 0 && !v.is_empty() && !v.contains(char::is_whitespace)).then_some(v)
}

fn describe(name: &str, p: &Pkg) -> String {
    let mut parts = vec![
        format!("mac: {}", s(p, "mac").unwrap_or("-")),
        format!("linux: {}", s(p, "linux").unwrap_or("-")),
    ];
    if let Some(fb) = s(p, "linux_fallback") {
        parts.push(format!("fallback: {fb}"));
    }
    if !sends_to_servers(p) {
        parts.push("not on servers".into());
    }
    format!("{name} — {}", parts.join(" · "))
}

fn pinned(spec: &str) -> String {
    if spec.starts_with("system:") || spec.starts_with("builtin:") || spec.starts_with("cmd:") {
        return spec.into();
    }
    let (base, ver) = split_spec(spec);
    let ver = ver
        .or_else(|| mise_latest(&base))
        .unwrap_or_else(|| "latest".into());
    format!("{base}@{ver}")
}

/// The same recipe at the newest version mise knows (or unchanged when it can't tell).
fn bumped(spec: &str) -> String {
    if spec.starts_with("system:") || spec.starts_with("builtin:") || spec.starts_with("cmd:") {
        return spec.into();
    }
    let (base, old) = split_spec(spec);
    match mise_latest(&base) {
        Some(v) => format!("{base}@{v}"),
        None => old
            .map(|o| format!("{base}@{o}"))
            .unwrap_or_else(|| format!("{base}@latest")),
    }
}

/// `kit add <tool>`: work out how to install it on a Mac and on Linux, record it, install it here.
pub fn add_tools(names: &[String], a: &Args) {
    if a.flag("upgrade") {
        crate::updates::update_tools(names, false);
        return;
    }
    let mut pkgs = load_pkgs();
    let mut changed = false;
    for name in names {
        if let Some(spec) = a.one("linux").filter(|s| {
            !is_mise_spec(s)
                && !s.starts_with("cmd:")
                && !s.starts_with("system:")
                && !s.starts_with("builtin:")
        }) {
            fail(&format!("{spec} isn't a Linux recipe"));
            continue;
        }
        if let Some(spec) = a.one("fallback").filter(|s| !is_mise_spec(s)) {
            fail(&format!("--fallback {spec}: use a mise spec"));
            continue;
        }
        let name = name.clone();
        let key = pkg_key(&name);
        let old = pkgs.get(&key).cloned();
        let mut adopted_cargo = None;
        let mut entry = old.clone().unwrap_or_default();
        let kind = name.split_once(':').map(|(k, _)| k.to_string());
        // how to install it on a Mac
        if let Some(m) = a.one("mac") {
            entry.insert("mac".into(), m.into());
        } else if matches!(
            kind.as_deref(),
            Some("brew" | "cask" | "cargo" | "cargo-git" | "cmd")
        ) {
            entry.insert("mac".into(), name.clone().into()); // e.g. kit add cask:kitty
        }
        // Migrate the former fizsh placeholder rather than reusing its zsh recipe.
        if key == "fizsh" && s(&entry, "linux") == Some("builtin:zsh") {
            entry.insert("linux".into(), "builtin:fizsh".into());
        }
        // how to install it on Linux and servers (adding a tracked tool again pins the latest version)
        {
            let wanted = a
                .one("linux")
                .or_else(|| (kind.is_some() && is_mise_spec(&name)).then(|| name.clone()));
            match wanted {
                Some(spec) if is_mise_spec(&spec) || spec.starts_with("system:") || spec.starts_with("builtin:") || spec.starts_with("cmd:") => {
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
        if a.one("mac").is_none() {
            if let Some(old) = &old {
                if s(old, "linux")
                    .is_some_and(|linux| s(old, "mac") == Some(format!("mise:{linux}").as_str()))
                {
                    if let Some(linux) = s(&entry, "linux").map(String::from) {
                        entry.insert("mac".into(), format!("mise:{linux}").into());
                    }
                }
            }
        }
        if s(&entry, "mac").is_none() {
            let mac = brew_formula(&name).or_else(|| {
                s(&entry, "linux").map(|l| {
                    if l == "builtin:fizsh" {
                        l.to_string()
                    } else {
                        format!("mise:{l}")
                    }
                })
            });
            if let Some(m) = mac {
                entry.insert("mac".into(), m.into());
            }
        }
        if s(&entry, "linux").is_none() {
            if let Some(recipe) = s(&entry, "mac").and_then(|mac| portable_recipe(&key, mac)) {
                entry.insert("linux".into(), pinned(&recipe).into());
            }
        }
        if s(&entry, "bin").is_none() {
            if let Some(bin) = s(&entry, "linux").and_then(|spec| inferred_bin(&key, spec)) {
                entry.insert("bin".into(), bin.into());
            }
        }
        if let Some(b) = a.one("bin") {
            entry.insert("bin".into(), b.into());
        }
        let platform_arg = if ctx().is_mac { "mac" } else { "linux" };
        if kind.is_none() && a.one(platform_arg).is_none() {
            if let Some(spec) =
                crate::updates::installed_cargo_spec(package_bin_for(&key, &entry, ctx().platform))
            {
                entry.insert(ctx().platform.into(), spec.clone().into());
                adopted_cargo = Some(spec);
            }
        }
        if let Some(fb) = a.one("fallback") {
            if is_mise_spec(&fb) {
                entry.insert("linux_fallback".into(), pinned(&fb).into());
            } else {
                fail(&format!(
                    "--fallback {fb}: use a mise spec, e.g. cargo:<crate>@<version>"
                ));
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
            out(&format!(
                "  already tracked, nothing new: {}",
                describe(&key, &entry)
            ));
        } else {
            pkgs.insert(key.clone(), entry.clone());
            save_pkgs(&pkgs);
            changed = true;
            out(&format!(
                "  {} {}",
                if old.is_some() { "updated" } else { "added" },
                describe(&key, &entry)
            ));
        }
        if let Some(spec) = adopted_cargo {
            record_install(&key, &spec);
        }
        if ctx().is_mac && pkg_spec(&entry).is_some() && !pkg_installed(&key, &entry) {
            install_package(&key, &entry);
        }
    }
    if !ctx().is_mac
        && names.iter().any(|n| {
            pkgs.get(&pkg_key(n))
                .is_some_and(|p| s(p, "linux").is_some())
        })
    {
        crate::remote::linux_environment();
    }
    if changed {
        say("kit sync to back it up");
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
    let w = pkgs
        .keys()
        .map(|n| n.chars().count())
        .max()
        .unwrap_or(4)
        .max(4);
    out(&format!(
        "Tools:\n  {:w$}  {:4}  {:7}  {:28}  linux (fallback)",
        "name", "here", "servers", "mac"
    ));
    for (n, p) in &pkgs {
        let here = if pkg_spec(p).is_some() {
            if pkg_available(n, p) {
                "yes ".to_string()
            } else {
                c("33", "no  ")
            }
        } else {
            "-   ".to_string()
        };
        let srv = if sends_to_servers(p) { "yes" } else { "no" };
        let mut mac = s(p, "mac").unwrap_or("-").to_string();
        if mac.chars().count() > 28 {
            mac = mac.chars().take(27).collect::<String>() + "…";
        }
        let fb = s(p, "linux_fallback")
            .map(|f| format!("  ({f})"))
            .unwrap_or_default();
        out(&format!(
            "  {n:w$}  {here}  {srv:7}  {mac:28}  {}{fb}",
            s(p, "linux").unwrap_or("-")
        ));
    }
    note(&format!("  here: installed on this machine ('-' = no recipe for {}) · servers: installed by kit push", ctx().platform));
}
