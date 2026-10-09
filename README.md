# dotkit

`kit` keeps your dotfiles, tools and shell the same on every machine: a git repo holds them,
each machine syncs with it, and `kit push` puts your whole environment on a Linux server.

## Install
```bash
curl -fsSL https://raw.githubusercontent.com/El3ssar/dotkit/main/install.sh | sh
```
Prebuilt for macOS (Apple Silicon / Intel) and Linux (x86_64 / arm64, static, any distro).
With Rust: `cargo install dotkit` (the command is `kit`).

New machine, existing repo, in one go:
```bash
curl -fsSL https://raw.githubusercontent.com/El3ssar/dotkit/main/install.sh | sh -s -- git@github.com:you/dotfiles.git
```

## Commands
| | |
|---|---|
| `kit init [url]` | start a repo, set this machine up from yours, or set where it's backed up |
| `kit add <file\|folder\|tool>` | track it; a tool (`kit add lazygit`) is installed here and on every machine |
| `kit rm <file\|folder\|tool>` | stop tracking (nothing is deleted) |
| `kit status [file]` | what changed here or in the repo; with a file, its diff ([delta](https://github.com/dandavison/delta)) |
| `kit save [file]` | keep this machine's changes: copy them into the repo |
| `kit sync` | back up what you saved, get what other machines saved |
| `kit undo [file]` | with a file: take the repo's version; alone: put back what the last sync replaced |
| `kit push [host]` | your environment on a server; no host: all your servers |

## How it goes
```bash
kit init                                  # new repo in ~/.local/share/kit
kit add ~/.zshrc ~/.config/nvim lazygit   # files, folders and tools
kit init git@github.com:you/dotfiles.git  # an empty PRIVATE repo, for the backup
kit sync
```
Then, day to day: edit your files as usual, `kit save` what you want to keep, `kit sync`.

## Two machines
`kit sync` only moves what was **saved**. A change you didn't `kit save` stays on its machine.
When a sync brings a newer version of a file you also changed here (and didn't save):
- **different lines** — kit merges them: your file gets both changes. Yours still isn't in the
  repo; `kit save` and `kit sync` when you're happy.
- **the same lines** — kit writes both versions into the file between `<<<<<<< this machine` and
  `>>>>>>> repo`. Keep what you want, delete the markers, then `kit save`. Or `kit undo <file>`
  takes the repo's version, and `kit undo` puts yours back as it was before the sync.

Anything a sync or undo replaces is kept, so `kit undo` can always put it back.

## Tools
`kit add <tool>` looks the tool up once and writes down how to install it:
on a Mac from Homebrew (`brew:lazygit`), on Linux and servers from the GitHub releases mise knows
about (`aqua:jesseduffield/lazygit@0.66.0`, version pinned). Every machine then installs it the
same way. Adding it again pins the newest version. Not found? Give the GitHub repo
(`kit add owner/repo`) or the recipes: `--mac brew:…|cask:…|cargo:…`, `--linux <mise spec>`;
`--fallback cargo:<crate>` builds from source on old systems where no download runs.
delta and bat always come with kit.

## Servers
`kit push <host>` installs your tools, zsh, nvim and configs into `~/.local/kit` over ssh (no root;
old glibc is fine). A plain `ssh <host>` then lands in your shell; scp, rsync and
`ssh host <command>` are unaffected. `touch ~/.kit-off` there turns it off;
`kit push <host> --remove` takes it away. Only `~/.config/*` and `~/.fizsh/*` travel;
`kit add --no-servers` keeps something home.

## The repo
- `home/` tracked files, laid out like `$HOME`
- `packages.json` tools (written by `kit add`)
- `rules.json` small per-platform edits applied when writing files, e.g.
  `{"path": ".config/zellij/config.kdl", "on": ["linux"], "delete_lines": ["^copy_command "]}`
  (also `replace` and `regex`; `on` can be `mac`, `linux`, `remote`)
- `externals.json` git checkouts placed into `$HOME`: `{".fizsh/.antidote": {"git": "<url>", "ref": "v2.3.0"}}`
- `scripts/*.sh` run by `kit sync`; headers `# kit: on=mac|linux`, `# kit: run=onchange|once|always`,
  `# kit: watch=<paths>`. New or changed scripts from another machine run only after you agree.
- `.kitignore` gitignore-style; `[remote]` = not sent to servers; `[mac]`/`[linux]` = ignored there
- per-machine state (last-synced copies, backups) lives in `~/.local/state/kit`

Secrets (keys, tokens, passwords, by name and by content) are refused unless you `--force`.

## Development
```bash
cargo build && python3 tests/run.py     # black-box tests, each in a sandboxed $HOME
```
