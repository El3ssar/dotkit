# Reliability review — 2026-10-17

This review covers `0.3.0`. Existing dotfile repositories are not automatically migrated.

## Package contract

- Supported providers: selected built-in mise backends, including repository release
  assets; crates.io Cargo packages; Homebrew formulae; macOS-only Homebrew casks;
  and Kit's own bootstraps. No automatic distro-package, custom-command, arbitrary
  plugin, or mutable Git-source installation.
- A provider supporting two operating systems does not prove a package does.
  Binary recipe additions check provider release metadata for the declared OSes
  and x86_64/arm64. Missing URLs/checksums or Brew bottles reject the addition.
  Temporary probe files are removed; there is no Kit discovery database.
- Cargo/npm/Go/Python builds are explicitly conditional. They are allowed, but
  `kit packages check` reports that target compatibility remains unverified.
- Concrete mise/Cargo versions are required. Homebrew follows its current formula.
  Ordinary sync preserves existing tools from other managers. Explicit update
  checks for a newer version through the installed provider.
- Existing unsupported recipes remain inspectable. Use explicit recipes or
  `kit packages resolve --both`, then review `kit status`. Sync does not silently
  generate replacements or choose a different provider.

## Bugs and unsafe behavior addressed

| Problem | Result |
| --- | --- |
| Quiet, unbounded distro installs could appear stalled | Distro/custom install recipes are blocked; owned installers have live output, logs, no stdin, and timeouts |
| Timeout/cancellation could leave compilers/downloaders running | The runner stops its owned process group; timeout tests verify descendant cleanup |
| An installer could exit successfully without producing a tool | Local provisioning and remote Brew setup verify availability before recording success |
| Existing Cargo tools could be replaced by Brew | Availability takes precedence over recipe bookkeeping; updates retain Cargo ownership |
| Missing recipes or failed resolution could become invented/floating recipes | Recipes are explicitly resolved, pinned, checked, and saved only after successful validation |
| Empty/failed SSH OS detection was treated as Linux | OS detection must succeed and return a supported OS before upload |
| Provisioning launched user shell/editor configuration | Removed shell/editor warm-up and its unscoped installers/services |
| Setup continued after failed reconciliation or file conflicts | Setup stops after failed pulls/conflicts; hooks are skipped after package failures |
| Backup success concealed incomplete local setup | Output distinguishes repository backup from local setup completion |
| External checkout paths could traverse existing home-directory symlinks | Parent/path and `.git` symlinks are rejected locally and remotely |
| Invalid hook metadata could still execute | Invalid metadata and unsafe watched paths prevent execution |
| Optional package managers were listed as mandatory | Status requires Brew only for missing tracked Brew packages; mise bootstraps automatically |

## File manager

`kit files` shares the Ratatui/Crossterm event loop used by the package manager.
It lists tracked files only, shows file state and pending saved changes, and supports
search, batch selection, diffs, save, take-repo with backup, SSH/Linux scope toggles,
and untracking without deleting local files. Pending values are blue; cancellation
writes nothing. Selected file and ignore-rule snapshots are rechecked before applying.
A failed copy stops subsequent actions. Applying multiple files is not an atomic
filesystem transaction; completed actions remain applied if a later action fails.

## Adversarial refinement

Three independent reviewers attacked file/Git/recovery semantics, package provisioning,
and the CLI/TUI, then cross-reviewed fixes. Their reproducible findings are exercised by
`tests/adversarial_sync.py`, `tests/adversarial_ux.py`, and `tests/adversarial_inspect.py`.

Additional fixes:

- Source-root and hook symlinks/special files cannot escape scope or block metadata reads.
- Empty-directory undo and private file/directory modes survive recovery.
- External pins are exact; updates require a clean working tree and forward ancestry.
- Existing Rust roots and provider precedence survive activation.
- Shim creation cannot hide external Cargo binaries or pretend an absent tool is installed.
  Passive inventory uses one bounded offline provider query per command; it never runs tools.
- Post-install probes use the private environment; successful updates are confirmed against
  actual Cargo/mise metadata. Both mise JSON response shapes are supported.
- Missing platform recipes, GUI casks, private mise outside PATH, and destination aliases
  have explicit tests.
- Hooks and external Git operations are noninteractive, logged and bounded. Hooks can set
  `KIT_SCRIPT_TIMEOUT`; updates preserve local edits and commits.
- TUI package plans reject concurrent edits. Compact SSH layouts, help/review overlays,
  hidden-selection counts and scrollable diffs explain staged actions. Saving a deletion
  requires review; tiny panes cannot apply actions they cannot display.
- Offline doctor/dry-run emit structured diagnostics, never initialize snapshots/completions,
  and disable executable Git filters/fsmonitor while previewing saved changes. Malformed
  metadata, unusable merge bases, unsafe hooks and wrong-platform recipes fail visibly.

## Verification and limits

Local checks include the Rust suite, full CLI integration suites on macOS/Linux, deployment
scripts, the adversarial suites, strict Clippy, ShellCheck and formatting. Real cold installs
run in temporary homes without borrowing host tools on macOS ARM64 and Debian ARM64,
including provider inventory/update previews, Node, Cargo source builds, fizsh, retry and
partial failure. Earlier smoke coverage also includes Debian/Ubuntu x86_64. The real
localhost SSH test verifies push, repeated push, staging cleanup, command execution and
removal restoring an existing remote file.

CI runs the sandbox/adversarial suites on macOS and Ubuntu 22.04/24.04, checks Rust 1.88,
runs cold provisioning/fizsh on those operating systems, and uses a disposable Ubuntu Docker
SSH endpoint. See the PR checks for the exact final commit's results. The SSH fixture refuses
non-container execution; it uses temporary homes, keys, configuration and localhost only.

This does not certify every package, distribution, libc, macOS release or source build.
Homebrew formula upgrades are not locked to old bottle versions. Metadata checks establish
published artifact coverage, not runtime compatibility everywhere. No persistent verification
stamp or discovery database is stored. File actions are sequential rather than a filesystem
transaction; completed actions remain applied if a later action fails. Keyboard/render tests
and PTYs are distinct from desktop visual sign-off. Real SSH smoke coverage is Linux sshd,
not macOS sshd or SFTP/SCP.

The reliability baseline takes inspiration from chezmoi's explicit preview/apply and doctor
workflows ([official command overview](https://www.chezmoi.io/user-guide/command-overview/));
this review does not assert full feature parity with chezmoi's templates or encryption.
