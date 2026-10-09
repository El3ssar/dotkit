//! Shell completion. `kit __complete -- <words after kit>` prints candidates ("value<TAB>description"
//! per line, or __files__ for plain file completion). The small zsh/bash/fish scripts below just call
//! it, so completion always knows your tracked files, packages, servers and scripts.
use crate::cli::{build_cli, Args};
use crate::core::{managed, tracked_dirs};
use crate::util::*;
use clap::Command;
use sha1::{Digest, Sha1};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

pub const ZSH: &str = r#"#compdef kit
# kit completion for zsh (installed by kit; regenerate: kit completion install)
_kit() {
  local IFS=$'\n' line v d
  local -a reply vals disp dirs
  reply=( $(command kit __complete -- "${(@)words[2,CURRENT]}" 2>/dev/null) )
  if [[ ${reply[1]} == __files__ ]]; then _files; return; fi
  for line in $reply; do
    v=${line%%$'\t'*}; d=${line#*$'\t'}; [[ $d == $line ]] && d=
    if [[ $v == */ ]]; then dirs+=$v
    else vals+=$v; disp+="${v}${d:+  -- $d}"; fi
  done
  (( $#vals )) && compadd -Q -l -d disp -a vals
  (( $#dirs )) && compadd -Q -S '' -a dirs
  return 0
}
if [[ $zsh_eval_context[-1] == loadautofunc ]]; then _kit "$@"; else compdef _kit kit; fi
"#;

pub const BASH: &str = r#"# kit completion for bash (installed by kit; regenerate: kit completion install)
_kit() {
  local cur=${COMP_WORDS[COMP_CWORD]} tab=$'\t' l i
  local -a out args
  for ((i = 1; i <= COMP_CWORD; i++)); do args+=("${COMP_WORDS[i]}"); done
  local IFS=$'\n'
  out=($(command kit __complete -- "${args[@]}" 2>/dev/null))
  COMPREPLY=()
  if [[ ${out[0]} == __files__ ]]; then
    local f
    for f in $(compgen -f -- "$cur"); do
      if [[ -d $f ]]; then COMPREPLY+=("$f/"); else COMPREPLY+=("$f "); fi
    done
    return 0
  fi
  for l in "${out[@]}"; do
    l=${l%%"$tab"*}
    [[ $l == "$cur"* ]] || continue
    if [[ $l == */ ]]; then COMPREPLY+=("$l"); else COMPREPLY+=("$l "); fi   # folders: keep typing
  done
  return 0
}
complete -o nospace -F _kit kit
"#;

pub const FISH: &str = r#"# kit completion for fish (installed by kit; regenerate: kit completion install)
function __kit_complete
    set -l toks (commandline -opc) (commandline -ct)
    set -l out (command kit __complete -- $toks[2..-1] 2>/dev/null)
    if test "$out[1]" = __files__
        __fish_complete_path (commandline -ct)
    else
        printf '%s\n' $out
    end
end
complete -c kit -f -a '(__kit_complete)'
"#;

fn script_for(shell: &str) -> &'static str {
    match shell {
        "zsh" => ZSH,
        "bash" => BASH,
        _ => FISH,
    }
}

const FILE_ARGS: &[&[&str]] = &[&["add"], &["ignore"], &["git"]]; // complete any file, not just tracked ones

fn ssh_hosts() -> Vec<String> {
    let cx = ctx();
    let mut hosts: Vec<String> = load_obj(&cx.remotes).keys().cloned().collect();
    if let Ok(text) = fs::read_to_string(cx.home.join(".ssh/config")) {
        let re = regex::Regex::new(r"(?i)^\s*Host\s+(.+)").unwrap();
        for line in text.lines() {
            if let Some(m) = re.captures(line) {
                hosts.extend(m[1].split_whitespace().filter(|h| !h.contains(['*', '?', '!'])).map(String::from));
            }
        }
    }
    hosts.sort();
    hosts.dedup();
    hosts
}

/// Complete tracked paths one folder level at a time, like files.
fn tracked_paths(cur: &str) -> Vec<String> {
    let home = path_str(&ctx().home);
    let (base, typed) = if cur.starts_with("~/") || cur.is_empty() || cur == "~" {
        ("~/".to_string(), cur.get(2..).unwrap_or("").to_string())
    } else if let Some(rest) = cur.strip_prefix(&format!("{home}/")) {
        (format!("{home}/"), rest.to_string())
    } else if std::env::current_dir().map(|d| path_str(&d) == home).unwrap_or(false) {
        (String::new(), cur.to_string())
    } else {
        return vec!["__files__".into()];
    };
    let head = match typed.rfind('/') {
        Some(i) => typed[..=i].to_string(),
        None => String::new(),
    };
    let mut seen: BTreeMap<String, &str> = BTreeMap::new();
    let mut all = managed(None);
    all.extend(tracked_dirs().into_iter().map(|d| format!("{d}/")));
    for rel in all {
        if !rel.starts_with(&typed) || !rel.starts_with(&head) {
            continue;
        }
        let rest = &rel[head.len()..];
        let (seg, more) = match rest.find('/') {
            Some(i) => (&rest[..i], true),
            None => (rest, false),
        };
        if !seg.is_empty() {
            seen.insert(format!("{base}{head}{seg}{}", if more { "/" } else { "" }), if more { "" } else { "tracked" });
        }
    }
    seen.into_iter().map(|(k, v)| if v.is_empty() { k } else { format!("{k}\t{v}") }).collect()
}

fn about(c: &Command) -> String {
    c.get_about().map(|s| s.to_string()).unwrap_or_default()
}

/// Candidates for the last word, given the words typed after `kit`.
pub fn complete(words: &[String]) -> Vec<String> {
    let mut words = words.to_vec();
    if words.is_empty() {
        words.push(String::new());
    }
    let cur = words.pop().unwrap();
    let mut prev = words;
    if prev.len() >= 2 && prev[0] == "add" && prev[1] == "pkg" {
        prev.splice(0..2, ["pkg".to_string(), "add".to_string()]);
    }
    if prev.first().map(String::as_str) == Some("help") && prev.len() == 1 {
        return complete(&[String::new()]).into_iter().filter(|l| !l.starts_with("help\t")).collect();
    }
    let root = build_cli();
    let mut cmd = &root;
    let mut path: Vec<String> = Vec::new();
    let mut pos = 0;
    let mut expect: Option<clap::Arg> = None;
    for w in &prev {
        if expect.take().is_some() || w == "--" {
            continue;
        }
        if w.starts_with('-') {
            let key = w.split('=').next().unwrap_or("");
            let arg = cmd.get_arguments().find(|a| {
                a.get_long().is_some_and(|l| key == format!("--{l}")) || a.get_short().is_some_and(|s| key == format!("-{s}"))
            });
            if let Some(a) = arg {
                if a.get_action().takes_values() && !w.contains('=') {
                    expect = Some(a.clone());
                }
            }
            continue;
        }
        if pos == 0 && cmd.has_subcommands() {
            if let Some(sub) = cmd.find_subcommand(w) {
                path.push(sub.get_name().to_string());
                cmd = sub;
                continue;
            }
        }
        pos += 1;
    }
    if let Some(a) = expect {
        return a.get_possible_values().iter().map(|v| v.get_name().to_string()).collect();
    }
    if cur.starts_with('-') {
        let mut out = Vec::new();
        for a in cmd.get_arguments() {
            if a.is_hide_set() || a.is_positional() {
                continue;
            }
            let help = a.get_help().map(|h| h.to_string()).unwrap_or_default();
            if let Some(l) = a.get_long() {
                out.push(format!("--{l}\t{help}"));
            } else if let Some(s) = a.get_short() {
                out.push(format!("-{s}\t{help}"));
            }
        }
        if cmd.get_arguments().all(|a| a.get_long() != Some("help")) && !cmd.is_disable_help_flag_set() {
            out.push("--help\tshow this help".into());
        }
        return out;
    }
    if cmd.has_subcommands() && pos == 0 {
        let mut out: Vec<String> = cmd.get_subcommands().filter(|s| !s.is_hide_set()).map(|s| format!("{}\t{}", s.get_name(), about(s))).collect();
        if path.is_empty() {
            out.push("help\thelp for a command".into());
        }
        return out;
    }
    let positionals: Vec<&clap::Arg> = cmd.get_arguments().filter(|a| a.is_positional()).collect();
    let mut act = None;
    for (k, a) in positionals.iter().enumerate() {
        let many = a.get_num_args().is_some_and(|n| n.max_values() > 1);
        if k == pos || (k < pos && many) {
            act = Some(*a);
            break;
        }
    }
    let Some(act) = act else { return vec![] };
    let values = act.get_possible_values();
    if !values.is_empty() {
        return values.iter().map(|v| v.get_name().to_string()).collect();
    }
    let id = act.get_id().as_str();
    let cmdpath: Vec<&str> = path.iter().map(String::as_str).collect();
    let cx = ctx();
    match id {
        "paths" => {
            let mut v = if FILE_ARGS.contains(&cmdpath.as_slice()) { vec!["__files__".to_string()] } else { tracked_paths(&cur) };
            if cmdpath == ["rm"] && !cur.contains('/') && !cur.starts_with('~') {
                v.extend(crate::pkg::load_pkgs().keys().map(|n| format!("{n}\ttool")));
            }
            if cmdpath == ["undo"] && !cur.contains('/') && !cur.starts_with('~') {
                let mut stamps: Vec<String> = fs::read_dir(&cx.backups).map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
                stamps.sort();
                v.extend(stamps.into_iter().rev().take(10).map(|s| format!("{s}\tbackup")));
            }
            v
        }
        "host" => ssh_hosts(),
        _ => vec![],
    }
}

/// Where each shell loads completions from automatically, without editing any rc file.
fn completion_targets() -> Vec<(&'static str, PathBuf)> {
    let cx = ctx();
    let mut out = Vec::new();
    let zshes: Vec<PathBuf> = which("zsh").into_iter().chain([cx.home.join(REMOTE_KIT).join("opt/zsh/bin/zsh")]).filter(|z| is_executable(z)).collect();
    for z in zshes {
        let r = run(&[path_str(&z), "-fc".into(), "print -l $fpath".into()], true);
        for d in r.stdout.lines() {
            let p = PathBuf::from(d);
            if !d.is_empty() && p.is_dir() && writable(&p) {
                let target = p.join("_kit");
                if !out.iter().any(|(_, t)| t == &target) {
                    out.push(("zsh", target));
                }
                break;
            }
        }
    }
    let data = std::env::var("XDG_DATA_HOME").ok().filter(|s| !s.is_empty()).map(PathBuf::from).unwrap_or_else(|| cx.home.join(".local/share"));
    out.push(("bash", data.join("bash-completion/completions/kit")));
    if have("fish") || cx.home.join(".config/fish").is_dir() {
        let cfg = std::env::var("XDG_CONFIG_HOME").ok().filter(|s| !s.is_empty()).map(PathBuf::from).unwrap_or_else(|| cx.home.join(".config"));
        out.push(("fish", cfg.join("fish/completions/kit.fish")));
    }
    out
}

fn writable(p: &std::path::Path) -> bool {
    use std::ffi::CString;
    let Ok(c) = CString::new(path_str(p)) else { return false };
    unsafe { libc::access(c.as_ptr(), libc::W_OK) == 0 }
}

pub fn install_completions(quiet: bool) -> Vec<PathBuf> {
    let mut done = Vec::new();
    for (shell, dest) in completion_targets() {
        let script = script_for(shell);
        let current = fs::read_to_string(&dest).ok();
        let result = if current.as_deref() == Some(script) {
            Ok(())
        } else {
            dest.parent().map(fs::create_dir_all).transpose().and_then(|_| write_atomic(&dest, script.as_bytes(), 0o644))
        };
        match result {
            Ok(()) => done.push(dest),
            Err(e) if !quiet => warn(&format!("could not write {}: {e}", dest.display())),
            Err(_) => {}
        }
    }
    done
}

/// Keep the installed completion scripts current (cheap check, runs with every kit command).
pub fn ensure_completions() {
    let cx = ctx();
    let stamp = cx.state.join("completions");
    let me = hex::encode(Sha1::digest(format!("{VERSION}{ZSH}{BASH}{FISH}").as_bytes()));
    if let Ok(old) = fs::read_to_string(&stamp) {
        let mut lines = old.lines();
        if lines.next() == Some(me.as_str()) && lines.filter(|l| !l.is_empty()).all(|p| PathBuf::from(p).is_file()) {
            return;
        }
    }
    let files = install_completions(true);
    let _ = fs::create_dir_all(&cx.state);
    let mut text = me + "\n";
    for f in files {
        text += &format!("{}\n", f.display());
    }
    let _ = fs::write(stamp, text);
}

pub fn cmd_completion(a: &Args) {
    match a.one("shell").as_deref().unwrap_or("install") {
        "install" => {
            for f in install_completions(false) {
                out(&format!("  {}", f.display()));
            }
            say("completion installed — open a new shell (zsh: or run `compinit`)");
        }
        shell => out_raw(script_for(shell).as_bytes()),
    }
}
