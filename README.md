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
| `kit files` | review tracked files and stage actions interactively |
| `kit packages` | review tracked tools and stage actions interactively |
| `kit save [file]` | keep this machine's changes |
| `kit sync` | back up what you saved, get what other machines saved |
| `kit sync --dry-run` | offline preview; no commits, downloads, installs or file writes |
| `kit doctor` | offline configuration and prerequisite diagnostics |
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
Press `?` for help, Tab to review every pending action, and `A` to clear selections without
losing pending actions. Compact layouts keep the list and controls visible in SSH panes.
Hidden selections are counted explicitly; batch toggles still affect those selected entries.
If `packages.json` changes while reviewing, Kit rejects the stale plan instead of overwriting it.
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
kit packages check                       # verify declared OS/CPU release artifacts
kit packages resolve --both              # explicitly repair missing/unsupported recipes
kit packages local just
kit packages servers just
```

Normal `sync` preserves working tools provided by Cargo, Brew or the system instead of
installing a second copy just because the recorded recipe uses another manager. A recipe change also leaves an already available tool in place. `kit packages update <name>`
is the explicit action for upgrades. Package changes remain local until `kit sync` publishes them.

## Manage your files

`kit files` opens a Ratatui manager containing **tracked files only**. `kit files list` and
`kit files --json` provide noninteractive inventories. Search with `/`, navigate with arrows
or `j`/`k`, select with Space, and select every matching row with `a`.

- `v`: review the current file's diff (secrets and large files stay hidden).
- `s`: stage saving this machine's version into the repo.
- `r`: stage taking the repo's version, backing up the local copy first.
- `p`: toggle inclusion for SSH/Linux deployment.
- `d`: stage stopping tracking; the local file stays in place.

Pending values appear in blue. Each file can have several actions; saving and taking the
repo's version are mutually exclusive. Repeat a toggle to undo it. Enter applies the queue,
`c` clears it, and `q`/Escape cancels. Changes to a file or `.kitignore` while the manager is
open require reopening it before applying. A failed copy stops the remaining actions. Saving a local deletion is labeled **save deletion**
and requires reviewing the pending actions before applying. `?` opens help and Tab reviews the
queue. Diffs scroll with arrows (including Left/Right for long lines), Page Up/Down and Home/End.
The SSH/Linux column includes Linux platform exclusions; a macOS SSH destination evaluates
macOS rules instead. Local `sync` inclusion still follows the current platform's rules.

## Package sources and portability

Kit manages Homebrew formulae, selected built-in mise backends (including upstream repository
release assets), Cargo crates, and Kit's own bootstraps. Bare names resolve through mise's
registry; existing Cargo ownership is adopted when available. Recipes are recorded in
`packages.json`. `sync` and `push` never invent or rewrite missing recipes.

**Support is checked per package, not inferred from its package manager.** When recording a
binary recipe, Kit asks mise for release URLs and checksums for the declared operating systems
on x86_64 and arm64. Brew recipes require published bottles for those targets. Checks use
temporary provider files, removed afterward; Kit keeps no discovery database and does no
background scan. Providers may maintain their own caches. `kit packages check [names...]`
rechecks the current metadata and reports missing or unverified coverage without editing recipes.

Cargo, npm, Go and Python packages are source/package builds: their provider is supported,
but cross-platform build compatibility is **unverified** until tested on the destination.
They are labeled accordingly, and `packages check` exits unsuccessfully for unverified builds.
Published artifacts are stronger evidence, but still cannot guarantee execution on every
Linux libc, distribution, or macOS version. Kit reports an installation/runtime failure rather
than silently declaring the environment ready. Homebrew casks remain macOS-only.

```bash
kit add ripgrep                             # resolve and pin a release
kit add cargo:bookokrat                     # explicitly choose a source build
kit add brew:jq                             # check Homebrew bottle coverage
kit add github:owner/tool@1.2.3 --bin tool   # check upstream release assets via mise
kit add mytool --mac brew:mytool --linux aqua:owner/mytool@1.2.3 --bin mytool
kit add mytool --linux aqua:owner/mytool@1.2.3 --fallback cargo:mytool@1.2.3
kit packages check
```

Mise and Cargo recipes require concrete versions; failed resolution does not save `@latest`.
Homebrew recipes follow Homebrew's current formula. Raw `system:` distro recipes, arbitrary
plugins, mutable Git builds and `cmd:` install commands are unsupported for automatic installs.
Existing entries remain visible for inspection, migration or removal. Repair selected entries
explicitly with `kit packages resolve --both` or `kit add <name> --mac <recipe> --linux <recipe>`;
review the saved changes with `kit status`. Resolution does not search every provider or
promise that an arbitrary package is portable.
Legacy unpinned Cargo recipes retain Cargo when explicitly resolved. On the current OS,
Kit pins the installed crates.io version from Cargo metadata; otherwise it queries the
existing provider. Unsupported plugins/custom commands require a supported replacement.
`kit status` summarizes recipe issues; `kit status -v` shows individual reasons.
External checkouts newer than their recorded pin are identified separately: sync preserves
the newer commits rather than downgrading them. Review the pin in `externals.json`.

Working executables from Cargo, Brew, mise or the system are preserved during normal sync;
Kit does not install a second copy because another provider appears in the recipe. Use
`kit packages update` for upgrades. An installation failure retains the tracked recipe for retry.
A Linux fallback must be an explicitly declared mise/Cargo recipe; Kit does not scrape for
undeclared alternatives. Fallback choices are remembered per machine and requested version.

Linux setup uses a private checksum-verified mise and portable zsh. Packages and shims live
under `~/.local/kit`, without rewriting your own global mise configuration. Rust, Node, Go
and Python backends provision their required runtime when needed. Source builds still require
a suitable compiler and native libraries. Linux targets need Bash, tar, CA certificates, and
curl or wget; external checkouts need Git, and some archives need unzip/xz. Homebrew recipes
require Homebrew already installed on the destination; Kit does not install a system package
manager or request sudo. Both `~/.zshrc` and `~/.fizsh/.zshrc` are supported.

Installers show live output and a log path, have no stdin prompts, and time out after 30 minutes.
Set `KIT_INSTALL_TIMEOUT=<seconds>` to change that bound. Cancellation/timeout stops the owned
installer process group. Metadata requests have shorter bounds. Provisioning does not launch
your interactive shell configuration or run Neovim plugin installers.

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
`install-<tool>.log`. `kit status -v` shows the
tracked recipes and tools that are absent.

Custom hooks have live logs and no stdin prompts. They use the install timeout by default;
`KIT_SCRIPT_TIMEOUT=<seconds>` sets a separate bound. External Git operations are noninteractive
and bounded. Explicit ref updates require a clean working tree and fast-forward ancestry;
Kit preserves local commits rather than resetting them. Omitted external refs clone the default
branch initially and leave existing checkouts in place; specify a ref to control later updates.

Scripts and external repositories obtained from another machine require
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

## Preview and diagnose

```sh
kit sync --dry-run
kit sync --dry-run --json
kit doctor
kit doctor --json
```

The preview uses your current repository and sync snapshot. It lists saved repository changes,
file actions, missing tools and pending hooks. It does not fetch remote changes, create a sync
snapshot, install completions or execute Git filters/hooks. A normal sync can discover additional
incoming changes or merge conflicts. Blocking problems produce exit status 1. `--dry-run` cannot
be combined with `--trust`.

Doctor checks local prerequisites, metadata, supported recipes, pending code review and hook
metadata. It does not scan online package registries or certify arbitrary Linux distributions.
Use `kit packages check` for current provider release metadata.

## Development and verification

Install the current checkout with `cargo install --path . --locked --force`. Published
crates and release archives only gain these changes after a new release is published.


```bash
cargo build --locked
cargo clippy --all-targets --locked -- -D warnings
python3 tests/run.py
python3 tests/remote.py
for suite in tests/adversarial_*.py; do python3 "$suite"; done
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
on macOS and Ubuntu 22.04/24.04, runs the sandbox and adversarial suites on macOS/Linux, checks Rust 1.88,
and exercises real localhost SSH push/repeat/remove against an isolated Docker endpoint. The SSH
fixture refuses to run outside a disposable Linux Docker container.

Backend details: [mise registry](https://mise.jdx.dev/registry.html),
[Cargo runtime requirements](https://mise.jdx.dev/dev-tools/backends/cargo.html), and
[GitHub asset selection](https://mise.jdx.dev/dev-tools/backends/github.html).

MIT licensed.
