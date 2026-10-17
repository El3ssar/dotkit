//! The command line.
use clap::{Arg, ArgAction, ArgMatches, Command};

pub struct Args {
    m: ArgMatches,
}

impl Args {
    pub fn new(m: ArgMatches) -> Self {
        Args { m }
    }
    pub fn subcommand(&self) -> Option<(String, Self)> {
        self.m
            .subcommand()
            .map(|(name, args)| (name.into(), Self::new(args.clone())))
    }
    pub fn flag(&self, id: &str) -> bool {
        self.m
            .try_get_one::<bool>(id)
            .ok()
            .flatten()
            .copied()
            .unwrap_or(false)
    }
    pub fn one(&self, id: &str) -> Option<String> {
        self.m.try_get_one::<String>(id).ok().flatten().cloned()
    }
    pub fn many(&self, id: &str) -> Vec<String> {
        self.m
            .try_get_many::<String>(id)
            .ok()
            .flatten()
            .map(|v| v.cloned().collect())
            .unwrap_or_default()
    }
}

const HOW: &str = "\
how it goes:
  kit add ~/.config/nvim lazygit   track files, folders and tools
  kit status                       what changed (kit status <file> shows the diff)
  kit save                         keep this machine's changes: copy them into the repo
  kit sync                         back up what you saved, get what other machines saved
  kit push myserver                your whole environment on a server

kit help <command> for details.";

fn cmd(name: &'static str, about: &'static str, examples: &'static str) -> Command {
    Command::new(name)
        .about(about)
        .after_help(format!("examples: {examples}"))
}

fn flag(id: &'static str, help: &'static str) -> Arg {
    Arg::new(id)
        .long(id.replace('_', "-"))
        .action(ArgAction::SetTrue)
        .help(help)
}

fn spec(id: &'static str, help: &'static str) -> Arg {
    Arg::new(id)
        .long(id)
        .value_name("SPEC")
        .help(help)
        .help_heading("For tools")
}

fn things(required: bool, name: &'static str) -> Arg {
    let a = Arg::new("paths").value_name(name);
    if required {
        a.num_args(1..).required(true)
    } else {
        a.num_args(0..)
    }
}

pub fn build_cli() -> Command {
    Command::new("kit")
        .version(crate::util::VERSION)
        .about("Your dotfiles, tools and shell, the same on every machine.")
        .after_help(HOW)
        .disable_help_subcommand(true)
        .subcommand_value_name("command")
        .subcommand(cmd(
            "init",
            "start a new repo, set this machine up from yours, or set where it's backed up",
            "kit init · kit init git@github.com:you/dotfiles.git",
        ).arg(Arg::new("repo").value_name("URL")))
        .subcommand(
            cmd(
                "add",
                "track files, folders or tools with explicit platform recipes",
                "kit add ~/.config/nvim ~/.gitconfig · kit add lazygit · kit add BurntSushi/ripgrep · kit add cargo:eza@0.23.0",
            )
            .arg(things(true, "FILE|TOOL"))
            .arg(flag("force", "even if it's ignored or looks like a secret"))
            .arg(flag("upgrade", "check and update this tracked tool through its installed provider").conflicts_with_all(["mac", "linux", "fallback", "bin", "force", "no_servers"]))
            .arg(flag("no_servers", "keep it off servers (kit push)"))
            .arg(spec("mac", "how to install it on a Mac: a pinned mise/Cargo recipe or brew:<formula> / cask:<app>"))
            .arg(spec("linux", "how to install it on Linux and servers: a mise spec, e.g. aqua:owner/repo"))
            .arg(spec("fallback", "build it from source where no download runs, e.g. cargo:<crate>"))
            .arg(Arg::new("bin").long("bin").value_name("NAME").help("its command name, if different").help_heading("For tools")),
        )
        .subcommand(
            cmd("packages", "browse and manage the tools tracked for sync", "kit packages · kit packages list · kit packages update eza · kit packages resolve")
                .visible_alias("pkg")
                .arg(flag("json", "print the full package manifest, including kit's built-in tools").conflicts_with("tui"))
                .arg(flag("tui", "open the interactive package manager (requires a terminal)"))
                .subcommand(Command::new("list").about("list packages, recipes, installation state and server scope").arg(flag("json", "print the inventory as JSON")))
                .subcommand(Command::new("check").about("check provider artifacts for macOS/Linux x86_64/ARM64; source builds are reported separately").arg(Arg::new("names").num_args(0..)))
                .subcommand(Command::new("update").about("check selected installed tools and update only when newer versions exist").arg(flag("check", "check versions without installing or changing recipes")).arg(Arg::new("names").num_args(1..).required(true)))
                .subcommand(Command::new("remove").about("stop syncing selected tools; leave installed programs untouched").arg(Arg::new("names").num_args(1..).required(true)))
                .subcommand(Command::new("resolve").about("repair missing/unsupported Linux recipes, preserving supported providers; does not install tools").arg(flag("both", "also resolve missing/unsupported macOS recipes")).arg(Arg::new("names").num_args(0..)))
                .subcommand(Command::new("servers").about("include selected tools in kit push").arg(Arg::new("names").num_args(1..).required(true)))
                .subcommand(Command::new("local").about("exclude selected tools from kit push").arg(Arg::new("names").num_args(1..).required(true)))
        )
        .subcommand(
            cmd("files", "browse tracked files, compare versions and stage changes", "kit files · kit files list · kit files --json")
                .arg(flag("json", "print tracked file state and SSH deployment scope").conflicts_with("tui"))
                .arg(flag("tui", "open the interactive file manager"))
                .subcommand(Command::new("list").about("list tracked files and changes").arg(flag("json", "print tracked file state as JSON")))
        )
        .subcommand(
            cmd("rm", "stop tracking files, folders or tools (nothing is deleted)", "kit rm ~/.config/foo · kit rm lazygit")
                .arg(things(true, "FILE|TOOL")),
        )
        .subcommand(
            cmd("status", "what changed here or in the repo; with a file, its diff", "kit status · kit status -v · kit status ~/.zshrc")
                .visible_alias("st")
                .arg(things(false, "FILE"))
                .arg(Arg::new("verbose").short('v').long("verbose").action(ArgAction::SetTrue).help("list every file instead of grouping folders")),
        )
        .subcommand(
            cmd("save", "keep this machine's changes: copy them into the repo (kit sync backs them up)", "kit save · kit save ~/.zshrc · kit save --force ~/.zshrc")
                .arg(things(false, "FILE"))
                .arg(flag("force", "also when the repo changed too, the file has merge conflicts, or it looks like a secret")),
        )
        .subcommand(cmd(
            "sync",
            "back up what you saved, get what other machines saved (merging both)",
            "kit sync · kit sync --trust",
        ).arg(flag("trust", "approve pending scripts and external checkouts from this repo").conflicts_with("dry_run"))
          .arg(flag("dry_run", "preview local sync actions without commits, network requests, installs or writes"))
          .arg(flag("json", "print the offline preview as JSON").requires("dry_run")))
        .subcommand(
            cmd(
                "undo",
                "take the repo's version of a file, or put back what the last sync replaced",
                "kit undo ~/.zshrc · kit undo",
            )
            .arg(things(false, "FILE|BACKUP")),
        )
        .subcommand(
            cmd("push", "your environment on a server over ssh; no host: every server you pushed to", "kit push raven · kit push · kit push raven --remove")
                .arg(Arg::new("host"))
                .arg(flag("remove", "take kit off that server")),
        )
        .subcommand(cmd("doctor", "diagnose configuration and prerequisites without changing anything", "kit doctor · kit doctor --json").arg(flag("json", "print diagnostics as JSON")))
        .subcommand(
            Command::new("completion")
                .hide(true)
                .about("print a shell's completion script, or reinstall them (they install themselves)")
                .arg(Arg::new("shell").value_parser(["zsh", "bash", "fish", "install"]).default_value("install")),
        )
}
