#!/usr/bin/env python3
"""Opt-in real Google SDK/Python cold install in a disposable home; no cloud access."""
import os
from pathlib import Path
import subprocess
import tempfile

assets=Path(__file__).resolve().parents[1]/'assets/remote'
with tempfile.TemporaryDirectory(prefix='dotkit-gcloud-live-') as root:
    home=Path(root)/'home'; home.mkdir()
    kit=home/'.local/kit'
    env=dict(os.environ, HOME=str(home), KIT=str(kit), PATH='/usr/bin:/bin:/usr/sbin:/sbin',
             CLOUDSDK_CORE_DISABLE_PROMPTS='1', CLOUDSDK_CORE_DISABLE_USAGE_REPORTING='true')
    for key in list(env):
        if key.startswith('MISE_') or key in ('CLOUDSDK_PYTHON','CLOUDSDK_CONFIG','PYTHONHOME','PYTHONPATH'):
            env.pop(key)
    def run(args):
        result=subprocess.run(args,env=env,text=True,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,timeout=900)
        print(result.stdout,flush=True)
        if result.returncode: raise SystemExit(result.returncode)
        return result.stdout
    run(['bash',str(assets/'bootstrap.sh')])
    run(['bash',str(assets/'install-gcloud.sh'),'589.0.0'])
    inode=(kit/'opt/gcloud').stat().st_ino
    run(['bash',str(assets/'install-gcloud.sh'),'589.0.0'])
    assert (kit/'opt/gcloud').stat().st_ino==inode, 'repeat setup replaced a working SDK'
    output=run([str(kit/'bin/gcloud'),'--version'])
    assert 'Google Cloud SDK 589.0.0' in output
    assert (kit/'opt/gcloud/kit-version').read_text().strip()=='589.0.0'
    assert (kit/'mise/installs/python/3.14.8/bin/python3').is_file()
    assert not (home/'.bashrc').exists() and not (home/'.zshrc').exists()
    print('PASS private gcloud/Python cold install and command execution',flush=True)
