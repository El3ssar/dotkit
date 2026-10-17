#!/usr/bin/env python3
"""Opt-in real localhost SSH push/remove smoke test, only in a disposable Linux container.

KIT_SSH_TEST_CONTAINER=1 python3 tests/ssh_live.py --kit /target/debug/kit
Uses ephemeral sshd/client keys, isolated HOME/config/state, and no existing SSH files.
Downloads the actual managed tools; nothing is mocked. Requires root for test sshd.
"""
import argparse
import json
import os
from pathlib import Path
import shlex
import shutil
import signal
import socket
import subprocess
import tempfile
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--kit', default='target/debug/kit')
    parser.add_argument('--keep', action='store_true', help='retain temporary files for inspection')
    args = parser.parse_args()
    if (os.environ.get('KIT_SSH_TEST_CONTAINER') != '1' or os.uname().sysname != 'Linux'
            or os.geteuid() != 0 or not Path('/.dockerenv').exists()):
        parser.error('requires disposable Docker Linux container, root, and KIT_SSH_TEST_CONTAINER=1')
    kit = str(Path(args.kit).resolve())
    for name in ('ssh', 'sshd', 'ssh-keygen', 'git', 'curl', 'tar', 'bash'):
        if not shutil.which(name): parser.error(f'missing prerequisite: {name}')
    root = Path(tempfile.mkdtemp(prefix='kit-ssh-', dir='/tmp'))
    sender = root / 'sender'; remote = root / 'remote'
    sender.mkdir(); remote.mkdir()
    env = dict(os.environ, HOME=str(sender), KIT_SOURCE=str(sender/'.local/share/kit'),
               KIT_STATE=str(sender/'.local/state/kit'), XDG_CONFIG_HOME=str(sender/'.config'),
               XDG_DATA_HOME=str(sender/'.local/share'), XDG_STATE_HOME=str(sender/'.local/state'),
               XDG_CACHE_HOME=str(sender/'.cache'), GIT_CONFIG_GLOBAL=str(root/'gitconfig'),
               GIT_CONFIG_NOSYSTEM='1', GIT_AUTHOR_NAME='Kit SSH Test', GIT_AUTHOR_EMAIL='ssh@example.invalid',
               GIT_COMMITTER_NAME='Kit SSH Test', GIT_COMMITTER_EMAIL='ssh@example.invalid',
               GIT_TERMINAL_PROMPT='0', KIT_NO_DELTA='1')
    (root/'gitconfig').write_text('[init]\n defaultBranch = main\n')
    server = None
    server_log = None
    passed = False

    def run(*command, expected=0, timeout=1200):
        result = subprocess.run(command, cwd=sender, env=env, stdin=subprocess.DEVNULL,
                                text=True, capture_output=True, timeout=timeout)
        if result.returncode != expected:
            raise RuntimeError(f'{shlex.join(command)}: exit {result.returncode}\n{result.stdout}\n{result.stderr}')
        return result.stdout

    try:
        for name in ('host-key', 'client-key'):
            run('ssh-keygen', '-q', '-t', 'ed25519', '-N', '', '-f', str(root/name), timeout=30)
        (root/'authorized_keys').write_text((root/'client-key.pub').read_text())
        (root/'authorized_keys').chmod(0o600)
        # ForceCommand changes HOME before evaluating the actual original command.
        # The account's shell/configuration and real home are never modified.
        wrapper = root/'remote-command'
        wrapper.write_text('#!/bin/bash\nset -eu\n' +
            'export HOME=' + shlex.quote(str(remote)) + '\n' +
            'export XDG_CONFIG_HOME="$HOME/.config" XDG_DATA_HOME="$HOME/.local/share"\n' +
            'export XDG_STATE_HOME="$HOME/.local/state" XDG_CACHE_HOME="$HOME/.cache"\n' +
            'export KIT_INSTALL_TIMEOUT=300 GIT_TERMINAL_PROMPT=0\n' +
            'unset KIT_SOURCE KIT_STATE\ncd "$HOME"\nexec /bin/bash --noprofile --norc -c "$SSH_ORIGINAL_COMMAND"\n')
        wrapper.chmod(0o700)
        with socket.socket() as sock:
            sock.bind(('127.0.0.1', 0)); port = sock.getsockname()[1]
        config = root/'sshd_config'
        config.write_text(f'''Port {port}
ListenAddress 127.0.0.1
HostKey {root}/host-key
PidFile {root}/sshd.pid
AuthorizedKeysFile {root}/authorized_keys
StrictModes no
PasswordAuthentication no
KbdInteractiveAuthentication no
PubkeyAuthentication yes
PermitRootLogin yes
UsePAM yes
PrintMotd no
PrintLastLog no
LogLevel VERBOSE
AllowUsers root
PermitUserEnvironment no
ForceCommand {wrapper}
''')
        Path('/run/sshd').mkdir(mode=0o755, exist_ok=True)
        server_log = (root/'sshd.log').open('w')
        server = subprocess.Popen([shutil.which('sshd'), '-D', '-e', '-f', str(config)],
                                  stdout=server_log, stderr=subprocess.STDOUT, start_new_session=True)
        client_config = root/'ssh_config'
        client_config.write_text(f'''Host kit-smoke
 HostName 127.0.0.1
 Port {port}
 User root
 IdentityFile {root}/client-key
 IdentitiesOnly yes
 BatchMode yes
 StrictHostKeyChecking accept-new
 UserKnownHostsFile {root}/known_hosts
 GlobalKnownHostsFile /dev/null
 LogLevel ERROR
''')
        fakebin = root/'bin'; fakebin.mkdir()
        client = fakebin/'ssh'
        client.write_text('#!/bin/sh\nexec /usr/bin/ssh -F ' + shlex.quote(str(client_config)) + ' "$@"\n')
        client.chmod(0o700)
        env['PATH'] = str(fakebin) + ':' + os.environ['PATH']
        # Wait for this exact daemon, then authenticate with its temporary key.
        for _ in range(100):
            if server.poll() is not None: raise RuntimeError((root/'sshd.log').read_text())
            try:
                with socket.create_connection(('127.0.0.1', port), timeout=.1): break
            except OSError: time.sleep(.05)
        else: raise RuntimeError('ephemeral sshd did not start')
        assert run('ssh', 'kit-smoke', 'printf %s "$HOME"', timeout=30) == str(remote)
        run(kit, 'init')
        original = 'remote file before Kit\n'
        (remote/'kit-ssh-probe.conf').write_text(original)
        (sender/'kit-ssh-probe.conf').write_text('first deployment\n')
        (sender/'.zshrc').write_text('export KIT_SSH_PROBE=at-home\n')
        run(kit, 'add', '~/kit-ssh-probe.conf', '~/.zshrc')
        source = sender/'.local/share/kit'
        (source/'packages.json').write_text(json.dumps({
            'ripgrep': {'linux': 'aqua:BurntSushi/ripgrep@15.2.0', 'bin': 'rg'}}))
        print('Real SSH: first push (downloads tools)', flush=True)
        first = run(kit, 'push', 'kit-smoke')
        (root/'first-push.log').write_text(first)
        assert (remote/'kit-ssh-probe.conf').read_text() == 'first deployment\n'
        output = run('ssh', 'kit-smoke', '. "$HOME/.local/kit/bin/kit-env"; rg --version', timeout=30)
        assert output.startswith('ripgrep '), output
        # Startup hooks must not print a banner or replace a command/pipe session.
        assert run('ssh', 'kit-smoke', '. "$HOME/.bashrc"; printf noninteractive-exact', timeout=30) == 'noninteractive-exact'
        print('Real SSH: second push', flush=True)
        (sender/'kit-ssh-probe.conf').write_text('second deployment\n')
        run(kit, 'save', '~/kit-ssh-probe.conf')
        second = run(kit, 'push', 'kit-smoke')
        (root/'second-push.log').write_text(second)
        assert (remote/'kit-ssh-probe.conf').read_text() == 'second deployment\n'
        assert not list((remote/'.local/kit').glob('upload.*')), 'SSH staging directory leaked'
        print('Real SSH: remove and restore original', flush=True)
        run(kit, 'push', 'kit-smoke', '--remove')
        assert (remote/'kit-ssh-probe.conf').read_text() == original, 'original remote file not restored'
        assert not (remote/'.local/kit').exists(), 'managed deployment not removed'
        assert not (remote/'.zshrc').exists(), 'new remote dotfile remained after removal'
        assert run('ssh', 'kit-smoke', 'printf removed-exact', timeout=30) == 'removed-exact'
        assert not json.loads((source/'remotes.json').read_text()), 'removed server remained enrolled'
        passed = True
        print('PASS: real localhost SSH, tar staging, two pushes, rg, noninteractive command, removal and original restoration', flush=True)
    finally:
        # Close Kit's temporary SSH multiplexers before shutting down the daemon.
        sockets = sender/'.local/state/kit/ssh'
        for control in sockets.glob('*'):
            subprocess.run(['/usr/bin/ssh', '-F', str(root/'ssh_config'), '-S', str(control),
                            '-O', 'exit', 'kit-smoke'], stdin=subprocess.DEVNULL,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=5)
        if server is not None:
            try: os.killpg(server.pid, signal.SIGTERM)
            except ProcessLookupError: pass
            server.wait(timeout=5)
        if server_log is not None: server_log.close()
        if passed and not args.keep: shutil.rmtree(root)
        else: print(f'SSH sandbox retained at {root}', flush=True)


if __name__ == '__main__': main()
