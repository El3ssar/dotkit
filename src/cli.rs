//! The command line: every command, its options and help text.
use clap::{Arg, ArgAction, ArgMatches, Command};

pub struct Args {
    m: ArgMatches,
}

impl Args {
    pub fn new(m: ArgMatches) -> Self {
        Args { m }
    }
    pub fn flag(&self, id: &str) -> bool {
        self.m.try_get_one::<bool>(id).ok().flatten().copied().unwrap_or(false)
    }
    pub fn one(&self, id: &str) -> Option<String> {
        self.m.try_get_one::<String>(id).ok().flatten().cloned()
    }
    pub fn many(&self, id: &str) -> Vec<String> {
        self.m.try_get_many::<String>(id).ok().flatten().map(|v| v.cloned().collect()).unwrap_or_default()
    }
}

const EVERYDAY: &str = "\
everyday:
  kit status                 what changed here vs the repo
  kit diff [file]            the details
  kit re-add [file]          keep what's on this machine (copy into the repo)
  kit apply [file]           take the repo's version (and install missing packages)
  kit add <file|folder>      start tracking · kit forget <file> to stop
  kit add pkg <tool>         install a tool here and remember it everywhere
  kit save [\"message\"]       commit + push the repo (your backup) · -a to re-add first
  kit update                 pull what other machines saved and apply it
  kit push <host>            your whole environment on a server
  kit undo                   put back what the last apply replaced

kit help <command> for details.";

fn examples(name: &str) -> Option<&'static str> {
    Some(match name {
        "add" => "kit add ~/.config/nvim · kit add ~/.gitconfig · kit add --no-servers ~/.config/kitty · kit add pkg lazygit",
        "re-add" => "kit re-add · kit re-add ~/.zshrc · kit re-add --force ~/.zshrc",
        "status" => "kit status · kit status -v · kit status ~/.config/nvim",
        "diff" => "kit diff · kit diff ~/.zshrc · kit diff -r",
        "apply" => "kit apply -n · kit apply · kit apply ~/.zshrc · kit apply --force ~/.zshrc",
        "forget" => "kit forget ~/.config/foo · kit forget ~/.config/nvim/lazy-lock.json",
        "ignore" => "kit ignore ~/.config/app/cache.db · kit ignore --remote ~/.config/kitty",
        "save" => "kit save · kit save -a \"new lazygit config\"",
        "push" => "kit push raven · kit push --all · kit push raven -n",
        "remote" => "kit remote · kit remote status raven -v · kit remote off raven · kit remote remove raven",
        "restore" => "kit restore · kit restore 20261007-212749 · kit undo",
        _ => return None,
    })
}

fn cmd(name: &'static str, about: &'static str) -> Command {
    let c = Command::new(name).about(about);
    match examples(name) {
        Some(e) => c.after_help(format!("examples: {e}")),
        None => c,
    }
}

fn flag(id: &'static str, long: &'static str, help: &'static str) -> Arg {
    Arg::new(id).long(long).action(ArgAction::SetTrue).help(help)
}

fn paths(required: bool) -> Arg {
    let a = Arg::new("paths").value_name("PATH");
    if required {
        a.num_args(1..).required(true)
    } else {
        a.num_args(0..)
    }
}

fn opt(id: &'static str, long: &'static str, help: &'static str) -> Arg {
    Arg::new(id).long(long).value_name("SPEC").help(help)
}

pub fn build_cli() -> Command {
    Command::new("kit")
        .version(crate::util::VERSION)
        .about("Your dotfiles, packages and shell, on every machine.")
        .after_help(EVERYDAY)
        .disable_help_subcommand(true)
        .subcommand_value_name("command")
        .subcommand(cmd("init", "create the repo, or clone an existing one (kit init <git-url>)").arg(Arg::new("repo")))
        .subcommand(
            cmd("add", "start tracking files or folders (for tools: kit add pkg <tool>)")
                .arg(paths(true))
                .arg(flag("force", "force", "add even if ignored or it looks like a secret"))
                .arg(flag("no_servers", "no-servers", "back it up, but don't send it to servers")),
        )
        .subcommand(
            cmd("re-add", "copy changes made on this machine into the repo (new and deleted files too)")
                .arg(paths(false))
                .arg(flag("force", "force", "also when the repo changed too, or the file looks like a secret")),
        )
        .subcommand(cmd("forget", "stop tracking (the files here stay)").visible_alias("rm").arg(paths(true)))
        .subcommand(
            cmd("ignore", "never track these (or keep them off servers with --remote)")
                .arg(paths(true))
                .arg(flag("remote", "remote", "tracked, but not sent to servers").conflicts_with_all(["mac", "linux"]))
                .arg(flag("mac", "mac", "ignored on Macs only").conflicts_with("linux"))
                .arg(flag("linux", "linux", "ignored on Linux only")),
        )
        .subcommand(cmd("managed", "list tracked files").arg(paths(false)))
        .subcommand(cmd("unmanaged", "untracked files in tracked folders, and untracked ~/.config folders").arg(paths(false)))
        .subcommand(
            cmd("status", "what differs between this machine and the repo")
                .visible_alias("st")
                .arg(paths(false))
                .arg(flag("verbose", "verbose", "list every file instead of grouping folders").short('v')),
        )
        .subcommand(cmd("verify", "exit 1 if files here differ from the repo (for scripts)").arg(paths(false)))
        .subcommand(
            cmd("diff", "show what `kit apply` would change (- here, + repo)")
                .arg(paths(false))
                .arg(flag("reverse", "reverse", "show what `kit re-add` would change instead").short('r'))
                .arg(flag("plain", "plain", "plain patch, without delta")),
        )
        .subcommand(
            cmd("apply", "write the repo's files here; also externals, packages and scripts")
                .arg(paths(false))
                .arg(flag("dry_run", "dry-run", "show what would happen, change nothing").short('n'))
                .arg(flag("verbose", "verbose", "show the diff of each file written").short('v'))
                .arg(flag("force", "force", "also overwrite files changed on this machine (backed up)"))
                .arg(flag("no_packages", "no-packages", "don't install packages"))
                .arg(flag("no_scripts", "no-scripts", "don't run scripts")),
        )
        .subcommand(cmd("edit", "edit the repo's copy of a file, then apply it").arg(Arg::new("path").required(true)))
        .subcommand(cmd("cat", "print the repo's version of a file (as it's written on this machine)").arg(Arg::new("path").required(true)))
        .subcommand(cmd("source-path", "where the repo (or a file in it) is").arg(Arg::new("path")))
        .subcommand(cmd("cd", "open a shell in the repo"))
        .subcommand(
            cmd("git", "run git in the repo")
                .disable_help_flag(true)
                .arg(Arg::new("args").num_args(0..).trailing_var_arg(true).allow_hyphen_values(true)),
        )
        .subcommand(
            cmd("save", "commit everything and push the repo (your backup)")
                .arg(Arg::new("message"))
                .arg(flag("all", "all", "kit re-add first, so this machine's changes are included").short('a'))
                .arg(flag("force", "force", "save even if files seem to contain secrets")),
        )
        .subcommand(
            cmd("update", "pull what other machines saved and apply it")
                .arg(flag("no_packages", "no-packages", "don't install packages"))
                .arg(flag("no_scripts", "no-scripts", "don't run scripts"))
                .arg(flag("yes", "yes", "run new or changed scripts without asking").short('y')),
        )
        .subcommand(
            cmd("restore", "list the versions apply replaced, or put a set back")
                .arg(Arg::new("stamp"))
                .arg(flag("yes", "yes", "don't ask").short('y')),
        )
        .subcommand(cmd("undo", "put back what the last apply replaced").arg(flag("yes", "yes", "don't ask").short('y')))
        .subcommand(
            cmd("pkg", "manage packages (tools installed on every machine)")
                .subcommand_value_name("action")
                .subcommand(
                    cmd("add", "install a tool here and track it: a name (looked up in Homebrew and mise), a GitHub owner/repo, or a spec like aqua:owner/repo or cask:kitty")
                        .arg(Arg::new("names").num_args(1..).required(true))
                        .arg(Arg::new("only").long("only").value_parser(["mac", "linux"]).help("one platform only (linux = Linux machines and servers)"))
                        .arg(opt("backend", "backend", "Linux recipe as a mise spec, e.g. github:owner/repo"))
                        .arg(Arg::new("bin").long("bin").help("its command name, if different from the package name"))
                        .arg(opt("fallback", "fallback", "build-from-source recipe for Linux machines where no download runs, e.g. cargo:crate@1.2.3"))
                        .arg(flag("no_servers", "no-servers", "install on your own machines, not on servers").conflicts_with("servers"))
                        .arg(flag("servers", "servers", "undo --no-servers")),
                )
                .subcommand(
                    cmd("rm", "stop tracking a package")
                        .arg(Arg::new("names").num_args(1..).required(true))
                        .arg(flag("uninstall", "uninstall", "also uninstall it here")),
                )
                .subcommand(
                    cmd("set", "change a tracked package")
                        .arg(Arg::new("name").required(true))
                        .arg(flag("no_servers", "no-servers", "don't install it on servers").conflicts_with("servers"))
                        .arg(flag("servers", "servers", "install it on servers"))
                        .arg(Arg::new("bin").long("bin").help("its command name"))
                        .arg(opt("fallback", "fallback", "build-from-source recipe, e.g. cargo:crate@1.2.3"))
                        .arg(opt("mac", "mac", "e.g. brew:bat, cask:kitty, cargo:eza, cmd:<shell>"))
                        .arg(opt("linux", "linux", "a mise spec, e.g. aqua:sharkdp/bat@0.26.1")),
                )
                .subcommand(cmd("list", "tracked packages"))
                .subcommand(cmd("install", "install tracked packages missing here"))
                .subcommand(cmd("upgrade", "bump the Linux/server versions to the latest release").arg(Arg::new("names").num_args(0..)))
                .subcommand(cmd("scan", "tools installed with brew/cargo that kit doesn't track")),
        )
        .subcommand(
            cmd("scripts", "scripts that run after apply: list, or `kit scripts run [name] [--force]`")
                .arg(Arg::new("action").value_parser(["list", "run"]).default_value("list"))
                .arg(Arg::new("names").num_args(0..))
                .arg(flag("force", "force", "run even if nothing changed")),
        )
        .subcommand(
            cmd("push", "install/update your environment on a server over ssh")
                .arg(Arg::new("host"))
                .arg(flag("all", "all", "every server you pushed to before"))
                .arg(flag("dry_run", "dry-run", "show what would happen").short('n'))
                .arg(flag("yes", "yes", "don't ask on the first push to a server").short('y')),
        )
        .subcommand(
            cmd("remote", "your servers: list, status, off, on, remove")
                .arg(Arg::new("action").value_parser(["list", "status", "off", "on", "remove"]).default_value("list"))
                .arg(Arg::new("host"))
                .arg(flag("verbose", "verbose", "also size and login-hook files").short('v'))
                .arg(flag("yes", "yes", "don't ask before removing").short('y')),
        )
        .subcommand(
            cmd("shell", "Linux: your shell environment on this machine (status, install, off, on, remove)")
                .arg(Arg::new("action").value_parser(["status", "install", "off", "on", "remove"]).default_value("status"))
                .arg(flag("yes", "yes", "don't ask").short('y')),
        )
        .subcommand(cmd("doctor", "check this machine's setup"))
        .subcommand(
            cmd("completion", "tab completion: installed automatically; print a shell's script, or reinstall")
                .arg(Arg::new("shell").value_parser(["zsh", "bash", "fish", "install"]).default_value("install")),
        )
}
