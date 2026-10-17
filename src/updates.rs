//! Check-first updates. Never change an installed program's package manager as a fallback.
use crate::core::guard_repo;
use crate::pkg::{
    brew_prefix, load_pkgs, package_bin_for, pkg_available, pkg_spec, save_pkgs, split_spec,
};
use crate::util::*;
use regex::Regex;
use semver::Version;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Clone)]
struct CargoInstall {
    root: PathBuf,
    name: String,
    version: String,
    source: String,
}

fn cargo_install(bin: &str) -> Option<CargoInstall> {
    let mut roots = vec![ctx().home.join(".cargo")];
    if let Some(root) = std::env::var_os("CARGO_HOME") {
        roots.push(root.into());
    }
    roots.push(ctx().home.join(REMOTE_KIT).join("cargo"));
    if let Some(path) = which(bin) {
        if let Some(root) = path.parent().and_then(|p| p.parent()) {
            roots.push(root.to_path_buf());
        }
    }
    // Prefer Cargo's explicit ownership record over a duplicate Brew copy on PATH.
    for root in roots {
        if !is_executable(&root.join("bin").join(bin)) {
            continue;
        }
        let metadata = load_obj(&root.join(".crates2.json"));
        let Some(installs) = metadata.get("installs").and_then(Value::as_object) else {
            continue;
        };
        for (key, info) in installs {
            if !info
                .get("bins")
                .and_then(Value::as_array)
                .is_some_and(|bins| bins.iter().any(|b| b.as_str() == Some(bin)))
            {
                continue;
            }
            let mut fields = key.splitn(3, ' ');
            let (Some(name), Some(version), Some(source)) =
                (fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            if name.starts_with('-')
                || !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                continue;
            }
            return Some(CargoInstall {
                root,
                name: name.into(),
                version: version.into(),
                source: source.trim_matches(['(', ')']).into(),
            });
        }
    }
    None
}

fn crates_io(source: &str) -> bool {
    matches!(
        source,
        "registry+https://github.com/rust-lang/crates.io-index"
            | "registry+https://index.crates.io/"
            | "registry+sparse+https://index.crates.io/"
    )
}

pub(crate) fn installed_cargo_spec(bin: &str) -> Option<String> {
    let installed = cargo_install(bin)?;
    if crates_io(&installed.source) {
        Some(format!("cargo:{}@{}", installed.name, installed.version))
    } else {
        None
    }
}

fn newer(latest: &str, current: &str) -> Option<bool> {
    Some(
        Version::parse(latest.trim_start_matches('v')).ok()?
            > Version::parse(current.trim_start_matches('v')).ok()?,
    )
}

fn cargo_update(name: &str, bin: &str, installed: &CargoInstall, check: bool) -> Option<String> {
    let local_cargo = installed.root.join("bin/cargo");
    let cargo = if is_executable(&local_cargo) {
        local_cargo
    } else {
        let Some(cargo) = which("cargo") else {
            fail(&format!("{name}: Cargo is unavailable; update skipped"));
            return None;
        };
        cargo
    };
    let mut env = vec![("CARGO_HOME".into(), path_str(&installed.root))];
    let rustup = if installed.root == ctx().home.join(REMOTE_KIT).join("cargo") {
        ctx().home.join(REMOTE_KIT).join("rustup")
    } else {
        std::env::var_os("RUSTUP_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| ctx().home.join(".rustup"))
    };
    if rustup.is_dir() {
        env.push(("RUSTUP_HOME".into(), path_str(&rustup)));
    }
    let cargo = path_str(&cargo);
    let call = |args: &[&str], capture| {
        let mut command = vec![cargo.as_str()];
        command.extend_from_slice(args);
        if capture {
            query_full(&command, 30, Some(&ctx().home), Some(&env))
        } else {
            run_install(name, &command, Some(&ctx().home), Some(&env))
        }
    };
    say(&format!(
        "checking {name} (Cargo: {} {})",
        installed.name, installed.version
    ));
    if crates_io(&installed.source) {
        let result = call(&["search", &installed.name, "--limit", "1"], true);
        let pattern = Regex::new(&format!(
            r#"(?m)^{}\s*=\s*"([^"]+)""#,
            regex::escape(&installed.name)
        ))
        .unwrap();
        let latest = pattern
            .captures(&result.stdout)
            .and_then(|c| c.get(1))
            .map(|v| v.as_str());
        let Some(latest) = latest.filter(|_| result.code == 0) else {
            fail(&format!(
                "{name}: could not check the latest Cargo version; nothing installed"
            ));
            return None;
        };
        match newer(latest, &installed.version) {
            Some(false) => {
                say(&format!("{name}: up to date (Cargo {})", installed.version));
                Some(format!("cargo:{}@{}", installed.name, installed.version))
            }
            Some(true) => {
                say(&format!(
                    "{name}: {} → {latest} (Cargo){}",
                    installed.version,
                    if check { " — update available" } else { "" }
                ));
                let spec = format!("{}@{latest}", installed.name);
                // Cargo decides whether a build is necessary. Never pass --force.
                if check {
                    return Some(format!("cargo:{spec}"));
                }
                if call(&["install", "--locked", &spec], false).code == 0
                    && cargo_install(bin).is_some_and(|actual| {
                        actual.root == installed.root
                            && actual.name == installed.name
                            && actual.version == latest
                    })
                {
                    Some(format!("cargo:{spec}"))
                } else {
                    fail(&format!(
                        "{name}: Cargo update did not confirm the requested installed version; recorded recipe preserved"
                    ));
                    None
                }
            }
            None => {
                fail(&format!(
                    "{name}: cannot compare Cargo versions; update skipped"
                ));
                None
            }
        }
    } else if installed.source.starts_with("git+") {
        warn(&format!("{name}: Cargo Git checkouts require an explicit pinned mise recipe; automatic HEAD updates are disabled"));
        None
    } else {
        warn(&format!(
            "{name}: custom Cargo registry needs a manual update; skipped"
        ));
        None
    }
}

fn brew_update(name: &str, spec: &str, check: bool) -> Option<String> {
    let (kind, package) = spec.split_once(':')?;
    if package.is_empty() || package.starts_with('-') {
        fail(&format!(
            "{name}: invalid Homebrew package name; update skipped"
        ));
        return None;
    }
    let dir = if kind == "cask" { "Caskroom" } else { "Cellar" };
    if !brew_prefix().is_some_and(|p| {
        p.join(dir)
            .join(package.rsplit('/').next().unwrap_or(package))
            .is_dir()
    }) {
        warn(&format!("{name}: not installed through Homebrew; update skipped instead of installing another copy"));
        return None;
    }
    say(&format!("checking {name} (Homebrew)"));
    let kind_flag = if kind == "cask" {
        "--cask"
    } else {
        "--formula"
    };
    let result = query(&["brew", "outdated", "--json=v2", kind_flag, package], 30);
    let data = serde_json::from_str::<Value>(&result.stdout).ok();
    let Some(outdated) = data
        .as_ref()
        .and_then(|v| v.get(if kind == "cask" { "casks" } else { "formulae" }))
        .and_then(Value::as_array)
        .filter(|_| result.code == 0)
    else {
        fail(&format!(
            "{name}: could not check Homebrew updates; nothing installed"
        ));
        return None;
    };
    if outdated.is_empty() {
        say(&format!("{name}: up to date (Homebrew)"));
    } else {
        say(&format!("{name}: Homebrew update available"));
        if !check
            && run_install(name, &["brew", "upgrade", kind_flag, package], None, None).code != 0
        {
            fail(&format!("{name}: Homebrew update failed"));
            return None;
        }
    }
    Some(spec.into())
}

// mise returns a flat array when a tool argument is supplied, and an object
// keyed by tool name for the complete inventory. Support both documented forms.
fn mise_versions(value: &Value) -> Vec<&Value> {
    match value {
        Value::Array(versions) => versions.iter().collect(),
        Value::Object(tools) => tools
            .values()
            .filter_map(Value::as_array)
            .flatten()
            .collect(),
        _ => Vec::new(),
    }
}

fn mise_update(name: &str, spec: &str, check: bool) -> Option<String> {
    let target = spec.strip_prefix("mise:").unwrap_or(spec);
    let (base, _) = split_spec(target);
    if base.is_empty() || base.starts_with('-') {
        fail(&format!("{name}: invalid mise recipe; update skipped"));
        return None;
    }
    let kit = ctx().home.join(REMOTE_KIT);
    let mise = kit.join("bin/mise");
    if !is_executable(&mise) {
        warn(&format!(
            "{name}: kit's mise is unavailable; update skipped"
        ));
        return None;
    }
    let env = vec![
        ("MISE_DATA_DIR".into(), path_str(&kit.join("mise"))),
        (
            "MISE_GLOBAL_CONFIG_FILE".into(),
            path_str(&kit.join("mise.toml")),
        ),
        ("MISE_CONFIG_DIR".into(), path_str(&kit.join("config/mise"))),
        ("MISE_YES".into(), "1".into()),
        ("MISE_TRUSTED_CONFIG_PATHS".into(), path_str(&kit)),
    ];
    let call = |args: &[&str], capture| {
        let exe = path_str(&mise);
        let mut cmd = vec![exe.as_str()];
        cmd.extend_from_slice(args);
        if capture {
            query_full(&cmd, 30, Some(&kit), Some(&env))
        } else {
            run_install(name, &cmd, Some(&kit), Some(&env))
        }
    };
    say(&format!("checking {name} (mise)"));
    let result = call(&["ls", "--json", "--installed", &base], true);
    let data = serde_json::from_str::<Value>(&result.stdout).ok();
    let versions = data.as_ref().map(mise_versions).unwrap_or_default();
    let active = versions
        .iter()
        .find(|v| v.get("active").and_then(Value::as_bool) == Some(true))
        .copied()
        .or_else(|| (versions.len() == 1).then(|| versions[0]));
    let current = active
        .and_then(|v| v.get("version"))
        .and_then(Value::as_str)
        .filter(|_| result.code == 0);
    let Some(current) = current else {
        warn(&format!(
            "{name}: could not identify its installed mise version; skipped"
        ));
        return None;
    };
    let latest = call(&["latest", &base], true);
    let version = latest.stdout.trim();
    if latest.code != 0 || version.is_empty() {
        fail(&format!(
            "{name}: could not check mise updates; nothing installed"
        ));
        return None;
    }
    let normalized = |v: &str| {
        if spec.starts_with("mise:") {
            format!("mise:{base}@{v}")
        } else {
            format!("{base}@{v}")
        }
    };
    match newer(version, current) {
        Some(false) => {
            say(&format!("{name}: up to date (mise {current})"));
            Some(normalized(current))
        }
        Some(true) => {
            say(&format!("{name}: {current} → {version} (mise)"));
            let target = format!("{base}@{version}");
            if check {
                return Some(normalized(version));
            }
            if call(&["use", "--global", &target], false).code == 0 {
                let result = call(&["ls", "--json", "--installed", &base], true);
                let data = serde_json::from_str::<Value>(&result.stdout).ok();
                let confirmed = result.code == 0
                    && data
                        .as_ref()
                        .map(mise_versions)
                        .unwrap_or_default()
                        .into_iter()
                        .any(|v| {
                            v.get("version").and_then(Value::as_str) == Some(version)
                                && v.get("active").and_then(Value::as_bool) == Some(true)
                                && v.get("installed").and_then(Value::as_bool) == Some(true)
                        });
                if confirmed {
                    return Some(normalized(version));
                }
            }
            fail(&format!("{name}: mise update did not confirm the requested active installed version; recorded recipe preserved"));
            None
        }
        None => {
            warn(&format!(
                "{name}: cannot compare these mise versions; skipped"
            ));
            None
        }
    }
}

/// Update only selected installed tools. No environment reconciliation and no provider fallback.
pub fn update_tools(names: &[String], check: bool) {
    guard_repo();
    let mut pkgs = load_pkgs();
    if let Some(name) = names.iter().find(|n| !pkgs.contains_key(*n)) {
        fail(&format!("{name} is not a tracked package"));
        return;
    }
    let original = pkgs.clone();
    let mut checked: BTreeMap<String, Option<String>> = BTreeMap::new();
    for name in names {
        let p = &pkgs[name];
        let bin = package_bin_for(name, p, ctx().platform);
        let cargo = cargo_install(bin);
        let spec = pkg_spec(p).unwrap_or("").to_string();
        let identity = cargo
            .as_ref()
            .map(|c| format!("cargo:{}:{}", c.root.display(), c.name))
            .unwrap_or_else(|| bin.to_string());
        let next = if let Some(next) = checked.get(&identity) {
            say(&format!(
                "{name}: same installed tool as another selected entry; checked once"
            ));
            next.clone()
        } else {
            let next = if let Some(cargo) = &cargo {
                cargo_update(name, bin, cargo, check)
            } else if let Some(why) = crate::recipe::problem(&spec) {
                fail(&format!("{name}: unsupported recipe: {why}; use kit add with an explicit mise/Cargo recipe"));
                None
            } else if !pkg_available(name, p) {
                warn(&format!(
                    "{name}: not installed here; skipped (kit sync installs missing tools)"
                ));
                None
            } else if spec.starts_with("brew:") || spec.starts_with("cask:") {
                brew_update(name, &spec, check)
            } else if spec.starts_with("builtin:")
                || spec.starts_with("system:")
                || spec.starts_with("cmd:")
            {
                warn(&format!("{name}: no supported installed-provider version check; skipped without reinstalling"));
                None
            } else if which(bin).is_some_and(|path| path.starts_with(ctx().home.join(REMOTE_KIT))) {
                mise_update(name, &spec, check)
            } else {
                warn(&format!(
                    "{name}: installed outside kit's managed tools; update skipped"
                ));
                None
            };
            checked.insert(identity, next.clone());
            next
        };
        if let Some(next) = next {
            if !check {
                if next != spec {
                    say(&format!("{name}: recording installed provider {next}"));
                }
                let other = if ctx().is_mac { "linux" } else { "mac" };
                if next.starts_with("cargo:") {
                    let (base, _) = split_spec(&next);
                    if pkgs[name]
                        .get(other)
                        .and_then(Value::as_str)
                        .is_some_and(|other_spec| {
                            split_spec(other_spec).0 == base && other_spec == spec
                        })
                    {
                        pkgs.get_mut(name)
                            .unwrap()
                            .insert(other.into(), next.clone().into());
                    }
                }
                pkgs.get_mut(name)
                    .unwrap()
                    .insert(ctx().platform.into(), next.clone().into());
                crate::pkg::record_install(name, &next);
            }
        }
    }
    if !check && pkgs != original {
        save_pkgs(&pkgs);
        say("provider/version changes recorded; kit status shows what kit sync will publish");
    } else if check {
        say("check only: no packages installed and no recipes changed");
    }
}

#[cfg(test)]
mod tests {
    use super::{mise_versions, newer};
    #[test]
    fn mise_installed_records_accept_single_tool_array_and_inventory_object() {
        let versions = serde_json::json!([{"version":"1.2.3","active":true,"installed":true}]);
        let inventory = serde_json::json!({"aqua:owner/tool":versions.clone()});
        assert_eq!(mise_versions(&versions), mise_versions(&inventory));
        assert_eq!(mise_versions(&versions).len(), 1);
    }
    #[test]
    fn upgrades_require_a_strictly_newer_valid_version() {
        assert_eq!(newer("1.2.3", "1.2.3"), Some(false));
        assert_eq!(newer("1.2.2", "1.2.3"), Some(false));
        assert_eq!(newer("1.2.4", "1.2.3"), Some(true));
        assert_eq!(newer("1.2.3", "1.2.3-beta.1"), Some(true));
        assert_eq!(newer("latest", "1.2.3"), None);
    }
}
