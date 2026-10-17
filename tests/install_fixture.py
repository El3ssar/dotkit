#!/usr/bin/env python3
"""Materialize the executable promised by a successful fake provider install."""
import json
import os
import platform
import sys
from pathlib import Path
home = Path(os.environ['HOME'])
repo = Path(os.environ['KIT_SOURCE'])
packages = json.loads((repo/'packages.json').read_text()) if (repo/'packages.json').exists() else {}
target = sys.argv[-1].removeprefix('mise:')
if target.startswith(('brew:', 'cask:')):
    kind, name = target.split(':', 1)
    prefix = Path(os.environ['PATH'].split(os.pathsep)[0]).parent
    (prefix/('Caskroom' if kind == 'cask' else 'Cellar')/name).mkdir(parents=True, exist_ok=True)
packages = {**{name:{'mac':spec,'linux':spec,'bin':binary} for name,spec,binary in (
    ('bat','aqua:sharkdp/bat@0.26.1','bat'), ('delta','aqua:dandavison/delta@0.20.1','delta'),
    ('neovim','aqua:neovim/neovim@0.12.6','nvim'))}, **packages}
field = 'mac' if platform.system() == 'Darwin' else 'linux'
for name, pkg in packages.items():
    if pkg.get(field, '').removeprefix('mise:') != target:
        continue
    binary = pkg.get('linux_bin' if field == 'linux' else 'bin') or pkg.get('bin') or {'ripgrep':'rg','git-delta':'delta','skim':'sk','neovim':'nvim'}.get(name, name)
    path = home/'.local/kit/mise/shims'/binary
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text('#!/bin/sh\necho test-tool\n')
    path.chmod(0o755)
