//! Provider-backed platform checks. Temporary provider lockfiles are removed;
//! there is no Kit discovery database or background package scan.
use crate::pkg::{load_pkgs, s, split_spec, Pkg};
use crate::util::*;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Result<Self, String> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        fs::create_dir_all(&ctx().state).map_err(|e| e.to_string())?;
        let p = ctx().state.join(format!(
            "package-check-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&p).map_err(|e| e.to_string())?;
        Ok(Self(p))
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub fn summary(p: &Pkg) -> &'static str {
    let recipes: Vec<_> = [s(p, "mac"), s(p, "linux")].into_iter().flatten().collect();
    if recipes.iter().any(|s| crate::recipe::problem(s).is_some()) {
        "unsupported recipe"
    } else if s(p, "mac").is_none() {
        "Linux only"
    } else if s(p, "linux").is_none() {
        "macOS only"
    } else if recipes.iter().any(|s| is_source(s)) {
        "source build; destination testing needed"
    } else {
        "two OS recipes; use check to verify release coverage"
    }
}
fn is_source(spec: &str) -> bool {
    let spec = spec.strip_prefix("mise:").unwrap_or(spec);
    ["cargo:", "npm:", "pypi:", "go:"]
        .iter()
        .any(|p| spec.starts_with(p))
}

/// Strictly require release URL + checksum entries for every requested target.
fn lock_coverage(text: &str, base: &str, version: &str, targets: &[String]) -> Result<(), String> {
    let lock: toml::Value =
        toml::from_str(text).map_err(|e| format!("invalid provider lockfile: {e}"))?;
    let entries = lock
        .get("tools")
        .and_then(|t| t.get(base))
        .and_then(toml::Value::as_array)
        .ok_or("provider omitted the requested tool")?;
    let entry = entries
        .iter()
        .find(|e| {
            e.get("version")
                .and_then(toml::Value::as_str)
                .is_some_and(|v| v.trim_start_matches('v') == version.trim_start_matches('v'))
        })
        .ok_or("provider omitted the requested version")?;
    for target in targets {
        let p = entry
            .get(format!("platforms.{target}"))
            .or_else(|| entry.get("platforms").and_then(|p| p.get(target)));
        let url = p
            .and_then(|p| p.get("url"))
            .and_then(toml::Value::as_str)
            .unwrap_or("");
        let checksum = p
            .and_then(|p| p.get("checksum"))
            .and_then(toml::Value::as_str)
            .unwrap_or("");
        let valid_checksum = checksum.split_once(':').is_some_and(|(kind, hash)| {
            let length = match kind {
                "sha256" => 64,
                "sha512" => 128,
                _ => 0,
            };
            length > 0 && hash.len() == length && hash.chars().all(|c| c.is_ascii_hexdigit())
        });
        if !url.starts_with("https://") || !valid_checksum {
            return Err(format!("no verified release URL/checksum for {target}"));
        }
    }
    Ok(())
}

fn rust_coverage(text: &str, version: &str, targets: &[String]) -> Result<(), String> {
    let manifest: toml::Value =
        toml::from_str(text).map_err(|_| "invalid official Rust manifest")?;
    let rust = manifest
        .get("pkg")
        .and_then(|p| p.get("rust"))
        .ok_or("Rust manifest omitted rust")?;
    if rust
        .get("version")
        .and_then(toml::Value::as_str)
        .and_then(|v| v.split_whitespace().next())
        != Some(version)
    {
        return Err("Rust manifest version mismatch".into());
    }
    for platform in targets {
        let target = match platform.as_str() {
            "macos-arm64" => "aarch64-apple-darwin",
            "macos-x64" => "x86_64-apple-darwin",
            "linux-arm64" => "aarch64-unknown-linux-gnu",
            "linux-x64" => "x86_64-unknown-linux-gnu",
            _ => return Err("unsupported Rust target".into()),
        };
        let release = rust
            .get("target")
            .and_then(|t| t.get(target))
            .ok_or_else(|| format!("Rust has no {target} release"))?;
        let hash = release
            .get("xz_hash")
            .and_then(toml::Value::as_str)
            .unwrap_or("");
        let url = release
            .get("xz_url")
            .and_then(toml::Value::as_str)
            .unwrap_or("");
        if release.get("available").and_then(toml::Value::as_bool) != Some(true)
            || !url.starts_with("https://static.rust-lang.org/dist/")
            || hash.len() != 64
            || !hash.chars().all(|c| c.is_ascii_hexdigit())
        {
            return Err(format!("no official Rust URL/checksum for {target}"));
        }
    }
    Ok(())
}

fn check_mise(name: &str, spec: &str, targets: &[String]) -> Result<(), String> {
    crate::remote::ensure_mise();
    let scratch = Scratch::new()?;
    let spec = spec.strip_prefix("mise:").unwrap_or(spec);
    let (base, version) = split_spec(spec);
    let version = version.ok_or("an exact version is required")?;
    // Rust's core backend installs through rustup and emits no mise lock entries.
    // Read the official release manifest instead of rejecting a supported runtime.
    if base == "core:rust" {
        let url = format!("https://static.rust-lang.org/dist/channel-rust-{version}.toml");
        let r = if have("curl") {
            query(
                &[
                    "curl",
                    "-fsSL",
                    "--connect-timeout",
                    "10",
                    "--max-time",
                    "25",
                    &url,
                ],
                30,
            )
        } else {
            query(&["wget", "-qO-", "-T", "25", &url], 30)
        };
        if r.code != 0 {
            return Err("could not read the official pinned Rust release manifest".into());
        }
        return rust_coverage(&r.stdout, &version, targets);
    }
    let config = format!(
        "[tools]\n{} = {}\n",
        Value::from(base.clone()),
        Value::from(version.clone())
    );
    fs::write(scratch.0.join("mise.toml"), config).map_err(|e| e.to_string())?;
    let env = vec![
        (
            "MISE_GLOBAL_CONFIG_FILE".into(),
            path_str(&scratch.0.join("empty.toml")),
        ),
        ("MISE_TRUSTED_CONFIG_PATHS".into(), path_str(&scratch.0)),
        ("MISE_YES".into(), "1".into()),
        ("MISE_HTTP_TIMEOUT".into(), "30s".into()),
        ("KIT_INSTALL_TIMEOUT".into(), "120".into()),
    ];
    let platforms = targets.join(",");
    let result = run_install(
        &format!("check-{name}"),
        &["mise", "lock", "--platform", &platforms],
        Some(&scratch.0),
        Some(&env),
    );
    if result.code != 0 {
        return Err(
            "mise could not resolve the requested target artifacts (see provider output)".into(),
        );
    }
    let text = fs::read_to_string(scratch.0.join("mise.lock"))
        .map_err(|_| "mise did not produce a lockfile; platform support is unverified")?;
    lock_coverage(&text, &base, &version, targets)
}

fn brew_coverage(
    info: &Value,
    package: &str,
    targets: &[String],
    cask: bool,
) -> Result<(), String> {
    if cask {
        if targets.iter().any(|t| t.starts_with("linux-")) {
            return Err("Homebrew casks are macOS-only".into());
        }
        if info
            .get("casks")
            .and_then(Value::as_array)
            .is_some_and(|a| {
                a.iter()
                    .any(|c| c.get("token").and_then(Value::as_str) == Some(package))
            })
        {
            return Ok(());
        }
        return Err("Homebrew did not confirm that cask".into());
    }
    let formula = info
        .get("formulae")
        .and_then(Value::as_array)
        .and_then(|a| {
            a.iter().find(|f| {
                f.get("name").and_then(Value::as_str)
                    == Some(package.rsplit('/').next().unwrap_or(package))
                    || f.get("full_name").and_then(Value::as_str) == Some(package)
            })
        })
        .ok_or("Homebrew did not confirm that formula")?;
    let files = formula
        .pointer("/bottle/stable/files")
        .and_then(Value::as_object)
        .ok_or("no published Homebrew bottles; source-only builds are not verified")?;
    for target in targets {
        let matched = files.iter().any(|(tag, v)| {
            let matches = match target.as_str() {
                "linux-x64" => tag == "x86_64_linux",
                "linux-arm64" => tag == "arm64_linux" || tag == "aarch64_linux",
                "macos-arm64" => tag.starts_with("arm64_") && !tag.ends_with("linux"),
                "macos-x64" => !tag.starts_with("arm64_") && !tag.ends_with("linux"),
                _ => false,
            };
            matches
                && v.get("url")
                    .and_then(Value::as_str)
                    .is_some_and(|s| s.starts_with("https://"))
                && v.get("sha256")
                    .and_then(Value::as_str)
                    .is_some_and(|s| s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit()))
        });
        if !matched {
            return Err(format!("no published Homebrew bottle for {target}"));
        }
    }
    Ok(())
}
fn check_brew(spec: &str, targets: &[String]) -> Result<(), String> {
    if !have("brew") {
        return Err("Homebrew is not installed here; cannot verify its platform metadata".into());
    }
    let (kind, package) = spec.split_once(':').ok_or("invalid Homebrew recipe")?;
    let r = query(&["brew", "info", "--json=v2", package], 30);
    if r.code != 0 {
        return Err("Homebrew metadata lookup failed".into());
    }
    let info: Value = serde_json::from_str(&r.stdout).map_err(|_| "invalid Homebrew metadata")?;
    brew_coverage(&info, package, targets, kind == "cask")
}

/// Returns false only for unsupported or missing binary artifacts. Source-based
/// installs are identified honestly; this does not claim they build on a target.
pub fn check_entry(name: &str, p: &Pkg, strict: bool) -> bool {
    let mut grouped: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut ok = true;
    for (os, targets) in [
        ("mac", ["macos-arm64", "macos-x64"]),
        ("linux", ["linux-x64", "linux-arm64"]),
    ] {
        let Some(spec) = s(p, os) else {
            if strict {
                warn(&format!(
                    "{name}: no {os} recipe; this package is not declared portable"
                ));
                ok = false;
            }
            continue;
        };
        if let Some(why) = crate::recipe::problem(spec) {
            fail(&format!("{name} ({os}): {why}"));
            ok = false;
            continue;
        }
        if spec.starts_with("cask:") && os == "linux" {
            fail(&format!(
                "{name} (linux): Homebrew casks are macOS-only; choose a CLI recipe"
            ));
            ok = false;
            continue;
        }
        if spec.starts_with("cask:") {
            warn(&format!(
                "{name} ({os}): macOS application; not a portable CLI recipe"
            ));
            continue;
        }
        if is_source(spec) {
            if strict {
                ok = false;
            }
            warn(&format!("{name} ({os}): source/package build; platform compatibility is not verified without building on that destination"));
            continue;
        }
        if spec.starts_with("builtin:") {
            note(&format!("{name} ({os}): Kit-owned bootstrap"));
            continue;
        }
        grouped
            .entry(spec.into())
            .or_default()
            .extend(targets.into_iter().map(String::from));
    }
    for (spec, targets) in grouped {
        say(&format!(
            "checking {name} release coverage: {}",
            targets.join(", ")
        ));
        let result = if spec.starts_with("brew:") || spec.starts_with("cask:") {
            check_brew(&spec, &targets)
        } else {
            check_mise(name, &spec, &targets)
        };
        match result {
            Ok(()) => say(&format!(
                "{name}: provider publishes artifacts for {}",
                targets.join(", ")
            )),
            Err(e) => {
                fail(&format!("{name}: {e}"));
                ok = false;
            }
        }
    }
    ok
}
pub fn command(names: &[String]) {
    let pkgs = load_pkgs();
    if let Some(name) = names.iter().find(|n| !pkgs.contains_key(*n)) {
        fail(&format!("{name} is not tracked"));
        return;
    }
    for (name, p) in pkgs
        .iter()
        .filter(|(n, _)| names.is_empty() || names.contains(n))
    {
        if !check_entry(name, p, true) {
            set_exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn official_rust_manifest_requires_pinned_available_targets_and_hashes() {
        let text = format!("[pkg.rust]\nversion='1.99.0 (release)'\n[pkg.rust.target.aarch64-apple-darwin]\navailable=true\nxz_url='https://static.rust-lang.org/dist/rust.tar.xz'\nxz_hash='{}'\n", "a".repeat(64));
        let target = ["macos-arm64".into()];
        assert!(rust_coverage(&text, "1.99.0", &target).is_ok());
        assert!(rust_coverage(&text, "1.99.1", &target).is_err());
        assert!(rust_coverage(
            &text.replace("available=true", "available=false"),
            "1.99.0",
            &target
        )
        .is_err());
        assert!(rust_coverage(
            &text.replace("static.rust-lang.org", "evil.example"),
            "1.99.0",
            &target
        )
        .is_err());
        assert!(rust_coverage(&text, "1.99.0", &["linux-x64".into()]).is_err());
    }
    #[test]
    fn require_every_target_and_checksum() {
        let hash = "a".repeat(64);
        let text=format!("[[tools.\"aqua:o/tool\"]]\nversion=\"1.0.0\"\n[tools.\"aqua:o/tool\".\"platforms.linux-x64\"]\nurl=\"https://example.com/file\"\nchecksum=\"sha256:{hash}\"\n");
        assert!(lock_coverage(&text, "aqua:o/tool", "1.0.0", &["linux-x64".into()]).is_ok());
        assert!(lock_coverage(
            &text,
            "aqua:o/tool",
            "1.0.0",
            &["linux-x64".into(), "macos-arm64".into()]
        )
        .is_err());
        assert!(lock_coverage(
            &text.replace("sha256:", "invalid:"),
            "aqua:o/tool",
            "1.0.0",
            &["linux-x64".into()]
        )
        .is_err());
    }
    #[test]
    fn brew_formulas_and_casks_have_different_coverage() {
        let info = serde_json::json!({"formulae":[{"name":"tool","bottle":{"stable":{"files":{"arm64_sequoia":{"url":"https://example.com/mac","sha256":"a".repeat(64)},"x86_64_linux":{"url":"https://example.com/linux","sha256":"b".repeat(64)}}}}}],"casks":[{"token":"app"}]});
        assert!(brew_coverage(
            &info,
            "tool",
            &["macos-arm64".into(), "linux-x64".into()],
            false
        )
        .is_ok());
        assert!(brew_coverage(&info, "tool", &["linux-arm64".into()], false).is_err());
        assert!(brew_coverage(&info, "app", &["linux-x64".into()], true).is_err());
    }
}
