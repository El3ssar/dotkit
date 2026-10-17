#!/usr/bin/env python3
"""Real, opt-in macOS/Linux provisioning smoke test (downloads tools into a temporary HOME).
Run: python3 tests/live.py --kit target/debug/kit [--source]
No package managers or downloads are stubbed. Never uses the real dotfile repo.
"""
import argparse
import json
import os
import platform
from pathlib import Path
import shutil
import subprocess
import tempfile


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--kit', default='target/debug/kit')
    ap.add_argument('--source', action='store_true', help='also compile a Cargo CLI and bootstrap Rust')
    ap.add_argument('--keep', action='store_true', help='keep the test home for inspection')
    args = ap.parse_args()
    kit = str(Path(args.kit).resolve())
    root = Path(tempfile.mkdtemp(prefix='dotkit-live-'))
    home = root / 'home'; home.mkdir()
    env = dict(os.environ, HOME=str(home), KIT_SOURCE=str(home/'.local/share/kit'),
               KIT_STATE=str(home/'.local/state/kit'), XDG_CONFIG_HOME=str(home/'.config'),
               XDG_DATA_HOME=str(home/'.local/share'), XDG_STATE_HOME=str(home/'.local/state'),
               XDG_CACHE_HOME=str(home/'.cache'), GIT_CONFIG_GLOBAL=str(root/'gitconfig'),
               GIT_CONFIG_NOSYSTEM='1', GIT_AUTHOR_NAME='Kit Test', GIT_AUTHOR_EMAIL='test@example.com',
               GIT_COMMITTER_NAME='Kit Test', GIT_COMMITTER_EMAIL='test@example.com', KIT_NO_DELTA='1')
    # A cold machine must not borrow the caller's tools or Rust installation.
    env['PATH']='/usr/bin:/bin:/usr/sbin:/sbin'
    for key in ('CARGO_HOME','RUSTUP_HOME','RUSTUP_TOOLCHAIN'):
        env.pop(key,None)
    for key in list(env):
        if key.startswith('MISE_'):env.pop(key,None)
    is_linux=platform.system()=='Linux'
    current='linux' if is_linux else 'mac'
    other='mac' if is_linux else 'linux'
    (root/'gitconfig').write_text('[init]\n defaultBranch = main\n')
    def run(*command, capture=False, expected=0):
        r = subprocess.run(command, cwd=home, env=env, text=True, capture_output=capture, timeout=1200)
        if r.returncode != expected: raise RuntimeError(f'{command}: exit {r.returncode}, expected {expected}\n{r.stdout or ""}\n{r.stderr or ""}')
        return r.stdout or ''
    try:
        run(kit, 'init')
        (home/'.zshrc').write_text('export KIT_LIVE_PROBE=at-home\n')
        (home/'.gitconfig').write_text('[user]\n name = Test\n')
        run(kit, 'add', '~/.zshrc', '~/.gitconfig')
        pkgs = {'ripgrep': {'linux':'aqua:BurntSushi/ripgrep@15.2.0', 'bin':'rg'},
                'jq': {'linux':'aqua:jqlang/jq@1.8.2'},
                'prettier': {'linux':'npm:prettier@3.9.10'}}
        for recipe in pkgs.values():recipe['mac']=recipe['linux']
        if args.source: pkgs['just'] = {'mac':'cargo:just@1.48.0', 'linux':'cargo:just@1.48.0'}
        source = home/'.local/share/kit'
        (source/'packages.json').write_text(json.dumps(pkgs))
        run(kit, 'packages', 'resolve')
        # Later failure/retry fixtures must retain the explicitly resolved recipes.
        # Sync deliberately does not regenerate a recipe removed from the manifest.
        pkgs = json.loads((source/'packages.json').read_text())
        run(kit, 'sync')
        tools = ['rg', 'jq', 'prettier', 'node', 'nvim', 'bat', 'delta'] + (['just'] if args.source else [])
        checks = '; '.join(f'{tool} --version >/dev/null || exit 1' for tool in tools)
        output = run('bash', '-c', f'. "$HOME/.local/kit/bin/kit-env"; {checks}; echo tools-ok', capture=True)
        assert 'tools-ok' in output
        # Actual shell config and activation, using the installed portable zsh.
        output = run(str(home/'.local/kit/bin/kit-shell'), '-i', '-c', 'echo "$KIT_LIVE_PROBE"; command -v rg', capture=True)
        assert 'at-home' in output and str(home/'.local/kit') in output
        # Exercise actual mise JSON/installed state, not only fake provider fixtures.
        inventory=json.loads(run(kit,'packages','list','--json',capture=True))
        assert all(row['installed'] for row in inventory if row['name'] in pkgs), inventory
        # Capture real installed mise ownership and verify the other OS at the same version.
        run(kit, 'add', 'ripgrep')
        captured=json.loads((source/'packages.json').read_text())['ripgrep']
        assert captured['observed'][current]['provider']=='mise', captured
        assert captured['observed'][current]['version']=='15.2.0', captured
        assert captured[other]=='aqua:BurntSushi/ripgrep@15.2.0', captured
        pkgs=json.loads((source/'packages.json').read_text())
        before_manifest=(source/'packages.json').read_bytes()
        selected=['ripgrep','jq','prettier'] + (['just'] if args.source else [])
        preview=run(kit,'packages','update','--check',*selected,capture=True)
        assert 'could not identify' not in preview and 'outside kit' not in preview, preview
        assert (source/'packages.json').read_bytes()==before_manifest
        run(kit, 'sync')   # idempotence and no stale shim/recipe state
        assert 'in sync' in run(kit, 'status', '~/.zshrc', capture=True)
        # Missing recipes must be visible failures; earlier successful tools must still run.
        pkgs['unportable'] = {other:'aqua:example/unportable@1.2.3'}
        (source/'packages.json').write_text(json.dumps(pkgs))
        run(kit, 'sync', expected=1)
        if is_linux:
            results = (home/'.local/kit/state/setup-results.tsv').read_text()
            assert 'unportable\tmissing\t' in results
            assert 'ripgrep\tinstalled\t' in results
        del pkgs['unportable']; (source/'packages.json').write_text(json.dumps(pkgs))
        run(kit, 'sync')
        print(f'PASS: real {current} install, runtime dependencies, shell, provider inventory/update preview, retry, and partial failure ({root})')
    except Exception:
        print(f'FAILED: test home kept at {root}')
        raise
    else:
        if not args.keep: shutil.rmtree(root)


if __name__ == '__main__': main()
