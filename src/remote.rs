//! Servers (`kit push`, `kit remote`), the Linux shell environment, and `kit doctor`.
use crate::cli::Args;
use crate::core::*;
use crate::pkg::{s, split_spec};
use crate::util::*;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

/// The scripts that run on servers, built into kit.
const REMOTE_FILES: &[(&str, &str)] = &[
    (
        "install-gcloud.sh",
        include_str!("../assets/remote/install-gcloud.sh"),
    ),
    (
        "clone-external.sh",
        include_str!("../assets/remote/clone-external.sh"),
    ),
    (
        "run-install.sh",
        include_str!("../assets/remote/run-install.sh"),
    ),
    (
        "bootstrap.sh",
        include_str!("../assets/remote/bootstrap.sh"),
    ),
    ("deploy.sh", include_str!("../assets/remote/deploy.sh")),
    (
        "install-fizsh.sh",
        include_str!("../assets/remote/install-fizsh.sh"),
    ),
    ("setup.sh", include_str!("../assets/remote/setup.sh")),
    ("hook.sh", include_str!("../assets/remote/hook.sh")),
    ("kit-env", include_str!("../assets/remote/kit-env")),
    ("kit-shell", include_str!("../assets/remote/kit-shell")),
];
const RC_FILES: &[&str] = &[
    ".zshenv",
    ".bashrc",
    ".bash_profile",
    ".bash_login",
    ".profile",
    ".zshrc",
    ".zprofile",
    ".fizsh/.zshrc",
];
const KITTY: &str = "/Applications/kitty.app/Contents/Resources/kitty";

fn write_file(p: &Path, data: &[u8], mode: u32) {
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).unwrap_or_else(|e| die(&format!("cannot build payload: {e}")));
    }
    write_atomic(p, data, mode)
        .unwrap_or_else(|e| die(&format!("cannot build payload {}: {e}", p.display())));
}

/// Everything setup.sh needs, for a server (mode=remote) or this Linux machine (mode=local).
pub fn build_payload(dest: &Path, mode: &str) -> String {
    build_payload_for(dest, mode, "linux")
}

fn build_payload_for(dest: &Path, mode: &str, platform: &str) -> String {
    if dest.exists() {
        fs::remove_dir_all(dest).unwrap_or_else(|e| die(&format!("cannot clear payload: {e}")));
    }
    for sub in ["remote", "home", "commands", "share"] {
        fs::create_dir_all(dest.join(sub))
            .unwrap_or_else(|e| die(&format!("cannot build payload: {e}")));
    }
    for (name, text) in REMOTE_FILES {
        write_file(&dest.join("remote").join(name), text.as_bytes(), 0o755);
    }
    write_file(&dest.join("mode"), format!("{mode}\n").as_bytes(), 0o644);
    let tracked = managed(None);
    let mut home_list = Vec::new();
    if mode == "remote" {
        let ctxs = ["all", platform, "remote"];
        for rel in &tracked {
            validate_rel(rel).unwrap_or_else(|e| die(&e));
            if ignored(rel, &ctxs) {
                continue;
            }
            let p = dest.join("home").join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap_or_else(|e| die(&e.to_string()));
            match source_entry(rel, &ctxs) {
                Ok(Some(Entry::Link(t))) => {
                    std::os::unix::fs::symlink(t, &p).unwrap_or_else(|e| die(&e.to_string()))
                }
                Ok(Some(Entry::File(b, m))) => write_file(
                    &p,
                    &b,
                    if private(rel) {
                        if m & 0o111 != 0 {
                            0o700
                        } else {
                            0o600
                        }
                    } else if m & 0o111 != 0 {
                        0o755
                    } else {
                        0o644
                    },
                ),
                _ => die(&format!("cannot read payload file ~/{rel}")),
            }
            home_list.extend_from_slice(rel.as_bytes());
            home_list.push(0);
        }
    }
    write_file(&dest.join("home.list"), &home_list, 0o644);
    // rc files kit tracks on this machine: the login hook never goes into those
    let rc: String = tracked
        .iter()
        .filter(|r| mode == "local" && RC_FILES.contains(&r.as_str()))
        .map(|r| format!("{r}\n"))
        .collect();
    write_file(&dest.join("managed-rc.list"), rc.as_bytes(), 0o644);
    let mut tools = std::collections::BTreeMap::new();
    let mut plist = String::new();
    let mut seen_bins = std::collections::BTreeSet::new();
    for (n, p) in crate::pkg::all_packages_for(platform) {
        if mode != "local" && !p.get("remote").and_then(Value::as_bool).unwrap_or(true) {
            continue;
        }
        let recipe = s(&p, platform);
        if platform == "linux"
            && recipe.is_none()
            && s(&p, "mac").is_some_and(|s| s.starts_with("cask:"))
        {
            note(&format!("{n}: Mac application, skipped on Linux"));
            continue;
        }
        let bin = crate::pkg::package_bin_for(&n, &p, platform);
        if !seen_bins.insert(bin.to_string()) && recipe.is_none() {
            continue;
        }
        let mut spec = recipe.unwrap_or("missing:recipe").to_string();
        if let Some(rest) = spec.strip_prefix("mise:") {
            spec = rest.into();
        }
        if let Some(why) = crate::recipe::problem(&spec).filter(|_| recipe.is_some()) {
            warn(&format!("{n}: blocked recipe {spec:?}: {why}"));
            spec = "blocked:unsupported-recipe".into();
        } else if spec == "builtin:nvim" {
            tools.insert("aqua:neovim/neovim".into(), "0.12.6".into());
        } else if crate::pkg::is_mise_spec(&spec) {
            let (base, ver) = split_spec(&spec);
            let ver = ver.unwrap_or_else(|| "latest".into());
            spec = format!("{base}@{ver}");
            tools.insert(base, ver);
        }
        let fb = if platform == "linux" {
            s(&p, "linux_fallback")
                .filter(|s| {
                    crate::recipe::problem(s).is_none() && crate::recipe::backend_supported(s)
                })
                .unwrap_or("-")
        } else {
            "-"
        };
        plist += &format!("{n}\t{bin}\t{spec}\t{fb}\n");
    }
    for (key, _) in tools.clone() {
        let dep = if key.starts_with("cargo:") {
            Some(("core:rust", "stable"))
        } else if key.starts_with("npm:") {
            Some(("core:node", "lts"))
        } else if key.starts_with("go:") {
            Some(("core:go", "latest"))
        } else if key.starts_with("pypi:") {
            Some(("aqua:astral-sh/uv", "latest"))
        } else {
            None
        };
        if let Some((name, version)) = dep {
            tools.entry(name.into()).or_insert_with(|| version.into());
        }
    }
    let toml = "# generated by kit\n[tools]\n".to_string()
        + &tools
            .iter()
            .map(|(k, v)| format!("{} = {}\n", Value::from(k.clone()), Value::from(v.clone())))
            .collect::<String>();
    write_file(&dest.join("mise.toml"), toml.as_bytes(), 0o644);
    write_file(&dest.join("packages.list"), plist.as_bytes(), 0o644);
    let mut ext = String::new();
    for (rel, spec) in load_obj(&ctx().externals) {
        validate_rel(&rel).unwrap_or_else(|e| die(&format!("externals.json: {e}")));
        if never_track(&rel) {
            die("externals.json cannot target kit's own folders");
        }
        if ignored(&rel, &["all", platform, "remote"]) && mode == "remote" {
            continue;
        }
        let url = spec.get("git").and_then(Value::as_str).unwrap_or("");
        let reference = spec.get("ref").and_then(Value::as_str).unwrap_or("");
        if url.is_empty()
            || url.starts_with('-')
            || reference.starts_with('-')
            || url.chars().any(char::is_control)
            || reference.chars().any(char::is_control)
        {
            die("invalid external repository URL/ref");
        }
        ext += &format!("{rel}\t{url}\t{reference}\n");
    }
    write_file(&dest.join("externals.list"), ext.as_bytes(), 0o644);
    let kitty = PathBuf::from(KITTY);
    if kitty.join("shell-integration").is_dir() {
        let _ = copy_dir(
            &kitty.join("shell-integration"),
            &dest.join("share/kitty/shell-integration"),
        );
    }
    let ti = kitty.join("terminfo/78/xterm-kitty");
    if ti.is_file() {
        let _ = fs::create_dir_all(dest.join("share/terminfo/x"));
        let _ = fs::copy(&ti, dest.join("share/terminfo/x/xterm-kitty"));
    }
    let mut h = Sha256::new();
    let mut entries = Vec::new();
    walk_entries(dest, "", &mut entries, &mut |_, _| false);
    entries.sort();
    for rel in entries {
        let p = dest.join(&rel);
        if let Ok(t) = fs::read_link(&p) {
            h.update(format!("{rel}->{}", t.display()).as_bytes());
        } else {
            h.update(rel.as_bytes());
            h.update(b"\0");
            h.update(fs::read(&p).unwrap_or_else(|e| die(&format!("cannot hash payload: {e}"))));
            if let Some(Entry::File(_, mode)) = read_entry(&p) {
                h.update(mode.to_le_bytes());
            }
        }
    }
    let stamp = hex::encode(h.finalize())[..12].to_string();
    write_file(&dest.join(".stamp"), format!("{stamp}\n").as_bytes(), 0o644);
    stamp
}

fn ssh_opts() -> Vec<String> {
    let dir = ctx().state.join("ssh");
    fs::create_dir_all(&dir)
        .unwrap_or_else(|e| die(&format!("cannot create SSH socket directory: {e}")));
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
        .unwrap_or_else(|e| die(&e.to_string()));
    let socket = format!("{}/%C", dir.display());
    let mut options = vec!["-o".into(), "ConnectTimeout=15".into()];
    if socket.len() + 40 < 100 {
        options.extend([
            "-o".into(),
            "ControlMaster=auto".into(),
            "-o".into(),
            format!("ControlPath={socket}"),
            "-o".into(),
            "ControlPersist=60".into(),
        ]);
    }
    options
}

fn ssh(host: &str, script: &str, capture: bool) -> Out {
    if host.starts_with('-') || host.chars().any(|c| c.is_control() || c.is_whitespace()) {
        die("invalid SSH host");
    }
    let mut cmd = vec!["ssh".to_string()];
    cmd.extend(ssh_opts());
    cmd.push(host.into());
    cmd.push(script.into());
    run(&cmd, capture)
}

/// Stream a staged payload over SSH; the target only needs bash and tar, not rsync.
fn send_payload(host: &str, payload: &Path) -> i32 {
    use std::process::{Command, Stdio};
    let script = "set -eu; mkdir -p \"$HOME/.local/kit\"; stage=$(mktemp -d \"$HOME/.local/kit/upload.XXXXXX\"); trap 'rm -rf \"$stage\"' EXIT; tar -xzf - -C \"$stage\"; KIT_PAYLOAD=\"$stage\" bash \"$stage/remote/setup.sh\"";
    let mut tar_cmd = Command::new("tar");
    tar_cmd.env("COPYFILE_DISABLE", "1");
    if cfg!(target_os = "macos") {
        tar_cmd.args(["--no-xattrs", "--no-mac-metadata"]);
    }
    let mut tar = tar_cmd
        .arg("-C")
        .arg(payload)
        .args(["-czf", "-", "."])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| die(&e.to_string()));
    let mut ssh_cmd = Command::new(which("ssh").unwrap());
    ssh_cmd
        .args(ssh_opts())
        .arg("--")
        .arg(host)
        .arg(script)
        .stdin(tar.stdout.take().unwrap());
    let ssh_status = ssh_cmd.status();
    let tar_status = tar.wait();
    match (ssh_status, tar_status) {
        (Ok(ssh), Ok(tar)) if ssh.success() && tar.success() => 0,
        (Ok(ssh), _) if !ssh.success() => ssh.code().unwrap_or(1),
        _ => 1,
    }
}

fn not_sent_summary() -> String {
    let kept = managed(None)
        .iter()
        .filter(|r| ignored(r, &["all", "linux", "remote"]))
        .count();
    if kept == 0 {
        String::new()
    } else {
        format!("{kept} file(s) excluded by .kitignore")
    }
}

/// Activate private tool shims in ordinary shells, including on macOS without Homebrew.
pub fn install_activation() {
    let kit = ctx().home.join(REMOTE_KIT);
    for (name, bytes) in [
        (
            "kit-env",
            include_bytes!("../assets/remote/kit-env").as_slice(),
        ),
        (
            "kit-shell",
            include_bytes!("../assets/remote/kit-shell").as_slice(),
        ),
    ] {
        write_file(&kit.join("bin").join(name), bytes, 0o755);
    }
    write_file(
        &kit.join("bin/fizsh"),
        include_bytes!("../assets/remote/kit-shell"),
        0o755,
    );
    let payload = kit.join("activation");
    write_file(
        &payload.join("remote/hook.sh"),
        include_bytes!("../assets/remote/hook.sh"),
        0o755,
    );
    write_file(&payload.join("mode"), b"local\n", 0o644);
    let rc = managed(None)
        .into_iter()
        .filter(|r| RC_FILES.contains(&r.as_str()))
        .map(|r| format!("{r}\n"))
        .collect::<String>();
    write_file(&payload.join("managed-rc.list"), rc.as_bytes(), 0o644);
    if run(
        &[
            "bash",
            &path_str(&payload.join("remote/hook.sh")),
            "install",
        ],
        false,
    )
    .code
        != 0
    {
        fail("shell activation failed");
    }
}

pub fn ensure_mise() {
    if have("mise") {
        return;
    }
    let script = ctx().state.join("bootstrap.sh");
    write_file(
        &script,
        include_bytes!("../assets/remote/bootstrap.sh"),
        0o700,
    );
    if run(&["bash", &path_str(&script)], false).code != 0 {
        die("could not bootstrap mise; see the prerequisite/download error above");
    }
}

pub fn cmd_push(a: &Args) {
    let cx = ctx();
    if !have("tar") || !have("ssh") {
        die("kit push needs ssh and tar");
    }
    guard_repo();
    let mut remotes = load_obj(&cx.remotes);
    if a.flag("remove") {
        let Some(host) = a.one("host") else {
            die("which server? kit push <host> --remove")
        };
        return remove_from(&host);
    }
    let hosts: Vec<String> = match a.one("host") {
        Some(h) => vec![h],
        None => remotes.keys().cloned().collect(),
    };
    if hosts.is_empty() {
        say("no servers yet — kit push <host> puts your environment on one");
        return;
    }
    let pending = file_changes(None, &load_synced())
        .into_iter()
        .filter(|(c, _)| matches!(*c, "here" | "new" | "deleted" | "both"))
        .count();
    if pending > 0 {
        warn(&format!("{pending} change(s) on this machine aren't in the repo, so servers get the repo's version (kit save first to send yours)"));
    }
    ensure_mise();
    let payload = cx.state.join("payload");
    say("building");
    build_payload(&payload, "remote");
    let summary = not_sent_summary();
    if !summary.is_empty() {
        note(&format!("not sent to servers: {summary}"));
    }
    for host in &hosts {
        if !remotes.contains_key(host) {
            out(&format!(
                "First push to {host}: tracked dotfiles go into its home folder; existing files are backed up in ~/{REMOTE_KIT}/originals.\n\
                 Tools live in ~/{REMOTE_KIT}, and interactive SSH logins start your configured shell.\n\
                 Noninteractive SSH and file transfers stay usable. Restore the originals and remove kit: kit push {host} --remove"
            ));
        }
        if host.starts_with('-') || host.contains(['\n', '\r', '\0', ' ']) {
            fail("invalid SSH host");
            continue;
        }
        let platform = ssh(host, "uname -s", true);
        if platform.code != 0 {
            fail(&format!("{host}: could not detect the remote OS over SSH"));
            continue;
        }
        let platform = match platform.stdout.trim() {
            "Darwin" => "mac",
            "Linux" => "linux",
            _ => {
                fail(&format!("{host}: unsupported OS"));
                continue;
            }
        };
        let stamp = build_payload_for(&payload, "remote", platform);
        say(&format!("syncing to {host}"));
        if ssh(host, &format!("mkdir -p ~/{REMOTE_KIT}/payload"), false).code != 0 {
            fail(&format!("{host}: can't reach it over ssh"));
            continue;
        }
        say(&format!("setting up {host}"));
        let rc = send_payload(host, &payload);
        let record = |stamp: &str, at: &str| serde_json::json!({"stamp": stamp, "at": at});
        if rc == 0 {
            remotes.insert(
                host.clone(),
                record(
                    &stamp,
                    &chrono::Local::now().format("%Y-%m-%dT%H:%M").to_string(),
                ),
            );
            save_json(&cx.remotes, &Value::Object(remotes.clone()));
        } else if rc == 255 {
            fail(&format!("{host}: lost the ssh connection during setup"));
        } else {
            remotes
                .entry(host.clone())
                .or_insert_with(|| record("", ""));
            save_json(&cx.remotes, &Value::Object(remotes.clone()));
            fail(&format!("{host}: setup reported problems (see above)"));
        }
    }
    if hosts.len() > 1 {
        say(if exit_code() == 0 {
            "done"
        } else {
            "done, with problems (see above)"
        });
    }
}

/// `kit push <host> --remove`: take kit off a server.
fn remove_from(host: &str) {
    let cx = ctx();
    let script = format!(
        "if [ -f ~/{REMOTE_KIT}/payload/remote/hook.sh ]; then bash ~/{REMOTE_KIT}/payload/remote/hook.sh remove; fi && if [ -f ~/{REMOTE_KIT}/payload/remote/deploy.sh ]; then bash ~/{REMOTE_KIT}/payload/remote/deploy.sh remove; fi && mkdir -p ~/.local/state/kit-remote-backups && if [ -d ~/{REMOTE_KIT}/backups ]; then mv ~/{REMOTE_KIT}/backups ~/.local/state/kit-remote-backups/$(date +%Y%m%d-%H%M%S)-$$; fi && rm -rf ~/{REMOTE_KIT} ~/.kit-off && echo \"kit removed from $(hostname)\""
    );
    if ssh(host, &script, false).code != 0 {
        fail(&format!("could not remove kit from {host}"));
        return;
    }
    let mut remotes = load_obj(&cx.remotes);
    remotes.remove(host);
    save_json(&cx.remotes, &Value::Object(remotes));
}

/// On a Linux machine you use directly: the same tested setup servers get (tools, zsh, nvim) in
/// ~/.local/kit, but using your real ~/.config and ~/.fizsh.
pub fn linux_environment() {
    let home = &ctx().home;
    let payload = home.join(REMOTE_KIT).join("payload");
    build_payload(&payload, "local");
    say("setting up tools, zsh and nvim in ~/.local/kit");
    let env = vec![("KIT_HOOK".to_string(), "0".to_string())];
    let setup = path_str(&payload.join("remote/setup.sh"));
    if run_full(&["bash", setup.as_str()], false, None, Some(&env), None).code != 0 {
        fail("environment setup reported problems (see above)");
    }
    crate::complete::install_completions(true); // kit's zsh now exists too
}
