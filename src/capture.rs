//! Capture installed ownership; an executable name alone never proves its source.
use crate::pkg::{package_bin_for, Pkg};
use crate::util::*;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::fs;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

pub(crate) struct Installed {
    pub recipe: String,
    pub version: String,
    pub binary: String,
    pub identity: String,
    pub provider: &'static str,
    pub repository: Option<String>,
}

fn same_file(a: &Path, b: &Path) -> bool {
    matches!((fs::canonicalize(a), fs::canonicalize(b)), (Ok(a), Ok(b)) if a == b)
}
fn object(path: &Path) -> Option<Value> {
    // Refuse symlinks/FIFOs atomically; a receipt is data, never executable code.
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.len() > 8 * 1024 * 1024 {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(8 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > 8 * 1024 * 1024 {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

fn safe_bin(bin: &str) -> bool {
    !bin.is_empty()
        && !bin.starts_with('-')
        && bin
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_.+-".contains(c))
}

pub(crate) fn detect(
    name: &str,
    entry: &Pkg,
    registry: Option<&Value>,
) -> Result<Installed, String> {
    let mut bins = vec![package_bin_for(name, entry, ctx().platform).to_string()];
    if let Some(values) = registry
        .and_then(|r| r.get("bins"))
        .and_then(Value::as_array)
    {
        bins.extend(
            values
                .iter()
                .filter_map(Value::as_str)
                .filter(|b| safe_bin(b))
                .map(String::from),
        );
    }
    // Kit-owned receipts identify the narrowly bundled packages without guessing.
    for (bin, recipe, marker, version) in [
        (
            "gcloud",
            "builtin:gcloud@589.0.0",
            "opt/gcloud/kit-version",
            "589.0.0",
        ),
        (
            "fizsh",
            "builtin:fizsh",
            "opt/fizsh/source-revision",
            "f047f8a68a6499500bc46d40c412d99f01d2378d",
        ),
    ] {
        let kit = ctx().home.join(REMOTE_KIT);
        if bins.iter().any(|b| b == bin)
            && fs::read_to_string(kit.join(marker))
                .ok()
                .is_some_and(|v| v.trim() == version)
            && which(bin).is_some_and(|p| {
                same_file(&p, &kit.join("bin").join(bin))
                    || same_file(&p, &kit.join("opt/fizsh/bin/fizsh"))
            })
        {
            return Ok(Installed {
                recipe: recipe.into(),
                version: version.into(),
                binary: bin.into(),
                identity: bin.into(),
                provider: "Kit",
                repository: None,
            });
        }
    }
    // Cargo also records binaries whose command differs from their crate name.
    let mut roots = vec![
        ctx().home.join(".cargo"),
        ctx().home.join(REMOTE_KIT).join("cargo"),
    ];
    if let Some(root) = std::env::var_os("CARGO_HOME") {
        roots.insert(0, root.into());
    }
    let mut seen = BTreeSet::new();
    for root in roots {
        if !seen.insert(root.clone()) {
            continue;
        }
        let Some(meta) = object(&root.join(".crates2.json")) else {
            continue;
        };
        let Some(installs) = meta.get("installs").and_then(Value::as_object) else {
            continue;
        };
        for (key, info) in installs {
            let mut fields = key.splitn(3, ' ');
            let (Some(crate_name), Some(version), Some(source)) =
                (fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            let Some(installed_bins) = info.get("bins").and_then(Value::as_array) else {
                continue;
            };
            for bin in installed_bins
                .iter()
                .filter_map(Value::as_str)
                .filter(|b| safe_bin(b))
            {
                if crate_name != name && !bins.iter().any(|b| b == bin) {
                    continue;
                }
                let Some(active) = which(bin) else {
                    continue;
                };
                if !is_executable(&root.join("bin").join(bin))
                    || !same_file(&active, &root.join("bin").join(bin))
                {
                    continue;
                }
                if !crate::updates::crates_io(source.trim_matches(['(', ')'])) {
                    return Err(format!("{name} is installed through Cargo from a Git/private registry source that Kit cannot reproduce safely; nothing recorded"));
                }
                if info
                    .get("features")
                    .and_then(Value::as_array)
                    .is_some_and(|a| !a.is_empty())
                    || info.get("all_features").and_then(Value::as_bool) == Some(true)
                    || info.get("no_default_features").and_then(Value::as_bool) == Some(true)
                {
                    return Err(format!("{name} uses custom Cargo build features that Kit cannot yet reproduce; nothing recorded"));
                }
                if info
                    .get("profile")
                    .and_then(Value::as_str)
                    .is_some_and(|p| p != "release")
                {
                    return Err(format!("{name} uses a custom Cargo build profile that Kit cannot yet reproduce; nothing recorded"));
                }
                let recipe = format!("cargo:{crate_name}@{version}");
                if crate::recipe::problem(&recipe).is_some() {
                    return Err(format!(
                        "{name}: invalid Cargo ownership receipt; nothing recorded"
                    ));
                }
                return Ok(Installed {
                    recipe,
                    version: version.into(),
                    binary: bin.into(),
                    identity: crate_name.into(),
                    provider: "Cargo",
                    repository: None,
                });
            }
        }
    }
    // Homebrew's canonical Cellar path plus installation receipt establish ownership,
    // unlike `brew info NAME`, which can describe packages that are not installed.
    if let Some(prefix) = crate::pkg::brew_prefix() {
        for bin in &bins {
            let Some(active) = which(bin).and_then(|p| fs::canonicalize(p).ok()) else {
                continue;
            };
            let Ok(relative) = active.strip_prefix(prefix.join("Cellar")) else {
                continue;
            };
            let mut parts = relative.components();
            let (Some(formula), Some(version)) = (parts.next(), parts.next()) else {
                continue;
            };
            let formula = formula.as_os_str().to_string_lossy().to_string();
            let version = version.as_os_str().to_string_lossy().to_string();
            let receipt = prefix
                .join("Cellar")
                .join(&formula)
                .join(&version)
                .join("INSTALL_RECEIPT.json");
            let Some(receipt) = object(&receipt) else {
                continue;
            };
            if receipt
                .pointer("/source/spec")
                .and_then(Value::as_str)
                .is_some_and(|s| s != "stable")
                || receipt
                    .get("used_options")
                    .and_then(Value::as_array)
                    .is_some_and(|a| !a.is_empty())
            {
                return Err(format!("{name}: custom/HEAD Homebrew build options cannot yet be reproduced; nothing recorded"));
            }
            let Some(tap) = receipt.pointer("/source/tap").and_then(Value::as_str) else {
                continue;
            };
            let full = if tap == "homebrew/core" {
                formula.clone()
            } else {
                format!("{tap}/{formula}")
            };
            let recipe = format!("brew:{full}");
            if crate::recipe::problem(&recipe).is_some() {
                continue;
            }
            let repository = brew_repository(
                &prefix
                    .join("Cellar")
                    .join(&formula)
                    .join(&version)
                    .join(".brew")
                    .join(format!("{formula}.rb")),
            );
            return Ok(Installed {
                recipe,
                version,
                binary: bin.clone(),
                identity: formula,
                provider: "Homebrew",
                repository,
            });
        }
        // CLI-bearing casks keep installation receipts too; inspect installed metadata
        // only and require its declared binary to match the executable actually selected.
        if ctx().is_mac && have("brew") {
            let env = vec![
                ("HOMEBREW_NO_AUTO_UPDATE".into(), "1".into()),
                ("HOMEBREW_NO_ANALYTICS".into(), "1".into()),
            ];
            let r = query_full(
                &["brew", "info", "--json=v2", "--cask", name],
                15,
                None,
                Some(&env),
            );
            if r.code == 0 {
                if let Ok(meta) = serde_json::from_str::<Value>(&r.stdout) {
                    if let Some(casks) = meta.get("casks").and_then(Value::as_array) {
                        for cask in casks {
                            let (Some(token), Some(version)) = (
                                cask.get("token").and_then(Value::as_str),
                                cask.get("installed").and_then(Value::as_str),
                            ) else {
                                continue;
                            };
                            if version.is_empty()
                                || !prefix.join("Caskroom").join(token).join(version).is_dir()
                            {
                                continue;
                            }
                            let Some(artifacts) = cask.get("artifacts").and_then(Value::as_array)
                            else {
                                continue;
                            };
                            for artifact in artifacts {
                                let Some(paths) = artifact.get("binary").and_then(Value::as_array)
                                else {
                                    continue;
                                };
                                let Some(path) = paths.first().and_then(Value::as_str) else {
                                    continue;
                                };
                                for bin in &bins {
                                    if which(bin).is_some_and(|p| same_file(&p, Path::new(path))) {
                                        let recipe = format!("cask:{token}");
                                        if crate::recipe::problem(&recipe).is_none() {
                                            return Ok(Installed {
                                                recipe,
                                                version: version.into(),
                                                binary: bin.clone(),
                                                identity: token.into(),
                                                provider: "Homebrew cask",
                                                repository: None,
                                            });
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    if let Some(found) = mise_installed(name, &bins)? {
        return Ok(found);
    }
    Err(format!("{name}: could not prove an installed Cargo, Homebrew or mise source; nothing recorded. Install it first, or choose explicit --mac/--linux recipes (owner/repo sources are also explicit)."))
}

fn mise_installed(name: &str, bins: &[String]) -> Result<Option<Installed>, String> {
    type ProviderEnv = Vec<(String, String)>;
    let mut contexts: Vec<(PathBuf, ProviderEnv)> = vec![];
    if let Some(mise) = which("mise") {
        contexts.push((mise, vec![]));
    }
    let kit = ctx().home.join(REMOTE_KIT);
    if is_executable(&kit.join("bin/mise")) {
        contexts.push((
            kit.join("bin/mise"),
            vec![
                ("MISE_DATA_DIR".into(), path_str(&kit.join("mise"))),
                (
                    "MISE_GLOBAL_CONFIG_FILE".into(),
                    path_str(&kit.join("mise.toml")),
                ),
                ("MISE_CONFIG_DIR".into(), path_str(&kit.join("config/mise"))),
                ("MISE_TRUSTED_CONFIG_PATHS".into(), path_str(&kit)),
            ],
        ));
    }
    for (exe, mut env) in contexts {
        env.extend([
            ("MISE_OFFLINE".into(), "1".into()),
            ("MISE_AUTO_INSTALL".into(), "0".into()),
        ]);
        let call = |args: &[&str]| {
            let mut cmd = vec![path_str(&exe)];
            cmd.extend(args.iter().map(|s| s.to_string()));
            query_full(&cmd, 10, None, Some(&env))
        };
        let r = call(&["ls", "--installed", "--json", "--offline"]);
        if r.code != 0 {
            continue;
        }
        let Ok(Value::Object(tools)) = serde_json::from_str::<Value>(&r.stdout) else {
            continue;
        };
        for (key, versions) in tools {
            let Some(versions) = versions.as_array() else {
                continue;
            };
            for v in versions {
                if v.get("installed").and_then(Value::as_bool) != Some(true)
                    || v.get("active").and_then(Value::as_bool) != Some(true)
                {
                    continue;
                }
                let (Some(version), Some(root)) = (
                    v.get("version").and_then(Value::as_str),
                    v.get("install_path").and_then(Value::as_str),
                ) else {
                    continue;
                };
                let Some(root) = fs::canonicalize(root).ok() else {
                    continue;
                };
                for bin in bins {
                    let Some(active) = which(bin) else {
                        continue;
                    };
                    let direct = fs::canonicalize(&active)
                        .ok()
                        .is_some_and(|p| p.starts_with(&root));
                    let data = env
                        .iter()
                        .find(|(k, _)| k == "MISE_DATA_DIR")
                        .map(|(_, v)| PathBuf::from(v))
                        .or_else(|| std::env::var_os("MISE_DATA_DIR").map(PathBuf::from))
                        .unwrap_or_else(|| {
                            std::env::var_os("XDG_DATA_HOME")
                                .map(PathBuf::from)
                                .unwrap_or_else(|| ctx().home.join(".local/share"))
                                .join("mise")
                        });
                    let shim = active
                        .parent()
                        .is_some_and(|p| same_file(p, &data.join("shims")));
                    if !direct && !shim {
                        continue;
                    }
                    let resolved = call(&["which", bin]);
                    if resolved.code != 0
                        || !fs::canonicalize(resolved.stdout.trim())
                            .ok()
                            .is_some_and(|p| p.starts_with(&root) && is_executable(&p))
                    {
                        continue;
                    }
                    let info = call(&["tool", "--json", &key]);
                    if info.code != 0 {
                        continue;
                    }
                    let Ok(info) = serde_json::from_str::<Value>(&info.stdout) else {
                        continue;
                    };
                    let Some(backend) = info.get("backend").and_then(Value::as_str) else {
                        continue;
                    };
                    let Some(options) = info.get("tool_options") else {
                        return Err(format!(
                            "{name}: mise did not report its install options; nothing recorded"
                        ));
                    };
                    if meaningful_option(options) {
                        return Err(format!("{name}: custom mise install options cannot yet be reproduced; nothing recorded"));
                    }
                    let recipe = format!("{backend}@{version}");
                    if crate::recipe::problem(&recipe).is_some() {
                        return Err(format!(
                            "{name}: its installed mise backend is not supported; nothing recorded"
                        ));
                    }
                    return Ok(Some(Installed {
                        recipe,
                        version: version.into(),
                        binary: bin.clone(),
                        identity: key.clone(),
                        provider: "mise",
                        repository: None,
                    }));
                }
            }
        }
    }
    Ok(None)
}

fn meaningful_option(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => false,
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => o.values().any(meaningful_option),
        _ => true,
    }
}

pub(crate) fn receipt(found: &Installed) -> Value {
    json!({"source":found.recipe,"version":found.version,"binary":found.binary,"provider":found.provider})
}

/// Only recognized upstream repository URLs become release backends, never arbitrary commands.
pub(crate) fn repository_backend(url: &str) -> Option<String> {
    let (kind, path) = url
        .strip_prefix("https://github.com/")
        .map(|p| ("github", p))
        .or_else(|| {
            url.strip_prefix("https://gitlab.com/")
                .map(|p| ("gitlab", p))
        })?;
    let mut parts = path.trim_end_matches('/').split('/');
    let owner = parts.next()?;
    let repo = parts.next()?.trim_end_matches(".git");
    if owner.is_empty()
        || repo.is_empty()
        || [owner, repo].iter().any(|s| {
            !s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
        })
    {
        return None;
    }
    Some(format!("{kind}:{owner}/{repo}"))
}
fn brew_repository(path: &Path) -> Option<String> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut text = String::new();
    file.take(1024 * 1024).read_to_string(&mut text).ok()?;
    let pattern = regex::Regex::new(
        r#"(?m)^\s*(?:homepage|url)\s+["'](https://(?:github|gitlab)\.com/[^"' ]+)"#,
    )
    .unwrap();
    let repository = pattern
        .captures_iter(&text)
        .find_map(|m| repository_backend(m.get(1)?.as_str()));
    repository
}
pub(crate) fn cargo_repository(found: &Installed) -> Option<String> {
    let url = format!(
        "https://crates.io/api/v1/crates/{}/{}",
        found.identity, found.version
    );
    let r = if have("curl") {
        query(
            &[
                "curl",
                "-fsSL",
                "--connect-timeout",
                "5",
                "--max-time",
                "15",
                "-A",
                "dotkit installed-package capture",
                &url,
            ],
            20,
        )
    } else {
        query(&["wget", "-qO-", "-T", "15", &url], 20)
    };
    if r.code != 0 {
        return None;
    }
    let data: Value = serde_json::from_str(&r.stdout).ok()?;
    let release = data.get("version")?;
    if release.get("crate")?.as_str()? != found.identity
        || release.get("num")?.as_str()? != found.version
    {
        return None;
    }
    if !release
        .get("bin_names")?
        .as_array()?
        .iter()
        .any(|v| v.as_str() == Some(&found.binary))
    {
        return None;
    }
    repository_backend(release.get("repository")?.as_str()?)
}
