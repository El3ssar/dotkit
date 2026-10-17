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

`kit add <tool>` records a tool **already installed here**. It identifies the actual Cargo,
Homebrew or mise installation and records its source and installed version. It then finds a
same-version binary counterpart for the other OS, checking release URLs and checksums for
Intel and ARM. If ownership or the counterpart cannot be established, the package list is
left unchanged. Recording does not install/upgrade tools or set up the Linux environment.
Multiple plain tool names are saved together only after every item passes.

For example, an existing Cargo installation can remain Cargo on macOS while Linux uses a
checked upstream release binary. Homebrew remains Homebrew on the source machine; its
observed installed version is recorded, but future Brew installs follow the current formula.
Copied/unowned executables, custom Cargo features/profiles, custom mise options and unsupported
sources are rejected rather than silently losing how they were installed. Kit uses provider
receipts, its provider catalog, and (when needed) one targeted request for the exact Cargo
release's upstream repository. It keeps no discovery database or background scan.

`kit sync` commits and pushes the recipes to your backup repo. On a new computer, run
`kit init <repo>` followed by `kit sync` to install its tools and files. An explicit source
such as `kit add builtin:fizsh` or `kit add cargo:bookokrat@<version>` remains an advanced
override for tools not installed yet or packages needing manual platform choices; it can
install locally and source builds remain unverified on other destinations. Plain-name
capture and explicit source recipes must be added in separate commands. Track your
`~/.fizsh` configuration separately with `kit add ~/.fizsh`.

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
Pending values appear in blue, including the new included/excluded status. Enter opens a review;
Enter in that review applies all pending actions; `q` cancels, and `c` clears the queue and selections. Updates run before
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

`kit files` opens a collapsible tree containing **tracked files only**. Folders are groups of
tracked paths; opening the manager never discovers or tracks other files. Unchanged branches
start collapsed and branches containing changes start open. Chains of single-child folders
are compressed. `kit files list` and `kit files --json` keep their noninteractive inventories.

- `←` / `→` (or `h` / `l`): collapse, expand, or move to a parent/child folder.
- `O` / `C`: expand / collapse all folders. `n` / `N`: next / previous changed file.
- `1` All, `2` Changed (including saved changes awaiting backup), `3` Conflicts, `4` Pending.
- `/`: search paths. Matching branches are temporarily revealed; clearing search restores folds.
- Space: select a file or a folder's matching tracked descendants. `a` selects every matching
  file, including collapsed folders. `A` clears selection while preserving pending actions.
- `v`: compare Saved in Kit (-) with Your machine (+). Wide terminals also show a preview beside
  the tree. Arrows, Page Up/Down and Home/End scroll the full-screen comparison.
- `s`: stage saving this machine's version. If both sides changed, this explicitly keeps the
  machine's whole file; unresolved conflict markers must be resolved first.
- `r`: stage using the repo's whole copy, backing up the local file first.
- `m`: open the text conflict resolver for the cursor file.
- `p`: toggle SSH/Linux inclusion. `d`: stop tracking and keep the local file.

The resolver shows each conflicting section's machine and repo sides. `1` keeps the machine
side, `2` keeps the repo side, and `3` keeps both **in that order**; this can require further
editing for valid configuration syntax. `[` / `]` moves between sections, `0` clears a choice,
and `v` previews the completed result. Enter queues a fully resolved result; Escape discards
that merge session. Nonconflicting text is preserved. Two-sided edits can be merged here before
sync writes markers. Files without a shared ancestor need an explicit whole-file choice.
Binary, large, secret, non-UTF-8, or platform-transformed files may require an external editor.

Blue values are pending; folder checkboxes show partial selections. Actions use **all selected
files**, including those hidden by folding or filters; without a selection, they use the cursor
file or matching descendants of the cursor folder. Counts and review include hidden actions.
Enter or Tab opens the review; Enter there applies. `q`/Escape in the main list cancels everything;
`c` clears selection and pending actions. No home/repo content is written before applying.
Merge preparation uses temporary state files that are removed immediately.

Applying a resolution backs up the original, writes the resolved local file, and saves it in Kit.
`kit undo` restores the local backup. Changes to files or `.kitignore` while the manager is open
require reopening before applying. A failed copy stops subsequent actions; a batch of files is
not an atomic filesystem transaction. Saving a local deletion is labeled **save deletion**.
Local sync follows the current platform rules; SSH/Linux inclusion controls deployment scope,
while macOS SSH destinations evaluate their macOS rules. `?` opens the full keyboard help.

## Package sources and portability

Kit manages Homebrew formulae, selected built-in mise backends (including upstream repository
release assets), Cargo crates, and Kit's own bootstraps. Bare names capture installed ownership
and require a checked binary counterpart; explicit source names select recipes directly. Recipes are recorded in
`packages.json`. `sync` and `push` never invent or rewrite missing recipes.

**Support is checked per package, not inferred from its package manager.** When recording a
binary recipe, Kit asks mise for release URLs and checksums for the declared operating systems
on x86_64 and arm64. Brew recipes require published bottles for those targets. Checks use
temporary provider files, removed afterward; Kit keeps no discovery database and does no
background scan. Providers may maintain their own caches. `kit packages check [names...]`
rechecks the current metadata and reports missing or unverified coverage without editing recipes.

Google Cloud CLI also has a narrow Kit-owned recipe, `builtin:gcloud@589.0.0`, using
versioned Google archives with pinned SHA-256 hashes and private Python 3.14.8. No Google
installer, sudo, login, shell edits or arbitrary plugin is used. This bundled SDK version
is updated through Kit releases; package update reports it as a bundled recipe rather
than silently upgrading or reinstalling it. Existing external installations are kept.
Rust core recipes validate the official pinned Rust release manifest because mise's Rust
backend does not emit artifact lock entries.

Cargo, npm, Go and Python packages are source/package builds: their provider is supported,
but cross-platform build compatibility is **unverified** until tested on the destination.
They are labeled accordingly, and `packages check` exits unsuccessfully for unverified builds.
Published artifacts are stronger evidence, but still cannot guarantee execution on every
Linux libc, distribution, or macOS version. Kit reports an installation/runtime failure rather
than silently declaring the environment ready. Homebrew casks remain macOS-only.

```bash
kit add ripgrep                             # capture installed ripgrep and check the other OS
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
