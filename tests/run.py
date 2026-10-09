#!/usr/bin/env python3
"""Black-box test suite for kit (the dotfile manager).

    python3 tests/run.py [--kit CMD] [-k substring] [-v] [-j N]

CMD defaults to target/debug/kit (or $KIT_BIN); build first with `cargo build`. Also accepts
`--kit target/debug/kit`. Every test runs in its own sandbox: a fake $HOME, kit's repo and
state inside it, and a fakebin/ with logging stubs for brew, launchctl, clang, cargo, mise,
gh, delta, ssh, rsync and zsh. git is the real git. Nothing outside the sandbox is touched.
"""
from __future__ import annotations

import argparse
import concurrent.futures
import json
import os
import platform
import shlex
import shutil
import stat
import subprocess
import sys
import tempfile
import time
import traceback
from pathlib import Path

# Tests that fail against the Python reference because of a genuine bug in it.
# A failure here is reported as XFAIL (not a suite failure); a pass as XPASS.
EXPECTED_BUG: list = []

HERE = Path(__file__).resolve().parent
BASE = Path(os.environ.get("KIT_TEST_TMP") or
            os.path.join(tempfile.gettempdir(), "kittests"))
REAL_HOME = os.path.realpath(os.path.expanduser("~"))
PLAT = "mac" if platform.system() == "Darwin" else "linux"
OTHER = "linux" if PLAT == "mac" else "mac"
STUBS = ["brew", "launchctl", "clang", "cargo", "mise", "gh", "delta", "ssh", "rsync", "zsh"]
ZSH_SITE = Path("/opt/homebrew/share/zsh/site-functions/_kit")
KIT: list = []
VERBOSE = False

TESTS = []


def test(fn):
    TESTS.append(fn)
    return fn


class Fail(AssertionError):
    pass


# --------------------------------------------------------------------------- sandbox
class Result:
    def __init__(self, args, code, out, err):
        self.args, self.code, self.out, self.err = args, code, out, err

    @property
    def both(self):
        return self.out + self.err

    def show(self):
        return (f"$ kit {' '.join(shlex.quote(a) for a in self.args)}\n  exit {self.code}\n"
                f"  --- stdout ---\n{_indent(self.out)}\n  --- stderr ---\n{_indent(self.err)}")


def _indent(s):
    return "\n".join("    " + l for l in s.rstrip("\n").splitlines()) if s.strip() else "    (empty)"


class Sandbox:
    """One test's world: a root folder with fakebin/, calls.log and one or more machines."""

    def __init__(self, name):
        BASE.mkdir(parents=True, exist_ok=True)
        self.root = Path(os.path.realpath(tempfile.mkdtemp(prefix=name[:40] + "-", dir=BASE)))
        assert str(self.root).startswith(str(Path(os.path.realpath(BASE)))), self.root
        assert not REAL_HOME.startswith(str(self.root)) and str(self.root) != REAL_HOME
        self.history = []
        self.fakebin = self.root / "fakebin"
        self.stubcfg = self.root / "stubcfg"
        self.log = self.root / "calls.log"
        for d in (self.fakebin, self.stubcfg, self.root / "tmp", self.root / "xdg/data",
                  self.root / "xdg/config", self.root / "xdg/cache"):
            d.mkdir(parents=True)
        self.log.write_text("")
        (self.root / "gitconfig").write_text(
            "[user]\n\tname = Kit Tester\n\temail = kit@example.com\n[init]\n\tdefaultBranch = main\n"
            "[advice]\n\tdetachedHead = false\n[pull]\n\trebase = false\n[protocol \"file\"]\n\tallow = always\n")
        for name_ in STUBS:
            self.make_stub(name_)
        self.stub("zsh", code=0)   # `zsh -fc 'print -l $fpath'` prints nothing: no real fpath dir is used
        self.machines = {}

    def make_stub(self, name):
        p = self.fakebin / name
        p.write_text(f"""#!/bin/sh
{{ printf '%s' {shlex.quote(name)}; for a in "$@"; do printf ' %s' "$a"; done; printf '\\n'; }} >> {shlex.quote(str(self.log))}
cfg={shlex.quote(str(self.stubcfg))}
if [ -f "$cfg/{name}.sh" ]; then exec /bin/sh "$cfg/{name}.sh" "$@"; fi
code=1
if [ -f "$cfg/{name}.code" ]; then code=$(cat "$cfg/{name}.code"); fi
exit "$code"
""")
        p.chmod(0o755)

    def stub(self, name, code=None, script=None):
        if not (self.fakebin / name).exists():
            self.make_stub(name)
        if code is not None:
            (self.stubcfg / f"{name}.code").write_text(str(code))
        if script is not None:
            (self.stubcfg / f"{name}.sh").write_text(script)

    def unstub(self, name):
        (self.fakebin / name).unlink()

    def calls(self, name=None):
        lines = self.log.read_text().splitlines()
        return [l for l in lines if name is None or l.split(" ", 1)[0] == name]

    def machine(self, name="m1"):
        if name not in self.machines:
            self.machines[name] = Machine(self, name)
        return self.machines[name]

    def bare(self, name="remote.git"):
        p = self.root / name
        if not p.exists():
            self.git_raw(["init", "-q", "--bare", str(p)])
        return p

    def env(self, home):
        e = {
            "HOME": str(home),
            "PATH": f"{self.fakebin}:/usr/bin:/bin:/usr/sbin:/sbin",
            "KIT_SOURCE": f"{home}/.local/share/kit",
            "KIT_STATE": f"{home}/.local/state/kit",
            "XDG_DATA_HOME": str(self.root / "xdg/data"),
            "XDG_CONFIG_HOME": str(self.root / "xdg/config"),
            "XDG_STATE_HOME": str(self.root / "xdg/state"),
            "XDG_CACHE_HOME": str(self.root / "xdg/cache"),
            "GIT_CONFIG_GLOBAL": str(self.root / "gitconfig"),
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_AUTHOR_NAME": "Kit Tester", "GIT_AUTHOR_EMAIL": "kit@example.com",
            "GIT_COMMITTER_NAME": "Kit Tester", "GIT_COMMITTER_EMAIL": "kit@example.com",
            "GIT_TERMINAL_PROMPT": "0",
            "KIT_NO_DELTA": "1",
            "LANG": "en_US.UTF-8", "LC_ALL": "en_US.UTF-8",
            "TMPDIR": str(self.root / "tmp"),
            "USER": "tester", "LOGNAME": "tester", "SHELL": "/bin/sh", "TERM": "dumb",
            "PYTHONDONTWRITEBYTECODE": "1",
        }
        return e

    def git_raw(self, args, cwd=None, check=True):
        r = subprocess.run(["git", *args], cwd=cwd or self.root, env=self.env(self.root / "nohome"),
                           stdin=subprocess.DEVNULL, capture_output=True, text=True)
        if check and r.returncode != 0:
            raise Fail(f"git {' '.join(args)} failed:\n{r.stderr}")
        return r

    def make_git_repo(self, name, files):
        """A plain local git repo with the given files committed; returns its path."""
        p = self.root / name
        p.mkdir()
        self.git_raw(["init", "-q", str(p)])
        for rel, content in files.items():
            f = p / rel
            f.parent.mkdir(parents=True, exist_ok=True)
            f.write_text(content)
        self.git_raw(["add", "-A"], cwd=p)
        self.git_raw(["commit", "-q", "-m", "init"], cwd=p)
        return p

    def cleanup(self):
        shutil.rmtree(self.root, ignore_errors=True)


class Machine:
    def __init__(self, sb: Sandbox, name):
        self.sb = sb
        self.name = name
        self.home = sb.root / name / "home"
        self.home.mkdir(parents=True)
        self.src = self.home / ".local/share/kit"
        self.state = self.home / ".local/state/kit"
        self.envvars = sb.env(self.home)

    # --- running kit
    def kit(self, *args, code=0, env=None, cwd=None, input=None, timeout=60):
        e = dict(self.envvars, **(env or {}))
        assert os.path.realpath(e["HOME"]).startswith(str(self.sb.root)), "HOME must be inside the sandbox"
        assert os.path.realpath(e["HOME"]) != REAL_HOME
        cwd = Path(cwd or self.home)
        assert str(os.path.realpath(cwd)).startswith(str(self.sb.root))
        try:
            r = subprocess.run(KIT + [str(a) for a in args], cwd=cwd, env=e, capture_output=True,
                               stdin=subprocess.DEVNULL if input is None else None, input=input,
                               timeout=timeout, text=True, errors="replace")
            res = Result([str(a) for a in args], r.returncode, r.stdout, r.stderr)
        except subprocess.TimeoutExpired as ex:
            res = Result([str(a) for a in args], "TIMEOUT", str(ex.stdout or ""), str(ex.stderr or ""))
        self.sb.history.append(res)
        if code is not None and res.code != code:
            raise Fail(f"expected exit {code}, got {res.code}")
        if "Traceback (most recent call last)" in res.err or "panicked at" in res.err:
            raise Fail("crash (traceback/panic) in stderr")
        return res

    def init(self):
        return self.kit("init")

    def git(self, *args, check=True):
        r = subprocess.run(["git", "-C", str(self.src), *args], env=self.envvars, capture_output=True,
                           text=True, stdin=subprocess.DEVNULL)
        if check and r.returncode != 0:
            raise Fail(f"git {' '.join(args)} failed:\n{r.stderr}")
        return r

    # --- files
    def write(self, rel, content="x\n", mode=None):
        p = self.home / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        if isinstance(content, bytes):
            p.write_bytes(content)
        else:
            p.write_text(content)
        if mode is not None:
            p.chmod(mode)
        return p

    def read(self, rel):
        return (self.home / rel).read_text()

    def exists(self, rel):
        p = self.home / rel
        return p.exists() or p.is_symlink()

    def repo_write(self, rel, content="x\n", mode=None):
        """Write a file inside the repo (rel is relative to the repo root, e.g. home/.zshrc)."""
        p = self.src / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        if isinstance(content, bytes):
            p.write_bytes(content)
        else:
            p.write_text(content)
        if mode is not None:
            p.chmod(mode)
        return p

    def repo_read(self, rel):
        return (self.src / rel).read_text()

    def repo_has(self, rel):
        p = self.src / rel
        return p.exists() or p.is_symlink()

    def json(self, rel):
        return json.loads((self.src / rel).read_text())

    def kitignore(self):
        return parse_kitignore((self.src / ".kitignore").read_text())

    def mode(self, rel):
        return stat.S_IMODE(os.lstat(self.home / rel).st_mode)

    def tracked(self):
        return self.kit("managed").out.split()


def parse_kitignore(text):
    out, section = {"all": []}, "all"
    for line in text.splitlines():
        s = line.split("  #")[0].strip()
        if not s or s.startswith("#"):
            continue
        if s.startswith("[") and s.endswith("]") and s[1:-1].isidentifier():
            section = s[1:-1]
            out.setdefault(section, [])
            continue
        out[section].append(s)
    return out


def ok(cond, msg="assertion failed"):
    if not cond:
        raise Fail(msg)


def has(text, *needles):
    for n in needles:
        if n not in text:
            raise Fail(f"expected to find {n!r} in output")


def hasnt(text, *needles):
    for n in needles:
        if n in text:
            raise Fail(f"did not expect {n!r} in output")


def setup(sb, files=None, add=None):
    """A machine with a fresh repo, some files in $HOME, optionally `kit add`ed."""
    m = sb.machine()
    m.init()
    for rel, content in (files or {}).items():
        m.write(rel, content)
    if add:
        m.kit("add", *[f"~/{a}" for a in add])
    return m


TOKEN = "ghp_" + "A1b2C3d4E5" * 4
MISE_STUB = """case "$1" in
  latest) echo 1.2.3; exit 0;;
  registry) echo "$2 aqua:owner/$2 ubi:x/$2"; exit 0;;
  *) exit 0;;
esac
"""


# mise that installs fine but knows no versions (on Linux, `kit pkg add` really installs with mise)
MISE_USE_OK = """case "$1" in use|uninstall|install) exit 0;; *) exit 1;; esac
"""


# =========================================================================== init
@test
def init_creates_repo(sb):
    m = sb.machine()
    r = m.kit("init")
    has(r.out, "new kit repo")
    ok((m.src / ".git").is_dir(), "no .git")
    has(m.repo_read(".gitignore"), ".kit/")
    ki = m.kitignore()
    ok(".DS_Store" in ki["all"] and "remote" in ki, ".kitignore missing defaults or [remote]")
    ok(m.json("packages.json") == {}, "packages.json")
    ok(m.json("rules.json") == [], "rules.json")
    ok(m.json("externals.json") == {}, "externals.json")
    ok(m.json("dirs.json") == [], "dirs.json")
    ok((m.src / "scripts").is_dir(), "scripts/")
    has(m.kit("init").out, "already")


@test
def init_clone_from_bare_repo(sb):
    m1 = setup(sb, {".zshrc": "from m1\n"}, add=[".zshrc"])
    bare = sb.bare()
    m1.kit("git", "remote", "add", "origin", str(bare))
    m1.kit("save", "first")
    m2 = sb.machine("m2")
    r = m2.kit("init", str(bare))
    has(r.out, "cloned")
    ok((m2.src / "home/.zshrc").is_file(), "clone has no home/.zshrc")
    ok(not m2.exists(".zshrc"), "init must not apply")
    m2.kit("apply", "--no-packages")
    ok(m2.read(".zshrc") == "from m1\n", "apply after clone")


@test
def commands_need_a_repo(sb):
    m = sb.machine()
    r = m.kit("status", code=1)
    has(r.err, "kit init")


# =========================================================================== add
@test
def add_file(sb):
    m = setup(sb, {".zshrc": "hi\n"})
    r = m.kit("add", "~/.zshrc")
    has(r.out, "tracking 1 file(s)")
    ok(m.repo_read("home/.zshrc") == "hi\n")
    ok(json.loads((m.state / "synced.json").read_text()).get(".zshrc"), "synced.json records it")


@test
def add_relative_and_absolute_paths(sb):
    m = setup(sb, {".a": "a\n", ".b": "b\n"})
    m.kit("add", ".a")                       # relative to cwd (= $HOME)
    m.kit("add", str(m.home / ".b"))         # absolute
    ok(m.tracked() == [".a", ".b"], m.tracked())


@test
def add_folder_updates_dirs_json(sb):
    m = setup(sb, {".config/nvim/init.lua": "x\n", ".config/nvim/lua/a.lua": "y\n"})
    r = m.kit("add", "~/.config/nvim")
    has(r.out, "tracking 2 file(s)")
    ok(m.json("dirs.json") == [".config/nvim"], m.json("dirs.json"))
    ok(m.repo_read("home/.config/nvim/lua/a.lua") == "y\n")


@test
def add_symlink(sb):
    m = setup(sb, {".config/vim/vimrc": "set nu\n"})
    os.symlink(".config/vim/vimrc", m.home / ".vimrc")
    m.kit("add", "~/.vimrc")
    p = m.src / "home/.vimrc"
    ok(p.is_symlink() and os.readlink(p) == ".config/vim/vimrc", "repo keeps the symlink")
    os.symlink("/etc/hosts", m.home / ".abs")
    r = m.kit("add", "~/.abs")
    has(r.err, "absolute path")


@test
def add_whole_home_refused(sb):
    m = setup(sb)
    r = m.kit("add", "~", code=1)
    has(r.err, "whole home")
    ok(m.tracked() == [])


@test
def add_folder_containing_kit_refused(sb):
    m = setup(sb, {".local/bin/tool": "#!/bin/sh\n"})
    r = m.kit("add", "~/.local", code=1)
    has(r.err, "kit's own folders")
    ok(m.tracked() == [])
    m.kit("add", "~/.local/bin")
    ok(m.tracked() == [".local/bin/tool"])


@test
def add_kit_repo_itself_refused(sb):
    m = setup(sb)
    m.kit("add", "~/.local/share/kit", code=1)
    ok(m.tracked() == [])


@test
def add_ignored_file_refused(sb):
    m = setup(sb, {"notes.swp": "x\n"})
    r = m.kit("add", "~/notes.swp", code=1)
    has(r.err, ".kitignore line")
    ok(not m.repo_has("home/notes.swp"))


@test
def add_force_ignored_file_exits_zero(sb):
    m = setup(sb, {"notes.swp": "x\n"})
    m.kit("add", "--force", "~/notes.swp", code=0)
    ok(m.repo_has("home/notes.swp"))


@test
def add_force_ignored_file_tracks_it(sb):
    m = setup(sb, {"notes.swp": "x\n"})
    m.kit("add", "--force", "~/notes.swp", code=None)
    ok(m.repo_has("home/notes.swp"), "--force adds an ignored file")


@test
def add_missing_path_fails(sb):
    m = setup(sb)
    r = m.kit("add", "~/.nope", code=1)
    has(r.err, "does not exist")


@test
def add_secret_by_name_refused(sb):
    m = setup(sb, {".ssh/id_rsa": "not really a key\n"})
    r = m.kit("add", "~/.ssh/id_rsa", code=1)
    has(r.err, "looks like a secret")
    ok(not m.repo_has("home/.ssh/id_rsa"))
    m.kit("add", "--force", "~/.ssh/id_rsa")
    ok(m.repo_has("home/.ssh/id_rsa"))


@test
def add_secret_by_content_refused(sb):
    m = setup(sb, {".config/app/env": f"export X=1\nexport GH={TOKEN}\n",
                   ".config/other/conf": "api_key=abcdef0123456789abcd\n"})
    r = m.kit("add", "~/.config/app/env", code=1)
    has(r.err, "secret", "line 2")
    hasnt(r.both, TOKEN)
    r = m.kit("add", "~/.config/other/conf", code=1)
    has(r.err, "secret")
    ok(m.tracked() == [])
    m.kit("add", "--force", "~/.config/app/env")
    ok(m.tracked() == [".config/app/env"])


@test
def add_folder_skips_secrets_and_ignored(sb):
    m = setup(sb, {".config/app/a": "a\n", ".config/app/x.swp": "s\n",
                   ".config/app/tok": f"{TOKEN}\n", ".config/app/.DS_Store": "d"})
    r = m.kit("add", "~/.config/app", code=1)   # the secret is reported
    has(r.out, "tracking 1 file(s)")
    ok(m.tracked() == [".config/app/a"], m.tracked())


@test
def add_no_servers_writes_remote_section(sb):
    m = setup(sb, {".config/kitty/kitty.conf": "x\n"})
    r = m.kit("add", "--no-servers", "~/.config/kitty")
    has(r.out, "[remote]")
    ki = m.kitignore()
    ok(".config/kitty" in ki["remote"] and ".config/kitty" not in ki["all"], ki)


@test
def add_mentions_what_travels_to_servers(sb):
    m = setup(sb, {".zshrc": "x\n", ".config/a/b": "x\n"})
    has(m.kit("add", "~/.zshrc").out, "travel to servers")
    hasnt(m.kit("add", "~/.config/a").out, "travel to servers")


# =========================================================================== status
def status_line(out, label, path):
    return any(label in l and l.rstrip().endswith(path) for l in out.splitlines())


def check_status(m, label, path, *args):
    r = m.kit("status", "-v", *args, code=None)
    ok(status_line(r.out, label, path), f"expected '{label}  {path}' in status")
    return r


@test
def status_changed_here(sb):
    m = setup(sb, {".zshrc": "a\n"}, add=[".zshrc"])
    m.write(".zshrc", "b\n")
    r = check_status(m, "changed here", "~/.zshrc")
    has(r.out, "kit re-add")


@test
def status_changed_in_repo(sb):
    m = setup(sb, {".zshrc": "a\n"}, add=[".zshrc"])
    m.repo_write("home/.zshrc", "b\n")
    check_status(m, "changed in repo", "~/.zshrc")


@test
def status_changed_on_both(sb):
    m = setup(sb, {".zshrc": "a\n"}, add=[".zshrc"])
    m.repo_write("home/.zshrc", "b\n")
    m.write(".zshrc", "c\n")
    check_status(m, "changed on both", "~/.zshrc")


@test
def status_missing_here(sb):
    m = setup(sb)
    m.repo_write("home/.gitconfig", "[user]\n")
    check_status(m, "missing here", "~/.gitconfig")


@test
def status_deleted_here(sb):
    m = setup(sb, {".zshrc": "a\n"}, add=[".zshrc"])
    (m.home / ".zshrc").unlink()
    check_status(m, "deleted here", "~/.zshrc")


@test
def status_new_here(sb):
    m = setup(sb, {".config/app/a": "a\n"}, add=[".config/app"])
    m.write(".config/app/b", "b\n")
    r = check_status(m, "new here", "~/.config/app/b")
    has(r.out, "kit ignore")


@test
def status_conflict_folder_vs_file(sb):
    m = setup(sb, {".zshrc": "a\n"}, add=[".zshrc"])
    (m.home / ".zshrc").unlink()
    m.write(".zshrc/inner", "x\n")
    check_status(m, "conflict", "~/.zshrc")


@test
def status_conflict_folder_vs_symlink(sb):
    m = setup(sb, {".config/foo/x": "x\n"})
    (m.src / "home/.config").mkdir(parents=True)
    os.symlink("../bar", m.src / "home/.config/foo")
    check_status(m, "conflict", "~/.config/foo")


@test
def status_in_sync_and_executable_bit(sb):
    m = setup(sb, {".local/bin/t": "#!/bin/sh\n"}, add=[".local/bin/t"])
    r = m.kit("status", "~/.local/bin/t")
    has(r.out, "in sync")
    (m.home / ".local/bin/t").chmod(0o755)
    check_status(m, "changed here", "~/.local/bin/t")


@test
def status_groups_big_folders(sb):
    m = setup(sb, {f".config/app/f{i}": "a\n" for i in range(5)}, add=[".config/app"])
    for i in range(5):
        m.write(f".config/app/f{i}", "changed\n")
    m.write(".zshrc", "z\n")
    r = m.kit("status", code=None)
    has(r.out, "~/.config/app/ (5 files)", "kit status -v")
    hasnt(r.out, "~/.config/app/f0")
    r = m.kit("status", "-v", code=None)
    for i in range(5):
        has(r.out, f"~/.config/app/f{i}")
    hasnt(r.out, "(5 files)")


@test
def status_three_files_not_grouped(sb):
    m = setup(sb, {f".config/app/f{i}": "a\n" for i in range(3)}, add=[".config/app"])
    for i in range(3):
        m.write(f".config/app/f{i}", "changed\n")
    r = m.kit("status", code=None)
    has(r.out, "~/.config/app/f0", "~/.config/app/f2")
    hasnt(r.out, "files)")


@test
def status_path_scope(sb):
    m = setup(sb, {".zshrc": "a\n", ".bashrc": "b\n"}, add=[".zshrc", ".bashrc"])
    m.write(".bashrc", "changed\n")
    r = m.kit("status", "~/.zshrc")
    has(r.out, "in sync")
    hasnt(r.out, ".bashrc")
    r = m.kit("status", "~/.bashrc", code=None)
    has(r.out, "~/.bashrc")


@test
def status_untracked_path_exits_1(sb):
    m = setup(sb, {".zshrc": "a\n", ".other": "o\n"}, add=[".zshrc"])
    r = m.kit("status", "~/.other", code=1)
    has(r.err, "not tracked")


@test
def status_lookalike_prefix_folders(sb):
    m = setup(sb, {".config/app/a": "a\n", ".config/application/b": "b\n"},
              add=[".config/app", ".config/application"])
    for i in range(4):
        m.write(f".config/application/n{i}", "n\n")
    r = m.kit("status", code=None)
    has(r.out, "~/.config/application/ (4 files)")
    hasnt(r.out, "~/.config/app/ (")
    r = m.kit("status", "~/.config/app")
    has(r.out, "in sync")
    hasnt(r.out, "application")
    m.write(".config/app/a", "changed\n")
    r = m.kit("status", "-v", "~/.config/app", code=None)
    has(r.out, "~/.config/app/a")
    hasnt(r.out, "application")


@test
def status_everything_in_sync_message(sb):
    m = setup(sb, {".zshrc": "a\n"}, add=[".zshrc"])
    m.kit("save", "x")
    sb.stub("delta", code=0)
    sb.stub("bat", code=0)   # core packages present
    r = m.kit("status")
    has(r.out, "everything in sync")


@test
def status_core_packages_not_installed(sb):
    m = setup(sb)
    sb.unstub("delta")
    r = m.kit("status")
    line = next((l for l in r.out.splitlines() if "not installed" in l), "")
    ok("bat" in line and "delta" in line, f"core packages line: {line!r}")
    ok(m.json("packages.json") == {})


# =========================================================================== apply
@test
def apply_writes_missing(sb):
    m = setup(sb)
    m.repo_write("home/.config/git/config", "[core]\n")
    r = m.kit("apply", "--no-packages")
    has(r.out, "wrote ~/.config/git/config")
    ok(m.read(".config/git/config") == "[core]\n")
    has(m.kit("apply", "--no-packages").out, "already in sync")


@test
def apply_takes_repo_changes(sb):
    m = setup(sb, {".zshrc": "a\n"}, add=[".zshrc"])
    m.repo_write("home/.zshrc", "b\n")
    m.kit("apply", "--no-packages")
    ok(m.read(".zshrc") == "b\n")
    ok(not (m.state / "backups").exists() or m.kit("restore").out.count("file(s)") <= 1)


@test
def apply_leaves_local_changes_without_tty(sb):
    m = setup(sb, {".zshrc": "a\n", ".bashrc": "b\n"}, add=[".zshrc", ".bashrc"])
    m.write(".zshrc", "mine\n")
    m.repo_write("home/.bashrc", "theirs\n")
    m.write(".bashrc", "mine too\n")
    r = m.kit("apply", "--no-packages")
    ok(m.read(".zshrc") == "mine\n" and m.read(".bashrc") == "mine too\n", "local changes overwritten")
    has(r.err, "left alone", "~/.zshrc", "~/.bashrc", "changed here", "changed on both")


@test
def apply_force_overwrites_and_backs_up(sb):
    m = setup(sb, {".zshrc": "a\n"}, add=[".zshrc"])
    m.write(".zshrc", "mine\n")
    r = m.kit("apply", "--force", "--no-packages")
    has(r.out, "kit restore")
    ok(m.read(".zshrc") == "a\n")
    stamps = list((m.state / "backups").iterdir())
    ok(len(stamps) == 1 and (stamps[0] / ".zshrc").read_text() == "mine\n", "backup content")
    r = m.kit("restore")
    has(r.out, stamps[0].name, ".zshrc")
    r = m.kit("undo", code=1)   # no tty, no --yes
    has(r.err, "--yes")
    r = m.kit("undo", "-y")
    has(r.out, "restored ~/.zshrc")
    ok(m.read(".zshrc") == "mine\n", "undo put it back")


@test
def restore_named_stamp_and_unknown(sb):
    m = setup(sb, {".zshrc": "a\n"}, add=[".zshrc"])
    has(m.kit("restore").out, "no backups")
    m.write(".zshrc", "mine\n")
    m.kit("apply", "--force", "~/.zshrc")
    stamp = next((m.state / "backups").iterdir()).name
    m.kit("restore", stamp, "--yes")
    ok(m.read(".zshrc") == "mine\n")
    r = m.kit("restore", "19990101-000000", "--yes", code=1)
    has(r.err, "no backup")


@test
def apply_never_deletes_folder_without_force_and_undo(sb):
    m = setup(sb, {".config/foo/x": "precious\n"})
    (m.src / "home/.config").mkdir(parents=True)
    os.symlink("../bar", m.src / "home/.config/foo")
    r = m.kit("apply", "--no-packages")
    ok((m.home / ".config/foo").is_dir() and not (m.home / ".config/foo").is_symlink(), "folder removed")
    ok(m.read(".config/foo/x") == "precious\n")
    has(r.err, "conflict")
    m.kit("apply", "--force", "--no-packages")
    p = m.home / ".config/foo"
    ok(p.is_symlink() and os.readlink(p) == "../bar", "--force replaces the folder with the symlink")
    m.kit("undo", "-y")
    ok(p.is_dir() and not p.is_symlink(), "undo brings the folder back")
    ok(m.read(".config/foo/x") == "precious\n")


@test
def apply_dry_run_changes_nothing(sb):
    m = setup(sb, {".zshrc": "a\n"}, add=[".zshrc"])
    m.repo_write("home/.zshrc", "b\n")
    m.repo_write("home/.newfile", "n\n")
    synced = (m.state / "synced.json").read_text()
    r = m.kit("apply", "-n", "--no-packages")
    has(r.out, "would write ~/.zshrc", "would write ~/.newfile")
    ok(m.read(".zshrc") == "a\n" and not m.exists(".newfile"), "dry run wrote files")
    ok((m.state / "synced.json").read_text() == synced, "dry run changed synced.json")
    ok(not (m.state / "backups").exists())
    r = m.kit("apply", "-n", "--force", "--no-packages")
    ok(m.read(".zshrc") == "a\n")


@test
def apply_refuses_conflict_markers(sb):
    m = setup(sb, {".zshrc": "a\n"}, add=[".zshrc"])
    m.repo_write("home/.zshrc", "<<<<<<< HEAD\nx\n=======\ny\n>>>>>>> other\n")
    r = m.kit("apply", "--no-packages", code=1)
    has(r.err, "conflict markers")
    ok(m.read(".zshrc") == "a\n")


@test
def apply_runs_packages_and_scripts_unless_told_not_to(sb):
    m = setup(sb)
    m.repo_write("scripts/hello.sh", 'echo ran >> "$HOME/ran"\n')
    m.kit("apply", "--no-packages", "--no-scripts")
    ok(not m.exists("ran"), "--no-scripts ran a script")
    ok(sb.calls("brew") == [], "--no-packages called brew")
    if PLAT == "mac":
        sb.stub("brew", code=0)
        m.kit("apply")
        ok(any("install" in c and "bat" in c for c in sb.calls("brew")), sb.calls())
        ok(m.exists("ran"), "scripts run by default")


@test
def apply_creates_ssh_dir_private(sb):
    m = setup(sb)
    m.repo_write("home/.ssh/config", "Host x\n")
    m.repo_write("home/.config/app/conf", "x\n")
    m.kit("apply", "--no-packages")
    ok(m.mode(".ssh") == 0o700, oct(m.mode(".ssh")))
    ok(m.mode(".ssh/config") == 0o600, oct(m.mode(".ssh/config")))
    ok(m.mode(".config/app/conf") == 0o644, oct(m.mode(".config/app/conf")))


@test
def apply_preserves_executable_bit(sb):
    m = setup(sb)
    m.repo_write("home/.local/bin/tool", "#!/bin/sh\necho hi\n", mode=0o755)
    m.kit("apply", "--no-packages")
    ok(m.mode(".local/bin/tool") & 0o111, "not executable")


@test
def apply_keeps_local_permissions(sb):
    m = setup(sb, {".zshrc": "a\n"}, add=[".zshrc"])
    (m.home / ".zshrc").chmod(0o600)
    m.repo_write("home/.zshrc", "b\n")
    m.kit("apply", "--no-packages")
    ok(m.read(".zshrc") == "b\n")
    ok(m.mode(".zshrc") == 0o600, oct(m.mode(".zshrc")))


@test
def apply_path_scope(sb):
    m = setup(sb, {".a": "a\n", ".b": "b\n"}, add=[".a", ".b"])
    m.repo_write("home/.a", "A\n")
    m.repo_write("home/.b", "B\n")
    m.kit("apply", "~/.a")
    ok(m.read(".a") == "A\n" and m.read(".b") == "b\n")


@test
def apply_file_where_folder_expected_fails_cleanly(sb):
    m = setup(sb)
    m.write(".config", "i am a file\n")
    m.repo_write("home/.config/app/x", "x\n")
    r = m.kit("apply", "--no-packages", code=1)
    has(r.err, ".config")
    ok(m.read(".config") == "i am a file\n")


@test
def edit_changes_repo_and_applies(sb):
    m = setup(sb, {".zshrc": "a\n"}, add=[".zshrc"])
    ed = sb.root / "editor.sh"
    ed.write_text('#!/bin/sh\necho edited >> "$1"\n')
    ed.chmod(0o755)
    m.kit("edit", "~/.zshrc", env={"EDITOR": str(ed), "VISUAL": str(ed)})
    ok(m.repo_read("home/.zshrc") == "a\nedited\n")
    ok(m.read(".zshrc") == "a\nedited\n")
    m.kit("edit", "~/.nope", env={"EDITOR": str(ed)}, code=1)


# =========================================================================== re-add
@test
def re_add_copies_changes(sb):
    m = setup(sb, {".zshrc": "a\n"}, add=[".zshrc"])
    m.write(".zshrc", "b\n")
    r = m.kit("re-add")
    has(r.out, "1 change(s)")
    ok(m.repo_read("home/.zshrc") == "b\n")
    has(m.kit("status", "~/.zshrc").out, "in sync")


@test
def re_add_new_and_deleted_files(sb):
    m = setup(sb, {".config/app/a": "a\n", ".config/app/b": "b\n"}, add=[".config/app"])
    m.write(".config/app/c", "c\n")
    (m.home / ".config/app/b").unlink()
    r = m.kit("re-add")
    has(r.out, "added", ".config/app/c", "removed", ".config/app/b")
    ok(m.repo_has("home/.config/app/c") and not m.repo_has("home/.config/app/b"))


@test
def re_add_skips_both_without_force(sb):
    m = setup(sb, {".zshrc": "a\n"}, add=[".zshrc"])
    m.repo_write("home/.zshrc", "repo\n")
    m.write(".zshrc", "mine\n")
    r = m.kit("re-add")
    has(r.err, "changed here AND in the repo")
    ok(m.repo_read("home/.zshrc") == "repo\n")
    m.kit("re-add", "--force")
    ok(m.repo_read("home/.zshrc") == "mine\n")


@test
def re_add_leaves_repo_changes(sb):
    m = setup(sb, {".zshrc": "a\n"}, add=[".zshrc"])
    m.repo_write("home/.zshrc", "repo\n")
    m.kit("re-add")
    ok(m.repo_read("home/.zshrc") == "repo\n", "re-add overwrote a repo change")


@test
def re_add_skips_files_with_rules(sb):
    m = setup(sb, {".zshrc": "alias x=pbcopy\nok\n"}, add=[".zshrc"])
    (m.src / "rules.json").write_text(json.dumps([{"path": ".zshrc", "on": [PLAT], "delete_lines": ["pbcopy"]}]))
    m.kit("apply", "--no-packages", "--force")
    ok(m.read(".zshrc") == "ok\n")
    m.write(".zshrc", "ok\nmore\n")
    r = m.kit("re-add")
    has(r.err, "rules")
    ok(m.repo_read("home/.zshrc") == "alias x=pbcopy\nok\n")


@test
def re_add_refuses_secret_content(sb):
    m = setup(sb, {".config/app/env": "x=1\n"}, add=[".config/app/env"])
    m.write(".config/app/env", f"x={TOKEN}\n")
    r = m.kit("re-add", code=1)
    has(r.err, "secret")
    hasnt(r.both, TOKEN)
    ok(m.repo_read("home/.config/app/env") == "x=1\n")
    m.kit("re-add", "--force")
    has(m.repo_read("home/.config/app/env"), TOKEN)


# =========================================================================== forget
@test
def forget_file(sb):
    m = setup(sb, {".zshrc": "a\n"}, add=[".zshrc"])
    r = m.kit("forget", "~/.zshrc")
    has(r.out, "forgot")
    ok(not m.repo_has("home/.zshrc") and m.read(".zshrc") == "a\n")
    m.kit("forget", "~/.zshrc", code=1)


@test
def forget_folder(sb):
    m = setup(sb, {".config/app/a": "a\n", ".config/app/b": "b\n"}, add=[".config/app"])
    r = m.kit("rm", "~/.config/app")
    has(r.out, "2 file(s)")
    ok(m.json("dirs.json") == [] and m.tracked() == [])
    ok(m.exists(".config/app/a"))


@test
def forget_inside_tracked_folder_writes_global_ignore(sb):
    m = setup(sb, {".config/app/a": "a\n", ".config/app/cache": "c\n"}, add=[".config/app"])
    (m.src / ".kitignore").write_text("[mac]\n.Trash\n\n[remote]\n")
    m.kit("forget", "~/.config/app/cache")
    lines = (m.src / ".kitignore").read_text().splitlines()
    first_header = next(i for i, l in enumerate(lines) if l.startswith("["))
    idx = next((i for i, l in enumerate(lines) if l.startswith(".config/app/cache") and "forgotten" in l), None)
    ok(idx is not None and idx < first_header, f"forgotten line not in the global section: {lines}")
    r = m.kit("status", "-v", code=None)
    hasnt(r.out, "cache")
    ok(m.json("dirs.json") == [".config/app"])
    # add brings it back and drops the forgotten line
    r = m.kit("add", "~/.config/app/cache")
    has(r.out, "forgotten before")
    hasnt((m.src / ".kitignore").read_text(), ".config/app/cache")
    ok(".config/app/cache" in m.tracked())


@test
def forget_untracked_file_in_tracked_folder(sb):
    m = setup(sb, {".config/app/a": "a\n"}, add=[".config/app"])
    m.write(".config/app/junk", "j\n")
    r = m.kit("forget", "~/.config/app/junk")
    has(r.out, "won't show up")
    hasnt(m.kit("status", "-v", code=None).out, "junk")


@test
def forget_through_tracked_symlink_refused(sb):
    m = setup(sb, {"realdir/f": "keep me\n"})
    os.makedirs(m.home / ".config")
    os.symlink("../realdir", m.home / ".config/link")
    m.kit("add", "~/.config/link")
    ok((m.src / "home/.config/link").is_symlink())
    r = m.kit("forget", "~/.config/link/f", code=1)
    has(r.err, "symlink")
    ok(m.read("realdir/f") == "keep me\n", "real file touched")
    ok((m.src / "home/.config/link").is_symlink())


# =========================================================================== ignore
@test
def ignore_global_remote_mac(sb):
    m = setup(sb)
    m.kit("ignore", "~/.config/app/cache.db")
    m.kit("ignore", "--remote", "~/.config/kitty")
    r = m.kit("ignore", "--mac", "~/.config/linuxonly")
    has(r.out, "[mac]")
    ki = m.kitignore()
    ok(".config/app/cache.db" in ki["all"], ki)
    ok(".config/kitty" in ki["remote"], ki)
    ok(".config/linuxonly" in ki.get("mac", []), ki)
    m.kit("ignore", "--linux", "~/.x")
    ok(".x" in m.kitignore().get("linux", []))
    m.kit("ignore", "--mac", "--linux", "~/.y", code=2)


@test
def ignore_tracked_file_notes_it(sb):
    m = setup(sb, {".zshrc": "a\n"}, add=[".zshrc"])
    has(m.kit("ignore", "~/.zshrc").out, "already tracked")


@test
def ignore_escapes_patterns(sb):
    m = setup(sb, {".config/app/a": "a\n"}, add=[".config/app"])
    for n in ("weird[1].txt", "weird1.txt", "a*b.txt", "axxb.txt"):
        m.write(f".config/app/{n}", "n\n")
    m.kit("ignore", "~/.config/app/weird[1].txt", "~/.config/app/a*b.txt")
    text = (m.src / ".kitignore").read_text()
    hasnt(text.splitlines(), ".config/app/weird[1].txt")
    r = m.kit("status", "-v", code=None)
    has(r.out, "~/.config/app/weird1.txt", "~/.config/app/axxb.txt")
    hasnt(r.out, "~/.config/app/weird[1].txt", "~/.config/app/a*b.txt")


# =========================================================================== .kitignore semantics
def kitignore_case(sb, patterns, files):
    m = setup(sb)
    (m.src / ".kitignore").write_text(patterns)
    for f in files:
        m.write(f, "x\n")
    m.kit("add", "~/.config/app", code=None)
    return set(m.tracked())


@test
def kitignore_bare_name_matches_any_component(sb):
    t = kitignore_case(sb, "cache\n", [".config/app/cache/x", ".config/app/sub/cache", ".config/app/cached", ".config/app/a"])
    ok(t == {".config/app/cached", ".config/app/a"}, t)


@test
def kitignore_path_and_dir_patterns(sb):
    t = kitignore_case(sb, ".config/app/logs/\n.config/app/*.tmp\n",
                       [".config/app/logs/l1", ".config/app/logs/sub/l2", ".config/app/x.tmp",
                        ".config/app/a", ".config/other/logs"])
    ok(t == {".config/app/a"}, t)


@test
def kitignore_star_does_not_cross_folders(sb):
    # .kitignore is documented as gitignore-style: `*` must not match a `/`
    t = kitignore_case(sb, ".config/app/*.tmp\n", [".config/app/x.tmp", ".config/app/sub/y.tmp", ".config/app/a"])
    ok(t == {".config/app/sub/y.tmp", ".config/app/a"}, t)


@test
def kitignore_double_star(sb):
    t = kitignore_case(sb, "**/*.log\n.config/app/deep/**\n",
                       [".config/app/a.log", ".config/app/s/t/b.log", ".config/app/deep/x/y", ".config/app/keep"])
    ok(t == {".config/app/keep"}, t)


@test
def kitignore_negation(sb):
    t = kitignore_case(sb, "*.log\n!keep.log\n", [".config/app/a.log", ".config/app/keep.log", ".config/app/b"])
    ok(t == {".config/app/keep.log", ".config/app/b"}, t)


@test
def kitignore_platform_sections(sb):
    t = kitignore_case(sb, f"[{PLAT}]\nhere-only\n[{OTHER}]\nthere-only\n[remote]\nsrv\n",
                       [".config/app/here-only", ".config/app/there-only", ".config/app/srv"])
    ok(t == {".config/app/there-only", ".config/app/srv"}, t)


@test
def kitignore_trailing_comment(sb):
    t = kitignore_case(sb, "*.bak  # backups\n", [".config/app/x.bak", ".config/app/y"])
    ok(t == {".config/app/y"}, t)


# =========================================================================== rules.json
@test
def rules_applied_for_this_platform(sb):
    m = setup(sb)
    m.repo_write("home/.zshrc", "alias c=pbcopy\nexport A=1\nfoo here\nfoo2\n")
    (m.src / "rules.json").write_text(json.dumps([
        {"path": ".zshrc", "on": [PLAT], "delete_lines": ["pbcopy"], "replace": [["foo", "bar"]],
         "regex": [["^export A=[0-9]+$", "export A=2"]]},
        {"path": ".zshrc", "on": [OTHER], "replace": [["export", "EXPORT"]]},
    ]))
    m.kit("apply", "--no-packages")
    ok(m.read(".zshrc") == "export A=2\nbar here\nbar2\n", repr(m.read(".zshrc")))
    ok(m.kit("cat", "~/.zshrc").out == "export A=2\nbar here\nbar2\n")
    has(m.kit("status", "~/.zshrc").out, "in sync")


@test
def rules_invalid_shape_clean_error(sb):
    m = setup(sb, {".zshrc": "a\n"}, add=[".zshrc"])
    (m.src / "rules.json").write_text('{"path": ".zshrc"}')
    r = m.kit("status", code=1)
    has(r.err, "rules.json")


@test
def rules_invalid_json_clean_error(sb):
    m = setup(sb, {".zshrc": "a\n"}, add=[".zshrc"])
    (m.src / "rules.json").write_text('[{"path": ".zshrc",')
    r = m.kit("status", code=1)
    has(r.err, "rules.json")


@test
def packages_invalid_json_clean_error(sb):
    m = setup(sb)
    (m.src / "packages.json").write_text("{\n<<<<<<< HEAD\n")
    r = m.kit("pkg", "list", code=1)
    has(r.err, "packages.json", "JSON")
    r = m.kit("status", code=1)
    has(r.err, "packages.json")


# =========================================================================== diff
@test
def diff_plain_patch(sb):
    m = setup(sb, {".zshrc": "one\n"}, add=[".zshrc"])
    m.write(".zshrc", "one\nlocal\n")
    r = m.kit("diff")
    has(r.out, "# .zshrc: changed here", "--- a/.zshrc", "+++ b/.zshrc", "-local")
    hasnt(r.out, "\033[")
    r = m.kit("diff", "-r")
    has(r.out, "+local")
    r = m.kit("diff", "--plain", "~/.zshrc")
    has(r.out, "-local")


@test
def diff_new_file(sb):
    m = setup(sb, {".config/app/a": "a\n"}, add=[".config/app"])
    m.write(".config/app/n", "brand new\n")
    r = m.kit("diff")
    has(r.out, "# .config/app/n: new here", "+brand new")


@test
def diff_hides_secrets(sb):
    m = setup(sb, {".config/app/env": "x=1\n"}, add=[".config/app/env"])
    m.write(".config/app/env", f"x={TOKEN}\n")
    r = m.kit("diff")
    has(r.out, "hidden")
    hasnt(r.both, TOKEN)


@test
def diff_binary_summary(sb):
    m = setup(sb, {".config/app/blob": b"ab\0cd"}, add=[".config/app/blob"])
    m.write(".config/app/blob", b"ab\0cdef")
    r = m.kit("diff")
    has(r.out, "binary file")
    hasnt(r.out, "@@")


@test
def diff_nothing_when_in_sync(sb):
    m = setup(sb, {".zshrc": "a\n"}, add=[".zshrc"])
    ok(m.kit("diff").out.strip() == "")


# =========================================================================== cat, source-path, managed, unmanaged, verify
@test
def cat_and_source_path(sb):
    m = setup(sb, {".zshrc": "hello\n"}, add=[".zshrc"])
    m.write(".zshrc", "local\n")
    ok(m.kit("cat", "~/.zshrc").out == "hello\n")
    r = m.kit("cat", "~/.nope", code=1)
    has(r.err, "not a tracked file")
    ok(m.kit("source-path").out.strip() == str(m.src))
    ok(m.kit("source-path", "~/.zshrc").out.strip() == str(m.src / "home/.zshrc"))


@test
def cat_symlink(sb):
    m = setup(sb)
    os.symlink("target-file", m.home / ".lnk")
    m.kit("add", "~/.lnk")
    has(m.kit("cat", "~/.lnk").out, "-> target-file")


@test
def managed_lists_tracked(sb):
    m = setup(sb, {".zshrc": "a\n", ".config/app/a": "a\n", ".config/app/b/c": "c\n"},
              add=[".zshrc", ".config/app"])
    ok(m.tracked() == [".config/app/a", ".config/app/b/c", ".zshrc"], m.tracked())
    ok(m.kit("managed", "~/.config").out.split() == [".config/app/a", ".config/app/b/c"])


@test
def unmanaged_lists_new_files_and_config_dirs(sb):
    m = setup(sb, {".config/app/a": "a\n", ".config/other/x": "x\n", ".config/single": "s\n"},
              add=[".config/app"])
    m.write(".config/app/new", "n\n")
    out = m.kit("unmanaged").out.split()
    ok(".config/app/new" in out and ".config/other/" in out and ".config/single" in out, out)
    ok(".config/app/a" not in out and ".config/app/" not in out, out)


@test
def verify_exit_codes(sb):
    m = setup(sb, {".zshrc": "a\n", ".config/app/a": "a\n"}, add=[".zshrc", ".config/app"])
    m.write(".config/app/new", "n\n")       # new files don't count
    has(m.kit("verify").out, "match")
    m.write(".zshrc", "changed\n")
    r = m.kit("verify", code=1)
    has(r.out, "~/.zshrc")


# =========================================================================== save
@test
def save_commits_and_warns_without_remote(sb):
    m = setup(sb, {".zshrc": "a\n"}, add=[".zshrc"])
    r = m.kit("save", "my message")
    has(r.out, "committed")
    has(r.err, "not backed up")
    ok("my message" in m.git("log", "--format=%s").stdout)
    ok(m.git("status", "--porcelain").stdout.strip() == "", "repo dirty after save")
    has(m.kit("save").out, "nothing new")


@test
def save_refuses_secrets(sb):
    m = setup(sb, {".config/app/env": f"t={TOKEN}\n"})
    m.kit("add", "--force", "~/.config/app/env")
    r = m.kit("save", code=1)
    has(r.err, "secret", "config/app/env")
    hasnt(r.both, TOKEN)
    ok(m.git("diff", "--cached", "--name-only").stdout.strip() == "", "files left staged")
    ok(m.git("rev-parse", "HEAD", check=False).returncode != 0, "something was committed")
    m.kit("save", "--force")
    ok("home/.config/app/env" in m.git("ls-files").stdout)


@test
def save_pushes_to_remote(sb):
    m = setup(sb, {".zshrc": "a\n"}, add=[".zshrc"])
    bare = sb.bare()
    m.kit("git", "remote", "add", "origin", str(bare))
    r = m.kit("save", "pushed")
    has(r.out, "backed up")
    log = sb.git_raw(["--git-dir", str(bare), "log", "--all", "--format=%s"]).stdout
    has(log, "pushed")


@test
def save_warns_about_pending_and_a_re_adds(sb):
    m = setup(sb, {".zshrc": "a\n"}, add=[".zshrc"])
    m.kit("save", "one")
    m.write(".zshrc", "b\n")
    r = m.kit("save", "two")
    has(r.err, "NOT in the repo")
    m.kit("save", "-a", "three")
    ok(m.git("show", "HEAD:home/.zshrc").stdout == "b\n")


@test
def save_includes_files_hidden_by_nested_gitignore(sb):
    m = setup(sb, {".config/app/.gitignore": "*.log\n", ".config/app/app.log": "log\n"},
              add=[".config/app"])
    ok(".config/app/app.log" in m.tracked())
    r = m.kit("status", code=None)
    has(r.out, "Not in the backup")
    m.kit("save")
    ok("home/.config/app/app.log" in m.git("ls-files").stdout, "hidden file not committed")


@test
def save_without_repo(sb):
    m = sb.machine()
    m.kit("save", code=1)


# =========================================================================== update (two machines)
def two_machines(sb, files):
    m1 = setup(sb, files, add=list(files))
    bare = sb.bare()
    m1.kit("git", "remote", "add", "origin", str(bare))
    m1.kit("save", "m1 first")
    m2 = sb.machine("m2")
    m2.kit("init", str(bare))
    m2.kit("apply", "--no-packages")
    return m1, m2


@test
def update_pulls_and_applies(sb):
    m1, m2 = two_machines(sb, {".zshrc": "v1\n"})
    ok(m2.read(".zshrc") == "v1\n")
    m1.write(".zshrc", "v2\n")
    m1.kit("save", "-a", "v2")
    r = m2.kit("update", "--no-packages")
    has(r.out, "pulled 1 commit")
    ok(m2.read(".zshrc") == "v2\n")
    has(m2.kit("update", "--no-packages").out, "already up to date")


@test
def update_without_remote(sb):
    m = setup(sb)
    r = m.kit("update", code=1)
    has(r.err, "no backup repo")


@test
def update_conflict_aborts_cleanly(sb):
    m1, m2 = two_machines(sb, {".zshrc": "base\n"})
    m1.write(".zshrc", "from m1\n")
    m1.kit("save", "-a", "m1 edit")
    m2.write(".zshrc", "from m2\n")
    r = m2.kit("save", "-a", "m2 edit", code=1)
    has(r.err, "kit update")
    r = m2.kit("update", "--no-packages", code=1)
    has(r.err, "couldn't combine")
    ok(m2.read(".zshrc") == "from m2\n", "home changed")
    hasnt(m2.repo_read("home/.zshrc"), "<<<<<<<")
    ok(not (m2.src / ".git/rebase-merge").exists() and not (m2.src / ".git/rebase-apply").exists(),
       "repo left mid-rebase")
    hasnt(m2.kit("status", code=None).out, "in the middle")


@test
def update_does_not_run_new_scripts_without_tty(sb):
    m1, m2 = two_machines(sb, {".zshrc": "v1\n"})
    m1.repo_write("scripts/a.sh", 'touch "$HOME/ran-a"\n')
    m1.kit("save", "script a")
    r = m2.kit("update", "--no-packages")
    has(r.out, "scripts/a.sh")
    has(r.err, "--yes")
    ok(not m2.exists("ran-a"), "new script ran without a terminal")
    has(m2.kit("status", code=None).out, "a.sh")
    m1.repo_write("scripts/b.sh", 'touch "$HOME/ran-b"\n')
    m1.kit("save", "script b")
    m2.kit("update", "--yes", "--no-packages")
    ok(m2.exists("ran-a") and m2.exists("ran-b"), "--yes runs scripts")


@test
def guard_repo_mid_rebase(sb):
    m = setup(sb, {".zshrc": "a\n"}, add=[".zshrc"])
    (m.src / ".git/rebase-merge").mkdir()
    r = m.kit("apply", "--no-packages", code=1)
    has(r.err, "rebase")
    has(m.kit("status", code=None).out, "middle of a git rebase")


# =========================================================================== scripts
def count(m, name="count"):
    p = m.home / name
    return len(p.read_text().splitlines()) if p.exists() else 0


def script(m, name, header, body='echo x >> "$HOME/count"\n'):
    m.repo_write(f"scripts/{name}", "#!/bin/bash\n" + header + body)


@test
def scripts_onchange_default(sb):
    m = setup(sb)
    script(m, "c.sh", "")
    has(m.kit("status", code=None).out, "c.sh")
    m.kit("apply", "--no-packages")
    m.kit("apply", "--no-packages")
    ok(count(m) == 1, count(m))
    script(m, "c.sh", "# changed\n")
    m.kit("apply", "--no-packages")
    ok(count(m) == 2, count(m))
    ok((m.state / "scripts.json").exists() and not (m.src / ".kit").exists())


@test
def scripts_once(sb):
    m = setup(sb)
    script(m, "o.sh", "# kit: run=once\n")
    m.kit("apply", "--no-packages")
    script(m, "o.sh", "# kit: run=once\n# edited\n")
    m.kit("apply", "--no-packages")
    ok(count(m) == 1, count(m))


@test
def scripts_always(sb):
    m = setup(sb)
    script(m, "a.sh", "# kit: run=always\n")
    for _ in range(3):
        m.kit("apply", "--no-packages")
    ok(count(m) == 3, count(m))


@test
def scripts_on_other_platform_skipped(sb):
    m = setup(sb)
    script(m, "p.sh", f"# kit: on={OTHER}\n")
    script(m, "q.sh", f"# kit: on={PLAT}\n", body='echo q >> "$HOME/q"\n')
    m.kit("apply", "--no-packages")
    ok(count(m) == 0 and count(m, "q") == 1)
    r = m.kit("scripts")
    line = next(l for l in r.out.splitlines() if "p.sh" in l)
    has(line, "not here")


@test
def scripts_watch(sb):
    m = setup(sb, {".config/app/x": "1\n"})
    script(m, "w.sh", "# kit: watch=.config/app .other\n")
    m.kit("apply", "--no-packages")
    m.kit("apply", "--no-packages")
    ok(count(m) == 1)
    m.write(".config/app/x", "2\n")
    m.kit("apply", "--no-packages")
    ok(count(m) == 2)
    m.write(".other", "now exists\n")
    m.kit("apply", "--no-packages")
    ok(count(m) == 3)


@test
def scripts_header_only_in_top_comment_block(sb):
    m = setup(sb)
    m.repo_write("scripts/h.sh", "#!/bin/bash\n# kit: run=always  # every time\n\necho hi\n# kit: run=once\n")
    m.repo_write("scripts/i.sh", "#!/bin/bash\necho hi\n# kit: run=always\n")
    out = m.kit("scripts").out
    has(next(l for l in out.splitlines() if "h.sh" in l), "run=always")
    has(next(l for l in out.splitlines() if "i.sh" in l), "run=onchange")


@test
def scripts_unknown_header_reported(sb):
    m = setup(sb)
    script(m, "u.sh", "# kit: frequency=daily\n# kit: run=sometimes\n")
    r = m.kit("scripts")
    has(r.out, "unknown header 'frequency'", "run=sometimes")
    r = m.kit("apply", "--no-packages")
    has(r.err, "frequency")


@test
def scripts_failing_runs_again(sb):
    m = setup(sb)
    script(m, "f.sh", "", body='echo x >> "$HOME/count"\nexit 3\n')
    r = m.kit("apply", "--no-packages", code=1)
    has(r.err, "f.sh")
    m.kit("apply", "--no-packages", code=1)
    ok(count(m) == 2, count(m))


@test
def scripts_run_by_name(sb):
    m = setup(sb)
    script(m, "one.sh", "")
    script(m, "two.sh", "", body='echo y >> "$HOME/two"\n')
    m.kit("scripts", "run", "one")
    ok(count(m) == 1 and count(m, "two") == 0)
    m.kit("scripts", "run", "one.sh")
    ok(count(m) == 1, "done script ran again without --force")
    m.kit("scripts", "run", "one", "--force")
    ok(count(m) == 2)
    r = m.kit("scripts", "run", "nope", code=1)
    has(r.err, "nope")


@test
def scripts_get_env_and_cwd(sb):
    m = setup(sb)
    script(m, "e.sh", "", body='pwd > "$HOME/pwd"; echo "$KIT_SOURCE" > "$HOME/src"\n')
    m.kit("apply", "--no-packages")
    ok(os.path.realpath(m.read("pwd").strip()) == str(m.home))
    ok(m.read("src").strip() == str(m.src))


@test
def scripts_list_empty(sb):
    m = setup(sb)
    has(m.kit("scripts").out, "no scripts")


# =========================================================================== packages
def pkgs(m):
    return m.json("packages.json")


@test
def pkg_add_explicit_cask(sb):
    m = setup(sb)
    sb.stub("brew", code=0)
    r = m.kit("pkg", "add", "cask:foo")
    has(r.out, "added")
    p = pkgs(m)["foo"]
    ok(p.get("mac") == "cask:foo" and "linux" not in p, p)
    if PLAT == "mac":
        ok(any(c == "brew install --cask foo" for c in sb.calls("brew")), sb.calls())


@test
def pkg_add_github_repo(sb):
    m = setup(sb)
    sb.stub("mise", script=MISE_STUB)
    m.kit("add", "pkg", "sharkdp/hexyl")
    p = pkgs(m)["hexyl"]
    ok(p["linux"] == "github:sharkdp/hexyl@1.2.3", p)
    ok(p["mac"] == "mise:github:sharkdp/hexyl@1.2.3", p)
    if PLAT == "mac":
        ok("mise use -g github:sharkdp/hexyl@1.2.3" in sb.calls("mise"), sb.calls())


@test
def pkg_add_name_via_brew_info_and_mise_registry(sb):
    m = setup(sb)
    sb.stub("mise", script=MISE_STUB)
    sb.stub("brew", script="""case "$1" in
  info) echo '{"formulae": [{"name": "lazygit"}], "casks": []}'; exit 0;;
  *) exit 0;;
esac
""")
    m.kit("pkg", "add", "lazygit")
    p = pkgs(m)["lazygit"]
    ok(p["mac"] == "brew:lazygit", p)
    ok(p["linux"] == "aqua:owner/lazygit@1.2.3", p)


@test
def pkg_add_unknown_fails(sb):
    m = setup(sb)
    r = m.kit("pkg", "add", "nosuchtool", code=1)
    has(r.err, "not found")
    ok(pkgs(m) == {})


@test
def pkg_add_only_and_bin(sb):
    m = setup(sb)
    sb.stub("mise", script=MISE_STUB)
    sb.stub("brew", code=0)
    m.kit("pkg", "add", "--only", "linux", "--bin", "rg", "aqua:BurntSushi/ripgrep")
    p = pkgs(m)["ripgrep"]
    ok(p == {"linux": "aqua:BurntSushi/ripgrep@1.2.3", "bin": "rg"}, p)
    m.kit("pkg", "add", "--only", "mac", "cask:kitty")
    ok(pkgs(m)["kitty"] == {"mac": "cask:kitty"}, pkgs(m)["kitty"])


@test
def pkg_add_fallback(sb):
    m = setup(sb)
    sb.stub("mise", script=MISE_STUB)
    m.kit("pkg", "add", "--only", "linux", "--fallback", "cargo:foo", "aqua:o/foo@1.0")
    ok(pkgs(m)["foo"] == {"linux": "aqua:o/foo@1.0", "linux_fallback": "cargo:foo@1.2.3"}, pkgs(m)["foo"])
    sb.stub("mise", script=MISE_USE_OK)
    m.kit("pkg", "add", "--only", "linux", "--fallback", "cargo:bar", "aqua:o/bar@2.0")
    ok(pkgs(m)["bar"]["linux_fallback"] == "cargo:bar@latest", pkgs(m)["bar"])
    m.kit("pkg", "add", "--only", "linux", "--fallback", "cargo:baz@0.9", "aqua:o/baz@3.0")
    ok(pkgs(m)["baz"]["linux_fallback"] == "cargo:baz@0.9")
    m.kit("pkg", "add", "--only", "linux", "--fallback", "notaspec", "aqua:o/qux@1.0", code=1)


@test
def pkg_add_no_servers(sb):
    m = setup(sb)
    sb.stub("mise", script=MISE_USE_OK)
    m.kit("pkg", "add", "--only", "linux", "--no-servers", "aqua:o/tool@1.0")
    ok(pkgs(m)["tool"]["remote"] is False)
    out = m.kit("pkg", "list").out
    line = next(l for l in out.splitlines() if l.startswith("tool"))
    has(line, " no ")
    m.kit("pkg", "add", "--only", "linux", "--servers", "aqua:o/tool@1.0")
    ok("remote" not in pkgs(m)["tool"])


@test
def pkg_split_keeps_npm_scope(sb):
    m = setup(sb)
    sb.stub("mise", script=MISE_USE_OK)
    m.kit("pkg", "add", "--only", "linux", "npm:@scope/pkg@1.2.3")
    ok(pkgs(m)["pkg"]["linux"] == "npm:@scope/pkg@1.2.3", pkgs(m))
    m.kit("pkg", "add", "--only", "linux", "npm:@other/cli")    # mise fails → @latest
    ok(pkgs(m)["cli"]["linux"] == "npm:@other/cli@latest", pkgs(m))


@test
def pkg_add_already_tracked(sb):
    m = setup(sb)
    sb.stub("mise", script=MISE_USE_OK)
    m.kit("pkg", "add", "--only", "linux", "aqua:o/tool@1.0")
    has(m.kit("pkg", "add", "--only", "linux", "aqua:o/tool@1.0").out, "already tracked")


@test
def pkg_rm(sb):
    m = setup(sb)
    (m.src / "packages.json").write_text(json.dumps({"foo": {"mac": "brew:foo"}, "bar": {"mac": "brew:bar"}}))
    r = m.kit("pkg", "rm", "foo")
    has(r.out, "no longer tracked")
    ok(list(pkgs(m)) == ["bar"])
    r = m.kit("pkg", "rm", "nope", code=1)
    has(r.err, "not a tracked package")
    m.kit("rm", "pkg", "bar")
    ok(pkgs(m) == {})


@test
def pkg_rm_uninstall(sb):
    m = setup(sb)
    sb.stub("mise", script=MISE_USE_OK)
    sb.stub("brew", code=0)
    (m.src / "packages.json").write_text(json.dumps({"foo": {"mac": "brew:foo", "linux": "aqua:o/foo@1"}}))
    m.kit("pkg", "rm", "--uninstall", "foo")
    if PLAT == "mac":
        ok("brew uninstall foo" in sb.calls("brew"), sb.calls())


@test
def pkg_set(sb):
    m = setup(sb)
    (m.src / "packages.json").write_text(json.dumps({"foo": {"mac": "brew:foo", "linux": "aqua:o/foo@1.0"}}))
    m.kit("pkg", "set", "foo", "--bin", "fu", "--no-servers", "--linux", "aqua:o/foo@2.0", "--mac", "cask:foo")
    ok(pkgs(m)["foo"] == {"mac": "cask:foo", "linux": "aqua:o/foo@2.0", "bin": "fu", "remote": False}, pkgs(m))
    m.kit("pkg", "set", "foo", "--servers")
    ok("remote" not in pkgs(m)["foo"])
    m.kit("pkg", "set", "foo", "--linux", "not a spec", code=1)
    m.kit("pkg", "set", "nope", "--bin", "x", code=1)


@test
def pkg_list_columns(sb):
    m = setup(sb)
    (m.src / "packages.json").write_text(json.dumps({
        "foo": {"mac": "brew:foo", "linux": "aqua:o/foo@1.0", "linux_fallback": "cargo:foo@1.0"},
        "bar": {"linux": "aqua:o/bar@2.0", "remote": False}}))
    out = m.kit("pkg", "list").out
    lines = out.splitlines()
    for col in ("name", "here", "servers", "mac", "linux"):
        has(lines[0], col)
    foo = next(l for l in lines if l.startswith("foo"))
    has(foo, "brew:foo", "aqua:o/foo@1.0", "(cargo:foo@1.0)", "yes")
    bar = next(l for l in lines if l.startswith("bar"))
    has(bar, "aqua:o/bar@2.0", " no ")


@test
def pkg_upgrade(sb):
    m = setup(sb)
    sb.stub("mise", script="[ \"$1\" = latest ] && echo 2.0\nexit 0\n")
    (m.src / "packages.json").write_text(json.dumps({
        "foo": {"linux": "aqua:o/foo@1.0", "linux_fallback": "cargo:foo@0.5"},
        "bar": {"linux": "aqua:o/bar@2.0"}, "baz": {"mac": "brew:baz"}}))
    r = m.kit("pkg", "upgrade")
    has(r.out, "1.0 → 2.0", "0.5 → 2.0")
    p = pkgs(m)
    ok(p["foo"] == {"linux": "aqua:o/foo@2.0", "linux_fallback": "cargo:foo@2.0"}, p)
    ok(p["bar"]["linux"] == "aqua:o/bar@2.0" and p["baz"] == {"mac": "brew:baz"})
    ok("mise latest aqua:o/foo" in sb.calls("mise"), sb.calls())
    has(m.kit("pkg", "upgrade").out, "already on the latest")
    m.kit("pkg", "upgrade", "nope", code=1)


@test
def pkg_upgrade_needs_mise(sb):
    m = setup(sb)
    sb.unstub("mise")
    (m.src / "packages.json").write_text(json.dumps({"foo": {"linux": "aqua:o/foo@1.0"}}))
    r = m.kit("pkg", "upgrade", code=1)
    has(r.err, "mise")


@test
def pkg_install_and_status(sb):
    m = setup(sb)
    (m.src / "packages.json").write_text(json.dumps({"foo": {"mac": "brew:foo", "linux": "aqua:o/foo@1"}}))
    has(m.kit("status").out, "foo")
    if PLAT == "mac":
        m.kit("pkg", "install", code=1)      # fake brew fails
        sb.stub("brew", code=0)
        m.kit("pkg", "install")
        ok("brew install foo" in sb.calls("brew"), sb.calls())


@test
def pkg_without_action_prints_help(sb):
    m = setup(sb)
    has(m.kit("pkg").out, "upgrade")


# =========================================================================== externals
@test
def externals_cloned_on_apply(sb):
    m = setup(sb)
    ext = sb.make_git_repo("ext", {"README": "ext\n"})
    sb.git_raw(["tag", "v1"], cwd=ext)
    (m.src / "externals.json").write_text(json.dumps({
        ".antidote": {"git": f"file://{ext}"}, ".tagged": {"git": f"file://{ext}", "ref": "v1"}}))
    has(m.kit("status").out, "External ~/.antidote: missing")
    r = m.kit("apply", "-n", "--no-packages")
    has(r.out, "would clone")
    ok(not m.exists(".antidote"))
    m.kit("apply", "--no-packages")
    ok(m.read(".antidote/README") == "ext\n" and m.read(".tagged/README") == "ext\n")
    hasnt(m.kit("status").out, "External")


# =========================================================================== push / remote
def push_ready(sb, m):
    m.repo_write("lib/remote/setup.sh", "echo setup\n")


@test
def push_without_host(sb):
    m = setup(sb)
    r = m.kit("push", code=1)
    has(r.err, "which server", "none yet")
    ok(sb.calls("ssh") == [])


@test
def push_first_time_without_tty_refused(sb):
    m = setup(sb, {".config/app/a": "a\n"}, add=[".config/app"])
    push_ready(sb, m)
    r = m.kit("push", "box", code=1)
    has(r.out, "First push to box")
    has(r.err, "--yes")
    ok(sb.calls("ssh") == [] and sb.calls("rsync") == [], sb.calls())


@test
def push_dry_run(sb):
    m = setup(sb, {".config/app/a": "a\n"}, add=[".config/app"])
    push_ready(sb, m)
    r = m.kit("push", "box", "-n")
    has(r.out, "First push to box")
    r = m.kit("push", "box", "-n", "-y")
    has(r.out, "would sync")
    ok(sb.calls("ssh") == [] and sb.calls("rsync") == [], sb.calls())
    ok(not (m.src / "remotes.json").exists() or "box" not in m.json("remotes.json"))


@test
def push_with_yes_syncs_payload(sb):
    m = setup(sb, {".config/app/a": "a\n", ".config/kitty/k": "k\n", ".zshrc": "z\n"})
    m.kit("add", "~/.config/app", "~/.zshrc")
    m.kit("add", "--no-servers", "~/.config/kitty")
    push_ready(sb, m)
    sb.stub("ssh", code=0)
    sb.stub("rsync", code=0)
    r = m.kit("push", "box", "--yes")
    has(r.out, "not sent to servers")
    ok("box" in m.json("remotes.json"))
    ok(any("box" in c and "mkdir" in c for c in sb.calls("ssh")), sb.calls())
    ok(any("box:" in c for c in sb.calls("rsync")), sb.calls())
    payload = m.state / "payload"
    ok((payload / "config/app/a").read_text() == "a\n", "payload lacks config/app/a")
    ok(not (payload / "config/kitty").exists(), "[remote] file was sent")
    ok(not any(p.name == ".zshrc" for p in payload.rglob("*")), ".zshrc was sent")
    has((payload / "mise.toml").read_text(), "aqua:sharkdp/bat")


@test
def push_unreachable_host(sb):
    m = setup(sb)
    push_ready(sb, m)
    r = m.kit("push", "box", "-y", code=1)
    has(r.err, "box")
    ok(not (m.src / "remotes.json").exists() or "box" not in m.json("remotes.json"))


@test
def remote_list_no_servers(sb):
    m = setup(sb)
    has(m.kit("remote").out, "no servers")
    has(m.kit("remote", "list").out, "no servers")
    m.kit("remote", "off", code=1)
    m.kit("remote", "remove", "box", code=1)    # no tty, no --yes


# =========================================================================== CLI
@test
def cli_version_and_help(sb):
    m = sb.machine()
    r = m.kit("--version")
    ok(r.out.strip().startswith("kit "), r.out)
    r = m.kit("help", "add")
    has(r.out, "--no-servers")
    r = m.kit("help", "pkg", "add")
    has(r.out, "--fallback")
    has(m.kit().out, "status")


@test
def cli_typo_suggests(sb):
    m = setup(sb)
    r = m.kit("stauts", code=2)
    has(r.err, "did you mean 'status'")
    r = m.kit("pkg", "lsit", code=2)
    has(r.err, "did you mean 'list'")


@test
def cli_chezmoi_hints(sb):
    m = setup(sb)
    r = m.kit("merge", code=2)
    has(r.err, "kit has no 'merge'", "re-add --force")


@test
def cli_usage_errors_exit_2(sb):
    m = setup(sb)
    m.kit("add", code=2)
    m.kit("status", "--bogus", code=2)
    m.kit("pkg", "add", "--only", "windows", "x", code=2)


@test
def cli_broken_pipe_is_quiet(sb):
    m = setup(sb, {".config/app/a": "a\n"}, add=[".config/app"])
    d = m.home / ".config/app"
    for i in range(3000):
        (d / f"some-rather-long-file-name-for-padding-{i:05d}.conf").write_text("x\n")
    env = dict(m.envvars)
    p = subprocess.Popen(KIT + ["status", "-v"], cwd=m.home, env=env, stdin=subprocess.DEVNULL,
                         stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    first = p.stdout.readline()
    p.stdout.close()
    err = p.stderr.read().decode(errors="replace")
    p.wait(timeout=60)
    sb.history.append(Result(["status", "-v", "| head -1"], p.returncode, first.decode(), err))
    ok(first.strip(), "no first line")
    hasnt(err, "Traceback", "BrokenPipe", "panicked")


# =========================================================================== completion
def complete(m, *words):
    return m.kit("__complete", "--", *words).out.splitlines()


@test
def complete_commands(sb):
    m = setup(sb)
    lines = complete(m, "")
    cmds = {l.split("\t")[0]: l for l in lines}
    for c in ("add", "status", "apply", "pkg", "push", "help"):
        ok(c in cmds, f"{c} missing from {sorted(cmds)}")
    ok("\t" in cmds["status"] and cmds["status"].split("\t", 1)[1].strip(), "no description")
    sub = [l.split("\t")[0] for l in complete(m, "pkg", "")]
    ok("rm" in sub and "upgrade" in sub, sub)


@test
def complete_packages(sb):
    m = setup(sb)
    (m.src / "packages.json").write_text(json.dumps({"bat": {"mac": "brew:bat"}, "eza": {"mac": "cargo:eza"}}))
    names = [l.split("\t")[0] for l in complete(m, "pkg", "rm", "")]
    ok(names == ["bat", "eza"], names)


@test
def complete_tracked_paths_folder_by_folder(sb):
    m = setup(sb, {".config/app/a": "a\n", ".config/nvim/init.lua": "x\n", ".zshrc": "z\n"},
              add=[".config/app", ".config/nvim/init.lua", ".zshrc"])
    vals = [l.split("\t")[0] for l in complete(m, "apply", "~/.con")]
    ok(vals == ["~/.config/"], vals)
    vals = [l.split("\t")[0] for l in complete(m, "apply", "~/.config/")]
    ok(vals == ["~/.config/app/", "~/.config/nvim/"], vals)
    vals = [l.split("\t")[0] for l in complete(m, "diff", "~/.zs")]
    ok(vals == ["~/.zshrc"], vals)


@test
def complete_files_for_add(sb):
    m = setup(sb)
    ok(complete(m, "add", "x") == ["__files__"])
    ok(complete(m, "ignore", "") == ["__files__"])


@test
def complete_hosts(sb):
    m = setup(sb)
    (m.src / "remotes.json").write_text(json.dumps({"alpha": {"stamp": "", "at": ""}}))
    m.write(".ssh/config", "Host beta gamma\nHost *.wild\n  HostName x\n")
    hosts = complete(m, "push", "")
    ok(hosts == ["alpha", "beta", "gamma"], hosts)


@test
def complete_options(sb):
    m = setup(sb)
    opts = [l.split("\t")[0] for l in complete(m, "add", "--")]
    ok("--force" in opts and "--no-servers" in opts, opts)
    vals = complete(m, "pkg", "add", "--only", "")
    ok(sorted(vals) == ["linux", "mac"], vals)


@test
def complete_scripts_and_choices(sb):
    m = setup(sb)
    m.repo_write("scripts/setup-mac.sh", "echo\n")
    ok(complete(m, "scripts", "run", "") == ["setup-mac"], complete(m, "scripts", "run", ""))
    ok(sorted(complete(m, "completion", "")) == ["bash", "fish", "install", "zsh"])


@test
def completion_scripts(sb):
    m = setup(sb)
    has(m.kit("completion", "zsh").out, "#compdef kit", "__complete")
    has(m.kit("completion", "bash").out, "complete -o nospace -F _kit kit")
    has(m.kit("completion", "fish").out, "complete -c kit")


@test
def completion_install_stays_in_sandbox(sb):
    m = setup(sb)
    r = m.kit("completion", "install")
    target = sb.root / "xdg/data/bash-completion/completions/kit"
    ok(target.is_file(), "bash completion not in XDG_DATA_HOME")
    has(target.read_text(), "complete -o nospace -F _kit kit")
    for line in r.out.splitlines():
        if line.startswith("  /"):
            ok(line.strip().startswith(str(sb.root)), f"completion written outside the sandbox: {line}")


# =========================================================================== state
@test
def state_lives_outside_repo(sb):
    m = setup(sb, {".zshrc": "a\n"}, add=[".zshrc"])
    m.write(".zshrc", "b\n")
    m.kit("apply", "--force", "--no-packages")
    script(m, "s.sh", "")
    m.kit("apply", "--no-packages")
    for f in ("synced.json", "scripts.json", "backups"):
        ok((m.state / f).exists(), f"{f} not in KIT_STATE")
    ok(not (m.src / ".kit").exists(), "<repo>/.kit exists")
    porcelain = m.git("status", "--porcelain", "--untracked-files=all").stdout
    hasnt(porcelain, "synced", "backups", "scripts.json")


@test
def state_honors_kit_state_env(sb):
    m = setup(sb, {".zshrc": "a\n"})
    elsewhere = sb.root / "otherstate"
    m.kit("add", "~/.zshrc", env={"KIT_STATE": str(elsewhere)})
    ok((elsewhere / "synced.json").exists(), "KIT_STATE ignored")


@test
def state_migrates_old_repo_folder(sb):
    m = setup(sb, {".zshrc": "a\n"}, add=[".zshrc"])
    old = m.src / ".kit"
    (old / "backups/20200101-000000").mkdir(parents=True)
    (old / "backups/20200101-000000/.zshrc").write_text("old\n")
    (old / "scripts.json").write_text('{"x.sh": "abc"}')
    m.kit("status", code=None)
    ok(not old.exists(), "old .kit folder still there")
    ok(json.loads((m.state / "scripts.json").read_text()) == {"x.sh": "abc"})
    ok((m.state / "backups/20200101-000000/.zshrc").read_text() == "old\n")
    has(m.kit("restore").out, "20200101-000000")


@test
def doctor_runs(sb):
    m = setup(sb)
    r = m.kit("doctor", code=None)
    has(r.out, "repo")
    ok(r.code in (0, 1))


@test
def git_passthrough(sb):
    m = setup(sb, {".zshrc": "a\n"}, add=[".zshrc"])
    m.kit("save", "hello")
    has(m.kit("git", "log", "--oneline").out, "hello")


# =========================================================================== runner
def run_one(fn):
    sb = Sandbox(fn.__name__)
    t0 = time.time()
    try:
        fn(sb)
        status, detail = "PASS", ""
    except Exception as e:
        status = "FAIL"
        tb = traceback.extract_tb(e.__traceback__)
        where = next((f"{Path(f.filename).name}:{f.lineno}" for f in reversed(tb)
                      if f.filename == __file__ and f.name == fn.__name__), "")
        if not where:
            where = next((f"{Path(f.filename).name}:{f.lineno}" for f in reversed(tb) if f.filename == __file__), "")
        msg = f"{type(e).__name__ if not isinstance(e, Fail) else ''}{': ' if not isinstance(e, Fail) else ''}{e}"
        detail = f"  at {where}: {msg}\n"
        if not isinstance(e, Fail):
            detail += "".join(traceback.format_exception(e)).rstrip() + "\n"
        for res in sb.history[-3:]:
            detail += _indent(res.show()) + "\n"
        detail += f"  sandbox kept at {sb.root}\n"
    if fn.__name__ in EXPECTED_BUG:
        status = {"FAIL": "XFAIL", "PASS": "XPASS"}[status]
    elapsed = time.time() - t0
    if status in ("PASS", "XFAIL"):
        if VERBOSE:
            detail += "".join(_indent(r.show()) + "\n" for r in sb.history)
        sb.cleanup()
    return fn.__name__, status, elapsed, detail


def resolve_kit(cmd: str) -> list:
    parts = shlex.split(cmd)
    out = []
    for i, p in enumerate(parts):
        if i == 0 and p in ("python3", "python"):
            out.append(sys.executable)
        elif not os.path.isabs(p) and os.path.exists(p):
            out.append(os.path.abspath(p))
        elif i == 0 and not os.path.isabs(p) and "/" not in p and shutil.which(p):
            out.append(shutil.which(p))
        else:
            out.append(p)
    return out


def main():
    global KIT, VERBOSE
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--kit", default=os.environ.get("KIT_BIN") or str(HERE.parent / "target/debug/kit"))
    ap.add_argument("-k", dest="pattern", help="only tests whose name contains this")
    ap.add_argument("-v", "--verbose", action="store_true", help="show every command and its output")
    ap.add_argument("-j", "--jobs", type=int, default=min(16, (os.cpu_count() or 4)))
    a = ap.parse_args()
    VERBOSE = a.verbose
    KIT = resolve_kit(a.kit)
    os.umask(0o022)
    selected = [t for t in TESTS if not a.pattern or a.pattern in t.__name__]
    if not selected:
        print(f"no tests match {a.pattern!r}")
        return 1
    zsh_before = ZSH_SITE.stat().st_mtime if ZSH_SITE.exists() else None
    print(f"kit: {' '.join(KIT)}\nplatform: {PLAT} · {len(selected)} test(s) · sandboxes in {BASE}\n")
    t0 = time.time()
    results = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=max(1, a.jobs)) as pool:
        futs = {pool.submit(run_one, t): t for t in selected}
        for f in concurrent.futures.as_completed(futs):
            name, status, el, detail = f.result()
            results.append((name, status))
            print(f"{status:5} {name}  ({el:.1f}s)")
            if detail and (status not in ("PASS", "XFAIL") or VERBOSE):
                print(detail)
    total = time.time() - t0
    zsh_after = ZSH_SITE.stat().st_mtime if ZSH_SITE.exists() else None
    failed = [n for n, s in results if s == "FAIL"]
    xfail = [n for n, s in results if s == "XFAIL"]
    xpass = [n for n, s in results if s == "XPASS"]
    print(f"\n{len(results) - len(failed) - len(xfail) - len(xpass)} passed, {len(failed)} failed, "
          f"{len(xfail)} expected-bug failures, {len(xpass)} expected-bug passes in {total:.1f}s")
    for n in failed:
        print(f"  FAIL {n}")
    for n in xpass:
        print(f"  XPASS {n} (the bug seems fixed: remove it from EXPECTED_BUG)")
    if zsh_before != zsh_after:
        print(f"\n!!! {ZSH_SITE} was modified during the test run (mtime {zsh_before} -> {zsh_after}): "
              f"kit wrote outside the sandbox !!!")
        return 1
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
