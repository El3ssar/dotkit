#!/usr/bin/env python3
"""Filesystem/SSH-hook regression tests. Runs the real deployment scripts, no downloads."""
import os
import hashlib
from pathlib import Path
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import unittest

ASSETS = Path(__file__).resolve().parents[1] / 'assets/remote'


class RemoteTests(unittest.TestCase):
    def setUp(self):
        self.root = Path(tempfile.mkdtemp(prefix='dotkit-remote-'))
        self.home = self.root/'home'; self.home.mkdir()
        self.kit = self.home/'.local/kit'
        self.payload = self.kit/'payload'
        (self.payload/'home').mkdir(parents=True)
        (self.payload/'remote').mkdir()
        for p in ASSETS.glob('*.sh'): shutil.copy(p, self.payload/'remote'/p.name)
        self.env = dict(os.environ, HOME=str(self.home), KIT=str(self.kit))

    def tearDown(self): shutil.rmtree(self.root)

    def payload_file(self, rel, data='config\n', mode=0o644):
        p = self.payload/'home'/rel; p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(data); p.chmod(mode)
        self.manifest()
        return p

    def manifest(self):
        files = [p for p in (self.payload/'home').rglob('*') if p.is_file() or p.is_symlink()]
        (self.payload/'home.list').write_bytes(b''.join(str(p.relative_to(self.payload/'home')).encode()+b'\0' for p in files))

    def run_script(self, script, *args, expected=0):
        r = subprocess.run(['bash', str(self.payload/'remote'/script), *args], env=self.env, text=True, capture_output=True)
        self.assertEqual(r.returncode, expected, r.stdout + r.stderr)
        return r

    def test_all_dotfiles_and_original_restore(self):
        (self.home/'.gitconfig').write_text('original\n')
        self.payload_file('.gitconfig', 'new\n')
        self.payload_file('.zshrc', 'shell\n')
        self.payload_file('ordinary.conf', 'also-travels\n')
        self.run_script('deploy.sh')
        self.assertEqual((self.home/'ordinary.conf').read_text(), 'also-travels\n')
        self.run_script('deploy.sh', 'remove')
        self.assertEqual((self.home/'.gitconfig').read_text(), 'original\n')
        self.assertFalse((self.home/'.zshrc').exists())
        self.assertTrue(any((self.kit/'backups').rglob('.gitconfig')))

    def test_parent_symlink_cannot_escape_home(self):
        outside = self.root/'outside'; outside.mkdir()
        os.symlink(outside, self.home/'.config')
        self.payload_file('.config/app', 'must-not-escape\n')
        self.run_script('deploy.sh', expected=1)
        self.assertFalse((outside/'app').exists())

    def test_symlink_to_directory_is_replaced_not_followed(self):
        outside = self.root/'outside'; outside.mkdir()
        os.symlink(outside, self.home/'config')
        self.payload_file('config', 'replacement\n')
        self.run_script('deploy.sh')
        self.assertFalse((self.home/'config').is_symlink())
        self.assertEqual((self.home/'config').read_text(), 'replacement\n')
        self.assertEqual(list(outside.iterdir()), [])
        self.run_script('deploy.sh', 'remove')
        self.assertTrue((self.home/'config').is_symlink())

    def test_directory_conflict_does_not_block_other_files(self):
        (self.home/'config').mkdir()
        self.payload_file('config', 'conflict\n')
        self.payload_file('other', 'success\n')
        self.run_script('deploy.sh', expected=1)
        self.assertTrue((self.home/'config').is_dir())
        self.assertEqual((self.home/'other').read_text(), 'success\n')

    def test_existing_private_permissions_are_preserved(self):
        p = self.home/'config'; p.write_text('old\n'); p.chmod(0o600)
        self.payload_file('config', 'new\n', 0o644)
        self.run_script('deploy.sh')
        self.assertEqual(stat.S_IMODE(p.stat().st_mode), 0o600)

    def test_executable_bit_updates_even_without_content_change(self):
        p = self.home/'script'; p.write_text('same\n'); p.chmod(0o600)
        self.payload_file('script', 'same\n', 0o755)
        self.run_script('deploy.sh')
        self.assertEqual(stat.S_IMODE(p.stat().st_mode), 0o700)

    def test_hook_installs_on_new_account_and_is_reversible(self):
        (self.payload/'mode').write_text('remote\n')
        self.run_script('hook.sh', 'install')
        self.assertIn('# >>> kit:', (self.home/'.bashrc').read_text())
        self.assertIn('# >>> kit:', (self.home/'.profile').read_text())
        self.run_script('hook.sh', 'install')
        self.assertEqual((self.home/'.bashrc').read_text().count('# >>> kit:'), 1)
        self.run_script('hook.sh', 'remove')
        self.assertNotIn('# >>> kit:', (self.home/'.bashrc').read_text())

    def test_noninteractive_hook_does_not_hijack_commands(self):
        (self.payload/'mode').write_text('remote\n')
        self.run_script('hook.sh', 'install')
        p = self.kit/'bin/kit-shell'; p.parent.mkdir()
        p.write_text('#!/bin/sh\necho HIJACKED\n'); p.chmod(0o755)
        r = subprocess.run(['bash', '-c', '. "$HOME/.bashrc"; printf command-ok'], env=self.env, capture_output=True, text=True)
        self.assertEqual(r.stdout, 'command-ok')


    def setup_package_fixture(self):
        (self.payload/'mode').write_text('local\n')
        (self.payload/'remote/bootstrap.sh').write_text('#!/bin/sh\nexit 0\n')
        (self.payload/'mise.toml').write_text('[tools]\n')
        (self.payload/'externals.list').write_text('')
        (self.payload/'managed-rc.list').write_text('')
        (self.payload/'.stamp').write_text('test\n')
        (self.payload/'share').mkdir()
        for name in ('kit-env', 'kit-shell'): shutil.copy(ASSETS/name, self.payload/'remote'/name)
        for directory in ('bin', 'opt/zsh/bin'): (self.kit/directory).mkdir(parents=True, exist_ok=True)
        (self.kit/'opt/zsh/bin/zsh').write_text('#!/bin/sh\nexit 0\n')
        (self.kit/'opt/zsh/bin/zsh').chmod(0o755)
        self.calls = self.root/'mise-calls'
        mise = self.kit/'bin/mise'
        mise.write_text(f'#!/bin/sh\necho "$*" >> "{self.calls}"\nexit 0\n'); mise.chmod(0o755)
        external = self.home/'.cargo/bin'; external.mkdir(parents=True)
        (external/'eza').write_text('#!/bin/sh\necho eza-test\n'); (external/'eza').chmod(0o755)
        (self.payload/'packages.list').write_text('eza\teza\tcargo:eza@1\t-\n')

    def test_setup_always_preserves_external_cargo_binary(self):
        self.setup_package_fixture()
        self.run_script('setup.sh')
        self.assertNotIn('install cargo:eza', self.calls.read_text())
        self.assertIn('eza\tavailable\texternal:', (self.kit/'state/setup-results.tsv').read_text())
        self.env['KIT_UPGRADE_TOOLS'] = 'eza'
        self.run_script('setup.sh')
        self.assertNotIn('install cargo:eza@1', self.calls.read_text())

    def test_successful_package_manager_without_binary_is_failure(self):
        self.setup_package_fixture()
        fakebin = self.root/'fakebin'; fakebin.mkdir()
        # Never invoke the host's package manager, even when tests run as root.
        for name, body in [('id', 'echo 0'), ('apt-get', 'exit 0')]:
            p = fakebin/name; p.write_text('#!/bin/sh\n' + body + '\n'); p.chmod(0o755)
        self.env['PATH'] = str(fakebin) + os.pathsep + os.environ['PATH']
        (self.payload/'packages.list').write_text('probe\tkit-system-probe-missing\tsystem:probe\t-\n')
        self.run_script('setup.sh', expected=1)
        self.assertIn('probe\tfailed\tsystem:probe', (self.kit/'state/setup-results.tsv').read_text())

    def test_arch_install_does_not_refresh_repositories_without_upgrade(self):
        fakebin = self.root/'fakebin'; fakebin.mkdir()
        log = self.root/'pacman-calls'
        for name, body in [('id', 'echo 0'), ('pacman', f'printf "%s\\n" "$*" > "{log}"')]:
            p = fakebin/name; p.write_text('#!/bin/sh\n' + body + '\n'); p.chmod(0o755)
        setup = (ASSETS/'setup.sh').read_text()
        function = setup[setup.index('system_install() {'):setup.index('# Install shell independently')]
        r = subprocess.run([shutil.which('bash'), '-c', function + '\nsystem_install kitty'],
                           env=dict(self.env, PATH=str(fakebin)), text=True, capture_output=True)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(log.read_text().strip(), '-S --needed --noconfirm kitty')

    def test_binary_installer_does_not_install_package_managers(self):
        fakebin = self.root/'fakebin'; fakebin.mkdir()
        exe = self.root/'kit'; exe.write_text('#!/bin/sh\necho kit-test\n'); exe.chmod(0o755)
        archive = self.root/'release.tar.gz'
        with tarfile.open(archive, 'w:gz') as tar:
            tar.add(exe, arcname='kit')
        Path(str(archive)+'.sha256').write_text(hashlib.sha256(archive.read_bytes()).hexdigest()+'  release.tar.gz\n')
        curl = fakebin/'curl'
        curl.write_text(f'#!{sys.executable}\nimport sys,shutil\nshutil.copyfile({str(archive)!r} + (".sha256" if sys.argv[-1].endswith(".sha256") else ""), sys.argv[sys.argv.index("-o")+1])\n')
        curl.chmod(0o755)
        uname = fakebin/'uname'
        uname.write_text('#!/bin/sh\ncase "$1" in -s) echo Darwin;; *) echo arm64;; esac\n'); uname.chmod(0o755)
        marker = self.root/'brew-called'
        brew = fakebin/'brew'; brew.write_text(f'#!/bin/sh\necho called > "{marker}"\nexit 99\n'); brew.chmod(0o755)
        dest = self.root/'installed'
        r = subprocess.run(['sh', str(ASSETS.parents[1]/'install.sh')],
                           env=dict(self.env, KIT_BIN_DIR=str(dest), PATH=str(fakebin)+os.pathsep+os.environ['PATH']),
                           text=True, capture_output=True)
        self.assertEqual(r.returncode, 0, r.stdout+r.stderr)
        self.assertTrue(os.access(dest/'kit', os.X_OK))
        self.assertFalse(marker.exists(), 'installing kit also invoked Homebrew')


if __name__ == '__main__': unittest.main()
