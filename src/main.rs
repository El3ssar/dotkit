//! kit — your dotfiles, packages and shell, on every machine.
//!
//! A chezmoi-style dotfile manager with packages and remote machines built in.
//! The source of truth is a git repo (default ~/.local/share/kit):
//!
//!   home/           tracked files, laid out like $HOME
//!   packages.json   tracked packages (managed by `kit pkg ...`)
//!   rules.json      small per-platform edits (e.g. drop pbcopy on Linux)
//!   externals.json  git checkouts placed into $HOME (e.g. antidote)
//!   scripts/        run after `kit apply` (like chezmoi's run_onchange_ scripts)
//!   remotes.json    servers you pushed to (managed by `kit push`)
//!   dirs.json       folders added whole: new files in them show up in `kit status`
//!   .kitignore      never tracked; [remote]/[mac]/[linux] sections
//!
//! Per-machine state (what was last synced, backups, script runs) lives in
//! ~/.local/state/kit, never in the repo.
mod cli;
mod complete;
mod core;
mod diff;
mod files;
mod pkg;
mod remote;
mod scripts;
mod util;

use clap::error::{ContextKind, ContextValue, ErrorKind};
use cli::{build_cli, Args};
use util::*;

const CHEZMOI_HINTS: &[(&str, &str)] = &[
    ("merge", "kit diff, then kit re-add --force (yours) or kit apply --force (repo's)"),
    ("chattr", "edit packages or rules in the repo (kit cd)"),
    ("data", "per-platform edits live in rules.json (see README)"),
    ("execute-template", "kit has no templates: rules.json does per-platform edits"),
    ("ignored", "see .kitignore (kit cd)"),
    ("purge", "kit forget, then delete the repo folder"),
    ("archive", "kit git archive"),
];

/// Edit distance where swapping two neighbouring letters counts as one typo.
fn typo_distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut d = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            d[i][j] = (d[i - 1][j] + 1).min(d[i][j - 1] + 1).min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d[i][j] = d[i][j].min(d[i - 2][j - 2] + 1);
            }
        }
    }
    d[a.len()][b.len()]
}

fn closest(bad: &str, names: &[String]) -> Option<String> {
    let limit = (bad.chars().count() / 3).max(2);
    names.iter().map(|n| (typo_distance(bad, n), n)).filter(|(d, _)| *d <= limit).min_by_key(|(d, _)| *d).map(|(_, n)| n.clone())
}

fn usage_error(e: clap::Error, argv: &[String]) -> ! {
    match e.kind() {
        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => {
            let _ = e.print();
            std::process::exit(0);
        }
        ErrorKind::InvalidSubcommand => {
            let bad = match e.get(ContextKind::InvalidSubcommand) {
                Some(ContextValue::String(s)) => s.clone(),
                _ => String::new(),
            };
            if let Some((_, hint)) = CHEZMOI_HINTS.iter().find(|(n, _)| *n == bad) {
                die_code(&format!("kit has no '{bad}': {hint}"), 2);
            }
            let parent = build_cli();
            let scope = match argv.first() {
                Some(first) if first != &bad => parent.find_subcommand(first).cloned().unwrap_or_else(build_cli),
                _ => parent,
            };
            let names: Vec<String> = scope.get_subcommands().map(|c| c.get_name().to_string()).collect();
            let near = closest(&bad, &names);
            let what = if argv.first().is_some_and(|a| a != &bad) { "action" } else { "command" };
            match near {
                Some(n) => die_code(&format!("unknown {what} '{bad}' — did you mean '{n}'?"), 2),
                None => die_code(&format!("unknown {what} '{bad}' (kit --help)"), 2),
            }
        }
        _ => {
            let _ = e.print();
            std::process::exit(2);
        }
    }
}

fn dispatch(m: &clap::ArgMatches) {
    let Some((name, sub)) = m.subcommand() else {
        let _ = build_cli().print_help();
        return;
    };
    let a = Args::new(sub.clone());
    match name {
        "init" => files::cmd_init(&a),
        "add" => files::cmd_add(&a),
        "re-add" => files::cmd_re_add(&a),
        "forget" => files::cmd_forget(&a),
        "ignore" => files::cmd_ignore(&a),
        "managed" => files::cmd_managed(&a),
        "unmanaged" => files::cmd_unmanaged(&a),
        "status" => files::cmd_status(&a),
        "verify" => files::cmd_verify(&a),
        "diff" => files::cmd_diff(&a),
        "apply" => files::cmd_apply(&a),
        "edit" => files::cmd_edit(&a),
        "cat" => files::cmd_cat(&a),
        "source-path" => files::cmd_source_path(&a),
        "cd" => files::cmd_cd(&a),
        "git" => files::cmd_git(&a),
        "save" => files::cmd_save(&a),
        "update" => files::cmd_update(&a),
        "restore" => files::cmd_restore(&a),
        "undo" => files::cmd_undo(&a),
        "scripts" => scripts::cmd_scripts(&a),
        "push" => remote::cmd_push(&a),
        "remote" => remote::cmd_remote(&a),
        "shell" => remote::cmd_shell(&a),
        "doctor" => remote::cmd_doctor(&a),
        "completion" => complete::cmd_completion(&a),
        "pkg" => match sub.subcommand() {
            Some((action, sm)) => {
                let pa = Args::new(sm.clone());
                match action {
                    "add" => pkg::cmd_pkg_add(&pa),
                    "rm" => pkg::cmd_pkg_rm(&pa),
                    "set" => pkg::cmd_pkg_set(&pa),
                    "list" => pkg::cmd_pkg_list(&pa),
                    "install" => pkg::cmd_pkg_install(&pa),
                    "upgrade" => pkg::cmd_pkg_upgrade(&pa),
                    _ => pkg::cmd_pkg_scan(&pa),
                }
            }
            None => {
                let mut root = build_cli();
                let _ = root.find_subcommand_mut("pkg").unwrap().print_help();
            }
        },
        _ => unreachable!(),
    }
}

fn main() {
    // die quietly (exit 141) instead of panicking when output goes to a closed pipe (kit status | head)
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let mut argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.first().map(String::as_str) == Some("__complete") {
        let words: Vec<String> = if argv.get(1).map(String::as_str) == Some("--") { argv[2..].to_vec() } else { argv[1..].to_vec() };
        let lines = complete::complete(&words);
        if !lines.is_empty() {
            out(&lines.join("\n"));
        }
        return;
    }
    if argv.len() >= 2 && argv[0] == "add" && argv[1] == "pkg" {
        argv.splice(0..2, ["pkg".to_string(), "add".to_string()]);
    } else if argv.len() >= 2 && (argv[0] == "rm" || argv[0] == "forget") && argv[1] == "pkg" {
        argv.splice(0..2, ["pkg".to_string(), "rm".to_string()]);
    }
    if argv.first().map(String::as_str) == Some("help") {
        argv = argv[1..].iter().take(2).cloned().chain(["--help".to_string()]).collect();
    }
    let m = build_cli().try_get_matches_from(std::iter::once("kit".to_string()).chain(argv.iter().cloned())).unwrap_or_else(|e| usage_error(e, &argv));
    let cmd = m.subcommand_name().unwrap_or("");
    if !cmd.is_empty() && cmd != "init" && cmd != "doctor" && cmd != "completion" && !ctx().source.exists() {
        die(&format!("no kit repo at {} — run `kit init` (or `kit init <git-url>`)", ctx().source.display()));
    }
    if !cmd.is_empty() {
        files::migrate_state();
        complete::ensure_completions();
    }
    dispatch(&m);
    std::process::exit(exit_code());
}
