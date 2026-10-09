# dotkit

`kit` keeps your dotfiles, packages and shell the same on every machine. It works like
chezmoi (a git repo is the source of truth; `status` / `diff` / `apply` / `re-add`), and it also
installs your tools everywhere and can push your whole environment to a Linux server over ssh.

## Install
```bash
curl -fsSL https://raw.githubusercontent.com/El3ssar/dotkit/main/install.sh | sh
```
Prebuilt for macOS (Apple Silicon / Intel) and Linux (x86_64 / arm64, static, any distro).
With Rust: `cargo install dotkit` (the command is `kit`).

New machine, existing kit repo, in one go:
```bash
curl -fsSL https://raw.githubusercontent.com/El3ssar/dotkit/main/install.sh | sh -s -- git@github.com:you/dotfiles.git
```

## Start
```bash
kit init                          # new repo in ~/.local/share/kit
kit add ~/.zshrc ~/.config/nvim   # track files and folders
kit add pkg ripgrep               # install a tool here, remember it for every machine
kit git remote add origin <url-of-an-empty-private-repo>
kit save                          # commit + push: your backup
```

## Everyday
| | |
|---|---|
| `kit status` | what differs between this machine and the repo (`-v` lists every file) |
| `kit diff [file]` | the details, through [delta](https://github.com/dandavison/delta) (`-r` other direction, `--plain` raw patch) |
| `kit re-add [file]` | keep this machine's version: copies changed, new and deleted files into the repo |
| `kit apply [file]` | take the repo's version (plus packages, externals, scripts). `-n` previews |
| `kit add <file/folder>` | start tracking (new files in tracked folders show up in `status`) |
| `kit forget <file/folder>` | stop tracking (the file stays) |
| `kit ignore <file>` | never track it · `--remote`: tracked, but not sent to servers |
| `kit edit <file>` | edit the repo copy, then apply it |
| `kit save ["msg"]` | commit + push the repo · `-a` re-adds first |
| `kit update` | pull what other machines saved, then apply it |
| `kit undo` / `kit restore` | put back what an apply replaced |
| `kit cd` / `kit git …` | work in the repo directly |

kit remembers what it last synced on each machine, so `status` can tell "changed here",
"changed in repo" and "changed on both". `apply` never silently overwrites a file you changed
here: it asks (or skips it without a terminal); `--force` overwrites, keeping a backup.

Tab completion (commands, options, tracked files, packages, servers, scripts) installs itself
for zsh, bash and fish the first time kit runs.

## Packages
| | |
|---|---|
| `kit add pkg <tool>` | install here + track (name, `owner/repo`, or a mise spec like `aqua:owner/repo`) |
| `kit pkg add <tool> --only linux` | only for Linux machines and servers |
| `kit pkg list` | what's tracked, installed here, sent to servers |
| `kit pkg set <tool> --no-servers` | change one (`--bin`, `--fallback`, `--mac`, `--linux`) |
| `kit pkg rm <tool>` | stop tracking (`--uninstall` also removes it here) |
| `kit pkg upgrade [tool]` | bump Linux/server versions to the latest release |
| `kit pkg scan` | installed with brew/cargo but not tracked |

On a Mac packages come from Homebrew, cargo or casks; on Linux and servers from
[mise](https://mise.jdx.dev) (GitHub releases, no root). `--fallback cargo:<crate>@<ver>` builds
from source on old systems where no download runs. delta and bat always come with kit.

## Servers
`kit push <host>` installs your tools, zsh, nvim and configs into `~/.local/kit` on a Linux server
over ssh (no root; works on old glibc). A plain `ssh <host>` then lands in your shell; scp, rsync
and `ssh host <command>` are unaffected. `kit push --all` updates every server; `kit remote`
lists them; `kit remote status|off|on|remove <host>`. Only `~/.config/*` and `~/.fizsh/*` travel;
`[remote]` in `.kitignore` keeps things home.

On your own Linux machine `kit apply` installs the same tools into `~/.local/kit` but uses your
real `~/.config`; `kit shell install` makes new terminals start that shell.

## Scripts
`scripts/*.sh` in the repo run after `kit apply`. Headers at the top of the file:
`# kit: on=mac|linux`, `# kit: run=onchange|once|always`, `# kit: watch=<paths relative to $HOME>`.
`kit update` asks before running new or changed scripts. `kit scripts` lists them.

## The repo
- `home/` tracked files, laid out like `$HOME`
- `packages.json` packages (via `kit pkg`)
- `rules.json` small per-platform edits applied when writing files, e.g.
  `{"path": ".config/zellij/config.kdl", "on": ["linux"], "delete_lines": ["^copy_command "]}`
  (also `replace` and `regex`; `on` can be `mac`, `linux`, `remote`)
- `externals.json` git checkouts placed into `$HOME`: `{".fizsh/.antidote": {"git": "<url>", "ref": "v2.3.0"}}`
- `remotes.json` servers you pushed to
- `.kitignore` gitignore-style patterns; `[remote]` = not sent to servers; `[mac]`/`[linux]` = ignored there; `!pattern` un-ignores
- per-machine state (sync records, backups) lives in `~/.local/state/kit`, not in git

Secrets are refused (by file name and by content: keys, tokens, passwords) unless you `--force`;
`kit save` checks again before committing.

## Development
```bash
cargo build && python3 tests/run.py     # 150 black-box tests, each in a sandboxed $HOME
```
