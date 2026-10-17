#!/usr/bin/env python3
"""Offline installed inventory for the provider sandbox, never a real mise call."""
import json
import os
from pathlib import Path
home = Path(os.environ['HOME'])
repo = Path(os.environ['KIT_SOURCE'])
packages = json.loads((repo/'packages.json').read_text()) if (repo/'packages.json').exists() else {}
shims = home/'.local/kit/mise/shims'
records = {}
for shim in shims.iterdir() if shims.exists() else []:
    if not os.access(shim, os.X_OK):
        continue
    records[shim.name] = [{'version':'1.2.3', 'active':True, 'installed':True}]
for name, pkg in packages.items():
    binary = pkg.get('bin') or {'ripgrep':'rg','git-delta':'delta','skim':'sk','neovim':'nvim'}.get(name,name)
    if (shims/binary).exists():
        records[name] = [{'version':'1.2.3','active':True,'installed':True}]
print(json.dumps(records))
