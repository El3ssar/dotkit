#!/usr/bin/env python3
"""Adversarial sync regressions. Every operation uses the isolated CLI test harness."""
from pathlib import Path
import argparse
import importlib.util
import json
import os
import subprocess
import sys
import time

spec = importlib.util.spec_from_file_location("kit_harness", Path(__file__).with_name("run.py"))
h = importlib.util.module_from_spec(spec)
spec.loader.exec_module(h)


def source_root_symlink_never_written(sb):
    m = sb.machine(); m.init()
    outside = sb.root / "outside"; outside.mkdir()
    (m.src / "home").symlink_to(outside, target_is_directory=True)
    m.write("victim.conf", "local payload\n")
    result = m.kit("add", "~/victim.conf", code=1)
    h.has(result.err, "home/ must be a real directory")
    h.ok(not (outside / "victim.conf").exists(), "escaped repository home/ root")
    m.kit("sync", code=1)
    h.ok(not any(outside.iterdir()), "sync altered the outside directory")


def external_fixture(sb):
    m = sb.machine(); m.init()
    upstream = sb.root / "upstream"; upstream.mkdir()
    sb.git_raw(["init", "-q"], cwd=upstream)
    (upstream / "data").write_text("v1")
    sb.git_raw(["add", "."], cwd=upstream)
    sb.git_raw(["commit", "-qm", "one"], cwd=upstream)
    first = sb.git_raw(["rev-parse", "HEAD"], cwd=upstream).stdout.strip()
    (upstream / "data").write_text("v2")
    sb.git_raw(["commit", "-qam", "two"], cwd=upstream)
    second = sb.git_raw(["rev-parse", "HEAD"], cwd=upstream).stdout.strip()
    dest = m.home / "external"
    sb.git_raw(["clone", "-q", str(upstream), str(dest)])
    return m, upstream, dest, first, second


def external_pin_rejects_unadvertised_newer_commits(sb):
    m, upstream, dest, first, second = external_fixture(sb)
    m.repo_write("externals.json", json.dumps({"external": {"git": str(upstream), "ref": first}}))
    result = m.kit("sync", code=1)
    h.has(result.err, "would abandon commits", "left untouched")
    h.ok((dest / "data").read_text() == "v2")
    h.ok(sb.git_raw(["rev-parse", "HEAD"], cwd=dest).stdout.strip() == second)


def external_status_explains_newer_checkout_without_promising_downgrade(sb):
    m, upstream, dest, first, second = external_fixture(sb)
    m.repo_write("externals.json", json.dumps({"external": {"git": str(upstream), "ref": first}}))
    result = m.kit("status")
    line = next(line for line in result.out.splitlines() if line.startswith("External ~/external:"))
    h.has(line, "newer than pin", "sync will not downgrade", "externals.json")
    h.hasnt(line, "→ kit sync")
    h.ok(sb.git_raw(["rev-parse", "HEAD"], cwd=dest).stdout.strip() == second)


def external_pin_accepts_forward_updates(sb):
    m, upstream, dest, first, second = external_fixture(sb)
    sb.git_raw(["checkout", "-q", first], cwd=dest)
    m.repo_write("externals.json", json.dumps({"external": {"git": str(upstream), "ref": second}}))
    m.kit("sync")
    h.ok((dest / "data").read_text() == "v2")
    h.ok(sb.git_raw(["rev-parse", "HEAD"], cwd=dest).stdout.strip() == second)


def diff_symlink_target_cannot_inject_terminal_commands(sb):
    m = sb.machine(); m.init()
    link = m.home / "terminal-link"
    link.symlink_to("safe-target")
    m.kit("add", "~/terminal-link")
    link.unlink(); link.symlink_to("\x1b]52;c;Y2xpcGJvYXJk\x07")
    result = m.kit("status", "~/terminal-link")
    h.ok("\x1b" not in result.both and "\x07" not in result.both, "terminal command escaped diff")


def undo_restores_empty_directories_and_permissions(sb):
    m = sb.machine(); m.init()
    m.repo_write("home/empty", "repo file\n")
    empty = m.home / "empty"; empty.mkdir(mode=0o700)
    m.kit("undo", "~/empty")
    h.ok(empty.is_file())
    m.kit("undo")
    h.ok(empty.is_dir(), "undo lost the backed-up empty directory")
    h.ok(m.mode("empty") == 0o700, "undo changed private directory permissions")
    m.kit("undo")
    h.ok(empty.is_file(), "undoing a directory restore failed")


def undo_restores_nested_empty_directories_and_modes(sb):
    m = sb.machine(); m.init()
    m.repo_write("home/private", "repo file\n")
    root = m.home / "private"; root.mkdir(mode=0o700)
    (root / "empty-child").mkdir(mode=0o700)
    (root / "data").write_text("original\n")
    m.kit("undo", "~/private"); m.kit("undo")
    h.ok((root / "empty-child").is_dir(), "nested empty directory lost")
    h.ok((root / "data").read_text() == "original\n")
    h.ok(m.mode("private") == 0o700 and m.mode("private/empty-child") == 0o700)


def undo_never_widens_private_parent_permissions(sb):
    m = sb.machine(); m.init()
    m.write(".ssh/config", "local\n")
    (m.home / ".ssh").chmod(0o700)
    m.repo_write("home/.ssh/config", "repo\n")
    m.kit("undo", "~/.ssh/config"); m.kit("undo")
    h.ok(m.mode(".ssh") == 0o700, "backup parent relaxed .ssh permissions")


def undo_restores_original_private_file_permissions(sb):
    m = sb.machine(); m.init()
    m.repo_write("home/private-file", "repo\n")
    local = m.write("private-file", "local\n", mode=0o600)
    m.kit("undo", "~/private-file")
    local.unlink(); local.mkdir()
    m.kit("undo")
    h.ok(local.read_text() == "local\n")
    h.ok(m.mode("private-file") == 0o600, "undo exposed a private file")


def hooks_are_noninteractive_logged_and_failures_rerun(sb):
    m = sb.machine(); m.init()
    m.repo_write("scripts/check.sh", '#!/bin/sh\nif read answer; then exit 77; fi\necho visible-hook-output\necho attempt >> "$HOME/attempts"\nexit 9\n')
    first = m.kit("sync", code=1)
    h.has(first.out, "visible-hook-output")
    h.ok("visible-hook-output" in (m.state / "install-script-check.sh.log").read_text())
    m.kit("sync", code=1)
    h.ok(m.read("attempts") == "attempt\nattempt\n", "failed hook was marked done")


def hook_timeout_stops_background_children(sb):
    m = sb.machine(); m.init()
    m.repo_write("scripts/hang.sh", '#!/bin/sh\n(sleep 3; echo escaped > "$HOME/escaped") &\nsleep 20\n')
    started = time.monotonic()
    result = m.kit("sync", code=1, env={"KIT_SCRIPT_TIMEOUT": "1"}, timeout=15)
    h.ok(time.monotonic() - started < 10, "hook deadline not enforced")
    h.has(result.err, "script hang.sh failed")
    time.sleep(3.2)
    h.ok(not m.exists("escaped"), "timed-out hook left a running child")
    h.ok(not (m.state / "scripts.json").exists(), "timed-out hook marked successful")


def external_forward_update_preserves_branch_and_local_edits(sb):
    m, upstream, dest, first, second = external_fixture(sb)
    sb.git_raw(["checkout", "-q", "-b", "kept-branch", first], cwd=dest)
    m.repo_write("externals.json", json.dumps({"external": {"git": str(upstream), "ref": second}}))
    (dest / "data").write_text("local change\n")
    result = m.kit("sync", code=1)
    h.has(result.err, "local changes are present")
    h.ok((dest / "data").read_text() == "local change\n")
    sb.git_raw(["checkout", "--", "data"], cwd=dest)
    m.kit("sync")
    h.ok(sb.git_raw(["branch", "--show-current"], cwd=dest).stdout.strip() == "kept-branch")
    h.ok((dest / "data").read_text() == "v2")


def hook_metadata_never_reads_fifos_or_symlinks(sb):
    for kind in ("fifo", "fifo-link", "directory", "regular-link"):
        m = sb.machine(kind); m.init()
        hook = m.src / "scripts/unsafe.sh"
        if kind == "fifo": os.mkfifo(hook)
        elif kind == "fifo-link":
            target = sb.root / "outside-fifo"; os.mkfifo(target); hook.symlink_to(target)
        elif kind == "directory": hook.mkdir()
        else:
            target = sb.root / "outside-script"
            target.write_text('echo unsafe > "$HOME/escaped"\n'); hook.symlink_to(target)
        result = m.kit("sync", code=1, timeout=5)
        h.has(result.err, "regular")
        h.ok(not m.exists("escaped"), f"executed unsafe {kind} hook")


def hooks_root_never_follows_symlinks_or_special_files(sb):
    for kind in ("link", "fifo"):
        m = sb.machine("root-" + kind); m.init()
        scripts = m.src / "scripts"; scripts.rmdir()
        if kind == "fifo": os.mkfifo(scripts)
        else:
            outside = sb.root / "outside-hooks"; outside.mkdir()
            (outside / "unsafe.sh").write_text('echo unsafe > "$HOME/escaped"\n')
            scripts.symlink_to(outside, target_is_directory=True)
        result = m.kit("sync", code=1, timeout=5)
        h.has(result.err, "scripts/ must be a real directory")
        h.ok(not m.exists("escaped"), "followed symlinked scripts root")


TESTS = [external_status_explains_newer_checkout_without_promising_downgrade, hook_metadata_never_reads_fifos_or_symlinks, hooks_root_never_follows_symlinks_or_special_files, external_forward_update_preserves_branch_and_local_edits, hooks_are_noninteractive_logged_and_failures_rerun, hook_timeout_stops_background_children, undo_restores_original_private_file_permissions, undo_never_widens_private_parent_permissions, undo_restores_empty_directories_and_permissions, undo_restores_nested_empty_directories_and_modes, source_root_symlink_never_written, external_pin_rejects_unadvertised_newer_commits,
         external_pin_accepts_forward_updates, diff_symlink_target_cannot_inject_terminal_commands]
if __name__ == "__main__":
    parser = argparse.ArgumentParser(); parser.add_argument("--kit", default="target/debug/kit")
    args = parser.parse_args(); h.KIT = [str(Path(args.kit).resolve())]
    failures = 0
    for test in TESTS:
        name, status, elapsed, detail = h.run_one(test)
        print(f"{status} {name} ({elapsed:.2f}s)")
        if detail: print(detail)
        failures += status == "FAIL"
    sys.exit(bool(failures))
