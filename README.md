# dotkit

**Your dotfiles and tools, the same on every machine.**

`kit` keeps your config files, and the tools they need, in a git repo. It syncs them between
your machines, merges when two of them changed the same file, and can put your whole shell on a
server over ssh.

![kit status, save and sync](https://raw.githubusercontent.com/El3ssar/dotkit/main/docs/daily.gif)

## Install

```bash
cargo install dotkit --locked # the command is `kit`
```

No Rust? On macOS or Linux:

```bash
curl -fsSL https://raw.githubusercontent.com/El3ssar/dotkit/main/install.sh | sh
```

## Start

Make an empty, private repo on GitHub for the backup, then:

```bash
kit init                       # a new repo
kit add ~/.zshrc lazygit       # files, folders and tools
kit init git@github.com:you/dotfiles.git
kit sync
```

![setting up](https://raw.githubusercontent.com/El3ssar/dotkit/main/docs/setup.gif)

Adding a folder tracks the files present when you add it. Later files stay untracked until
`kit add <file>` (or adding the folder again). `kit status` and `kit save` only consider
tracked files. Package status recognizes installed executables regardless of whether they
came from Cargo, Homebrew, mise, or your system package manager.

`kit add` takes files, folders and **tools**. For a tool, kit looks up once how to install it
and records the macOS/Linux recipes in `packages.json`; it also installs the tool locally.
`kit sync` commits and pushes those recipes to your backup repo. On a new machine,
`kit init <repo>` only clones the repo; **run `kit sync` to install its tools and files**.

Automatic lookup covers the Homebrew and mise registries, including Cargo/npm/Python tools.
A package outside those registries needs an explicit `--mac` / `--linux` recipe; arbitrary
packages are not universally portable. `kit add fizsh` is supported: Homebrew on macOS,
upstream fizsh scripts with kit's portable zsh on Linux. Track your `~/.fizsh` configuration
separately with `kit add ~/.fizsh`.

## Another machine

```bash
kit init git@github.com:you/dotfiles.git
kit sync
```

![a new machine](https://raw.githubusercontent.com/El3ssar/dotkit/main/docs/newmachine.gif)

## Every day

Edit your files as usual, then:

```bash
kit status       # what changed
kit save         # keep this machine's changes
kit sync         # back up, get the others'
```

`kit status` names unsaved local edits, saved files and package recipes waiting to be
backed up, and committed changes waiting to be sent or received. Incoming changes use
the last fetched Git state; `kit sync` fetches the current remote state.
`kit status ~/.zshrc` shows local differences and any saved change waiting to be backed up.

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

Puts your tracked dotfiles in the server's home and tools, shell and editor setup in
`~/.local/kit` over ssh. Most CLI installs need no root; native system packages may need it. From then on `ssh myserver` lands in your shell, while
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


## Manage your packages

`kit packages` (or `kit pkg`) opens an interactive inventory in a terminal. Search with `/`,
move with arrows or `j`/`k`, and select packages with Space (`a` selects every matching package).
Toggle actions with `u` (update), `d` (stop syncing), `l` (resolve Linux recipes), and `s`
(include/exclude in SSH deployment). Each package can have several pending actions; toggling
again undoes that action. Selections stay selected so you can apply several toggles to a batch.
**SSH deployment** means whether `kit push` sends that tool to remote machines; it does not
control local `kit sync`. An included tool still needs a Linux recipe before deployment.
Pending values appear in blue, including the new included/excluded status. Enter applies all
pending actions; `q` cancels, and `c` clears the queue and selections. Updates run before
removing entries when both actions are queued for the same package.
The interface uses Ratatui with Crossterm for keyboard input, scrolling and incremental rendering.
Updates check the selected installed programs through their actual provider and upgrade only
when a newer version is found. Cargo ownership is read from its installation metadata; a
Cargo installation is never replaced with a Brew installation. Missing tools and providers
without a reliable version check are reported and skipped. Removing an entry leaves the installed
program alone. Kit's built-in dependencies are shown separately and are read-only in this manager.

For scripts or a plain list:

```sh
kit packages list
kit packages --json
kit packages update --check eza starship  # read-only update preview
kit packages update eza starship          # apply available updates
kit packages remove some-tool
kit packages resolve
kit packages local just
kit packages servers just
```

Normal `sync` preserves working tools provided by Cargo, Brew or the system instead of
installing a second copy just because the recorded recipe uses another manager. Tools already
managed by kit are reconciled when their recorded recipes change. `kit packages update <name>`
is the explicit action for upgrades. Package changes remain local until `kit sync` publishes them.

## Portable packages

Bare tool names are resolved through mise's registry, including the command name (`ripgrep`
installs `rg`, for example). Existing Mac-only Cargo, Homebrew and portable command recipes
can be migrated explicitly with `kit packages resolve`; review the resulting Linux recipes
with `kit status` before syncing. `sync` and `push` never generate recipes automatically. Mac GUI apps are skipped on Linux. An unresolved CLI is reported as a
failure, so a partial installation cannot say that everything is ready.

Explicit recipes cover tools that need a different installation:

```bash
kit add ripgrep
kit add cargo:bookokrat
kit add mytool --mac brew:mytool --linux aqua:owner/mytool --bin mytool
kit add mytool --linux aqua:owner/mytool@1.2.3 --fallback cargo:mytool@1.2.3
kit add graphviz --linux system:graphviz --bin dot
```

On Linux, kit installs a private, checksum-verified mise, a portable zsh, and Neovim.
Rust, Node, Go and Python tool backends get their required runtimes automatically. Failed
binary installs can try a matching musl release and then a declared source fallback.
Fallbacks are remembered per machine and per requested version. Changing a version does
not silently reuse the fallback for the previous release.

Packages, shims and caches live under `~/.local/kit`; kit does not rewrite your own global
mise configuration. Ordinary shells gain the tool paths through generated activation.
Both `~/.zshrc` and fizsh's `~/.fizsh/.zshrc` are supported. Generated login blocks are not
saved as dotfile changes.

Linux targets need Bash, tar, CA certificates, and curl or wget. Git is needed for external
checkouts; archive formats may also need unzip/xz. x86_64 and arm64 are supported. Source
builds still need a compiler and the native libraries the tool uses. `system:` recipes use
apt, dnf/yum, apk, pacman or zypper and need root or noninteractive sudo; kit reports the
failure when those privileges or packages are unavailable. This is not a guarantee that
an arbitrary CLI can run on every distribution or old libc version.

On macOS, Homebrew recipes use Homebrew when available. Portable CLI recipes can use mise
without it; casks and other native Mac software still require Homebrew.

## SSH deployment and recovery

`kit push host` streams a staged payload over SSH using tar, so the server does not need
rsync. It sends **all tracked home-relative files**, including `~/.zshrc`, `~/.gitconfig`,
and ordinary files outside `~/.config`. `.kitignore` platform and `[remote]` exclusions
still apply. Platform/remote rules take precedence over global ignore rules.

Pushed dotfiles are placed in the server's real home directory. Existing versions are
backed up before replacement; files blocked by directories or directory symlinks are
left alone and reported. `kit push host --remove` restores original dotfiles, removes
activation and tools, and preserves replaced versions in
`~/.local/state/kit-remote-backups`. Stopping tracking a file does not delete its existing
copies from other machines.

Setup runs again on later syncs/pushes to check missing tools and retry failures. Per-tool
results and logs are under `~/.local/kit/state/`, including `setup-results.tsv` and
`install-<tool>.log`. Shell/editor warm-up has separate logs. `kit status -v` shows the
tracked recipes and tools that are absent.

Scripts, external repositories and command recipes obtained from another machine require
review, including on the first clone. A refusal stays in effect on later syncs:

```bash
kit sync                 # review in a terminal
kit sync --trust         # explicitly approve pending code from a repo you trust
```

A forced secret addition/save authorizes that exact content on the current machine;
changing it requires a new override. Sync and undo stop if backups cannot be created.
Failed restores retain their recovery data. Git reconciliation preserves unrelated saved
additions/deletions, merges independent JSON keys, and keeps a recovery ref under
`refs/kit/recovery/` before any reset. Commands sharing a repo or machine state are locked.

## Development and verification

Install the current checkout with `cargo install --path . --locked --force`. Published
crates and release archives only gain these changes after a new release is published.


```bash
cargo build --locked
cargo clippy --all-targets --locked -- -D warnings
python3 tests/run.py
python3 tests/remote.py
shellcheck -S warning -s bash install.sh assets/remote/*.sh assets/remote/kit-env assets/remote/kit-shell
```

`tests/run.py` uses isolated homes and fake package managers. `tests/remote.py` executes
real deployment and login-hook scripts against temporary files. For an actual Linux
installation with real downloads and runtimes:

```bash
python3 tests/live.py --kit target/debug/kit
python3 tests/live.py --kit target/debug/kit --source   # also builds a Cargo tool
```

The live test never uses your real dotfile repo. It checks cold installation, executable
versions, shell config, idempotence, partial failures and recovery by retry. CI runs this
on Ubuntu, the sandbox suites on macOS/Linux, and checks the minimum Rust version (1.88).

Backend details: [mise registry](https://mise.jdx.dev/registry.html),
[Cargo runtime requirements](https://mise.jdx.dev/dev-tools/backends/cargo.html), and
[GitHub asset selection](https://mise.jdx.dev/dev-tools/backends/github.html).

MIT licensed.
