//! packages.json: { name: {"mac": "brew:bat"|"cask:kitty"|"cargo:eza"|"mise:<spec>",
//!                         "linux": "aqua:sharkdp/bat@0.26.1"   (a mise tool spec, Linux machines + servers),
//!                         "linux_fallback": "cargo:...@1.0.0"   (built from source when no download runs),
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
    "missing",
    "blocked",
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
            mk(
                "aqua:dandavison/delta@0.20.1",
                "aqua:dandavison/delta@0.20.1",
                "delta",
            ),
        ),
        (
            "bat".to_string(),
            mk("aqua:sharkdp/bat@0.26.1", "aqua:sharkdp/bat@0.26.1", "bat"),
        ),
        (
            "neovim".to_string(),
            mk(
                "aqua:neovim/neovim@0.12.6",
                "aqua:neovim/neovim@0.12.6",
                "nvim",
            ),
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
    all_packages_for(ctx().platform)
}

/// Default-tool aliases must be resolved for the destination machine, which
/// may use different binary names from the host that builds an SSH payload.
pub(crate) fn all_packages_for(platform: &str) -> BTreeMap<String, Pkg> {
    let pkgs = load_pkgs();
    let bins: Vec<String> = pkgs
        .iter()
        .map(|(n, p)| package_bin_for(n, p, platform).to_string())
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
            let result = query(&["brew", "--prefix"], 15);
            if result.code == 0 && !result.stdout.trim().is_empty() {
                return Some(PathBuf::from(result.stdout.trim()));
            }
            // Homebrew's bin/brew is a symlink into Library/Homebrew; do not use that target's parent.
            Some(b.parent()?.parent()?.to_path_buf())
        })
        .clone()
}

pub fn pkg_installed(name: &str, p: &Pkg) -> bool {
    // Availability wins over old provider/version bookkeeping. Sync never replaces
    // a working external installation; explicit update manages version changes.
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

// One bounded offline provider inventory per command, never a persistent cache.
// Executing an arbitrary tracked tool to inspect it would violate status and
// dry-run's read-only contract; executable shim files alone prove nothing.
static MISE_INVENTORY: OnceLock<std::sync::Mutex<Option<std::collections::BTreeSet<String>>>> =
    OnceLock::new();
fn clear_mise_inventory() {
    if let Some(cache) = MISE_INVENTORY.get() {
        *cache.lock().unwrap() = None;
    }
}
fn installed_mise_keys() -> std::collections::BTreeSet<String> {
    let cache = MISE_INVENTORY.get_or_init(Default::default);
    let mut cache = cache.lock().unwrap();
    if let Some(keys) = cache.as_ref() {
        return keys.clone();
    }
    let kit = ctx().home.join(REMOTE_KIT);
    let mise = kit.join("bin/mise");
    let mut keys = std::collections::BTreeSet::new();
    if is_executable(&mise) {
        let env = vec![
            ("MISE_DATA_DIR".into(), path_str(&kit.join("mise"))),
            (
                "MISE_GLOBAL_CONFIG_FILE".into(),
                path_str(&kit.join("mise.toml")),
            ),
            ("MISE_CONFIG_DIR".into(), path_str(&kit.join("config/mise"))),
            ("MISE_CACHE_DIR".into(), path_str(&kit.join("cache/mise"))),
            ("MISE_STATE_DIR".into(), path_str(&kit.join("state/mise"))),
            ("MISE_TRUSTED_CONFIG_PATHS".into(), path_str(&kit)),
            ("MISE_OFFLINE".into(), "1".into()),
            ("MISE_AUTO_INSTALL".into(), "0".into()),
            ("MISE_NOT_FOUND_AUTO_INSTALL".into(), "0".into()),
            ("MISE_DISABLE_HOOKS".into(), "1".into()),
        ];
        let result = query_full(
            &[
                mise.as_os_str(),
                std::ffi::OsStr::new("ls"),
                std::ffi::OsStr::new("--installed"),
                std::ffi::OsStr::new("--json"),
                std::ffi::OsStr::new("--offline"),
            ],
            3,
            Some(&kit),
            Some(&env),
        );
        if result.code == 0 {
            if let Ok(Value::Object(tools)) = serde_json::from_str::<Value>(&result.stdout) {
                for (key, versions) in tools {
                    if versions.as_array().is_some_and(|versions| {
                        versions.iter().any(|v| {
                            v.get("installed").and_then(Value::as_bool) == Some(true)
                                && v.get("active").and_then(Value::as_bool) == Some(true)
                        })
                    }) {
                        keys.insert(key);
                    }
                }
            }
        }
    }
    *cache = Some(keys.clone());
    keys
}
fn passive_binary_available(name: &str, p: &Pkg) -> bool {
    let bin = package_bin(name, p);
    let shims = ctx().home.join(REMOTE_KIT).join("mise/shims");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let paths: Vec<PathBuf> = std::env::split_paths(&path)
        .map(|d| d.join(bin))
        .chain(std::env::var_os("CARGO_HOME").map(|h| PathBuf::from(h).join("bin").join(bin)))
        .chain([
            ctx().home.join(REMOTE_KIT).join("bin").join(bin),
            shims.join(bin),
            ctx().home.join(".cargo/bin").join(bin),
        ])
        .collect();
    // A generated shim must not hide a working external installation.
    if paths
        .iter()
        .any(|path| !path.starts_with(&shims) && is_executable(path))
    {
        return true;
    }
    if !paths
        .iter()
        .any(|path| path.starts_with(&shims) && is_executable(path))
    {
        return false;
    }
    let keys = installed_mise_keys();
    let base = pkg_spec(p).map(|s| split_spec(s.strip_prefix("mise:").unwrap_or(s)).0);
    [
        Some(name.to_string()),
        Some(bin.to_string()),
        base.clone(),
        base.map(|b| pkg_key(&b)),
    ]
    .into_iter()
    .flatten()
    .any(|key| keys.contains(&key))
}

/// Runtime validation is only for completed installs, never passive status,
/// preview or TUI inventory. A stale executable mise shim is not proof that its
/// configured tool can start.
pub(crate) fn installed_tool_runs(name: &str, p: &Pkg) -> bool {
    if pkg_spec(p).is_some_and(|s| s.starts_with("cask:") || s.starts_with("builtin:")) {
        return true;
    }
    let Some(path) = which(package_bin(name, p)) else {
        return false;
    };
    if !path.starts_with(ctx().home.join(REMOTE_KIT).join("mise/shims")) {
        return true;
    }
    // A mise shim resolves tools through its environment, not its filesystem
    // location. Match the installation's private roots on a cold account that
    // has not sourced Kit's shell activation yet.
    let kit = ctx().home.join(REMOTE_KIT);
    let env = vec![
        ("MISE_DATA_DIR".into(), path_str(&kit.join("mise"))),
        (
            "MISE_GLOBAL_CONFIG_FILE".into(),
            path_str(&kit.join("mise.toml")),
        ),
        ("MISE_CONFIG_DIR".into(), path_str(&kit.join("config/mise"))),
        ("MISE_CACHE_DIR".into(), path_str(&kit.join("cache/mise"))),
        ("MISE_STATE_DIR".into(), path_str(&kit.join("state/mise"))),
        ("MISE_TRUSTED_CONFIG_PATHS".into(), path_str(&kit)),
        ("MISE_AUTO_INSTALL".into(), "0".into()),
        ("MISE_OFFLINE".into(), "1".into()),
    ];
    for flag in ["--version", "--help", "-h"] {
        let r = query_full(
            &[path.as_os_str(), std::ffi::OsStr::new(flag)],
            5,
            Some(&kit),
            Some(&env),
        );
        let text = format!("{}{}", r.stdout, r.stderr);
        if [
            "No version is set",
            "mise ERROR",
            "GLIBC_",
            "GLIBCXX",
            "Exec format",
            "cannot execute",
        ]
        .iter()
        .any(|message| text.contains(message))
        {
            return false;
        }
        if r.code == 0 {
            return true;
        }
        let lower = text.to_ascii_lowercase();
        if !matches!(r.code, 1 | 2)
            || ![
                "unknown option",
                "unknown flag",
                "unrecognized option",
                "unrecognized argument",
                "invalid option",
                "unexpected argument",
            ]
            .iter()
            .any(|message| lower.contains(message))
        {
            return false;
        }
    }
    false
}

fn pkg_present(name: &str, p: &Pkg, any_provider: bool) -> bool {
    let spec = pkg_spec(p).unwrap_or("");
    let (kind, what) = spec.split_once(':').unwrap_or((spec, ""));
    if spec == "builtin:fizsh" {
        return is_executable(&ctx().home.join(REMOTE_KIT).join("opt/fizsh/bin/fizsh"));
    }
    let bin = package_bin(name, p);
    if kind != "cask" && any_provider && passive_binary_available(name, p) {
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
            .any(|d| *d != "mise/shims" && is_executable(&kit.join(d).join(bin)))
        {
            return true;
        }
    }
    passive_binary_available(name, p)
}

pub fn install_package(name: &str, p: &Pkg) -> bool {
    let Some(spec) = pkg_spec(p) else { return true };
    if let Some(problem) = crate::recipe::problem(spec) {
        fail(&format!("{name}: unsupported recipe {spec:?}: {problem}; replace it with kit add {name} --{} <mise-or-cargo-recipe>", ctx().platform));
        return false;
    }
    // Legacy Kit-owned recipes are not mise backend identifiers. Keep the
    // local path consistent with setup.sh rather than sending builtin:* to
    // `mise use`, which can never resolve it.
    if spec == "builtin:mise" {
        crate::remote::ensure_mise();
        record_install(name, spec);
        return true;
    }
    if spec == "builtin:nvim" {
        let mut translated = p.clone();
        translated.insert(ctx().platform.into(), "aqua:neovim/neovim@0.12.6".into());
        return install_package(name, &translated);
    }
    if spec == "builtin:zsh" {
        if have("zsh") {
            record_install(name, spec);
            return true;
        }
        fail(&format!("{name}: zsh is unavailable; Linux setup provisions Kit's portable shell, while macOS requires its system zsh"));
        return false;
    }
    if !spec.starts_with("cargo:")
        && !spec.starts_with("builtin:")
        && !spec.starts_with("brew:")
        && !spec.starts_with("cask:")
    {
        crate::remote::ensure_mise();
    }
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
        "brew" => vec![
            "brew".into(),
            "install".into(),
            "--formula".into(),
            what.into(),
        ],
        "cask" if !ctx().is_mac => {
            fail(&format!(
                "{name}: Homebrew casks are macOS-only; set an explicit Linux recipe"
            ));
            return false;
        }
        "cask" => vec![
            "brew".into(),
            "install".into(),
            "--cask".into(),
            what.into(),
        ],
        "cargo" if !have("cargo") => {
            crate::remote::ensure_mise();
            vec!["mise".into(), "use".into(), "-g".into(), spec.into()]
        }
        "cargo" => vec![
            "cargo".into(),
            "install".into(),
            "--locked".into(),
            what.into(),
        ],
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
    let mut env = if cmd[0] == "mise" {
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
    env.extend([
        ("HOMEBREW_NO_AUTO_UPDATE".into(), "1".into()),
        ("GIT_TERMINAL_PROMPT".into(), "0".into()),
    ]);
    if cmd[0] == "mise" {
        let target = cmd.last().unwrap();
        let dependencies: &[&str] = if target.starts_with("cargo:") {
            &["core:rust@stable"]
        } else if target.starts_with("npm:") {
            &["core:node@lts"]
        } else if target.starts_with("go:") {
            &["core:go@latest"]
        } else if target.starts_with("pypi:") {
            &["aqua:astral-sh/uv@latest", "core:python@3.12"]
        } else {
            &[]
        };
        for dep in dependencies {
            if run_install(
                name,
                &["mise", "use", "-g", *dep],
                Some(&ctx().home),
                Some(&env),
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
    if run_install(name, &cmd, Some(&ctx().home), Some(&env)).code != 0 {
        fail(&format!("{name}: install failed"));
        return false;
    }
    clear_mise_inventory();
    if !pkg_available(name, p) || !installed_tool_runs(name, p) {
        fail(&format!("{name}: installer exited successfully but the expected tool is unavailable; check --bin and the install log"));
        return false;
    }
    record_install(name, spec);
    true
}

pub(crate) fn record_install(name: &str, spec: &str) {
    clear_mise_inventory();
    let path = ctx().state.join("packages-installed.json");
    let mut installed = load_obj(&path);
    installed.insert(name.into(), spec.into());
    save_json(&path, &Value::Object(installed));
}

pub fn install_packages() {
    for (n, p) in all_packages() {
        if pkg_spec(&p).is_none() {
            if !ctx().is_mac && s(&p, "mac").is_some_and(|s| s.starts_with("cask:")) {
                continue;
            }
            if !pkg_installed(&n, &p) {
                fail(&format!("{n}: no {} recipe and the tool is not installed here; kit add {n} --{} <recipe>", ctx().platform, ctx().platform));
            }
        } else if !pkg_installed(&n, &p) {
            install_package(&n, &p);
        }
    }
}

fn mise_query(args: &[&str]) -> Out {
    let kit = ctx().home.join(REMOTE_KIT);
    let env = vec![
        (
            "MISE_GLOBAL_CONFIG_FILE".into(),
            path_str(&kit.join("mise.toml")),
        ),
        ("MISE_CONFIG_DIR".into(), path_str(&kit.join("config/mise"))),
        ("MISE_DATA_DIR".into(), path_str(&kit.join("mise"))),
    ];
    let mut cmd = vec!["mise"];
    cmd.extend_from_slice(args);
    query_full(&cmd, 30, Some(&ctx().home), Some(&env))
}

fn registry_entry(name: &str) -> Option<Value> {
    static REGISTRY: OnceLock<Vec<Value>> = OnceLock::new();
    let registry = REGISTRY.get_or_init(|| {
        if !have("mise") {
            return vec![];
        }
        let r = mise_query(&["registry", "--json"]);
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
        if s(p, "linux").is_some_and(|s| crate::recipe::problem(s).is_none()) {
            note(&format!(
                "{name}: Linux recipe already set; kept existing recipe"
            ));
            continue;
        }
        let Some(mac) = s(p, "mac").map(String::from) else {
            fail(&format!(
                "{name}: no supported source recipe; kit add {name} --linux <recipe>"
            ));
            continue;
        };
        if let Some(recipe) = portable_recipe(name, &mac) {
            if s(p, "bin").is_none() {
                if let Some(bin) = inferred_bin(name, &recipe) {
                    p.insert("bin".into(), bin.into());
                }
            }
            let recipe = pinned(&recipe);
            if let Some(why) = crate::recipe::problem(&recipe) {
                fail(&format!(
                    "{name}: could not resolve a supported Linux recipe: {why}"
                ));
                continue;
            }
            let mut candidate = Pkg::new();
            candidate.insert("linux".into(), recipe.clone().into());
            if !crate::capability::check_entry(name, &candidate, false) {
                continue;
            }
            p.insert("linux".into(), recipe.into());
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

/// Explicit migration only; never rewrite a valid platform recipe during sync.
pub fn resolve_mac_tools(names: &[String]) {
    let mut pkgs = load_pkgs();
    let mut changed = false;
    for (name, p) in &mut pkgs {
        if !names.is_empty() && !names.contains(name) {
            continue;
        }
        if s(p, "mac").is_some_and(|s| crate::recipe::problem(s).is_none()) {
            continue;
        }
        let source = s(p, "linux")
            .filter(|s| crate::recipe::problem(s).is_none())
            .map(String::from)
            .or_else(|| mise_backend(name).map(|s| pinned(&s)));
        let Some(source) = source else {
            fail(&format!("{name}: no supported macOS recipe"));
            continue;
        };
        if let Some(why) = crate::recipe::problem(&source) {
            fail(&format!("{name}: {why}"));
            continue;
        }
        let mut candidate = Pkg::new();
        candidate.insert("mac".into(), source.clone().into());
        if !crate::capability::check_entry(name, &candidate, false) {
            continue;
        }
        p.insert("mac".into(), source.into());
        changed = true;
        note(&format!(
            "{name}: added macOS recipe; kit packages check {name} checks artifact coverage"
        ));
    }
    if changed {
        save_pkgs(&pkgs);
    }
}

fn portable_recipe(name: &str, mac: &str) -> Option<String> {
    if let Some(spec) = mac.strip_prefix("mise:") {
        return Some(spec.into());
    }
    if crate::recipe::backend_supported(mac) {
        return Some(mac.into());
    }
    if name == "fizsh" {
        return Some("builtin:fizsh".into());
    }
    // Migration is explicit: consult mise, never translate to apt/Brew/custom commands.
    mise_backend(name)
}

fn mise_backend(name: &str) -> Option<String> {
    if name == "fizsh" {
        return Some("builtin:fizsh".into());
    }
    if let Some(spec) = name.strip_prefix("mise:") {
        return crate::recipe::backend_supported(spec).then(|| spec.to_string());
    }
    if name.contains(':') {
        return crate::recipe::backend_supported(name).then(|| name.to_string());
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
            return bs
                .iter()
                .filter_map(Value::as_str)
                .find(|b| crate::recipe::backend_supported(b))
                .map(String::from);
        }
    }
    if !have("mise") {
        return None;
    }
    let r = mise_query(&["registry", name]);
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
    backends
        .into_iter()
        .find(|b| crate::recipe::backend_supported(b))
        .map(String::from)
}

pub(crate) fn brew_formula(name: &str) -> Option<String> {
    if !have("brew") || name.contains('/') || name.contains(':') {
        return None;
    }
    let r = query(&["brew", "info", "--json=v2", name], 30);
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
    let r = mise_query(&["latest", backend]);
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
    if spec.starts_with("brew:")
        || spec.starts_with("cask:")
        || spec.starts_with("system:")
        || spec.starts_with("builtin:")
        || spec.starts_with("cmd:")
    {
        return spec.into();
    }
    let (base, ver) = split_spec(spec);
    let ver = ver
        .filter(|v| !matches!(v.as_str(), "latest" | "stable" | "lts"))
        .or_else(|| mise_latest(&base))
        .unwrap_or_else(|| "latest".into());
    format!("{base}@{ver}")
}

/// Record explicit recipes or resolve a bare name through Cargo ownership / mise.
pub fn add_tools(names: &[String], a: &Args) {
    if a.flag("upgrade") {
        crate::updates::update_tools(names, false);
        return;
    }
    let mut pkgs = load_pkgs();
    let valid_name = Regex::new(r"^[A-Za-z0-9_.+-]+$").unwrap();
    for name in names {
        let key = pkg_key(name);
        if !valid_name.is_match(&key) || key.starts_with('-') {
            fail(&format!("invalid package name: {name:?}"));
            continue;
        }
        let old = pkgs.get(&key).cloned();
        let mut entry = old.clone().unwrap_or_default();
        let explicit = a.one("mac").is_some() || a.one("linux").is_some();
        let named_spec = name.contains(':');
        if named_spec
            && !crate::recipe::backend_supported(name)
            && !name.starts_with("brew:")
            && !name.starts_with("cask:")
            && name != "builtin:fizsh"
        {
            fail(&format!(
                "{name}: use a supported mise, Cargo or Homebrew source"
            ));
            continue;
        }
        if [a.one("mac"), a.one("linux"), a.one("fallback")]
            .iter()
            .flatten()
            .any(|s| {
                !crate::recipe::backend_supported(s)
                    && !s.starts_with("brew:")
                    && !s.starts_with("cask:")
                    && s != "builtin:fizsh"
            })
        {
            fail(&format!("{name}: use a supported mise, Cargo or Homebrew recipe; system packages and custom install commands are not managed"));
            continue;
        }
        let adopted = if !named_spec && !explicit {
            crate::updates::installed_cargo_spec(package_bin(&key, &entry))
        } else {
            None
        };
        let resolved = if old.is_some() && !named_spec {
            None
        } else if let Some(spec) = adopted.clone() {
            Some(spec)
        } else if name.starts_with("brew:") || name.starts_with("cask:") {
            Some(name.clone())
        } else {
            crate::remote::ensure_mise();
            mise_backend(name)
                .map(|s| pinned(&s))
                .or_else(|| brew_formula(name))
        };
        if let Some(spec) = resolved {
            if named_spec {
                entry.insert("mac".into(), spec.clone().into());
                if !spec.starts_with("cask:") {
                    entry.insert("linux".into(), spec.into());
                }
            } else {
                entry.entry("mac").or_insert_with(|| spec.clone().into());
                if !spec.starts_with("cask:") {
                    entry.entry("linux").or_insert_with(|| spec.into());
                }
            }
        }
        for (field, wanted) in [
            ("mac", a.one("mac")),
            ("linux", a.one("linux")),
            ("linux_fallback", a.one("fallback")),
        ] {
            if let Some(spec) = wanted {
                if crate::recipe::backend_supported(&spec) {
                    crate::remote::ensure_mise();
                }
                entry.insert(field.into(), pinned(&spec).into());
            }
        }
        if let Some(spec) = adopted {
            entry.insert(ctx().platform.into(), spec.into());
        }
        if let Some(bin) = a.one("bin") {
            entry.insert("bin".into(), bin.into());
        }
        if s(&entry, "bin").is_none() {
            if let Some(bin) = s(&entry, "linux").and_then(|spec| inferred_bin(&key, spec)) {
                entry.insert("bin".into(), bin.into());
            }
        }
        if a.flag("no_servers") {
            entry.insert("remote".into(), false.into());
        }
        if s(&entry, "mac").is_none() && s(&entry, "linux").is_none() {
            fail(&format!("{name}: mise could not resolve a recipe; use an explicit pinned mise/Cargo recipe or owner/repo release source"));
            continue;
        }
        if s(&entry, "linux_fallback").is_some_and(|s| !crate::recipe::backend_supported(s)) {
            fail(&format!(
                "{key}: Linux fallback must use a mise backend or Cargo source"
            ));
            continue;
        }
        let mut invalid = false;
        for field in ["mac", "linux", "linux_fallback"] {
            if let Some(spec) = s(&entry, field) {
                if let Some(why) = crate::recipe::problem(spec) {
                    fail(&format!("{key} ({field}): {why}"));
                    invalid = true;
                }
            }
        }
        if invalid {
            continue;
        }
        if old.as_ref() != Some(&entry) {
            if !crate::capability::check_entry(&key, &entry, false) {
                continue;
            }
            pkgs.insert(key.clone(), entry.clone());
            save_pkgs(&pkgs);
            say(&format!(
                "{} {}",
                if old.is_some() { "updated" } else { "added" },
                describe(&key, &entry)
            ));
        } else {
            note(&format!(
                "{key}: already tracked; use kit packages update for upgrades"
            ));
        }
        if ctx().is_mac && pkg_spec(&entry).is_some() && !pkg_installed(&key, &entry) {
            install_package(&key, &entry);
        }
    }
    if !ctx().is_mac && exit_code() == 0 {
        crate::remote::linux_environment();
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
