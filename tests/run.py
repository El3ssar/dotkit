#!/usr/bin/env python3
"""Black-box test suite for kit (the dotfile manager).

    python3 tests/run.py [--kit CMD] [-k substring] [-v] [-j N]

CMD defaults to target/debug/kit (or $KIT_BIN); build first with `cargo build`. Also accepts
`--kit target/debug/kit`. Every test runs in its own sandbox: a fake $HOME, kit's repo and
state inside it, and a fakebin/ with logging stubs for brew, launchctl, clang, cargo, mise,
gh, delta, bat, ssh, rsync and zsh. git is the real git. Nothing outside the sandbox is touched.
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
STUBS = ["brew", "launchctl", "clang", "cargo", "mise", "gh", "delta", "bat", "ssh", "rsync", "zsh"]
COMMANDS = ["init", "add", "rm", "status", "save", "sync", "undo", "push"]
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

    def sync(self, code=0, **kw):
        """kit sync. On Linux the (fake) environment setup always fails, so exit 0 isn't checked there."""
        return self.kit("sync", code=None if (PLAT == "linux" and code == 0) else code, **kw)

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
        """Every file and symlink under the repo's home/, sorted (what `kit managed` used to print)."""
        base = self.src / "home"
        out = []
        for d, dirs, files in os.walk(base):
            for n in list(dirs):
                if (Path(d) / n).is_symlink():
                    out.append((Path(d) / n).relative_to(base).as_posix())
                    dirs.remove(n)
            out += [(Path(d) / f).relative_to(base).as_posix() for f in files]
        return sorted(out)

    def head(self):
        return self.git("rev-parse", "HEAD", check=False).stdout.strip()

    def base_file(self, rel):
        """kit's copy of the last-synced version of ~/rel (KIT_STATE/base/rel), or None."""
        p = self.state / "base" / rel
        return p.read_text() if p.exists() else None


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


def bare_log(sb, bare):
    return sb.git_raw(["--git-dir", str(bare), "log", "--all", "--format=%s"]).stdout


def bare_show(sb, bare, rel):
    return sb.git_raw(["--git-dir", str(bare), "show", f"main:{rel}"]).stdout


def two_machines(sb, files):
    """m1 with `files` tracked and backed up to a bare remote; m2 cloned from it and synced."""
    m1 = setup(sb, files, add=list(files))
    bare = sb.bare()
    m1.kit("init", str(bare))
    m1.sync()
    m2 = sb.machine("m2")
    m2.kit("init", str(bare))
    m2.sync()
    return m1, m2, bare


def no_rebase_left(m):
    ok(not (m.src / ".git/rebase-merge").exists() and not (m.src / ".git/rebase-apply").exists(),
       "repo left mid-rebase")


MARK_OURS, MARK_SEP, MARK_THEIRS = "<<<<<<< this machine", "=======", ">>>>>>> repo"


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
    r = m.kit("init")             # already there, no url: just says so
    has(r.out, "already")


@test
def init_clone_from_bare_repo(sb):
    m1 = setup(sb, {".zshrc": "from m1\n"}, add=[".zshrc"])
    bare = sb.bare()
    m1.kit("init", str(bare))
    m1.sync()
    m2 = sb.machine("m2")
    r = m2.kit("init", str(bare))
    has(r.out, "cloned")
    ok((m2.src / "home/.zshrc").is_file(), "clone has no home/.zshrc")
    ok(not m2.exists(".zshrc"), "init must not write files into $HOME")
    m2.sync()
    ok(m2.read(".zshrc") == "from m1\n", "sync after clone")


@test
def init_url_on_existing_repo_sets_backup_remote(sb):
    m = setup(sb, {".zshrc": "a\n"}, add=[".zshrc"])
    ok(m.git("remote").stdout.strip() == "", "a fresh repo has no remote")
    bare = sb.bare()
    r = m.kit("init", str(bare))
    has(r.out, f"backup: {bare}")
    ok(m.git("remote", "get-url", "origin").stdout.strip() == str(bare))
    ok(m.tracked() == [".zshrc"], "init <url> on an existing repo must not replace it")
    m.sync()
    ok(bare_show(sb, bare, "home/.zshrc") == "a\n", "sync after init <url> didn't push")
    other = sb.bare("other.git")
    r = m.kit("init", str(other))       # again: set-url
    has(r.out, f"backup: {other}")
    ok(m.git("remote", "get-url", "origin").stdout.strip() == str(other))


@test
def commands_need_a_repo(sb):
    m = sb.machine()
    r = m.kit("status", code=1)
    has(r.err, "kit init")
    m.kit("save", code=1)
    m.kit("sync", code=1)


# =========================================================================== add: files
@test
def add_file(sb):
    m = setup(sb, {".zshrc": "hi\n"})
    r = m.kit("add", "~/.zshrc")
    has(r.out, "tracking 1 file(s)")
    ok(m.repo_read("home/.zshrc") == "hi\n")
    ok(json.loads((m.state / "synced.json").read_text()).get(".zshrc"), "synced.json records it")
    ok(sb.calls("brew") == [] and sb.calls("mise") == [], "a file was looked up as a tool")


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
def add_force_ignored_file(sb):
    m = setup(sb, {"notes.swp": "x\n"})
    m.kit("add", "--force", "~/notes.swp", code=0)
    ok(m.repo_has("home/notes.swp"), "--force adds an ignored file")


@test
def add_missing_path_fails(sb):
    m = setup(sb)
    (m.home / ".config").mkdir()
    for arg in ("~/.nope", "./nope", "../nope", str(m.home / "nope/deeper"), ".nope", "~/sub/x"):
        r = m.kit("add", arg, code=1, cwd=m.home / ".config")
        has(r.err, "does not exist")
    ok(m.tracked() == [] and m.json("packages.json") == {})
    ok(sb.calls("brew") == [] and sb.calls("mise") == [], f"a path was looked up as a tool: {sb.calls()}")


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


# =========================================================================== add: path or tool?
@test
def add_bare_name_that_exists_is_a_file(sb):
    m = setup(sb, {"work/bat": "a file called bat\n", "notes.conf": "n\n"})
    sb.stub("mise", script=MISE_STUB)
    sb.stub("brew", code=0)
    m.kit("add", "bat", cwd=m.home / "work")              # exists relative to cwd
    ok(m.tracked() == ["work/bat"], m.tracked())
    m.kit("add", "notes.conf", cwd=m.home / "work")       # not in cwd, but in $HOME
    ok(m.tracked() == ["notes.conf", "work/bat"], m.tracked())
    ok(m.json("packages.json") == {}, m.json("packages.json"))
    ok(sb.calls("brew") == [] and sb.calls("mise") == [], f"looked up as a tool: {sb.calls()}")


@test
def add_files_and_tools_together(sb):
    m = setup(sb, {".zshrc": "z\n"})
    sb.stub("brew", code=0)
    r = m.kit("add", "~/.zshrc", "cask:foo")
    ok(m.tracked() == [".zshrc"], m.tracked())
    ok(pkgs(m) == {"foo": {"mac": "cask:foo"}}, pkgs(m))
    hasnt(r.err, "does not exist")


# =========================================================================== add: tools
def pkgs(m):
    return m.json("packages.json")


@test
def add_tool_explicit_cask(sb):
    m = setup(sb)
    sb.stub("brew", code=0)
    r = m.kit("add", "cask:foo")
    has(r.out, "added")
    p = pkgs(m)["foo"]
    ok(p.get("mac") == "cask:foo" and "linux" not in p, p)
    if PLAT == "mac":
        ok(any(c == "brew install --cask foo" for c in sb.calls("brew")), sb.calls())


@test
def add_tool_github_repo(sb):
    m = setup(sb)
    sb.stub("mise", script=MISE_STUB)
    m.kit("add", "sharkdp/hexyl")
    p = pkgs(m)["hexyl"]
    ok(p["linux"] == "github:sharkdp/hexyl@1.2.3", p)
    ok(p["mac"] == "mise:github:sharkdp/hexyl@1.2.3", p)
    if PLAT == "mac":
        ok("mise use -g github:sharkdp/hexyl@1.2.3" in sb.calls("mise"), sb.calls())


@test
def add_tool_name_via_brew_info_and_mise_registry(sb):
    m = setup(sb)
    sb.stub("mise", script=MISE_STUB)
    sb.stub("brew", script="""case "$1" in
  info) echo '{"formulae": [{"name": "lazygit"}], "casks": []}'; exit 0;;
  *) exit 0;;
esac
""")
    m.kit("add", "lazygit")
    p = pkgs(m)["lazygit"]
    ok(p["mac"] == "brew:lazygit", p)
    ok(p["linux"] == "aqua:owner/lazygit@1.2.3", p)
    ok(m.tracked() == [])


@test
def add_tool_unknown_fails(sb):
    m = setup(sb)
    r = m.kit("add", "nosuchtool", code=1)
    has(r.err, "nosuchtool", "owner/repo")
    ok(pkgs(m) == {})


@test
def add_tool_bin_mac_linux(sb):
    m = setup(sb)
    sb.stub("mise", script=MISE_STUB)
    sb.stub("brew", code=0)
    m.kit("add", "--bin", "rg", "aqua:BurntSushi/ripgrep")
    p = pkgs(m)["ripgrep"]
    ok(p.get("linux") == "aqua:BurntSushi/ripgrep@1.2.3" and p.get("bin") == "rg", p)
    m.kit("add", "cask:kitty")
    ok(pkgs(m)["kitty"] == {"mac": "cask:kitty"}, pkgs(m)["kitty"])
    m.kit("add", "--mac", "brew:foo", "--linux", "aqua:o/foo@1.0", "foo")
    ok(pkgs(m)["foo"] == {"mac": "brew:foo", "linux": "aqua:o/foo@1.0"}, pkgs(m)["foo"])
    m.kit("add", "--linux", "not a spec", "bar", code=1)
    ok("bar" not in pkgs(m))


@test
def add_tool_fallback(sb):
    m = setup(sb)
    sb.stub("mise", script=MISE_STUB)
    m.kit("add", "--fallback", "cargo:foo", "aqua:o/foo@1.0")
    p = pkgs(m)["foo"]
    ok(p["linux"] == "aqua:o/foo@1.0" and p["linux_fallback"] == "cargo:foo@1.2.3", p)
    sb.stub("mise", script=MISE_USE_OK)
    m.kit("add", "--fallback", "cargo:bar", "aqua:o/bar@2.0")
    ok(pkgs(m)["bar"]["linux_fallback"] == "cargo:bar@latest", pkgs(m)["bar"])
    m.kit("add", "--fallback", "cargo:baz@0.9", "aqua:o/baz@3.0")
    ok(pkgs(m)["baz"]["linux_fallback"] == "cargo:baz@0.9")
    m.kit("add", "--fallback", "notaspec", "aqua:o/qux@1.0", code=1)


@test
def add_tool_no_servers(sb):
    m = setup(sb)
    sb.stub("mise", script=MISE_USE_OK)
    m.kit("add", "--no-servers", "aqua:o/tool@1.0")
    ok(pkgs(m)["tool"]["remote"] is False, pkgs(m))


@test
def add_tool_keeps_npm_scope(sb):
    m = setup(sb)
    sb.stub("mise", script=MISE_USE_OK)
    m.kit("add", "npm:@scope/pkg@1.2.3")
    ok(pkgs(m)["pkg"]["linux"] == "npm:@scope/pkg@1.2.3", pkgs(m))
    m.kit("add", "npm:@other/cli")    # mise knows no version → @latest
    ok(pkgs(m)["cli"]["linux"] == "npm:@other/cli@latest", pkgs(m))


@test
def add_tool_already_tracked(sb):
    m = setup(sb)
    sb.stub("mise", script=MISE_USE_OK)
    m.kit("add", "aqua:o/tool@1.0")
    has(m.kit("add", "aqua:o/tool@1.0").out, "already tracked, nothing new")


@test
def add_tracked_tool_again_repins_latest(sb):
    m = setup(sb)
    sb.stub("brew", code=0)
    sb.stub("mise", script="[ \"$1\" = latest ] && echo 2.0\nexit 0\n")
    (m.src / "packages.json").write_text(json.dumps({
        "foo": {"mac": "brew:foo", "linux": "aqua:o/foo@1.0", "linux_fallback": "cargo:foo@0.5"},
        "bar": {"mac": "brew:bar", "linux": "aqua:o/bar@1.0"}}))
    r = m.kit("add", "foo")
    has(r.out, "updated")
    p = pkgs(m)
    ok(p["foo"] == {"mac": "brew:foo", "linux": "aqua:o/foo@2.0", "linux_fallback": "cargo:foo@2.0"}, p)
    ok(p["bar"]["linux"] == "aqua:o/bar@1.0", "another tool was re-pinned")
    ok("mise latest aqua:o/foo" in sb.calls("mise"), sb.calls())
    has(m.kit("add", "foo").out, "already tracked, nothing new")


# =========================================================================== rm
@test
def rm_file(sb):
    m = setup(sb, {".zshrc": "a\n"}, add=[".zshrc"])
    r = m.kit("rm", "~/.zshrc")
    has(r.out, ".zshrc", "untouched")
    ok(not m.repo_has("home/.zshrc") and m.read(".zshrc") == "a\n")
    r = m.kit("rm", "~/.zshrc", code=1)
    has(r.err, "not tracked")


@test
def rm_folder(sb):
    m = setup(sb, {".config/app/a": "a\n", ".config/app/b": "b\n"}, add=[".config/app"])
    r = m.kit("rm", "~/.config/app")
    has(r.out, "2 file(s)")
    ok(m.json("dirs.json") == [] and m.tracked() == [])
    ok(m.exists(".config/app/a"))


@test
def rm_inside_tracked_folder_writes_global_ignore(sb):
    m = setup(sb, {".config/app/a": "a\n", ".config/app/cache": "c\n"}, add=[".config/app"])
    (m.src / ".kitignore").write_text("[mac]\n.Trash\n\n[remote]\n")
    m.kit("rm", "~/.config/app/cache")
    ok(m.read(".config/app/cache") == "c\n")
    lines = (m.src / ".kitignore").read_text().splitlines()
    first_header = next(i for i, l in enumerate(lines) if l.startswith("["))
    idx = next((i for i, l in enumerate(lines) if l.startswith(".config/app/cache") and "forgotten" in l), None)
    ok(idx is not None and idx < first_header, f"forgotten line not in the global section: {lines}")
    r = m.kit("status", "-v", code=None)
    hasnt(r.out, "cache")
    ok(m.json("dirs.json") == [".config/app"])
    # add brings it back and drops the forgotten line
    r = m.kit("add", "~/.config/app/cache")
    has(r.out, "before")
    hasnt((m.src / ".kitignore").read_text(), ".config/app/cache")
    ok(".config/app/cache" in m.tracked())


@test
def rm_untracked_file_in_tracked_folder(sb):
    m = setup(sb, {".config/app/a": "a\n"}, add=[".config/app"])
    m.write(".config/app/junk", "j\n")
    r = m.kit("rm", "~/.config/app/junk")
    has(r.out, "won't show up")
    hasnt(m.kit("status", "-v", code=None).out, "junk")
    ok(m.exists(".config/app/junk"))


@test
def rm_escapes_patterns(sb):
    m = setup(sb, {".config/app/a": "a\n"}, add=[".config/app"])
    for n in ("weird[1].txt", "weird1.txt", "a*b.txt", "axxb.txt"):
        m.write(f".config/app/{n}", "n\n")
    m.kit("rm", "~/.config/app/weird[1].txt", "~/.config/app/a*b.txt")
    r = m.kit("status", "-v", code=None)
    has(r.out, "~/.config/app/weird1.txt", "~/.config/app/axxb.txt")
    hasnt(r.out, "~/.config/app/weird[1].txt", "~/.config/app/a*b.txt")


@test
def rm_through_tracked_symlink_refused(sb):
    m = setup(sb, {"realdir/f": "keep me\n"})
    os.makedirs(m.home / ".config")
    os.symlink("../realdir", m.home / ".config/link")
    m.kit("add", "~/.config/link")
    ok((m.src / "home/.config/link").is_symlink())
    r = m.kit("rm", "~/.config/link/f", code=1)
    has(r.err, "symlink")
    ok(m.read("realdir/f") == "keep me\n", "real file touched")
    ok((m.src / "home/.config/link").is_symlink())


@test
def rm_tool(sb):
    m = setup(sb)
    sb.stub("brew", code=0)
    sb.stub("mise", code=0)
    (m.src / "packages.json").write_text(json.dumps({"foo": {"mac": "brew:foo"}, "bar": {"mac": "brew:bar"}}))
    r = m.kit("rm", "foo")
    has(r.both, "no longer tracked")
    ok(list(pkgs(m)) == ["bar"], pkgs(m))
    ok(not any("uninstall" in c for c in sb.calls()), f"rm uninstalled: {sb.calls()}")
    r = m.kit("rm", "nope", code=1)
    has(r.err, "not tracked")
    ok(list(pkgs(m)) == ["bar"])


@test
def rm_path_wins_over_tool_name(sb):
    m = setup(sb, {"foo": "a file\n"}, add=["foo"])
    (m.src / "packages.json").write_text(json.dumps({"foo": {"mac": "brew:foo"}}))
    m.kit("rm", "foo")
    ok(m.tracked() == [], "the tracked file foo was not forgotten")
    ok("foo" in pkgs(m), "the tool foo was removed instead of the file")


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
    has(r.out, "kit save")


@test
def status_changed_in_repo(sb):
    m = setup(sb, {".zshrc": "a\n"}, add=[".zshrc"])
    m.repo_write("home/.zshrc", "b\n")
    r = check_status(m, "changed in repo", "~/.zshrc")
    has(r.out, "kit sync")


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
    has(r.out, "kit rm")


@test
def status_has_conflicts(sb):
    m = setup(sb, {".zshrc": "a\n"}, add=[".zshrc"])
    m.write(".zshrc", f"{MARK_OURS}\nmine\n{MARK_SEP}\ntheirs\n{MARK_THEIRS}\n")
    check_status(m, "has conflicts", "~/.zshrc")


@test
def status_file_vs_folder(sb):
    m = setup(sb, {".zshrc": "a\n"}, add=[".zshrc"])
    (m.home / ".zshrc").unlink()
    m.write(".zshrc/inner", "x\n")
    check_status(m, "file vs folder", "~/.zshrc")


@test
def status_folder_vs_symlink(sb):
    m = setup(sb, {".config/foo/x": "x\n"})
    (m.src / "home/.config").mkdir(parents=True)
    os.symlink("../bar", m.src / "home/.config/foo")
    check_status(m, "file vs folder", "~/.config/foo")


@test
def status_unreadable(sb):
    m = setup(sb, {".zshrc": "a\n"}, add=[".zshrc"])
    (m.home / ".zshrc").chmod(0)
    try:
        if os.access(m.home / ".zshrc", os.R_OK):
            return                       # running as root: can't make it unreadable
        check_status(m, "unreadable", "~/.zshrc")
    finally:
        (m.home / ".zshrc").chmod(0o644)


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
    r = m.kit("status", "~/.bashrc")
    has(r.out, ".bashrc", "changed here")
    hasnt(r.out, ".zshrc")


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
    has(r.out, ".config/app/a")
    hasnt(r.out, "application")


@test
def status_everything_in_sync_message(sb):
    m = setup(sb, {".zshrc": "a\n"}, add=[".zshrc"])
    m.sync()
    has(m.kit("status").out, "kit init")       # no backup repo yet
    m.kit("init", str(sb.bare()))
    m.sync()
    r = m.kit("status")
    has(r.out, "everything in sync")
    hasnt(r.out, "Missing", "not installed")


@test
def status_core_packages_not_installed(sb):
    m = setup(sb)
    sb.unstub("delta")
    sb.unstub("bat")
    r = m.kit("status")
    line = next((l for l in r.out.splitlines() if "not installed" in l), "")
    ok("bat" in line and "delta" in line, f"core packages line: {line!r}")
    ok(m.json("packages.json") == {})


@test
def status_missing_required_tools(sb):
    m = setup(sb)
    hasnt(m.kit("status").out, "Missing")
    sb.unstub("delta")
    sb.unstub("brew" if PLAT == "mac" else "mise")
    r = m.kit("status", code=None)
    line = next((l for l in r.out.splitlines() if "Missing" in l), "")
    ok(line, "no 'Missing:' line")
    has(line, "delta", "brew" if PLAT == "mac" else "mise")
    hasnt(line, "git")


@test
def status_repo_unsaved_and_unpushed(sb):
    m = setup(sb, {".zshrc": "a\n"}, add=[".zshrc"])
    bare = sb.bare()
    m.kit("init", str(bare))
    m.sync()
    hasnt(m.kit("status").out, "Repo:")
    m.write(".zshrc", "b\n")
    m.kit("save")
    r = m.kit("status", code=None)
    has(r.out, "not backed up", "kit sync")
    m.sync()
    hasnt(m.kit("status").out, "not backed up")
    m.repo_write("notes.txt", "n\n")
    m.git("add", "-A")
    m.git("commit", "-q", "-m", "by hand")
    r = m.kit("status", code=None)
    has(r.out, "1 commit(s) to send")


@test
def status_shows_packages_externals_scripts(sb):
    m = setup(sb)
    (m.src / "packages.json").write_text(json.dumps({"foo": {"mac": "brew:foo", "linux": "aqua:o/foo@1"}}))
    (m.src / "externals.json").write_text(json.dumps({".antidote": {"git": "file:///nonexistent"}}))
    m.repo_write("scripts/hello.sh", "echo hi\n")
    r = m.kit("status", code=None)
    has(r.out, "foo", "External ~/.antidote: missing", "hello.sh")


@test
def packages_invalid_json_clean_error(sb):
    m = setup(sb)
    (m.src / "packages.json").write_text("{\n<<<<<<< HEAD\n")
    r = m.kit("status", code=1)
    has(r.err, "packages.json")
    r = m.kit("add", "cask:foo", code=1)
    has(r.err, "packages.json")


# =========================================================================== status <file>: the diff
def diff_lines(out):
    return [l for l in out.splitlines() if l[:1] in "+-" and not l.startswith(("---", "+++"))]


@test
def status_file_shows_plain_patch(sb):
    m = setup(sb, {".zshrc": "one\n"}, add=[".zshrc"])
    m.write(".zshrc", "one\nlocal\n")
    r = m.kit("status", "~/.zshrc")
    has(r.out, "changed here", "--- a/.zshrc", "+++ b/.zshrc", "@@")
    ok(any(l[1:] == "local" for l in diff_lines(r.out)), "the changed line isn't in the patch")
    hasnt(r.out, "\033[")
    ok(sb.calls("delta") == [], "KIT_NO_DELTA=1 must keep delta out")
    env = dict(m.envvars)
    env.pop("KIT_NO_DELTA")
    sb.stub("delta", code=0)
    # without a tty the patch is plain even when KIT_NO_DELTA is unset
    r2 = subprocess.run(KIT + ["status", "~/.zshrc"], cwd=m.home, env=env, capture_output=True,
                        text=True, stdin=subprocess.DEVNULL)
    sb.history.append(Result(["status", "~/.zshrc", "(KIT_NO_DELTA unset)"], r2.returncode, r2.stdout, r2.stderr))
    has(r2.stdout, "--- a/.zshrc", "@@")
    hasnt(r2.stdout, "\033[")
    ok(sb.calls("delta") == [], f"delta used without a tty: {sb.calls('delta')}")


def run_in_pty(m, args, env=None, timeout=30):
    """Run kit with stdin/stdout/stderr on a pseudo-terminal; returns (exit code, output)."""
    import pty, select
    master, slave = pty.openpty()
    e = {k: v for k, v in dict(m.envvars, **(env or {})).items() if v is not None}
    p = subprocess.Popen(KIT + list(args), cwd=m.home, env=e, stdin=slave, stdout=slave, stderr=slave,
                         start_new_session=True)
    os.close(slave)
    buf, t0 = b"", time.time()
    while time.time() - t0 < timeout:
        r, _, _ = select.select([master], [], [], 0.2)
        if r:
            try:
                chunk = os.read(master, 4096)
            except OSError:
                break
            if not chunk:
                break
            buf += chunk
        elif p.poll() is not None:
            break
    p.wait(timeout=timeout)
    os.close(master)
    out = buf.decode(errors="replace")
    m.sb.history.append(Result(list(args) + ["(pty)"], p.returncode, out, ""))
    return p.returncode, out


@test
def status_file_uses_delta_on_a_terminal(sb):
    m = setup(sb, {".zshrc": "one\n"}, add=[".zshrc"])
    m.write(".zshrc", "one\nlocal\n")
    sb.stub("delta", script='echo DELTA-WAS-HERE; cat >/dev/null; exit 0\n')
    code, out = run_in_pty(m, ["status", "~/.zshrc"], env={"KIT_NO_DELTA": None, "TERM": "xterm-256color"})
    ok(code == 0, f"exit {code}")
    ok(sb.calls("delta"), "delta not used on a terminal")
    has(out, "DELTA-WAS-HERE")
    n = len(sb.calls("delta"))
    code, out = run_in_pty(m, ["status", "~/.zshrc"], env={"KIT_NO_DELTA": "1", "TERM": "xterm-256color"})
    ok(len(sb.calls("delta")) == n, "KIT_NO_DELTA=1 still used delta on a terminal")
    has(out, "local")


@test
def status_file_new_here(sb):
    m = setup(sb, {".config/app/a": "a\n"}, add=[".config/app"])
    m.write(".config/app/n", "brand new\n")
    r = m.kit("status", "~/.config/app/n")
    has(r.out, "new here")
    ok(any(l[1:] == "brand new" for l in diff_lines(r.out)), "new file's line isn't in the patch")


@test
def status_file_hides_secrets(sb):
    m = setup(sb, {".config/app/env": "x=1\n"}, add=[".config/app/env"])
    m.write(".config/app/env", f"x={TOKEN}\n")
    r = m.kit("status", "~/.config/app/env")
    has(r.out, "hidden")
    hasnt(r.both, TOKEN)


@test
def status_file_binary_summary(sb):
    m = setup(sb, {".config/app/blob": b"ab\0cd"}, add=[".config/app/blob"])
    m.write(".config/app/blob", b"ab\0cdef")
    r = m.kit("status", "~/.config/app/blob")
    has(r.out, "binary file")
    hasnt(r.out, "@@")


@test
def status_folder_shows_patches_of_changed_files(sb):
    m = setup(sb, {".config/app/a": "a\n", ".config/app/b": "b\n"}, add=[".config/app"])
    m.write(".config/app/a", "a2\n")
    r = m.kit("status", "~/.config/app")
    has(r.out, ".config/app/a", "@@")
    ok(any(l[1:] == "a2" for l in diff_lines(r.out)))
    hasnt(r.out, "--- a/.config/app/b")


# =========================================================================== save
@test
def save_copies_changes_without_committing(sb):
    m = setup(sb, {".zshrc": "a\n"}, add=[".zshrc"])
    m.sync()
    head = m.head()
    m.write(".zshrc", "b\n")
    r = m.kit("save")
    has(r.out, "1 change(s) saved", "kit sync")
    ok(m.repo_read("home/.zshrc") == "b\n")
    ok(m.head() == head, "save made a commit")
    ok(m.git("status", "--porcelain").stdout.strip() != "", "save left nothing to commit")
    has(m.kit("status", "~/.zshrc").out, "in sync")
    has(m.kit("save").out, "nothing to save")


@test
def save_new_and_deleted_files(sb):
    m = setup(sb, {".config/app/a": "a\n", ".config/app/b": "b\n"}, add=[".config/app"])
    m.write(".config/app/c", "c\n")
    (m.home / ".config/app/b").unlink()
    r = m.kit("save")
    has(r.out, "2 change(s) saved", "added", ".config/app/c", "removed", ".config/app/b")
    ok(m.repo_has("home/.config/app/c") and not m.repo_has("home/.config/app/b"))


@test
def save_path_scope(sb):
    m = setup(sb, {".a": "a\n", ".b": "b\n"}, add=[".a", ".b"])
    m.write(".a", "A\n")
    m.write(".b", "B\n")
    has(m.kit("save", "~/.a").out, "1 change(s) saved")
    ok(m.repo_read("home/.a") == "A\n" and m.repo_read("home/.b") == "b\n")


@test
def save_skips_both_without_force(sb):
    m = setup(sb, {".zshrc": "a\n"}, add=[".zshrc"])
    m.repo_write("home/.zshrc", "repo\n")
    m.write(".zshrc", "mine\n")
    r = m.kit("save", code=1)
    has(r.both, ".zshrc")
    ok(m.repo_read("home/.zshrc") == "repo\n", "changed-on-both file saved without --force")
    m.kit("save", "--force")
    ok(m.repo_read("home/.zshrc") == "mine\n")


@test
def save_leaves_repo_changes(sb):
    m = setup(sb, {".zshrc": "a\n"}, add=[".zshrc"])
    m.repo_write("home/.zshrc", "repo\n")
    has(m.kit("save").out, "nothing to save")
    ok(m.repo_read("home/.zshrc") == "repo\n", "save overwrote a repo change")


@test
def save_refuses_conflict_markers(sb):
    m = setup(sb, {".zshrc": "a\n"}, add=[".zshrc"])
    m.write(".zshrc", f"{MARK_OURS}\nmine\n{MARK_SEP}\ntheirs\n{MARK_THEIRS}\n")
    r = m.kit("save", code=1)
    has(r.err, ".zshrc")
    ok(m.repo_read("home/.zshrc") == "a\n", "file with conflict markers saved without --force")
    m.kit("save", "--force")
    has(m.repo_read("home/.zshrc"), MARK_OURS)


@test
def save_skips_files_with_rules(sb):
    m = setup(sb, {".zshrc": "alias x=pbcopy\nok\n"}, add=[".zshrc"])
    (m.src / "rules.json").write_text(json.dumps([{"path": ".zshrc", "on": [PLAT], "delete_lines": ["pbcopy"]}]))
    m.kit("undo", "~/.zshrc")
    ok(m.read(".zshrc") == "ok\n", repr(m.read(".zshrc")))
    m.write(".zshrc", "ok\nmore\n")
    r = m.kit("save", code=None)
    has(r.err, "rules")
    ok(m.repo_read("home/.zshrc") == "alias x=pbcopy\nok\n")


@test
def save_refuses_secret_content(sb):
    m = setup(sb, {".config/app/env": "x=1\n"}, add=[".config/app/env"])
    m.write(".config/app/env", f"x={TOKEN}\n")
    r = m.kit("save", code=1)
    has(r.err, "secret")
    hasnt(r.both, TOKEN)
    ok(m.repo_read("home/.config/app/env") == "x=1\n")
    m.kit("save", "--force")
    has(m.repo_read("home/.config/app/env"), TOKEN)


# =========================================================================== sync: commit / push
@test
def sync_commits_and_warns_without_remote(sb):
    m = setup(sb, {".zshrc": "a\n"}, add=[".zshrc"])
    r = m.sync()
    has(r.err, "not backed up", "kit init")
    ok(m.git("log", "--format=%s").stdout.strip(), "nothing committed")
    ok(m.git("ls-files").stdout.count("home/.zshrc") == 1, "home/.zshrc not committed")
    ok(m.git("status", "--porcelain").stdout.strip() == "", "repo dirty after sync")
    head = m.head()
    m.sync()
    ok(m.head() == head, "a sync with nothing new made a commit")


@test
def sync_refuses_secrets(sb):
    m = setup(sb, {".config/app/env": f"t={TOKEN}\n"})
    m.kit("add", "--force", "~/.config/app/env")
    r = m.kit("sync", code=1)
    has(r.err, "secret", "config/app/env")
    hasnt(r.both, TOKEN)
    ok(m.git("diff", "--cached", "--name-only").stdout.strip() == "", "files left staged")
    ok(m.git("rev-parse", "HEAD", check=False).returncode != 0, "something was committed")


@test
def sync_pushes_to_remote(sb):
    m = setup(sb, {".zshrc": "a\n"}, add=[".zshrc"])
    bare = sb.bare()
    m.kit("init", str(bare))
    r = m.sync()
    hasnt(r.err, "not backed up")
    ok(bare_show(sb, bare, "home/.zshrc") == "a\n")
    ok(m.git("status", "--porcelain").stdout.strip() == "")


@test
def sync_commits_files_hidden_by_nested_gitignore(sb):
    m = setup(sb, {".config/app/.gitignore": "*.log\n", ".config/app/app.log": "log\n"},
              add=[".config/app"])
    ok(".config/app/app.log" in m.tracked())
    m.sync()
    ok("home/.config/app/app.log" in m.git("ls-files").stdout, "hidden file not committed")


@test
def sync_commits_hand_edits_in_repo(sb):
    m = setup(sb)
    m.repo_write("rules.json", "[]\n\n")
    m.repo_write("notes.md", "n\n")
    m.sync()
    ok("notes.md" in m.git("ls-files").stdout)
    ok(m.git("status", "--porcelain").stdout.strip() == "")


# =========================================================================== sync: writing $HOME
@test
def sync_writes_missing(sb):
    m = setup(sb)
    m.repo_write("home/.config/git/config", "[core]\n")
    r = m.sync()
    has(r.out, ".config/git/config")
    ok(m.read(".config/git/config") == "[core]\n")


@test
def sync_takes_repo_changes(sb):
    m = setup(sb, {".zshrc": "a\n"}, add=[".zshrc"])
    m.repo_write("home/.zshrc", "b\n")
    m.sync()
    ok(m.read(".zshrc") == "b\n")
    has(m.kit("status", "~/.zshrc").out, "in sync")


@test
def sync_leaves_changes_here_alone(sb):
    m = setup(sb, {".zshrc": "a\n"}, add=[".zshrc"])
    m.write(".zshrc", "mine\n")
    m.sync()
    ok(m.read(".zshrc") == "mine\n", "a change made here was overwritten")
    ok(m.repo_read("home/.zshrc") == "a\n", "sync saved a change (that's kit save's job)")
    check_status(m, "changed here", "~/.zshrc")


@test
def sync_merges_on_one_machine(sb):
    base = "l1\nl2\nl3\nl4\nl5\n"
    m = setup(sb, {".zshrc": base}, add=[".zshrc"])
    m.sync()
    ok(m.base_file(".zshrc") == base, f"KIT_STATE/base/.zshrc: {m.base_file('.zshrc')!r}")
    m.repo_write("home/.zshrc", "L1\nl2\nl3\nl4\nl5\n")
    m.write(".zshrc", "l1\nl2\nl3\nl4\nL5\n")
    m.sync()
    ok(m.read(".zshrc") == "L1\nl2\nl3\nl4\nL5\n", repr(m.read(".zshrc")))
    ok(m.repo_read("home/.zshrc") == "L1\nl2\nl3\nl4\nl5\n", "the merge changed the repo")
    check_status(m, "changed here", "~/.zshrc")
    m.kit("undo")
    ok(m.read(".zshrc") == "l1\nl2\nl3\nl4\nL5\n", "undo didn't put back the pre-merge file")


@test
def sync_creates_ssh_dir_private(sb):
    m = setup(sb)
    m.repo_write("home/.ssh/config", "Host x\n")
    m.repo_write("home/.config/app/conf", "x\n")
    m.sync()
    ok(m.mode(".ssh") == 0o700, oct(m.mode(".ssh")))
    ok(m.mode(".ssh/config") == 0o600, oct(m.mode(".ssh/config")))
    ok(m.mode(".config/app/conf") == 0o644, oct(m.mode(".config/app/conf")))


@test
def sync_preserves_executable_bit(sb):
    m = setup(sb)
    m.repo_write("home/.local/bin/tool", "#!/bin/sh\necho hi\n", mode=0o755)
    m.sync()
    ok(m.mode(".local/bin/tool") & 0o111, "not executable")


@test
def sync_keeps_local_permissions(sb):
    m = setup(sb, {".zshrc": "a\n"}, add=[".zshrc"])
    (m.home / ".zshrc").chmod(0o600)
    m.repo_write("home/.zshrc", "b\n")
    m.sync()
    ok(m.read(".zshrc") == "b\n")
    ok(m.mode(".zshrc") == 0o600, oct(m.mode(".zshrc")))


@test
def sync_refuses_repo_conflict_markers(sb):
    m = setup(sb, {".zshrc": "a\n"}, add=[".zshrc"])
    m.repo_write("home/.zshrc", "<<<<<<< HEAD\nx\n=======\ny\n>>>>>>> other\n")
    r = m.kit("sync", code=1)
    has(r.err, "conflict markers")
    ok(m.read(".zshrc") == "a\n")


@test
def sync_never_deletes_folder(sb):
    m = setup(sb, {".config/foo/x": "precious\n"})
    (m.src / "home/.config").mkdir(parents=True)
    os.symlink("../bar", m.src / "home/.config/foo")
    r = m.kit("sync", code=None)
    p = m.home / ".config/foo"
    ok(p.is_dir() and not p.is_symlink(), "folder removed")
    ok(m.read(".config/foo/x") == "precious\n")
    has(r.both, ".config/foo")
    m.kit("undo", "~/.config/foo")
    ok(p.is_symlink() and os.readlink(p) == "../bar", "undo <path> replaces the folder with the repo's symlink")
    m.kit("undo")
    ok(p.is_dir() and not p.is_symlink(), "undo brings the folder back")
    ok(m.read(".config/foo/x") == "precious\n")


@test
def sync_file_where_folder_expected_fails_cleanly(sb):
    m = setup(sb)
    m.write(".config", "i am a file\n")
    m.repo_write("home/.config/app/x", "x\n")
    r = m.kit("sync", code=1)
    has(r.err, ".config")
    ok(m.read(".config") == "i am a file\n")


@test
def sync_runs_packages_and_scripts(sb):
    m = setup(sb)
    m.repo_write("scripts/hello.sh", 'echo ran >> "$HOME/ran"\n')
    if PLAT == "mac":
        sb.unstub("bat")
        sb.stub("brew", code=0)
    m.sync()
    ok(m.exists("ran"), "scripts run by sync")
    if PLAT == "mac":
        ok(any("install" in c and "bat" in c for c in sb.calls("brew")), sb.calls())


@test
def sync_mac_packages_fail_exit_1(sb):
    if PLAT != "mac":
        return
    m = setup(sb)
    (m.src / "packages.json").write_text(json.dumps({"foo": {"mac": "brew:foo", "linux": "aqua:o/foo@1"}}))
    has(m.kit("status").out, "foo")
    m.kit("sync", code=1)      # fake brew fails
    sb.stub("brew", code=0)
    m.sync()
    ok("brew install foo" in sb.calls("brew"), sb.calls())


@test
def sync_guard_repo_mid_rebase(sb):
    m = setup(sb, {".zshrc": "a\n"}, add=[".zshrc"])
    (m.src / ".git/rebase-merge").mkdir()
    r = m.kit("sync", code=1)
    has(r.err, "rebase")
    has(m.kit("status", code=None).out, "middle of a git rebase")


@test
def sync_rejects_flags(sb):
    m = setup(sb)
    for f in ("--force", "--yes", "-n", "--no-packages", "--no-scripts"):
        m.kit("sync", f, code=2)
    m.kit("sync", "~/.zshrc", code=2)


# =========================================================================== sync: two machines
@test
def sync_pulls_from_other_machine(sb):
    m1, m2, bare = two_machines(sb, {".zshrc": "v1\n"})
    ok(m2.read(".zshrc") == "v1\n")
    m1.write(".zshrc", "v2\n")
    m1.kit("save")
    m1.sync()
    m2.sync()
    ok(m2.read(".zshrc") == "v2\n")
    ok(m2.repo_read("home/.zshrc") == "v2\n")


@test
def sync_does_not_run_pulled_scripts_without_tty(sb):
    m1, m2, bare = two_machines(sb, {".zshrc": "v1\n"})
    m1.repo_write("scripts/a.sh", 'touch "$HOME/ran-a"\n')
    m1.sync()
    ok(m1.exists("ran-a"), "m1's own new script didn't run")
    r = m2.sync(code=None)
    has(r.both, "a.sh")
    ok(not m2.exists("ran-a"), "a script pulled from the remote ran without a terminal")
    has(m2.kit("status", code=None).out, "a.sh")


MERGE_BASE = "one\ntwo\nthree\nfour\nfive\n"


@test
def merge_a_different_lines(sb):
    a, b, bare = two_machines(sb, {".zshrc": MERGE_BASE})
    a.write(".zshrc", "ONE-a\ntwo\nthree\nfour\nfive\n")
    a.kit("save")
    a.sync()
    b.write(".zshrc", "one\ntwo\nthree\nfour\nFIVE-b\n")
    b.sync()
    merged = "ONE-a\ntwo\nthree\nfour\nFIVE-b\n"
    ok(b.read(".zshrc") == merged, f"B's file: {b.read('.zshrc')!r}")
    ok(b.repo_read("home/.zshrc") == "ONE-a\ntwo\nthree\nfour\nfive\n", "the merge changed B's repo copy")
    no_rebase_left(b)
    check_status(b, "changed here", "~/.zshrc")
    b.kit("save")
    b.sync()
    ok(bare_show(sb, bare, "home/.zshrc") == merged, "merged file not published")
    a.sync()
    ok(a.read(".zshrc") == merged, f"A's file: {a.read('.zshrc')!r}")
    hasnt(a.kit("status", "-v", code=None).out, ".zshrc")
    hasnt(b.kit("status", "-v", code=None).out, ".zshrc")


def same_line_conflict(sb):
    a, b, bare = two_machines(sb, {".zshrc": MERGE_BASE})
    a.write(".zshrc", "one\ntwo\nTHREE-a\nfour\nfive\n")
    a.kit("save")
    a.sync()
    b.write(".zshrc", "one\ntwo\nTHREE-b\nfour\nfive\n")
    r = b.kit("sync", code=1)
    return a, b, bare, r


def check_markers(text):
    has(text, MARK_OURS, MARK_SEP, MARK_THEIRS, "THREE-a", "THREE-b")
    ok(text.index(MARK_OURS) < text.index("THREE-b") < text.index(MARK_SEP) < text.index("THREE-a")
       < text.index(MARK_THEIRS), f"markers in the wrong order:\n{text}")
    ok(text.startswith("one\ntwo\n") and text.endswith("four\nfive\n"), "untouched lines lost")


@test
def merge_b_same_line_conflict(sb):
    a, b, bare, r = same_line_conflict(sb)
    check_markers(b.read(".zshrc"))
    has(r.err, ".zshrc", "kit save", "kit undo")
    ok(b.repo_read("home/.zshrc") == "one\ntwo\nTHREE-a\nfour\nfive\n", "B's repo copy isn't A's version")
    no_rebase_left(b)
    check_status(b, "has conflicts", "~/.zshrc")
    b.kit("save", code=1)
    hasnt(b.repo_read("home/.zshrc"), "<<<<<<<")
    # resolve by hand, then save + sync
    b.write(".zshrc", "one\ntwo\nTHREE-ab\nfour\nfive\n")
    b.kit("save")
    b.sync()
    ok(bare_show(sb, bare, "home/.zshrc") == "one\ntwo\nTHREE-ab\nfour\nfive\n")
    a.sync()
    ok(a.read(".zshrc") == "one\ntwo\nTHREE-ab\nfour\nfive\n")


@test
def merge_b_conflict_undo_file_takes_repo(sb):
    a, b, bare, r = same_line_conflict(sb)
    b.kit("undo", "~/.zshrc")
    ok(b.read(".zshrc") == "one\ntwo\nTHREE-a\nfour\nfive\n", repr(b.read(".zshrc")))
    hasnt(b.kit("status", "-v", code=None).out, ".zshrc")


@test
def merge_b_conflict_undo_restores_pre_merge(sb):
    a, b, bare, r = same_line_conflict(sb)
    b.kit("undo")
    ok(b.read(".zshrc") == "one\ntwo\nTHREE-b\nfour\nfive\n", repr(b.read(".zshrc")))
    r = b.kit("status", "-v", code=None)
    ok(any(l.rstrip().endswith("~/.zshrc") and "has conflicts" not in l for l in r.out.splitlines()),
       "status doesn't list ~/.zshrc as changed after undo")


@test
def merge_b_conflict_force_save(sb):
    a, b, bare, r = same_line_conflict(sb)
    b.kit("save", "--force")
    has(b.repo_read("home/.zshrc"), MARK_OURS)


@test
def merge_c_repo_level_conflict(sb):
    a, b, bare = two_machines(sb, {".zshrc": MERGE_BASE})
    a.write(".zshrc", "one\ntwo\nTHREE-a\nfour\nfive\n")
    a.kit("save")
    a.sync()
    remote_head = sb.git_raw(["--git-dir", str(bare), "rev-parse", "main"]).stdout.strip()
    b.write(".zshrc", "one\ntwo\nTHREE-b\nfour\nfive\n")
    b.kit("save")
    r = b.kit("sync", code=1)
    no_rebase_left(b)
    hasnt(b.kit("status", code=None).out, "middle of")
    ok(b.head() == remote_head, "B's repo doesn't match the remote after the failed rebase")
    ok(b.git("status", "--porcelain").stdout.strip() == "", "B's repo left dirty")
    ok(b.repo_read("home/.zshrc") == "one\ntwo\nTHREE-a\nfour\nfive\n")
    check_markers(b.read(".zshrc"))
    has(r.err, ".zshrc")
    check_status(b, "has conflicts", "~/.zshrc")
    ok(sb.git_raw(["--git-dir", str(bare), "rev-parse", "main"]).stdout.strip() == remote_head,
       "the conflicting sync pushed")
    b.write(".zshrc", "one\ntwo\nTHREE-ab\nfour\nfive\n")
    b.kit("save")
    b.sync()
    a.sync()
    ok(a.read(".zshrc") == "one\ntwo\nTHREE-ab\nfour\nfive\n")


@test
def merge_c_repo_level_conflict_clean_home_merge(sb):
    # both saved+synced the same file, different lines: the rebase goes through, B's home gets both
    a, b, bare = two_machines(sb, {".zshrc": MERGE_BASE})
    a.write(".zshrc", "ONE-a\ntwo\nthree\nfour\nfive\n")
    a.kit("save")
    a.sync()
    b.write(".zshrc", "one\ntwo\nthree\nfour\nFIVE-b\n")
    b.kit("save")
    b.sync()
    merged = "ONE-a\ntwo\nthree\nfour\nFIVE-b\n"
    no_rebase_left(b)
    ok(b.read(".zshrc") == merged, f"B's file: {b.read('.zshrc')!r}")
    b.kit("save", code=None)
    b.sync()
    ok(bare_show(sb, bare, "home/.zshrc") == merged)
    a.sync()
    ok(a.read(".zshrc") == merged)


@test
def merge_c_unrelated_files_rebase_cleanly(sb):
    a, b, bare = two_machines(sb, {".zshrc": "z\n", ".bashrc": "b\n"})
    a.write(".zshrc", "z-a\n")
    a.kit("save")
    a.sync()
    b.write(".bashrc", "b-b\n")
    b.kit("save")
    b.sync()
    no_rebase_left(b)
    ok(b.read(".zshrc") == "z-a\n" and b.read(".bashrc") == "b-b\n")
    ok(bare_show(sb, bare, "home/.bashrc") == "b-b\n" and bare_show(sb, bare, "home/.zshrc") == "z-a\n")


@test
def merge_d_no_base_left_alone(sb):
    m1 = setup(sb, {".zshrc": "from m1\n"}, add=[".zshrc"])
    bare = sb.bare()
    m1.kit("init", str(bare))
    m1.sync()
    m2 = sb.machine("m2")
    m2.write(".zshrc", "m2's own zshrc\n")
    m2.kit("init", str(bare))
    r = m2.kit("sync", code=None)
    ok(m2.read(".zshrc") == "m2's own zshrc\n", "a never-synced file was overwritten")
    hasnt(m2.read(".zshrc"), "<<<<<<<")
    has(r.both, ".zshrc", "changed on both")
    r = check_status(m2, "changed on both", "~/.zshrc")
    ok(m2.repo_read("home/.zshrc") == "from m1\n")
    m2.kit("undo", "~/.zshrc")
    ok(m2.read(".zshrc") == "from m1\n")


# =========================================================================== undo
@test
def undo_file_takes_repo_and_backs_up(sb):
    m = setup(sb, {".zshrc": "a\n"}, add=[".zshrc"])
    m.write(".zshrc", "mine\n")
    m.kit("undo", "~/.zshrc")
    ok(m.read(".zshrc") == "a\n")
    stamps = list((m.state / "backups").iterdir())
    ok(len(stamps) == 1 and (stamps[0] / ".zshrc").read_text() == "mine\n", "backup content")
    r = m.kit("undo")
    has(r.out, ".zshrc")
    ok(m.read(".zshrc") == "mine\n", "undo put it back")


@test
def undo_path_scope(sb):
    m = setup(sb, {".a": "a\n", ".b": "b\n"}, add=[".a", ".b"])
    m.write(".a", "A\n")
    m.write(".b", "B\n")
    m.kit("undo", "~/.a")
    ok(m.read(".a") == "a\n" and m.read(".b") == "B\n")


@test
def undo_nothing_and_named_backup(sb):
    m = setup(sb, {".zshrc": "a\n"}, add=[".zshrc"])
    r = m.kit("undo")
    has(r.out, "nothing to undo")
    m.write(".zshrc", "mine\n")
    m.kit("undo", "~/.zshrc")
    stamp = next((m.state / "backups").iterdir()).name
    m.write(".zshrc", "something else\n")
    m.kit("undo", stamp)
    ok(m.read(".zshrc") == "mine\n")
    m.kit("undo", "19990101-000000", code=1)


@test
def undo_untracked_path_fails(sb):
    m = setup(sb, {".other": "o\n"})
    r = m.kit("undo", "~/.other", code=1)
    has(r.err, "not tracked")
    ok(m.read(".other") == "o\n")


@test
def undo_rejects_yes_flag(sb):
    m = setup(sb)
    m.kit("undo", "-y", code=2)
    m.kit("undo", "--yes", code=2)


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
    m.sync()
    m.sync()
    ok(count(m) == 1, count(m))
    hasnt(m.kit("status", code=None).out, "c.sh")
    script(m, "c.sh", "# changed\n")
    m.sync()
    ok(count(m) == 2, count(m))
    ok((m.state / "scripts.json").exists() and not (m.src / ".kit").exists())


@test
def scripts_once(sb):
    m = setup(sb)
    script(m, "o.sh", "# kit: run=once\n")
    m.sync()
    script(m, "o.sh", "# kit: run=once\n# edited\n")
    m.sync()
    ok(count(m) == 1, count(m))


@test
def scripts_always(sb):
    m = setup(sb)
    script(m, "a.sh", "# kit: run=always\n")
    for _ in range(3):
        m.sync()
    ok(count(m) == 3, count(m))


@test
def scripts_on_other_platform_skipped(sb):
    m = setup(sb)
    script(m, "p.sh", f"# kit: on={OTHER}\n")
    script(m, "q.sh", f"# kit: on={PLAT}\n", body='echo q >> "$HOME/q"\n')
    m.sync()
    ok(count(m) == 0 and count(m, "q") == 1)
    hasnt(m.kit("status", code=None).out, "p.sh")


@test
def scripts_watch(sb):
    m = setup(sb, {".config/app/x": "1\n"})
    script(m, "w.sh", "# kit: watch=.config/app .other\n")
    m.sync()
    m.sync()
    ok(count(m) == 1)
    m.write(".config/app/x", "2\n")
    m.sync()
    ok(count(m) == 2)
    m.write(".other", "now exists\n")
    m.sync()
    ok(count(m) == 3)


@test
def scripts_header_only_in_top_comment_block(sb):
    m = setup(sb)
    m.repo_write("scripts/h.sh", '#!/bin/bash\n# kit: run=always  # every time\n\necho h >> "$HOME/h"\n# kit: run=once\n')
    m.repo_write("scripts/i.sh", '#!/bin/bash\necho i >> "$HOME/i"\n# kit: run=always\n')
    for _ in range(3):
        m.sync()
    ok(count(m, "h") == 3, f"h.sh (run=always) ran {count(m, 'h')} times")
    ok(count(m, "i") == 1, f"i.sh (header below code → run=onchange) ran {count(m, 'i')} times")


@test
def scripts_unknown_header_reported(sb):
    m = setup(sb)
    script(m, "u.sh", "# kit: frequency=daily\n# kit: run=sometimes\n")
    r = m.kit("sync", code=None)
    has(r.err, "frequency")


@test
def scripts_failing_runs_again(sb):
    m = setup(sb)
    script(m, "f.sh", "", body='echo x >> "$HOME/count"\nexit 3\n')
    r = m.kit("sync", code=1)
    has(r.err, "f.sh")
    m.kit("sync", code=1)
    ok(count(m) == 2, count(m))


@test
def scripts_get_env_and_cwd(sb):
    m = setup(sb)
    script(m, "e.sh", "", body='pwd > "$HOME/pwd"; echo "$KIT_SOURCE" > "$HOME/src"\n')
    m.sync()
    ok(os.path.realpath(m.read("pwd").strip()) == str(m.home))
    ok(m.read("src").strip() == str(m.src))


# =========================================================================== externals
@test
def externals_cloned_on_sync(sb):
    m = setup(sb)
    ext = sb.make_git_repo("ext", {"README": "ext\n"})
    sb.git_raw(["tag", "v1"], cwd=ext)
    (m.src / "externals.json").write_text(json.dumps({
        ".antidote": {"git": f"file://{ext}"}, ".tagged": {"git": f"file://{ext}", "ref": "v1"}}))
    has(m.kit("status").out, "External ~/.antidote: missing")
    m.sync()
    ok(m.read(".antidote/README") == "ext\n" and m.read(".tagged/README") == "ext\n")
    hasnt(m.kit("status").out, "External")


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
    m.sync()
    ok(m.read(".zshrc") == "export A=2\nbar here\nbar2\n", repr(m.read(".zshrc")))
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


# =========================================================================== push
def push_ready(sb, m):
    m.repo_write("lib/remote/setup.sh", "echo setup\n")


@test
def push_without_host_and_no_servers(sb):
    m = setup(sb)
    r = m.kit("push")
    has(r.both, "no servers yet")
    ok(sb.calls("ssh") == [] and sb.calls("rsync") == [], sb.calls())


@test
def push_without_host_pushes_every_server(sb):
    m = setup(sb, {".config/app/a": "a\n"}, add=[".config/app"])
    push_ready(sb, m)
    (m.src / "remotes.json").write_text(json.dumps({"alpha": {"stamp": "", "at": ""}, "beta": {"stamp": "", "at": ""}}))
    sb.stub("ssh", code=0)
    sb.stub("rsync", code=0)
    m.kit("push")
    rs = sb.calls("rsync")
    ok(any("alpha:" in c for c in rs) and any("beta:" in c for c in rs), sb.calls())


@test
def push_first_time_no_confirmation(sb):
    m = setup(sb, {".config/app/a": "a\n", ".config/kitty/k": "k\n", ".zshrc": "z\n"})
    m.kit("add", "~/.config/app", "~/.zshrc")
    m.kit("add", "--no-servers", "~/.config/kitty")
    push_ready(sb, m)
    sb.stub("ssh", code=0)
    sb.stub("rsync", code=0)
    r = m.kit("push", "box")              # no tty, no --yes: goes ahead
    has(r.out, "box")
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
def push_no_servers_tool_not_sent(sb):
    m = setup(sb)
    push_ready(sb, m)
    (m.src / "packages.json").write_text(json.dumps({
        "keep": {"linux": "aqua:o/keep@1.0"}, "skip": {"linux": "aqua:o/skip@1.0", "remote": False}}))
    sb.stub("ssh", code=0)
    sb.stub("rsync", code=0)
    m.kit("push", "box")
    toml = (m.state / "payload/mise.toml").read_text()
    has(toml, "aqua:o/keep")
    hasnt(toml, "aqua:o/skip")


@test
def push_unreachable_host(sb):
    m = setup(sb)
    push_ready(sb, m)
    r = m.kit("push", "box", code=1)
    has(r.err, "box")
    ok(not (m.src / "remotes.json").exists() or "box" not in m.json("remotes.json"))


@test
def push_remove_needs_host_and_old_flags_gone(sb):
    m = setup(sb)
    r = m.kit("push", "--remove", code=None)
    ok(r.code not in (0, None), "push --remove without a host succeeded")
    ok(sb.calls("ssh") == [])
    for f in ("--yes", "--all", "--off", "--on", "-n"):
        m.kit("push", "box", f, code=2)


# =========================================================================== CLI
def help_commands(text):
    lines = text.splitlines()
    start = next(i for i, l in enumerate(lines) if l.strip().lower().startswith("commands"))
    cmds = []
    for l in lines[start + 1:]:
        if not l.startswith("  ") or not l.strip():
            break
        cmds.append(l.split()[0])
    return cmds


@test
def cli_help_shows_exactly_eight_commands(sb):
    m = sb.machine()
    r = m.kit("--help")
    cmds = help_commands(r.out)
    ok(sorted(cmds) == sorted(COMMANDS), f"commands in --help: {cmds}")
    has(m.kit().out, "status")


@test
def cli_version_and_help(sb):
    m = sb.machine()
    r = m.kit("--version")
    ok(r.out.strip().startswith("kit "), r.out)
    r = m.kit("help", "add")
    has(r.out, "--no-servers", "--fallback", "--bin", "--mac", "--linux")
    hasnt(r.out, "--only")
    has(m.kit("help", "sync").out, "sync")


@test
def cli_removed_commands_redirect(sb):
    m = setup(sb)
    redirects = {"apply": "kit sync", "re-add": "kit save", "forget": "kit rm", "ignore": "kit rm",
                 "diff": "kit status <file>", "update": "kit sync", "restore": "kit undo",
                 "remote": "kit push", "pkg": "kit rm", "doctor": "kit status"}
    for cmd, hint in redirects.items():
        r = m.kit(cmd, code=2)
        has(r.err, hint)
    for cmd in ("cd", "git", "edit", "cat", "managed", "unmanaged", "verify", "scripts", "shell", "source-path"):
        r = m.kit(cmd, code=2)
        has(r.err, "kit ")
        hasnt(r.err, "did you mean")
    r = m.kit("diff", "~/.zshrc", code=2)
    has(r.err, "kit status <file>")
    r = m.kit("pkg", "add", "foo", code=2)
    has(r.err, "kit add", "kit rm")


@test
def cli_typo_suggests(sb):
    m = setup(sb)
    r = m.kit("stauts", code=2)
    has(r.err, "did you mean 'status'")
    r = m.kit("snyc", code=2)
    has(r.err, "did you mean 'sync'")


@test
def cli_chezmoi_hints(sb):
    m = setup(sb)
    r = m.kit("merge", code=2)
    has(r.err, "kit has no 'merge'")


@test
def cli_usage_errors_exit_2(sb):
    m = setup(sb)
    m.kit("add", code=2)
    m.kit("rm", code=2)
    m.kit("status", "--bogus", code=2)
    m.kit("add", "--only", "linux", "x", code=2)
    m.kit("save", "-a", code=2)


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


def values(m, *words):
    return [l.split("\t")[0] for l in complete(m, *words)]


@test
def complete_commands(sb):
    m = setup(sb)
    lines = complete(m, "")
    cmds = {l.split("\t")[0]: l for l in lines}
    for c in COMMANDS:
        ok(c in cmds, f"{c} missing from {sorted(cmds)}")
    for c in ("apply", "pkg", "diff", "update", "doctor", "cd"):
        ok(c not in cmds, f"removed command {c} offered: {sorted(cmds)}")
    ok("\t" in cmds["status"] and cmds["status"].split("\t", 1)[1].strip(), "no description")


@test
def complete_tracked_paths_folder_by_folder(sb):
    m = setup(sb, {".config/app/a": "a\n", ".config/nvim/init.lua": "x\n", ".zshrc": "z\n"},
              add=[".config/app", ".config/nvim/init.lua", ".zshrc"])
    for cmd in ("status", "save", "undo", "rm"):
        vals = values(m, cmd, "~/.con")
        ok(vals == ["~/.config/"], f"{cmd}: {vals}")
        vals = values(m, cmd, "~/.config/")
        ok(vals == ["~/.config/app/", "~/.config/nvim/"], f"{cmd}: {vals}")
        vals = values(m, cmd, "~/.zs")
        ok(vals == ["~/.zshrc"], f"{cmd}: {vals}")


@test
def complete_files_for_add(sb):
    m = setup(sb)
    ok(complete(m, "add", "x") == ["__files__"], complete(m, "add", "x"))


@test
def complete_rm_offers_tools(sb):
    m = setup(sb)
    (m.src / "packages.json").write_text(json.dumps({"bat": {"mac": "brew:bat"}, "eza": {"mac": "cargo:eza"}}))
    vals = values(m, "rm", "")
    ok("bat" in vals and "eza" in vals, vals)


@test
def complete_undo_offers_backups(sb):
    m = setup(sb, {".zshrc": "a\n"}, add=[".zshrc"])
    m.write(".zshrc", "mine\n")
    m.kit("undo", "~/.zshrc")
    stamp = next((m.state / "backups").iterdir()).name
    vals = values(m, "undo", stamp[:4])
    ok(stamp in vals, f"{stamp} not in {vals}")


@test
def complete_hosts(sb):
    m = setup(sb)
    (m.src / "remotes.json").write_text(json.dumps({"alpha": {"stamp": "", "at": ""}}))
    m.write(".ssh/config", "Host beta gamma\nHost *.wild\n  HostName x\n")
    hosts = values(m, "push", "")
    ok(hosts == ["alpha", "beta", "gamma"], hosts)


@test
def complete_options(sb):
    m = setup(sb)
    opts = values(m, "add", "--")
    ok("--force" in opts and "--no-servers" in opts and "--only" not in opts, opts)


@test
def complete_completion_choices(sb):
    m = setup(sb)
    ok(sorted(complete(m, "completion", "")) == ["bash", "fish", "install", "zsh"], complete(m, "completion", ""))


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
    m.kit("undo", "~/.zshrc")
    script(m, "s.sh", "")
    m.sync()
    for f in ("synced.json", "scripts.json", "backups", "base"):
        ok((m.state / f).exists(), f"{f} not in KIT_STATE")
    ok(m.base_file(".zshrc") == "a\n", f"base/.zshrc: {m.base_file('.zshrc')!r}")
    ok(not (m.src / ".kit").exists(), "<repo>/.kit exists")
    porcelain = m.git("status", "--porcelain", "--untracked-files=all").stdout
    hasnt(porcelain, "synced", "backups", "scripts.json", "base/")


@test
def state_base_follows_sync_and_save(sb):
    m = setup(sb, {".zshrc": "a\n"}, add=[".zshrc"])
    ok(m.base_file(".zshrc") == "a\n", "add didn't record a base")
    m.repo_write("home/.zshrc", "b\n")
    m.sync()
    ok(m.base_file(".zshrc") == "b\n", f"base after sync: {m.base_file('.zshrc')!r}")
    m.write(".zshrc", "c\n")
    m.kit("save")
    m.sync()
    # this machine's version is now the synced one: a later repo change is "changed in repo", not a conflict
    m.repo_write("home/.zshrc", "d\n")
    check_status(m, "changed in repo", "~/.zshrc")
    m.sync()
    ok(m.read(".zshrc") == "d\n", repr(m.read(".zshrc")))


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
    m.kit("undo", "20200101-000000")
    ok(m.read(".zshrc") == "old\n")

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
