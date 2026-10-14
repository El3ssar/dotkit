#!/usr/bin/env python3
"""Real fizsh smoke test: downloads pinned upstream scripts into a disposable HOME.
Run on macOS or Linux with Python, curl, tar and zsh available. Uses a real terminal.
"""
import os
from pathlib import Path
import pty
import select
import time
import shutil
import subprocess
import tempfile

ASSETS = Path(__file__).resolve().parents[1] / 'assets/remote'
root = Path(tempfile.mkdtemp(prefix='dotkit-fizsh-'))
home = root / 'home'; home.mkdir()
kit = home / '.local/kit'
(kit / 'bin').mkdir(parents=True)
for name in ('kit-env', 'kit-shell'):
    shutil.copy(ASSETS / name, kit / 'bin' / name)
    (kit / 'bin' / name).chmod(0o755)
shutil.copy(kit / 'bin/kit-shell', kit / 'bin/fizsh')
env = dict(os.environ, HOME=str(home), KIT=str(kit), TERM='xterm', ZDOTDIR=str(home))

def run(*cmd):
    r = subprocess.run(cmd, env=env, cwd=home, text=True, capture_output=True, timeout=300)
    assert r.returncode == 0, r.stdout + r.stderr
    return r.stdout


def terminal(commands):
    master, slave = pty.openpty()
    proc = subprocess.Popen([str(kit / 'bin/kit-shell')], env=env, cwd=home,
                            stdin=slave, stdout=slave, stderr=slave, start_new_session=True)
    os.close(slave)
    output = b''
    sent = False
    deadline = time.monotonic() + 30
    try:
        while time.monotonic() < deadline:
            ready, _, _ = select.select([master], [], [], 0.1)
            if ready:
                try: chunk = os.read(master, 65536)
                except OSError: break
                if not chunk: break
                output += chunk
            if not sent and time.monotonic() > deadline - 28:
                os.write(master, (commands + '\nexit\n').encode())
                sent = True
            if proc.poll() is not None: break
        out = output.decode(errors='replace')
        assert proc.poll() is not None, 'shell did not exit: ' + out
        assert proc.returncode == 0, out
        return out
    finally:
        os.close(master)
        if proc.poll() is None: proc.kill(); proc.wait()

try:
    run('bash', str(ASSETS / 'install-fizsh.sh'))
    shell = str(kit / 'bin/kit-shell')
    assert 'fizsh, version 1.0.9' in run(shell, '--version')
    out = terminal("printf '\\nFIZSH_PROBE=%s:%s\\n' \"$_fizsh_F_CENTRAL_VERSION\" \"${+functions[_zsh_highlight]}\"")
    assert 'FIZSH_PROBE=1.0.9:1' in out, out
    assert (home / '.fizsh/zsh-history-substring-search.zsh').is_file()
    # Upstream normally deletes .zshrc on version change. Kit must preserve it and user plugins.
    rc = home / '.fizsh/.zshrc'
    rc.write_text('export FIZSH_SAVED_CONFIG=untouched\n')
    plugin = home / '.fizsh/zsh-history-substring-search.zsh'
    plugin.write_text('# custom tracked plugin\n')
    (home / '.fizsh/.fizsh-version').write_text('old\n')
    run('bash', str(ASSETS / 'install-fizsh.sh'))
    assert 'fizsh, version 1.0.9' in run(shell, '--version')
    assert rc.read_text() == 'export FIZSH_SAVED_CONFIG=untouched\n'
    out = terminal('printf "\\nSAVED_CONFIG=%s\\n" "$FIZSH_SAVED_CONFIG"')
    assert 'SAVED_CONFIG=untouched' in out, out
    assert plugin.read_text() == '# custom tracked plugin\n'
    assert run(shell, '-c', 'printf noninteractive-ok') == 'noninteractive-ok'
    # Corrupt source archive must be rejected while preserving the working installation.
    bad = root / 'bad.tar.gz'; bad.write_bytes(b'invalid')
    r = subprocess.run(['bash', str(ASSETS / 'install-fizsh.sh'), str(bad)], env=env, capture_output=True)
    assert r.returncode != 0 and b'checksum mismatch' in r.stderr
    assert (kit / 'opt/fizsh/bin/fizsh').is_file()
    print('PASS: real fizsh, syntax highlighting, history search, preserved config, quiet remote commands, checksum rejection')
finally:
    shutil.rmtree(root)
