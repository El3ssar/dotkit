#!/usr/bin/env python3
"""Adversarial TUI/CLI checks. Every operation uses the existing isolated fake HOME harness.

Build first, then run: python3 tests/adversarial_ux.py [--kit PATH]
"""
import argparse
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import run as harness


def package_manifest_changed_during_review_is_rejected(sb):
    m = harness.setup(sb)
    original = {"alpha": {"mac": "cargo:alpha@1.2.3", "linux": "cargo:alpha@1.2.3"}}
    m.repo_write("packages.json", json.dumps(original))
    changed = {"alpha": {"mac": "cargo:alpha@9.8.7", "linux": "cargo:alpha@9.8.7"}}
    output = harness.package_dialogue(m, b"/alpha\rd\r", expected=1,
        on_ready=lambda: m.repo_write("packages.json", json.dumps(changed)))
    harness.has(output, "changed while the manager was open", "No queued actions applied")
    harness.ok(harness.pkgs(m) == changed, "stale TUI removed a concurrently changed package")


def deleted_local_file_requires_review_before_saving_deletion(sb):
    m = harness.setup(sb, {"one.conf": "keep in repo\n"}, ["one.conf"])
    (m.home / "one.conf").unlink()
    original = (m.src / "home/one.conf").read_bytes()
    # First Enter must open review. First q closes review; second q cancels the manager.
    output = harness.package_dialogue(m, b"/one.conf\rs\rqq", command="files")
    harness.has(output, "SAVE DELETION")
    harness.ok((m.src / "home/one.conf").read_bytes() == original)
    harness.package_dialogue(m, b"/one.conf\rs\r\r", command="files")
    harness.ok(not (m.src / "home/one.conf").exists())


def missing_local_file_save_is_explained_and_keeps_repo(sb):
    m = harness.setup(sb)
    m.repo_write("home/one.conf", "repo version\n")
    output = harness.package_dialogue(m, b"/one.conf\rs\r", command="files")
    harness.has(output, "Nothing to save", "no file changes queued")
    harness.ok((m.src / "home/one.conf").read_text() == "repo version\n")


def inventory_json_modes_are_consistent_and_conflicts_rejected(sb):
    m = harness.setup(sb)
    for command in ("files", "packages"):
        parent = json.loads(m.kit(command, "--json", "list").out)
        child = json.loads(m.kit(command, "list", "--json").out)
        harness.ok(parent == child, f"inconsistent {command} JSON inventory")
        m.kit(command, "--json", "--tui", code=2)
        m.kit(command, "--tui", "list", code=2)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kit", default=str(Path(__file__).resolve().parent.parent / "target/debug/kit"))
    args = parser.parse_args()
    harness.KIT = harness.resolve_kit(args.kit)
    failures = 0
    for check in (package_manifest_changed_during_review_is_rejected,
                  deleted_local_file_requires_review_before_saving_deletion,
                  missing_local_file_save_is_explained_and_keeps_repo,
                  inventory_json_modes_are_consistent_and_conflicts_rejected):
        sandbox = harness.Sandbox("adversarial-ux-" + check.__name__)
        try:
            check(sandbox)
            print("PASS", check.__name__)
        except Exception as error:
            failures += 1
            print("FAIL", check.__name__, str(error), "sandbox:", sandbox.root)
        else:
            sandbox.cleanup()
    return int(failures != 0)


if __name__ == "__main__":
    raise SystemExit(main())
