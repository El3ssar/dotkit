//! Patches: built here as git-style text, shown through delta when it's available.
use crate::core::{is_binary, looks_secret_name, secret_hit, Entry};
use crate::util::*;
use similar::TextDiff;
use std::path::PathBuf;

fn hidden(e: Option<&Entry>) -> bool {
    matches!(e, Some(Entry::File(b, _)) if secret_hit(b).is_some())
}

fn lines_of(e: Option<&Entry>) -> String {
    match e {
        None => String::new(),
        Some(Entry::Link(t)) => format!("symlink -> {t}\n"),
        Some(Entry::Dir) => "(a folder)\n".into(),
        Some(Entry::Other) => "(a special file)\n".into(),
        Some(Entry::Unreadable) => "(unreadable)\n".into(),
        Some(Entry::File(b, _)) => {
            let text = String::from_utf8_lossy(b);
            text.chars().map(|ch| if (ch as u32) < 0x20 && ch != '\n' && ch != '\t' || ch == '\x7f' { '\u{fffd}' } else { ch }).collect()
        }
    }
}

/// A git-style patch for one file (plain text; colors come from delta or colorize()).
pub fn diff_text(rel: &str, old: Option<&Entry>, new: Option<&Entry>, old_label: &str, new_label: &str, title: &str) -> String {
    let head = if title.is_empty() { String::new() } else { format!("# {rel}: {title}   (- {old_label} · + {new_label})\n") };
    if looks_secret_name(rel) || hidden(old) || hidden(new) {
        return head + &format!("# {rel}: contents hidden (may hold a secret)\n");
    }
    if let (Some(Entry::File(a, ma)), Some(Entry::File(b, mb))) = (old, new) {
        if a == b {
            return head + &format!("# {rel}: only the executable bit differs ({:#o} → {:#o})\n", ma & 0o777, mb & 0o777);
        }
    }
    let bin = |e: Option<&Entry>| matches!(e, Some(Entry::File(b, _)) if is_binary(b));
    if bin(old) || bin(new) {
        let size = |e: Option<&Entry>| match e {
            Some(Entry::File(b, _)) => b.len().to_string(),
            _ => "-".into(),
        };
        return head + &format!("# {rel}: binary file, {} → {} bytes\n", size(old), size(new));
    }
    let (a, b) = (lines_of(old), lines_of(new));
    if a == b {
        return head;
    }
    let from = if old.is_some() { format!("a/{rel}") } else { "/dev/null".into() };
    let to = if new.is_some() { format!("b/{rel}") } else { "/dev/null".into() };
    let diff = TextDiff::from_lines(&a, &b);
    let mut body = String::new();
    for hunk in diff.unified_diff().context_radius(3).iter_hunks() {
        let text = hunk.to_string();
        for line in text.split_inclusive('\n') {
            body.push_str(line);
        }
        if !body.ends_with('\n') {
            body.push_str("\n\\ No newline at end of file\n");
        }
    }
    // similar marks a missing final newline itself; normalize to git's wording
    let body = body.replace("\n\\ No newline at end of file\n\\ No newline at end of file\n", "\n\\ No newline at end of file\n");
    head + &format!("diff --git a/{rel} b/{rel}\n--- {from}\n+++ {to}\n{body}")
}

pub fn colorize(text: &str) -> String {
    let mut out = String::new();
    for line in text.split_inclusive('\n') {
        let bare = line.trim_end_matches('\n');
        let code = if line.starts_with("+++") || line.starts_with("---") || line.starts_with("diff --git") {
            Some("1")
        } else if line.starts_with('+') {
            Some("32")
        } else if line.starts_with('-') {
            Some("31")
        } else if line.starts_with("@@") {
            Some("36")
        } else if line.starts_with("# ") {
            Some("35")
        } else {
            None
        };
        match code {
            Some(code) => {
                out.push_str(&c(code, bare));
                out.push('\n');
            }
            None => out.push_str(line),
        }
    }
    out
}

pub fn delta_cmd() -> Option<PathBuf> {
    let home = &ctx().home;
    which("delta")
        .into_iter()
        .chain([home.join(REMOTE_KIT).join("mise/shims/delta"), home.join(".cargo/bin/delta"), PathBuf::from("/opt/homebrew/bin/delta")])
        .find(|p| is_executable(p))
}

/// Print a patch through delta (your git delta settings) when on a terminal, else plain.
pub fn show_patch(text: &str, plain: bool, page: bool) {
    if text.is_empty() {
        return;
    }
    let use_delta = !plain && ctx().tty && std::env::var_os("KIT_NO_DELTA").is_none();
    if let Some(delta) = use_delta.then(delta_cmd).flatten() {
        let paging = if page { "auto" } else { "never" };
        let mut cmd = std::process::Command::new(delta);
        cmd.args(["--paging", paging]).stdin(std::process::Stdio::piped()).stderr(std::process::Stdio::null());
        if let Ok(mut child) = cmd.spawn() {
            use std::io::Write;
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            if child.wait().map(|s| s.success()).unwrap_or(false) {
                return;
            }
        }
    }
    out_raw(colorize(text).as_bytes());
}
