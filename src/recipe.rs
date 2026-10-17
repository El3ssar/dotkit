//! Provider recipes: pinned mise backends, Cargo crates, Homebrew, and Kit bootstraps.
//! Legacy recipes remain readable so they can be inspected, migrated or removed.
use crate::pkg::split_spec;

pub fn backend_supported(spec: &str) -> bool {
    let spec = spec.strip_prefix("mise:").unwrap_or(spec);
    matches!(
        spec.split(':').next(),
        Some(
            "core"
                | "aqua"
                | "github"
                | "gitlab"
                | "forgejo"
                | "packslip"
                | "ubi"
                | "cargo"
                | "npm"
                | "go"
                | "pypi"
        )
    )
}

pub fn problem(spec: &str) -> Option<String> {
    let spec = spec.strip_prefix("mise:").unwrap_or(spec);
    if matches!(
        spec,
        "builtin:fizsh" | "builtin:zsh" | "builtin:nvim" | "builtin:mise"
    ) {
        return None;
    }
    if let Some(("brew" | "cask", name)) = spec.split_once(':') {
        return (!name.is_empty()
            && !name.starts_with('-')
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "_./@+-".contains(c)))
        .then_some(())
        .map_or_else(|| Some("invalid Homebrew package name".into()), |_| None);
    }
    if !backend_supported(spec) {
        return Some("use a mise backend, Cargo, or Homebrew; distro packages, arbitrary plugins and custom install commands are not managed".into());
    }
    let (base, version) = split_spec(spec);
    let Some((_, package)) = base.split_once(':') else {
        return Some("missing backend or package name".into());
    };
    if package.is_empty()
        || package.starts_with('-')
        || package.contains(['\n', '\r', '\t', '\0', ' ', '"', '\''])
    {
        return Some("invalid package name".into());
    }
    let Some(version) = version else {
        return Some(
            "an exact version is required; add the package again to resolve and pin one".into(),
        );
    };
    if !version.chars().any(|c| c.is_ascii_digit())
        || !version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || ".+_-".contains(c))
    {
        return Some(
            "use a concrete release version, not latest, stable, a branch or a version range"
                .into(),
        );
    }
    if (base.starts_with("npm:") || base.starts_with("go:"))
        && semver::Version::parse(version.trim_start_matches('v')).is_err()
    {
        return Some("source packages require a complete release version, for example 1.2.3; mutable distribution tags and branches are not pinned".into());
    }
    if base.starts_with("pypi:") {
        static RELEASE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        let release = RELEASE.get_or_init(|| regex::Regex::new(r"^v?[0-9]+(?:\.[0-9]+)*(?:(?:a|b|rc)[0-9]+)?(?:\.post[0-9]+)?(?:\.dev[0-9]+)?(?:\+[A-Za-z0-9._-]+)?$").unwrap());
        if !release.is_match(&version) {
            return Some("Python packages require a concrete release version; mutable tags and branches are not pinned".into());
        }
    }
    if base.starts_with("cargo:") {
        if !package
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-".contains(c))
        {
            return Some("Cargo recipes must name a crates.io crate; use a pinned release backend for repository assets".into());
        }
        if semver::Version::parse(&version).is_err() {
            return Some("Cargo requires a complete release version, for example 1.2.3".into());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn allowed_sources_are_explicit_and_pinned() {
        for s in [
            "aqua:eza-community/eza@0.23.0",
            "cargo:eza@0.23.0",
            "mise:npm:@scope/tool@1.2.3",
            "builtin:fizsh",
            "pypi:ruff@0.12.3",
            "pypi:example@1.2.0rc1.post2",
            "go:example.com/tool@v1.2.3",
        ] {
            assert!(problem(s).is_none(), "{s}");
        }
        for s in [
            "system:cbonsai",
            "cmd:echo yes",
            "asdf:plugin@1.0",
            "cargo:eza",
            "aqua:owner/tool@latest",
            "core:rust@stable",
            "cargo:eza@^1.0",
            "cargo:eza@1",
            "npm:example@next1",
            "npm:example@1",
            "go:example.com/tool@branch1",
            "pypi:example@latest1",
            "cargo:https://example.com/tool@1.2.3",
        ] {
            assert!(problem(s).is_some(), "{s}");
        }
    }
}
