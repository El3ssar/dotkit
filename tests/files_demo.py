#!/usr/bin/env python3
"""Try the file tree and conflicts using disposable examples, without touching your dotfiles.
Run: python3 tests/files_demo.py [--kit /path/to/kit]
"""
import argparse
import os
from pathlib import Path
import shutil
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--kit', default=str(Path.home()/'.cargo/bin/kit'))
    parser.add_argument('--keep', action='store_true', help='keep the demo home for inspection')
    parser.add_argument('--prepare-only', action='store_true', help='validate example setup without opening the terminal UI')
    args = parser.parse_args()
    kit = str(Path(args.kit).expanduser().resolve())
    root = Path(tempfile.mkdtemp(prefix='kit-files-demo-'))
    home = root/'home'; home.mkdir()
    source = root/'repo'
    env = dict(os.environ, HOME=str(home), KIT_SOURCE=str(source), KIT_STATE=str(root/'state'),
               XDG_CONFIG_HOME=str(home/'.config'), XDG_DATA_HOME=str(home/'.local/share'),
               XDG_STATE_HOME=str(home/'.local/state'), XDG_CACHE_HOME=str(home/'.cache'),
               GIT_CONFIG_GLOBAL=str(root/'gitconfig'), GIT_CONFIG_NOSYSTEM='1', KIT_NO_DELTA='1',
               GIT_AUTHOR_NAME='Kit Demo', GIT_AUTHOR_EMAIL='demo@example.invalid',
               GIT_COMMITTER_NAME='Kit Demo', GIT_COMMITTER_EMAIL='demo@example.invalid')
    def run(*cmd, capture=True):
        return subprocess.run(cmd, cwd=home, env=env, check=True, text=True, capture_output=capture)
    def write(path, text):
        path.parent.mkdir(parents=True, exist_ok=True); path.write_text(text)
    try:
        run(kit,'init')
        files = {
            '.config/kitty/kitty.conf':'font_size 14\nbackground_opacity 0.95\n',
            '.config/nvim/init.lua':"vim.opt.number = true\n-- display\n-- editing\nvim.opt.expandtab = true\n",
            '.config/git/config':'[core]\n  editor = vim\n',
            '.config/starship.toml':'add_newline = true\n',
            '.config/ripgrep/config':'--smart-case\n',
            '.zshrc':'export EDITOR=vim\n',
        }
        for app in ('bat','btop','zellij','herdr'):
            for i in range(6): files[f'.config/{app}/example-{i+1:02}.conf']=f'# {app} tracked example {i+1}\n'
        for path,text in files.items(): write(home/path,text)
        run(kit,'add',*[str(home/path) for path in files])
        run('git','-C',str(source),'add','-A'); run('git','-C',str(source),'commit','-qm','Demo baseline')
        write(source/'home/.config/kitty/kitty.conf','font_size 16\nbackground_opacity 0.95\n')
        write(home/'.config/kitty/kitty.conf','<<<<<<< this machine\nfont_size 18\n=======\nfont_size 16\n>>>>>>> repo\nbackground_opacity 0.95\n')
        write(home/'.config/nvim/init.lua',files['.config/nvim/init.lua'].replace('number = true','number = false'))
        write(source/'home/.config/nvim/init.lua',files['.config/nvim/init.lua'].replace('expandtab = true','expandtab = false'))
        write(home/'.config/git/config','[core]\n  editor = nvim\n')
        write(source/'home/.config/starship.toml','add_newline = false\n')
        (home/'.config/ripgrep/config').unlink()
        write(home/'.config/kitty/not-tracked.conf','# This must never appear in the tracked tree.\n')
        if args.prepare_only:
            run(kit,'files','--json')
        else:
            print('Disposable demo: 3 shows conflicts; select kitty.conf and press m. Your actual dotfiles are untouched.',flush=True)
            run(kit,'files','--tui',capture=False)
    finally:
        if args.keep: print(f'Demo files kept at {root}')
        else: shutil.rmtree(root)


if __name__ == '__main__': main()
