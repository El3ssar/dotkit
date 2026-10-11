# dotkit

**Your dotfiles and tools, the same on every machine.**

`kit` keeps your config files, and the tools they need, in a git repo. It syncs them between
your machines, merges when two of them changed the same file, and can put your whole shell on a
server over ssh.

![kit status, save and sync](https://raw.githubusercontent.com/El3ssar/dotkit/main/docs/daily.gif)

## Install

```bash
cargo install dotkit        # the command is `kit`
```

No Rust? On macOS or Linux:

```bash
curl -fsSL https://raw.githubusercontent.com/El3ssar/dotkit/main/install.sh | sh
```

## Start

```bash
kit init                                   # a new repo in ~/.local/share/kit
kit add ~/.zshrc ~/.config/nvim lazygit    # files, folders and tools
kit init git@github.com:you/dotfiles.git   # an empty, private repo for the backup
kit sync
```

![setting up](https://raw.githubusercontent.com/El3ssar/dotkit/main/docs/setup.gif)

`kit add` takes files, folders and **tools**. For a tool, kit looks up once how to install it
(Homebrew on macOS, GitHub releases on Linux) and every machine installs it the same way.

## Another machine

```bash
kit init git@github.com:you/dotfiles.git
kit sync
```

![a new machine](https://raw.githubusercontent.com/El3ssar/dotkit/main/docs/newmachine.gif)

## Every day

Edit your files as usual, then:

```bash
kit status        # what changed (kit status ~/.zshrc shows the diff)
kit save          # keep this machine's changes
kit sync          # back them up, and get what your other machines saved
```

Only what you `save` leaves the machine. A sync never overwrites a file you changed and didn't
save: it merges instead.

## When two machines changed the same file

Different lines: `kit sync` keeps both changes. The same lines: kit puts both versions in the
file, marked `<<<<<<< this machine` and `>>>>>>> repo`. Then either:

- keep what you want in the file, delete the markers, and `kit save`
- `kit undo ~/.zshrc` to take the repo's version
- `kit undo` to go back to yours, as it was before the sync

![resolving a conflict](https://raw.githubusercontent.com/El3ssar/dotkit/main/docs/conflict.gif)

## Servers

```bash
kit push myserver
```

Puts your tools, shell and editor setup in `~/.local/kit` on a Linux server over ssh. It needs
no root and works on old systems too. From then on `ssh myserver` lands in your shell, while
`scp` and `ssh myserver <command>` behave as before. `kit push` updates all your servers;
`kit push myserver --remove` takes it away.

## Commands

| | |
|---|---|
| `kit init [url]` | start a repo, set this machine up from yours, or set where it's backed up |
| `kit add <file\|folder\|tool>` | start tracking it |
| `kit rm <file\|folder\|tool>` | stop tracking it (nothing is deleted) |
| `kit status [file]` | what changed; with a file, its diff |
| `kit save [file]` | keep this machine's changes |
| `kit sync` | back up what you saved, get what other machines saved |
| `kit undo [file]` | take the repo's version of a file, or put back what the last sync replaced |
| `kit push [host]` | your environment on a server |

## Good to know

- Secrets (keys, tokens, passwords) are refused unless you add `--force`.
- Tab completion for zsh, bash and fish installs itself.
- In the repo: `.kitignore` lists what never to track, `rules.json` holds small per-platform
  edits (say, a line only for Linux), and `scripts/*.sh` run after a sync.
- Diffs look best with [delta](https://github.com/dandavison/delta), which kit installs for you.

MIT licensed.
