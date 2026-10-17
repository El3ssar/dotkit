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
STUBS = ["brew", "launchctl", "clang", "cargo", "mise", "gh", "delta", "bat", "ssh", "rsync", "zsh", "nvim"]
COMMANDS = ["init", "add", "rm", "status", "save", "sync", "undo", "push", "packages", "files", "doctor"]
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
        self.stub("mise", script=MISE_USE_OK)
        self.stub("zsh", code=0)   # `zsh -fc 'print -l $fpath'` prints nothing: no real fpath dir is used
        self.machines = {}
        for formula in ("bat", "git-delta", "neovim"):
            (self.root / "Cellar" / formula).mkdir(parents=True)

    def make_stub(self, name):
        p = self.fakebin / name
        p.write_text(f"""#!/bin/sh
{{ printf '%s' {shlex.quote(name)}; for a in "$@"; do printf ' %s' "$a"; done; printf '\\n'; }} >> {shlex.quote(str(self.log))}
cfg={shlex.quote(str(self.stubcfg))}
if [ -f "$cfg/{name}.sh" ]; then exec /bin/sh "$cfg/{name}.sh" "$@"; fi
if [ {shlex.quote(name)} = mise ] && [ "${{1:-}}" = --version ]; then echo mise-test; exit 0; fi
code=1
if [ -f "$cfg/{name}.code" ]; then code=$(cat "$cfg/{name}.code"); fi
if [ "$code" = 0 ] && [ "${{1:-}}" = install ]; then
  for last in "$@"; do :; done
  case {shlex.quote(name)} in
    cargo) {shlex.quote(sys.executable)} {shlex.quote(str(HERE/'install_fixture.py'))} "cargo:$last" ;;
    brew) kind=brew; [ "${{2:-}}" != --cask ] || kind=cask; {shlex.quote(sys.executable)} {shlex.quote(str(HERE/'install_fixture.py'))} "$kind:$last" ;;
  esac
fi
if [ "$code" = 0 ] && [ {shlex.quote(name)} = ssh ]; then
  case "$*" in *"uname -s"*) echo Linux;; esac
fi
exit "$code"
""")
        p.chmod(0o755)

    def stub(self, name, code=None, script=None):
        if not (self.fakebin / name).exists():
            self.make_stub(name)
        if code is not None:
            if script is None: (self.stubcfg / f"{name}.sh").unlink(missing_ok=True)
            (self.stubcfg / f"{name}.code").write_text(str(code))
        if script is not None:
            (self.stubcfg / f"{name}.sh").write_text(script)

    def unstub(self, name):
        (self.fakebin / name).unlink()
        formula = {"bat":"bat", "delta":"git-delta", "nvim":"neovim"}.get(name)
        if formula: shutil.rmtree(self.root / "Cellar" / formula, ignore_errors=True)

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
            "LANG": "C.UTF-8" if PLAT == "linux" else "en_US.UTF-8", "LC_ALL": "C.UTF-8" if PLAT == "linux" else "en_US.UTF-8",
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
        kit = self.home / ".local/kit"
        (kit / "bin").mkdir(parents=True)
        (kit / "opt/zsh/bin").mkdir(parents=True)
        private_mise = kit / "bin/mise"
        private_mise.write_text("#!/bin/bash\n" +
            'if [ "${1:-}" = --version ]; then echo mise-test; exit 0; fi\n' +
            'if [ "${1:-}" = reshim ]; then exit 0; fi\n' +
            f"{shlex.quote(str(sb.fakebin / 'mise'))} \"$@\" || exit $?\n" +
            'if [ "${1:-}" = install ] || [ "${1:-}" = reshim ]; then\n' +
            '  mkdir -p "$HOME/.local/kit/mise/shims"\n' +
            '  for bin in nvim bat delta; do printf "#!/bin/sh\\nexit 0\\n" > "$HOME/.local/kit/mise/shims/$bin"; chmod +x "$HOME/.local/kit/mise/shims/$bin"; done\n' +
            '  while IFS=$\'\\t\' read -r name bin spec fallback; do [ -n "$bin" ] || continue; printf "#!/bin/sh\\nexit 0\\n" > "$HOME/.local/kit/mise/shims/$bin"; chmod +x "$HOME/.local/kit/mise/shims/$bin"; done < "$HOME/.local/kit/payload/packages.list"\n' +
            'fi\n')
        private_mise.chmod(0o755)
        os.symlink(sb.fakebin / "zsh", kit / "opt/zsh/bin/zsh")

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
        """Check sync's exit status on both operating systems."""
        return self.kit("sync", code=code, **kw)

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
  lock) LOCK_FIXTURE; exit $?;;
  ls) INVENTORY_FIXTURE; exit $?;;
  use|install) INSTALL_FIXTURE "$@"; exit $?;;
  *) exit 0;;
esac
""".replace("INVENTORY_FIXTURE", shlex.quote(sys.executable) + " " + shlex.quote(str(HERE / "mise_inventory_fixture.py"))).replace("LOCK_FIXTURE", shlex.quote(sys.executable) + " " + shlex.quote(str(HERE / "mise_lock_fixture.py"))).replace("INSTALL_FIXTURE", shlex.quote(sys.executable) + " " + shlex.quote(str(HERE / "install_fixture.py")))


def brew_metadata_stub(sb, name):
    info={"formulae":[{"name":name,"bottle":{"stable":{"files":{
        t:{"sha256":"a"*64,"url":"https://example.invalid/bottle"} for t in ("arm64_sequoia","sequoia","x86_64_linux","arm64_linux")}}}}],"casks":[]}
    sb.stub("brew", script="case \"$1\" in info) printf '%s\\n' " + shlex.quote(json.dumps(info)) + ";; install) " + shlex.quote(sys.executable)+" "+shlex.quote(str(HERE/"install_fixture.py"))+" brew:"+name+";; *) exit 0;; esac\n")


# mise that installs fine but knows no versions (on Linux, `kit pkg add` really installs with mise)
MISE_USE_OK = """case "$1" in
  lock) LOCK_FIXTURE; exit $?;;
  ls) INVENTORY_FIXTURE; exit $?;;
  use|install) INSTALL_FIXTURE "$@"; exit $?;;
  uninstall) exit 0;; *) exit 1;; esac
""".replace("INVENTORY_FIXTURE", shlex.quote(sys.executable) + " " + shlex.quote(str(HERE / "mise_inventory_fixture.py"))).replace("LOCK_FIXTURE", shlex.quote(sys.executable) + " " + shlex.quote(str(HERE / "mise_lock_fixture.py"))).replace("INSTALL_FIXTURE", shlex.quote(sys.executable) + " " + shlex.quote(str(HERE / "install_fixture.py")))


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
    ok(p["mac"] == "github:sharkdp/hexyl@1.2.3", p)
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
    ok(p["mac"] == "aqua:owner/lazygit@1.2.3", p)
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
    brew_metadata_stub(sb,"foo")
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
    m.kit("add", "--fallback", "cargo:bar", "aqua:o/bar@2.0",code=1)
    ok("bar" not in pkgs(m), "failed lookup recorded a floating fallback")
    m.kit("add", "--fallback", "cargo:baz@0.9.0", "aqua:o/baz@3.0")
    ok(pkgs(m)["baz"]["linux_fallback"] == "cargo:baz@0.9.0")
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
    m.kit("add", "npm:@other/cli",code=1)
    ok("cli" not in pkgs(m), "failed version lookup recorded a floating recipe")


@test
def add_tool_already_tracked(sb):
    m = setup(sb)
    sb.stub("mise", script=MISE_USE_OK)
    m.kit("add", "aqua:o/tool@1.0")
    has(m.kit("add", "aqua:o/tool@1.0").out, "already tracked")


@test
def add_tracked_tool_again_does_not_repin(sb):
    m = setup(sb); sb.stub("foo",code=0)
    baseline={"foo":{"mac":"aqua:o/foo@1.0","linux":"aqua:o/foo@1.0","linux_fallback":"cargo:foo@0.5.0"}}
    m.repo_write("packages.json",json.dumps(baseline))
    has(m.kit("add","foo").out,"already tracked")
    ok(pkgs(m)==baseline)
    ok(not any(" latest " in c or " lock " in c for c in sb.calls("mise")),sb.calls())


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
    r = m.kit("rm", "~/.config/app/junk", code=1)
    has(r.err, "not tracked")
    hasnt(m.kit("status", "-v", code=None).out, "junk")
    ok(m.exists(".config/app/junk"))


@test
def rm_escapes_patterns(sb):
    m = setup(sb, {".config/app/a": "a\n"}, add=[".config/app"])
    for n in ("weird[1].txt", "weird1.txt", "a*b.txt", "axxb.txt"):
        m.write(f".config/app/{n}", "n\n")
    m.kit("add", "~/.config/app")
    for n in ("weird1.txt", "axxb.txt"):
        m.write(f".config/app/{n}", "changed\n")
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
def status_ignores_untracked_files_in_added_folder(sb):
    m = setup(sb, {".config/app/a": "a\n"}, add=[".config/app"])
    m.write(".config/app/b", "b\n")
    r = m.kit("status", "-v")
    hasnt(r.out, "new here", "~/.config/app/b")
    has(m.kit("save").out, "nothing to save")
    ok(not m.repo_has("home/.config/app/b"))
    m.kit("add", "~/.config/app/b")
    ok(m.repo_has("home/.config/app/b"))


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
    m.sync()
    r = m.kit("status", "~/.local/bin/t")
    has(r.out, "in sync")
    (m.home / ".local/bin/t").chmod(0o755)
    check_status(m, "changed here", "~/.local/bin/t")


@test
def status_groups_big_folders(sb):
    m = setup(sb, {f".config/app/f{i}": "a\n" for i in range(5)}, add=[".config/app"])
    m.sync()
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
    m.sync()
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
    m.kit("add", "~/.config/application")
    m.sync()
    for i in range(4):
        m.write(f".config/application/n{i}", "changed\n")
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
    if PLAT == "linux": (m.home / ".local/kit/bin/mise").unlink()
    else: m.repo_write("packages.json",json.dumps({"brew-required":{"mac":"brew:brew-required"}}))
    r = m.kit("status", code=None)
    line = next((l for l in r.out.splitlines() if "Missing" in l), "")
    ok(line, "no 'Missing:' line")
    has(line, "delta")
    if PLAT == "mac": has(line,"brew")
    else: hasnt(line,"mise")
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
def status_untracked_file_requires_add(sb):
    m = setup(sb, {".config/app/a": "a\n"}, add=[".config/app"])
    m.write(".config/app/n", "brand new\n")
    r = m.kit("status", "~/.config/app/n", code=1)
    has(r.err, "not tracked")
    hasnt(r.out, "brand new", "new here")


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
    has(m.kit("status", "~/.zshrc").out, "not backed up", "~/.zshrc")
    has(m.kit("save").out, "nothing to save")


@test
def save_ignores_new_files_and_saves_tracked_deletions(sb):
    m = setup(sb, {".config/app/a": "a\n", ".config/app/b": "b\n"}, add=[".config/app"])
    m.write(".config/app/c", "c\n")
    (m.home / ".config/app/b").unlink()
    r = m.kit("save")
    has(r.out, "1 change(s) saved", "removed", ".config/app/b")
    hasnt(r.out, ".config/app/c")
    ok(not m.repo_has("home/.config/app/c") and not m.repo_has("home/.config/app/b"))


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
    m.repo_write("home/.config/app/env", TOKEN + "\n")
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
        sb.stub("mise", script=MISE_STUB)
    m.sync()
    ok(m.exists("ran"), "scripts run by sync")
    if PLAT == "mac":
        ok(any("use -g aqua:sharkdp/bat" in c for c in sb.calls("mise")),sb.calls())


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
    ok("brew install --formula foo" in sb.calls("brew"), sb.calls())


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
    has(r.both, ".zshrc", "differs from the repo")
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
    uploads = [c for c in sb.calls("ssh") if "tar -xzf" in c]
    ok(any("alpha " in c for c in uploads) and any("beta " in c for c in uploads), sb.calls())


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
    ok(any("box " in c and "tar -xzf" in c for c in sb.calls("ssh")), sb.calls())
    payload = m.state / "payload"
    ok((payload / "home/.config/app/a").read_text() == "a\n", "payload lacks config/app/a")
    ok(not (payload / "home/.config/kitty").exists(), "[remote] file was sent")
    ok((payload / "home/.zshrc").read_text() == "z\n", "shell rc was not sent")
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
def cli_help_shows_public_commands(sb):
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
def cli_typo_suggests(sb):
    m = setup(sb)
    r = m.kit("stauts", code=2)
    has(r.err, "did you mean 'status'")
    r = m.kit("snyc", code=2)
    has(r.err, "did you mean 'sync'")


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
    for c in ("apply", "pkg", "diff", "update", "cd"):
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

# =========================================================================== regression coverage
@test
def pulled_code_requires_approval_on_every_retry(sb):
    a, b, _ = two_machines(sb, {".zshrc": "base\n"})
    a.repo_write("scripts/new.sh", 'touch "$HOME/ran-new"\n')
    a.sync()
    for _ in range(2):
        b.kit("sync", code=1)
        ok(not b.exists("ran-new"), "unapproved script executed")
    b.kit("sync", "--trust", code=None if PLAT == "linux" else 0)
    ok(b.exists("ran-new"), "approved script did not run")


@test
def pulled_commands_are_gated_with_scripts(sb):
    a, b, _ = two_machines(sb, {".zshrc": "base\n"})
    a.repo_write("packages.json", json.dumps({"probe": {PLAT: 'cmd:touch "$HOME/ran-command"', "bin": "missing-probe"}}))
    a.sync(code=None)
    for _ in range(2):
        b.kit("sync", code=1)
        ok(not b.exists("ran-command"), "unapproved recipe executed")


@test
def cloned_scripts_require_explicit_approval(sb):
    a = setup(sb)
    a.repo_write("scripts/probe.sh", 'touch "$HOME/cloned-code"\n')
    bare = sb.bare(); a.kit("init", str(bare)); a.sync()
    b = sb.machine("clone"); b.kit("init", str(bare))
    b.kit("sync", code=1)
    ok(not b.exists("cloned-code"))


@test
def undo_aborts_when_backup_cannot_be_created(sb):
    m = setup(sb, {".zshrc": "base\n"}, add=[".zshrc"])
    m.write(".zshrc", "valuable\n")
    (m.state / "backups").write_text("blocks directory")
    m.kit("undo", "~/.zshrc", code=1)
    ok(m.read(".zshrc") == "valuable\n", "original overwritten without backup")


@test
def failed_restore_keeps_original_backup(sb):
    m = setup(sb)
    stamp = m.state / "backups/20200101-000000"
    stamp.mkdir(parents=True)
    os.mkfifo(stamp / "unsupported")
    m.kit("undo", stamp.name, code=1)
    ok(stamp.exists(), "failed restore deleted the backup")


@test
def undo_restores_file_over_directory_with_backup(sb):
    m = setup(sb, {".zshrc": "base\n"}, add=[".zshrc"])
    m.write(".zshrc", "valuable\n"); m.kit("undo", "~/.zshrc")
    stamp = next((m.state / "backups").iterdir()).name
    (m.home / ".zshrc").unlink(); m.write(".zshrc/nested", "keep-directory\n")
    m.kit("undo", stamp)
    ok(m.read(".zshrc") == "valuable\n")
    ok(any(p.read_text() == "keep-directory\n" for p in (m.state / "backups").rglob("nested")))


@test
def predictable_temp_symlink_cannot_clobber_another_file(sb):
    m = setup(sb, {".zshrc": "base\n"}, add=[".zshrc"])
    m.write(".zshrc", "mine\n"); victim = m.write("victim", "valuable\n")
    os.symlink(victim, m.home / "..zshrc.kit-tmp")
    m.kit("undo", "~/.zshrc")
    ok(victim.read_text() == "valuable\n")
    ok(not (m.home / ".zshrc").is_symlink())


@test
def dirs_metadata_cannot_escape_home(sb):
    m = setup(sb)
    outside = sb.root / "outside"; outside.mkdir(); (outside / "config").write_text("private\n")
    m.repo_write("dirs.json", json.dumps([os.path.relpath(outside, m.home)]))
    r = m.kit("save", code=1)
    has(r.err, "dirs.json", "invalid")
    ok((outside / "config").read_text() == "private\n")


@test
def forced_secret_override_is_tied_to_content(sb):
    m = setup(sb, {".config/env": TOKEN + "\n"})
    m.kit("add", "--force", "~/.config/env"); m.sync()
    m.repo_write("home/.config/env", TOKEN + "changed\n")
    m.kit("sync", code=1)


@test
def secrets_after_two_megabytes_are_detected(sb):
    m = setup(sb, {".config/large": b"x" * 2_000_001 + b"\n" + TOKEN.encode()})
    m.kit("add", "~/.config/large", code=1)
    ok(not m.repo_has("home/.config/large"))


@test
def reconcile_preserves_unrelated_new_tracked_files(sb):
    a, b, _ = two_machines(sb, {".zshrc": MERGE_BASE})
    a.write(".zshrc", "one\ntwo\nTHREE-a\nfour\nfive\n"); a.kit("save"); a.sync()
    b.write(".zshrc", "one\ntwo\nTHREE-b\nfour\nfive\n"); b.kit("save")
    b.write(".newconfig", "valuable\n"); b.kit("add", "~/.newconfig")
    b.kit("sync", code=1)
    ok(b.repo_read("home/.newconfig") == "valuable\n", "unrelated addition untracked")
    ok(b.git("for-each-ref", "refs/kit/recovery").stdout.strip(), "no recovery ref")


@test
def saving_delete_vs_remote_edit_requires_force(sb):
    m = setup(sb, {".zshrc": "base\n"}, add=[".zshrc"])
    (m.home / ".zshrc").unlink(); m.repo_write("home/.zshrc", "remote-edit\n")
    m.kit("save", code=1)
    ok(m.repo_read("home/.zshrc") == "remote-edit\n")
    m.kit("save", "--force")
    ok(not m.repo_has("home/.zshrc"))


@test
def tracked_login_hooks_are_not_saved(sb):
    m = setup(sb, {".zshrc": "original\n"}, add=[".zshrc"])
    m.sync()
    block = '# >>> kit: generated hook >>>\nignore-this\n# <<< kit <<<\n'
    m.write(".zshrc", block + "original\n")
    has(m.kit("status", "~/.zshrc").out, "in sync")
    m.kit("save")
    ok(m.repo_read("home/.zshrc") == "original\n")


@test
def package_registry_infers_command_name(sb):
    m = setup(sb)
    sb.stub("mise", script='''case "$1" in
 registry) echo '[{"short":"ripgrep","backends":["aqua:BurntSushi/ripgrep"],"bins":["rg"]}]';;
 latest) echo 15.2.0;;
 lock) LOCK_FIXTURE;;
 ls) INVENTORY_FIXTURE;;
 use|install) INSTALL_FIXTURE "$@";;
esac
exit 0
'''.replace("INVENTORY_FIXTURE",shlex.quote(sys.executable)+" "+shlex.quote(str(HERE/"mise_inventory_fixture.py"))).replace("LOCK_FIXTURE",shlex.quote(sys.executable)+" "+shlex.quote(str(HERE/"mise_lock_fixture.py"))).replace("INSTALL_FIXTURE",shlex.quote(sys.executable)+" "+shlex.quote(str(HERE/"install_fixture.py"))))
    m.kit("add", "ripgrep")
    p = pkgs(m)["ripgrep"]
    ok(p["bin"] == "rg" and p["linux"] == "aqua:BurntSushi/ripgrep@15.2.0", p)


@test
def legacy_cargo_recipe_gets_linux_equivalent(sb):
    m = setup(sb)
    sb.stub("mise", script=MISE_STUB)
    sb.stub("cargo", code=0)
    m.kit("add", "cargo:portable")
    ok(pkgs(m)["portable"]["linux"] == "cargo:portable@1.2.3")


@test
def invalid_recipe_does_not_mutate_existing_package(sb):
    m = setup(sb); sb.stub("mise", script=MISE_USE_OK)
    m.kit("add", "aqua:o/tool@1.0"); before = pkgs(m)
    m.kit("add", "tool", "--linux", "not-a-recipe", code=1)
    ok(pkgs(m) == before)


@test
def push_payload_reports_missing_cli_recipes(sb):
    m = setup(sb)
    m.repo_write("packages.json", json.dumps({"unportable": {"mac": "cmd:osascript Mac-only"}}))
    sb.stub("ssh", code=0)
    m.kit("push", "box")
    has((m.state / "payload/packages.list").read_text(), "unportable", "missing:")


@test
def forced_ignored_file_is_applied_on_another_machine(sb):
    a = setup(sb, {"sample.bak": "forced\n"})
    a.kit("add", "--force", "--no-servers", "~/sample.bak")
    bare = sb.bare(); a.kit("init", str(bare)); a.sync()
    b = sb.machine("other"); b.kit("init", str(bare)); b.sync()
    ok(b.read("sample.bak") == "forced\n")
    sb.stub("ssh", code=0); a.kit("push", "box")
    ok(not (a.state / "payload/home/sample.bak").exists(), "force bypassed no-servers")


@test
def invalid_rule_regex_is_a_visible_error(sb):
    m = setup(sb, {".zshrc": "base\n"}, add=[".zshrc"])
    m.repo_write("rules.json", json.dumps([{"path":".zshrc", "on":[PLAT], "regex":[["[", "x"]]}]))
    r = m.kit("status", code=1)
    has(r.err, "rules.json", "invalid regex")


@test
def reconciliation_preserves_unrelated_saved_deletion(sb):
    a, b, _ = two_machines(sb, {".zshrc":MERGE_BASE, ".obsolete":"old\n"})
    a.write(".zshrc", "one\ntwo\nTHREE-a\nfour\nfive\n"); a.kit("save"); a.sync()
    b.write(".zshrc", "one\ntwo\nTHREE-b\nfour\nfive\n")
    (b.home / ".obsolete").unlink(); b.kit("save")
    b.kit("sync", code=1)
    ok(not b.repo_has("home/.obsolete"), "saved deletion was resurrected")


@test
def reconciliation_merges_independent_package_keys(sb):
    a, b, _ = two_machines(sb, {".zshrc":MERGE_BASE})
    a.repo_write("packages.json", json.dumps({"alpha":{"mac":"brew:alpha"}}, indent=2))
    a.write(".zshrc", "one\ntwo\nTHREE-a\nfour\nfive\n"); a.kit("save"); a.sync(code=None)
    b.repo_write("packages.json", json.dumps({"beta":{"mac":"brew:beta"}}, indent=2))
    b.write(".zshrc", "one\ntwo\nTHREE-b\nfour\nfive\n"); b.kit("save")
    b.kit("sync", code=1)
    ok(set(pkgs(b)) == {"alpha", "beta"}, pkgs(b))


@test
def package_recipe_change_keeps_existing_external_binary(sb):
    if PLAT != "mac": return
    m=setup(sb); sb.stub("mise",script=MISE_STUB)
    m.kit("add","aqua:o/probe@1.0")
    sb.stub("probe",code=0)
    m.kit("add","aqua:o/probe@2.0")
    ok(pkgs(m)["probe"]["mac"]=="aqua:o/probe@2.0")
    ok(not any("use -g aqua:o/probe@2.0" in c for c in sb.calls("mise")),sb.calls())


@test
def brew_recipe_accepts_existing_binary_from_another_manager(sb):
    if PLAT != "mac": return
    m = setup(sb); sb.stub("brew", code=0); sb.stub("probe", code=0)
    sb.stub("mise",script=MISE_STUB); brew_metadata_stub(sb,"probe")
    m.kit("add", "probe", "--mac", "brew:probe", "--linux", "aqua:o/probe@1.0")
    ok("brew install probe" not in sb.calls("brew"), sb.calls())


@test
def cask_detection_uses_reported_brew_prefix(sb):
    if PLAT != "mac": return
    m = setup(sb); prefix=sb.root/"actual-prefix"; (prefix/"Caskroom/foo").mkdir(parents=True)
    sb.stub("brew", script=f'case "$1" in --prefix) echo {shlex.quote(str(prefix))};; *) exit 1;; esac\n')
    m.repo_write("packages.json", json.dumps({"foo":{"mac":"cask:foo"}}))
    hasnt(m.kit("status").out, "Tools not installed here:")


@test
def status_recognizes_cargo_binaries_without_kit_install_records(sb):
    m = setup(sb)
    packages = {}
    for name, binary in (("git-delta", "delta"), ("ripgrep", "rg"), ("skim", "sk"),
                         ("eza", "eza"), ("bookokrat", "bookokrat"), ("just", "just"),
                         ("mcat", "mcat"), ("starship", "starship"), ("tokei", "tokei"),
                         ("zellij", "zellij"), ("zoxide", "zoxide")):
        m.write(f".cargo/bin/{binary}", "#!/bin/sh\nexit 0\n")
        (m.home / ".cargo/bin" / binary).chmod(0o755)
        packages[name] = {PLAT: f"cargo:{name}"}
    packages["delta"] = {PLAT: "brew:git-delta" if PLAT == "mac" else "aqua:dandavison/delta@1"}
    m.repo_write("packages.json", json.dumps(packages))
    # No Cargo bins on PATH, no Cellar delta, and deliberately stale kit records.
    sb.unstub("delta")
    (m.state / "packages-installed.json").write_text(json.dumps({"eza": "cargo:eza@old"}))
    r = m.kit("status")
    hasnt(r.out, "Tools not installed here:")
    (m.home / ".cargo/bin/eza").unlink()
    has(m.kit("status").out, "Tools not installed here: eza")


@test
def status_recognizes_custom_cargo_home(sb):
    m = setup(sb)
    cargo_home = m.home / "custom-cargo"
    m.write("custom-cargo/bin/probe", "#!/bin/sh\nexit 0\n")
    (cargo_home / "bin/probe").chmod(0o755)
    m.repo_write("packages.json", json.dumps({"probe": {PLAT: "cargo:probe"}}))
    hasnt(m.kit("status", env={"CARGO_HOME": str(cargo_home)}).out, "Tools not installed here:")
    has(m.kit("status").out, "Tools not installed here: probe")


@test
def fizsh_records_real_portable_recipe(sb):
    m = setup(sb); brew_metadata_stub(sb,"fizsh"); sb.stub("curl", code=1)
    m.kit("add", "fizsh", "--mac", "brew:fizsh", code=None if PLAT == "linux" else 0)
    ok(m.json("packages.json")["fizsh"]["linux"] == "builtin:fizsh")


@test
def fizsh_legacy_zsh_placeholder_is_migrated(sb):
    m = setup(sb); sb.stub("curl", code=1)
    m.repo_write("packages.json", json.dumps({"fizsh": {"mac": "brew:fizsh", "linux": "builtin:zsh"}}))
    m.kit("packages", "resolve", "fizsh")
    ok(m.json("packages.json")["fizsh"]["linux"] == "builtin:fizsh")


@test
def status_lists_saved_additions_changes_and_deletions(sb):
    m = setup(sb, {".changed": "old\n", ".deleted": "old\n"}, add=[".changed", ".deleted"])
    m.sync()
    m.write(".changed", "new\n"); m.write(".added", "new\n")
    (m.home / ".deleted").unlink()
    m.kit("add", "~/.added"); m.kit("save")
    r = m.kit("status")
    pending = r.out.split("Saved, not backed up yet:", 1)[1]
    has(pending, "changed          ~/.changed", "deleted          ~/.deleted", "added            ~/.added")
    hasnt(r.out, "Files (this machine vs the repo):")
    scoped = m.kit("status", "~/.changed").out
    has(scoped, "Saved, not backed up yet:", "~/.changed")
    hasnt(scoped, "~/.added", "~/.deleted", "in sync")
    m.sync()
    hasnt(m.kit("status").out, "Saved, not backed up yet:")


@test
def status_lists_package_recipe_changes_by_package_name(sb):
    m = setup(sb); m.sync()
    for name in ("existing", "added", "removed"): sb.stub(name, code=0)
    old = {"existing": {PLAT: "cargo:existing@1"}, "removed": {PLAT: "cargo:removed"}}
    m.repo_write("packages.json", json.dumps(old)); m.git("add", "-A"); m.git("commit", "-qm", "baseline")
    new = {"existing": {PLAT: "cargo:existing@2"}, "added": {PLAT: "cargo:added"}}
    m.repo_write("packages.json", json.dumps(new))
    pending = m.kit("status").out.split("Saved, not backed up yet:", 1)[1]
    has(pending, "updated          package existing", "added            package added", "removed          package removed", f"({PLAT})")
    hasnt(pending, "changed          packages.json")


@test
def status_lists_staged_renames_and_untracked_repo_files(sb):
    m = setup(sb, {'.old name': "old\n"}, add=['.old name']); m.sync()
    name = '.new "é"'
    m.git("mv", "home/.old name", "home/" + name)
    m.repo_write("scripts/nested/new.sh", "echo test\n")
    r = m.kit("status")
    pending = r.out.split("Saved, not backed up yet:", 1)[1]
    has(pending, "deleted          ~/.old name", "added            ~/" + name, "added            scripts/nested/new.sh")
    hasnt(pending, "\\303")


@test
def status_distinguishes_committed_outgoing_and_incoming_files(sb):
    a, b, _ = two_machines(sb, {".zshrc": "base\n"})
    a.repo_write("home/.to-send", "local\n"); a.git("add", "-A"); a.git("commit", "-qm", "local")
    b.write(".to-get", "remote\n"); b.kit("add", "~/.to-get"); b.sync()
    a.git("fetch", "-q")
    r = a.kit("status")
    has(r.out, "1 commit(s) to send, 1 to get", "To send:", "To get (last fetched):")
    outgoing, incoming = r.out.split("To send:", 1)[1].split("To get (last fetched):", 1)
    has(outgoing, "~/.to-send"); hasnt(outgoing, "~/.to-get")
    has(incoming, "~/.to-get"); hasnt(incoming, "~/.to-send")


@test
def sync_preserves_cargo_delta_and_never_generates_recipes(sb):
    m = setup(sb)
    m.write(".cargo/bin/delta", "#!/bin/sh\nexit 0\n"); (m.home/".cargo/bin/delta").chmod(0o755)
    sb.unstub("delta"); sb.stub("brew", code=0)
    m.repo_write("packages.json", json.dumps({"git-delta":{"mac":"cargo:git-delta", "linux":"cargo:git-delta@0.20.1"}, "legacy":{"mac":"cargo:legacy"}}))
    before = pkgs(m)
    r = m.kit("sync", code=None)
    ok(pkgs(m) == before, "sync silently modified package recipes")
    hasnt(r.both, "added Linux recipe", "installing delta", "installing git-delta")
    ok(not any("brew install git-delta" in c for c in sb.calls("brew")), sb.calls())
    names = [p["name"] for p in json.loads(m.kit("packages", "--json").out)]
    ok("git-delta" in names and "delta" not in names, names)


@test
def packages_resolve_is_explicit_and_does_not_install(sb):
    m = setup(sb); sb.stub("mise", script=MISE_STUB)
    m.repo_write("packages.json", json.dumps({"portable":{"mac":"cargo:portable"}}))
    m.git("add", "-A"); m.git("commit", "-qm", "legacy recipe")
    m.kit("packages", "resolve", "portable")
    ok(pkgs(m)["portable"]["linux"] == "cargo:portable@1.2.3")
    ok(not sb.calls("cargo"), sb.calls())
    has(m.kit("status").out, "updated          package portable (linux)")


@test
def resolve_preserves_legacy_cargo_provider_and_installed_version(sb):
    m = setup(sb); sb.stub("mise", script=MISE_STUB)
    installed_cargo_fixture(m, crate="probe", binary="probe", version="1.0.0")
    baseline = {PLAT: "cargo:probe", OTHER: "aqua:owner/probe@2.0.0", "remote": False}
    m.repo_write("packages.json", json.dumps({"probe": baseline}))
    m.kit("packages", "resolve", "--both", "probe")
    expected = dict(baseline); expected[PLAT] = "cargo:probe@1.0.0"
    ok(pkgs(m)["probe"] == expected, pkgs(m))
    ok(not sb.calls("cargo") and not sb.calls("brew"), sb.calls())
    ok(not any(" use " in c or " install " in c for c in sb.calls("mise")), sb.calls())


@test
def resolve_pins_existing_cargo_without_adopting_another_crate(sb):
    m = setup(sb); sb.stub("mise", script=MISE_STUB)
    installed_cargo_fixture(m, crate="other-crate", binary="probe", version="9.0.0")
    m.repo_write("packages.json", json.dumps({"probe": {PLAT: "cargo:probe", OTHER: "brew:probe"}}))
    m.kit("packages", "resolve", "--both", "probe")
    ok(pkgs(m)["probe"][PLAT] == "cargo:probe@1.2.3", pkgs(m))
    ok(pkgs(m)["probe"][OTHER] == "brew:probe")
    ok(not sb.calls("cargo"), sb.calls())


@test
def status_groups_legacy_recipes_and_preserves_unsynced_details(sb):
    m = setup(sb)
    m.repo_write("packages.json", json.dumps({"probe": {PLAT: "cargo:probe", OTHER: "cmd:unsafe"}}))
    before = (m.src / "packages.json").read_bytes()
    result = m.kit("status")
    has(result.out, "Package recipes need attention (2)", "Installed tools are kept", "kit packages resolve --both", "package probe")
    hasnt(result.out, "Unsupported recipe:", "an exact version is required")
    verbose = m.kit("status", "-v")
    has(verbose.out, "an exact version is required", "custom install commands are not managed")
    ok((m.src / "packages.json").read_bytes() == before, "status migrated recipes")


@test
def packages_resolve_queries_provider_and_reports_unresolved_casks(sb):
    m=setup(sb)
    sb.stub("mise",script='case "$1:$2" in registry:unknown) exit 1;; esac\n'+MISE_STUB)
    m.repo_write("packages.json",json.dumps({"kitty":{"mac":"cask:kitty","remote":False},"unknown":{"mac":"cask:unknown"},"empty":{},"custom":{"mac":"cask:kitty","linux":"github:custom/kitty@1"}}))
    r=m.kit("packages","resolve","kitty","unknown","empty","custom",code=1)
    has(r.both,"kitty: added Linux recipe","unknown: could not resolve","empty: no supported","custom: Linux recipe already set")
    ok(pkgs(m)["kitty"]=={"mac":"cask:kitty","linux":"aqua:owner/kitty@1.2.3","remote":False})
    ok(pkgs(m)["custom"]["linux"]=="github:custom/kitty@1")
    ok(not sb.calls("cargo") and not sb.calls("brew"),sb.calls())


@test
def packages_tui_linux_recipe_persists_on_reopen(sb):
    m=setup(sb); sb.stub("mise",script=MISE_STUB)
    m.repo_write("packages.json",json.dumps({"kitty":{"mac":"cask:kitty"}}))
    package_dialogue(m,b"/kitty\rlq")
    ok("linux" not in pkgs(m)["kitty"])
    has(package_dialogue(m,b"/kitty\rl\r"),"kitty: added Linux recipe")
    ok(pkgs(m)["kitty"]["linux"]=="aqua:owner/kitty@1.2.3")
    package_dialogue(m,b"/kitty\rq")
    ok(pkgs(m)["kitty"]["linux"]=="aqua:owner/kitty@1.2.3")


@test
def packages_inventory_scope_and_remove(sb):
    m = setup(sb)
    m.repo_write("packages.json", json.dumps({"probe":{PLAT:"cargo:probe", "linux":"cargo:probe"}}))
    sb.stub("probe", code=0)
    r = m.kit("packages", "list")
    has(r.out, "probe  [tracked]  installed", "servers: yes", "kit built-in")
    m.kit("pkg", "local", "probe"); ok(pkgs(m)["probe"]["remote"] is False)
    m.kit("packages", "servers", "probe"); ok("remote" not in pkgs(m)["probe"])
    m.kit("packages", "remove", "probe"); ok("probe" not in pkgs(m))
    ok((sb.fakebin/"probe").exists(), "remove uninstalled the tool")
    ok(not any("uninstall" in c for c in sb.calls()), sb.calls())
    m.kit("packages", "remove", "bat", code=1)
    m.kit("packages", "--tui", code=1)


@test
def packages_update_explicitly_upgrades_installed_brew_tool(sb):
    if PLAT != "mac": return
    m = setup(sb); sb.stub("probe", code=0)
    sb.stub("brew", script="case \"$1\" in outdated) echo '{\"formulae\":[{\"name\":\"probe\"}],\"casks\":[]}' ;; esac\nexit 0\n")
    (sb.root/"Cellar/probe").mkdir()
    m.repo_write("packages.json", json.dumps({"probe":{"mac":"brew:probe", "linux":"aqua:o/probe@1"}}))
    m.kit("packages", "update", "probe")
    ok("brew upgrade --formula probe" in sb.calls("brew"), sb.calls())


def package_dialogue(m, keys, command="packages", expected=0, on_ready=None):
    import pty, select, termios, fcntl, struct
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 26, 100, 0, 0))
    saved = termios.tcgetattr(slave)
    proc = subprocess.Popen(KIT + [command, "--tui"], cwd=m.home, env=m.envvars,
                            stdin=slave, stdout=slave, stderr=slave, start_new_session=True)
    buf = b""; sent = False; deadline = time.monotonic() + 10
    try:
        while time.monotonic() < deadline:
            readable, _, _ = select.select([master], [], [], 0.1)
            if readable:
                try: chunk = os.read(master, 65536)
                except OSError: break
                buf += chunk
            if not sent and b"KIT" in buf and (b"Packages" if command == "packages" else b"Files") in buf:
                if on_ready: on_ready()
                os.write(master, keys); sent = True
            if proc.poll() is not None: break
        try: proc.wait(timeout=2)
        except subprocess.TimeoutExpired: pass
        ok(proc.poll() is not None, "package UI hung: " + buf.decode(errors="replace"))
        ok(proc.returncode == expected, buf.decode(errors="replace"))
        restored = termios.tcgetattr(slave)
        # macOS sets PENDIN when canonical input is restored; this is kernel queue state.
        restored[3] &= ~getattr(termios, "PENDIN", 0)
        saved[3] &= ~getattr(termios, "PENDIN", 0)
        ok(restored == saved, "package UI did not restore terminal settings")
        return buf.decode(errors="replace")
    finally:
        if proc.poll() is None: proc.kill(); proc.wait()
        os.close(master); os.close(slave)


@test
def packages_tui_stages_cancels_applies_and_restores_terminal(sb):
    m = setup(sb)
    m.repo_write("packages.json", json.dumps({"alpha":{"mac":"cargo:alpha", "linux":"cargo:alpha"}, "beta":{"mac":"cargo:beta", "linux":"cargo:beta"}}))
    before = pkgs(m)
    has(package_dialogue(m, b"/alpha\rdq"), "manifest unchanged")
    ok(pkgs(m) == before, "cancel applied staged changes")
    package_dialogue(m, b"/alpha\rs\r")
    ok(pkgs(m)["alpha"]["remote"] is False)
    package_dialogue(m, b"/alpha\rd\r")
    ok("alpha" not in pkgs(m) and "beta" in pkgs(m))
    package_dialogue(m, b"\x03")
    m.kit("packages", "remove", "beta", "missing", code=1)
    ok("beta" in pkgs(m), "invalid batch partially removed valid entries")


@test
def packages_tui_combines_actions_and_reverses_scope(sb):
    m = setup(sb)
    sb.stub("mise", script=MISE_STUB)
    m.repo_write("packages.json", json.dumps({"alpha":{"mac":"cargo:alpha", "remote":False}}))
    package_dialogue(m, b"/alpha\rsl\r")
    p = pkgs(m)["alpha"]
    ok("remote" not in p, "resolving overwrote the SSH toggle")
    ok(p["linux"].startswith("cargo:alpha"), p)
    before = pkgs(m)
    package_dialogue(m, b"/alpha\rss\r")
    ok(pkgs(m) == before, "toggling twice did not undo the pending change")


@test
def packages_tui_runs_update_before_removing_same_entry(sb):
    m = setup(sb)
    installed_cargo_fixture(m, crate="probe", binary="probe")
    m.repo_write("packages.json", json.dumps({"probe":{"mac":"cargo:probe", "linux":"cargo:probe@1.0.0"}}))
    sb.stub("cargo", script="""case "$1" in search) echo 'probe = "1.0.0" # test';; install) exit 99;; esac\nexit 0\n""")
    package_dialogue(m, b"/probe\rud\r")
    ok("probe" not in pkgs(m), "removal was overwritten")
    ok(any(" search " in call for call in sb.calls("cargo")), "removal discarded the update action")


@test
def package_updates_preserve_other_platform_recipes(sb):
    m=setup(sb); installed_cargo_fixture(m,crate="probe",binary="probe")
    sb.stub("cargo",script="""case "$1" in search) echo 'probe = "1.0.0" # fixture';; install) exit 99;; esac\nexit 0\n""")
    for other in ("system:probe","builtin:fizsh","cmd:echo probe"):
        m.repo_write("packages.json",json.dumps({"probe":{PLAT:"cargo:probe@1.0.0",OTHER:other}}))
        m.kit("packages","update","probe")
        ok(pkgs(m)["probe"][OTHER]==other,pkgs(m))


@test
def packages_tui_arrow_keys_and_incremental_rendering(sb):
    m = setup(sb)
    baseline = {"probe-a":{"mac":"cargo:probe-a", "linux":"cargo:probe-a"},
                "probe-b":{"mac":"cargo:probe-b", "linux":"cargo:probe-b"}}
    # CSI is normal cursor mode; SS3 is application cursor mode (common in SSH/kitty).
    for down, up in ((b"\x1b[B", b"\x1b[A"), (b"\x1bOB", b"\x1bOA")):
        m.repo_write("packages.json", json.dumps(baseline))
        raw = package_dialogue(m, b"/probe\r" + down + b"s\r")
        ok(pkgs(m)["probe-b"]["remote"] is False, "Down did not select the next package")
        ok("remote" not in pkgs(m)["probe-a"])
        ok(raw.count("\x1b[2J") <= 1, "keypresses caused full-screen clears")
        m.repo_write("packages.json", json.dumps(baseline))
        package_dialogue(m, b"/probe\r" + down + up + b"s\r")
        ok(pkgs(m)["probe-a"]["remote"] is False, "Up did not select the previous package")
        ok("remote" not in pkgs(m)["probe-b"])


def installed_cargo_fixture(m, crate="git-delta", binary="delta", version="1.0.0", root=".cargo", source="registry+https://github.com/rust-lang/crates.io-index"):
    m.write(f"{root}/bin/{binary}", "#!/bin/sh\nexit 0\n")
    (m.home/root/"bin"/binary).chmod(0o755)
    m.write(f"{root}/.crates2.json", json.dumps({"installs": {f"{crate} {version} ({source})": {"bins":[binary]}}}))


@test
def update_keeps_cargo_provider_with_stale_brew_recipe_and_duplicate_entries(sb):
    m = setup(sb); installed_cargo_fixture(m)
    # Both providers are present. Cargo's ownership record must win over the stale recipe.
    sb.stub("delta", code=0); sb.stub("brew", code=0)
    m.repo_write("packages.json", json.dumps({"delta":{"mac":"brew:git-delta", "linux":"cargo:git-delta@1.0.0"},
                                               "git-delta":{"mac":"cargo:git-delta", "linux":"cargo:git-delta@1.0.0"}}))
    sb.stub("cargo", script="""case "$1" in search) echo 'git-delta = "1.0.0" # test';; install) exit 99;; esac\nexit 0\n""")
    r = m.kit("packages", "update", "delta", "git-delta")
    has(r.out, "up to date", "checked once")
    calls = sb.calls("cargo")
    ok(sum(" search " in c for c in calls) == 1, calls)
    ok(not any(" install " in c or "--force" in c for c in calls), calls)
    ok(not sb.calls("brew"), sb.calls())
    ok(pkgs(m)["delta"][PLAT] == "cargo:git-delta@1.0.0", pkgs(m))
    ok(pkgs(m)["git-delta"][PLAT] == "cargo:git-delta@1.0.0", pkgs(m))


@test
def update_installs_only_newer_selected_cargo_package_and_check_is_read_only(sb):
    m = setup(sb); installed_cargo_fixture(m, "probe", "probe")
    m.repo_write("packages.json", json.dumps({"probe": {PLAT:"cargo:probe@1.0.0"}, "other":{PLAT:"cargo:other"}}))
    metadata = json.dumps({"installs":{"probe 2.0.0 (registry+https://github.com/rust-lang/crates.io-index)":{"bins":["probe"]}}})
    sb.stub("cargo", script=f"""case "$1" in
search) echo 'probe = "2.0.0" # test';;
install) printf '%s' {shlex.quote(metadata)} > "$CARGO_HOME/.crates2.json";;
esac
exit 0
""")
    before = pkgs(m)
    r = m.kit("packages", "update", "--check", "probe")
    has(r.out, "update available", "check only")
    ok(pkgs(m) == before)
    ok(not any(" install " in c for c in sb.calls("cargo")), sb.calls())
    r = m.kit("packages", "update", "probe")
    has(r.out, "1.0.0 → 2.0.0")
    ok(sb.calls("cargo").count("cargo install --locked probe@2.0.0") == 1, sb.calls())
    ok(not any("--force" in c or "install other" in c for c in sb.calls()), sb.calls())
    hasnt(r.out, "setting up tools", "installing other")
    ok(pkgs(m)["probe"][PLAT] == "cargo:probe@2.0.0")


@test
def update_never_installs_when_cargo_lookup_fails_or_would_downgrade(sb):
    m = setup(sb); installed_cargo_fixture(m, "probe", "probe", "2.0.0")
    m.repo_write("packages.json", json.dumps({"probe": {PLAT:"cargo:probe@2.0.0"}}))
    before = pkgs(m)
    sb.stub("cargo", script="""case "$1" in search) echo 'probe = "1.0.0" # test';; esac\nexit 0\n""")
    has(m.kit("packages", "update", "probe").out, "up to date")
    sb.stub("cargo", script="exit 1\n")
    m.kit("packages", "update", "probe", code=1)
    ok(pkgs(m) == before)
    ok(not any(" install " in c for c in sb.calls("cargo")), sb.calls())


@test
def update_brew_checks_outdated_and_skips_current_or_missing_packages(sb):
    if PLAT != "mac": return
    m = setup(sb); sb.stub("probe", code=0)
    (sb.root/"Cellar/probe").mkdir()
    m.repo_write("packages.json", json.dumps({"probe":{"mac":"brew:probe"}, "missing":{"mac":"brew:missing"}}))
    sb.stub("brew", script="""case "$1" in outdated) echo '{"formulae":[],"casks":[]}';; esac\nexit 0\n""")
    has(m.kit("packages", "update", "probe", "missing").out, "up to date")
    ok(not any("install" in c or " upgrade " in c for c in sb.calls("brew")), sb.calls())


@test
def add_existing_cargo_binary_records_its_actual_provider(sb):
    m = setup(sb); installed_cargo_fixture(m, "probe", "probe")
    sb.stub("brew", code=0); sb.stub("mise", script=MISE_STUB)
    m.kit("add", "probe", code=None if PLAT == "linux" else 0)
    ok(pkgs(m)["probe"][PLAT] == "cargo:probe@1.0.0", pkgs(m))


@test
def update_uses_custom_cargo_home_without_forcing_install(sb):
    m = setup(sb); installed_cargo_fixture(m, "probe", "probe", root="custom-cargo")
    m.repo_write("packages.json", json.dumps({"probe":{PLAT:"cargo:probe"}}))
    sb.stub("cargo", script="""case "$1" in search) echo 'probe = "1.0.0" # test';; esac\nexit 0\n""")
    m.kit("packages", "update", "probe", env={"CARGO_HOME":str(m.home/"custom-cargo")})
    ok(not any(" install " in c for c in sb.calls("cargo")), sb.calls())


@test
def update_mise_checks_version_and_never_reconciles_unselected_tools(sb):
    m = setup(sb)
    m.write(".local/kit/mise/shims/probe", "#!/bin/sh\nexit 0\n")
    (m.home/".local/kit/mise/shims/probe").chmod(0o755)
    recipe = "mise:aqua:o/probe@1.0.0" if PLAT == "mac" else "aqua:o/probe@1.0.0"
    m.repo_write("packages.json", json.dumps({"probe":{PLAT:recipe}, "other":{PLAT:"cmd:touch should-not-run"}}))
    script = """case "$1" in
ls) echo '{"probe":[{"version":"1.0.0","active":true,"installed":true}]}' ;;
latest) echo 1.0.0 ;;
esac
exit 0
"""
    sb.stub("mise", script=script)
    has(m.kit("packages", "update", "probe").out, "up to date")
    ok(not any("use " in c or "install " in c for c in sb.calls("mise")), sb.calls())
    updated = script.replace("latest) echo 1.0.0", "latest) echo 2.0.0")
    updated = updated.replace("ls) echo", 'ls) if [ -e "$HOME/mise-updated" ]; then echo \'{"probe":[{"version":"2.0.0","active":true,"installed":true}]}\'; exit 0; fi; echo')
    updated = updated.replace("esac", 'use) touch "$HOME/mise-updated" ;;\nesac')
    sb.stub("mise", script=updated)
    r = m.kit("packages", "update", "probe")
    hasnt(r.out, "setting up tools", "other")
    ok("mise use --global aqua:o/probe@2.0.0" in sb.calls("mise"), sb.calls())
    ok(not m.exists("should-not-run"))


@test
def update_cargo_git_never_follows_mutable_head(sb):
    m=setup(sb); installed_cargo_fixture(m,"probe","probe",source="git+https://example.invalid/repo#abc123")
    recipe={"probe":{PLAT:"cargo-git:https://example.invalid/repo"}}
    m.repo_write("packages.json",json.dumps(recipe))
    r=m.kit("packages","update","probe")
    has(r.both,"automatic HEAD updates are disabled")
    ok(not sb.calls("cargo"),sb.calls())
    ok(pkgs(m)==recipe)



@test
def files_inventory_is_explicit_and_exposes_saved_state(sb):
    m=setup(sb,{".config/demo/one.conf":"repo\n"},add=[".config/demo"])
    m.git("add","-A");m.git("commit","-qm","baseline")
    m.write(".config/demo/untracked.conf","not tracked\n")
    m.write(".config/demo/one.conf","local\n")
    rows=json.loads(m.kit("files","--json").out)
    ok([r["path"] for r in rows]==[".config/demo/one.conf"],rows)
    ok(rows[0]["state"]=="changed here" and not rows[0]["saved"],rows)
    m.kit("save","~/.config/demo/one.conf")
    rows=json.loads(m.kit("files","--json").out)
    ok(rows[0]["saved"] and rows[0]["state"]=="in sync",rows)
    m.kit("files","--tui",code=1)


@test
def files_tui_combines_save_and_ssh_and_cancels_without_changes(sb):
    m=setup(sb,{".config/demo/one.conf":"repo\n"},add=[".config/demo"])
    m.write(".config/demo/one.conf","local\n")
    before=(m.src/".kitignore").read_bytes()
    package_dialogue(m,b"/one.conf\rspq",command="files")
    ok(m.repo_read("home/.config/demo/one.conf")=="repo\n")
    ok((m.src/".kitignore").read_bytes()==before)
    raw=package_dialogue(m,b"/one.conf\rsp\r",command="files")
    ok(m.repo_read("home/.config/demo/one.conf")=="local\n")
    rows=json.loads(m.kit("files","--json").out)
    ok(rows[0]["ssh"] is False,rows)
    ok(raw.count("\x1b[2J")<=1,"file UI cleared the screen on every key")
    ok(not sb.calls("mise") and not sb.calls("brew"),sb.calls())


@test
def files_tui_take_repo_backs_up_and_untracking_keeps_local(sb):
    m=setup(sb,{".config/demo/one.conf":"repo\n"},add=[".config/demo"])
    m.write(".config/demo/one.conf","local\n")
    package_dialogue(m,b"/one.conf\rr\r",command="files")
    ok(m.read(".config/demo/one.conf")=="repo\n")
    ok(any(p.read_text()=="local\n" for p in (m.state/"backups").rglob("one.conf")))
    package_dialogue(m,b"/one.conf\rd\r",command="files")
    ok(m.exists(".config/demo/one.conf") and not m.repo_has("home/.config/demo/one.conf"))


@test
def files_tui_detects_files_changed_while_reviewing(sb):
    m=setup(sb,{".config/demo/one.conf":"repo\n"},add=[".config/demo"])
    m.write(".config/demo/one.conf","local\n")
    before=(m.src/".kitignore").read_bytes()
    raw=package_dialogue(m,b"/one.conf\rsp\r",command="files",expected=1,
        on_ready=lambda:m.write(".config/demo/one.conf","newer edit\n"))
    has(raw,"changed while the manager was open")
    ok(m.repo_read("home/.config/demo/one.conf")=="repo\n")
    ok((m.src/".kitignore").read_bytes()==before)


@test
def files_tui_diff_is_read_only_and_scope_toggles_are_reversible(sb):
    m=setup(sb,{".config/demo/one.conf":"repo\n"},add=[".config/demo"])
    m.write(".config/demo/one.conf","local\n")
    before=(m.src/".kitignore").read_bytes()
    package_dialogue(m,b"/one.conf\rvsvapp\r",command="files")
    ok(m.repo_read("home/.config/demo/one.conf")=="repo\n")
    ok((m.src/".kitignore").read_bytes()==before)


@test
def missing_platform_artifact_is_not_recorded_or_installed(sb):
    m=setup(sb);sb.stub("mise",script=MISE_STUB)
    r=m.kit("add","aqua:example/tool@1.2.3",code=1,env={"KIT_TEST_MISSING_PLATFORM":"macos-arm64"})
    has(r.both,"no verified release URL/checksum for macos-arm64")
    ok(not pkgs(m))
    ok(not any(" use " in c or " install " in c for c in sb.calls("mise")),sb.calls())
    ok(not list(m.state.glob("package-check-*")),"provider probe left a discovery cache")


@test
def unsupported_install_commands_never_run(sb):
    m=setup(sb)
    for recipe in ("system:cbonsai","cmd:touch should-not-run","asdf:unknown@1.0"):
        m.kit("add","probe","--linux",recipe,code=1)
    ok(not pkgs(m))
    ok(not sb.calls("brew") and not sb.calls("mise"),sb.calls())
    ok(not m.exists("should-not-run"))


@test
def package_check_distinguishes_source_builds_and_mac_only_apps(sb):
    m=setup(sb)
    m.repo_write("packages.json",json.dumps({"source":{"mac":"cargo:source@1.2.3","linux":"cargo:source@1.2.3"},"app":{"mac":"cask:app"}}))
    before=(m.src/"packages.json").read_bytes()
    r=m.kit("packages","check",code=1)
    has(r.both,"source/package build","no linux recipe","macOS application")
    ok((m.src/"packages.json").read_bytes()==before)
    ok(not sb.calls("cargo") and not sb.calls("brew"),sb.calls())


@test
def legacy_source_recipes_are_readable_but_not_automatically_executed(sb):
    m=setup(sb)
    m.repo_write("packages.json",json.dumps({"legacy":{PLAT:"cmd:touch should-not-run",OTHER:"system:cbonsai"}}))
    has(m.kit("packages","list").out,"unsupported recipe")
    m.kit("sync",code=1)
    ok(not m.exists("should-not-run"))
    ok(pkgs(m)["legacy"][PLAT]=="cmd:touch should-not-run")


@test
def failed_pull_does_not_run_installers_or_hooks(sb):
    m=setup(sb)
    m.repo_write("scripts/probe.sh",'touch "$HOME/should-not-run"\n')
    m.kit("init",str(sb.root/"does-not-exist.git"))
    r=m.kit("sync",code=1)
    has(r.both,"couldn't reach the backup repo")
    ok(not m.exists("should-not-run"))
    ok(not sb.calls("mise") and not sb.calls("brew"),sb.calls())


@test
def file_conflicts_stop_package_setup_and_hooks(sb):
    m=setup(sb,{".config/demo/file":"baseline\n"},add=[".config/demo"])
    m.write(".config/demo/file","local\n")
    m.repo_write("home/.config/demo/file","repo\n")
    m.repo_write("scripts/probe.sh",'touch "$HOME/should-not-run"\n')
    r=m.kit("sync",code=1)
    has(r.both,"setup skipped until file conflicts are resolved")
    ok(not m.exists("should-not-run"))
    ok(not sb.calls("mise") and not sb.calls("brew"),sb.calls())


@test
def invalid_hook_metadata_is_not_executed(sb):
    m=setup(sb);sb.stub("mise",script=MISE_STUB)
    m.repo_write("scripts/probe.sh",'# kit: run=occasionally\ntouch "$HOME/should-not-run"\n')
    r=m.kit("sync",code=1)
    has(r.both,"run=occasionally")
    ok(not m.exists("should-not-run"))


@test
def external_checkout_cannot_follow_a_home_symlink(sb):
    m=setup(sb);outside=sb.root/"outside";outside.mkdir()
    upstream=sb.make_git_repo("external-source",{"file":"safe\n"})
    os.symlink(outside,m.home/"linked")
    m.repo_write("externals.json",json.dumps({"linked/repo":{"git":str(upstream)}}))
    m.kit("sync",code=1)
    ok(not (outside/"repo").exists())


@test
def remote_os_detection_must_succeed_before_upload(sb):
    m=setup(sb);sb.stub("ssh",script="exit 1\n")
    r=m.kit("push","box",code=1)
    has(r.both,"could not detect the remote OS")
    ok(not any("mkdir -p" in c for c in sb.calls("ssh")),sb.calls())


@test
def linux_casks_are_rejected_without_recording(sb):
    m=setup(sb)
    m.repo_write("packages.json",json.dumps({"app":{"mac":"cask:app","linux":"cask:app"}}))
    r=m.kit("packages","check",code=1)
    has(r.both,"Homebrew casks are macOS-only")
    ok(not sb.calls("brew") and not sb.calls("mise"),sb.calls())


@test
def local_installer_must_produce_the_expected_tool(sb):
    if PLAT != "mac": return  # Linux's equivalent is covered by remote.py.
    m=setup(sb)
    script=MISE_STUB.replace('INSTALL_FIXTURE', 'true')
    # Leave metadata resolution intact, but remove materialization from the fake installer.
    script=script.replace(shlex.quote(sys.executable)+" "+shlex.quote(str(HERE/"install_fixture.py")),"true")
    sb.stub("mise",script=script)
    r=m.kit("add","aqua:example/absent-tool@1.2.3",code=1)
    has(r.both,"installer exited successfully but the expected tool is unavailable")
    ok("absent-tool" in pkgs(m),"failed installs should retain a recipe for retry")
    installed=m.state/"packages-installed.json"
    ok(not installed.exists() or "absent-tool" not in json.loads(installed.read_text()))


@test
def status_does_not_require_optional_package_managers(sb):
    m=setup(sb)
    sb.unstub("brew");sb.unstub("mise")
    r=m.kit("status")
    hasnt(r.out,"brew (", "mise (")


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
            detail += "".join(traceback.format_exception(type(e), e, e.__traceback__)).rstrip() + "\n"
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


@test
def update_cargo_requires_actual_installed_version_after_success_exit(sb):
    m = setup(sb); installed_cargo_fixture(m, "probe", "probe")
    before = {"probe": {PLAT:"cargo:probe@1.0.0"}}
    m.repo_write("packages.json", json.dumps(before))
    sb.stub("cargo", script="""case "$1" in search) echo 'probe = "2.0.0"';; esac
exit 0
""")
    r = m.kit("packages", "update", "probe", code=1)
    has(r.both, "did not confirm", "recorded recipe preserved")
    ok(pkgs(m) == before, pkgs(m))


@test
def installs_use_private_mise_when_it_is_not_on_shell_path(sb):
    m = setup(sb)
    (sb.fakebin/"mise").unlink()
    private = m.home/".local/kit/bin/mise"
    fixture = shlex.quote(str(HERE/"mise_lock_fixture.py"))
    private.write_text(f'''#!/bin/sh
case "$1" in
--version) echo mise-test;;
ls) exec {shlex.quote(sys.executable)} {shlex.quote(str(HERE/'mise_inventory_fixture.py'))};;
lock) exec {shlex.quote(sys.executable)} {fixture};;
use|install|reshim)
  mkdir -p "$HOME/.local/kit/mise/shims"
  for tool in probe bat delta nvim; do
    printf '#!/bin/sh\\nexit 0\\n' > "$HOME/.local/kit/mise/shims/$tool"
    chmod +x "$HOME/.local/kit/mise/shims/$tool"
  done;;
esac
exit 0
''')
    private.chmod(0o755)
    r = m.kit("add", "aqua:owner/probe@1.2.3", "--bin", "probe",
              env={"PATH":str(sb.fakebin)+os.pathsep+"/usr/bin:/bin"})
    ok(pkgs(m)["probe"][PLAT] == "aqua:owner/probe@1.2.3", r.show())
    ok(os.access(m.home/".local/kit/mise/shims/probe", os.X_OK))


@test
def push_core_aliases_are_resolved_for_the_destination_platform(sb):
    m = setup(sb); push_ready(sb, m)
    # This entry replaces delta on the sending host, but is a different command
    # on the receiving host. The destination still needs Kit's normal delta.
    target_os = "Linux" if PLAT == "mac" else "Darwin"
    m.repo_write("packages.json", json.dumps({"alias": {
        "mac":"aqua:owner/alias@1.2.3", "linux":"aqua:owner/alias@1.2.3",
        "bin":"delta" if PLAT == "mac" else "different",
        "linux_bin":"different" if PLAT == "mac" else "delta"}}))
    sb.stub("ssh", script=f'''case "$*" in *"uname -s"*) echo {target_os};; *) cat >/dev/null;; esac
exit 0
''')
    m.kit("push", "box")
    payload = (m.state/"payload/packages.list").read_text()
    has(payload, "delta\tdelta\taqua:dandavison/delta@", "alias\tdifferent\t")


@test
def sync_missing_current_platform_recipe_is_a_failure_unless_tool_exists(sb):
    m = setup(sb)
    m.repo_write("packages.json", json.dumps({"kit-recipe-probe":{OTHER:"cargo:kit-recipe-probe@1.2.3"}}))
    r = m.sync(code=1)
    has(r.both, "kit-recipe-probe", "no " + PLAT + " recipe" if PLAT == "mac" else "no recipe")
    sb.stub("kit-recipe-probe", code=0)
    r = m.sync()
    hasnt(r.both, "no " + PLAT + " recipe")


@test
def update_mise_requires_actual_active_version_after_success_exit(sb):
    m = setup(sb)
    m.write(".local/kit/mise/shims/probe", "#!/bin/sh\nexit 0\n", mode=0o755)
    before = {"probe": {PLAT:"aqua:owner/probe@1.0.0"}}
    m.repo_write("packages.json", json.dumps(before))
    sb.stub("mise", script="""case "$1" in
ls) echo '{"probe":[{"version":"1.0.0","active":true,"installed":true}]}';;
latest) echo 2.0.0;;
esac
exit 0
""")
    r = m.kit("packages", "update", "probe", code=1)
    has(r.both, "did not confirm", "recorded recipe preserved")
    ok(pkgs(m) == before, pkgs(m))


@test
def passive_inventory_rejects_stale_shims_without_running_tools(sb):
    m = setup(sb)
    m.repo_write("packages.json", json.dumps({name:{PLAT:f"aqua:owner/{name}@1.2.3"} for name in ("stale-one", "stale-two")}))
    for name in ("stale-one", "stale-two"):
        m.write(f".local/kit/mise/shims/{name}", '#!/bin/sh\ntouch "$HOME/should-not-execute"\nexit 0\n', mode=0o755)
    sb.stub("mise", script='case "$1" in ls) echo "{}"; exit 0;; *) exit 99;; esac\n')
    rows = {p["name"]:p for p in json.loads(m.kit("packages", "--json").out)}
    ok(not rows["stale-one"]["installed"] and not rows["stale-two"]["installed"], rows)
    ok(not m.exists("should-not-execute"), "passive inventory executed a managed tool")
    calls = sb.calls("mise")
    ok(len(calls) == 1 and "ls --installed --json --offline" in calls[0], calls)
    m.kit("sync", "--dry-run")
    ok(not m.exists("should-not-execute"), "dry-run executed a managed tool")


@test
def post_install_probe_uses_private_mise_roots_before_shell_activation(sb):
    m = setup(sb)
    m.repo_write("packages.json", json.dumps({"probe":{PLAT:"aqua:owner/probe@1.2.3"}}))
    # A real mise shim cannot resolve its installation merely from argv[0].
    # The fake installer writes a shim that enforces the same environment.
    install = f'''{shlex.quote(sys.executable)} {shlex.quote(str(HERE/'install_fixture.py'))} "$@"
if [ "$1" = use ] || [ "$1" = install ]; then
  cat > "$HOME/.local/kit/mise/shims/probe" <<'SHIM'
#!/bin/sh
[ "$MISE_DATA_DIR" = "$HOME/.local/kit/mise" ] || exit 91
[ "$MISE_GLOBAL_CONFIG_FILE" = "$HOME/.local/kit/mise.toml" ] || exit 92
[ "$MISE_CONFIG_DIR" = "$HOME/.local/kit/config/mise" ] || exit 93
echo probe-1.2.3
SHIM
  chmod +x "$HOME/.local/kit/mise/shims/probe"
fi'''
    script = MISE_STUB.replace(shlex.quote(sys.executable)+" "+shlex.quote(str(HERE/'install_fixture.py'))+' "$@"', install)
    sb.stub("mise", script=script)
    m.kit("sync")
    ok(m.exists(".local/kit/mise/shims/probe"))


@test
def passive_inventory_finds_external_cargo_behind_a_stale_owned_shim(sb):
    m = setup(sb)
    m.repo_write("packages.json", json.dumps({"probe":{PLAT:"cargo:probe@1.2.3"}}))
    m.write(".local/kit/mise/shims/probe", '#!/bin/sh\ntouch "$HOME/should-not-execute"\nexit 1\n', mode=0o755)
    m.write(".cargo/bin/probe", '#!/bin/sh\ntouch "$HOME/should-not-execute"\nexit 0\n', mode=0o755)
    sb.stub("mise", script='exit 99\n')
    path = str(m.home/".local/kit/mise/shims")+os.pathsep+m.envvars["PATH"]
    rows = {p["name"]:p for p in json.loads(m.kit("packages", "--json", env={"PATH":path}).out)}
    ok(rows["probe"]["installed"], rows)
    ok(not m.exists("should-not-execute"), "passive inventory executed an installed binary")
    ok(not sb.calls("mise"), sb.calls())


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
