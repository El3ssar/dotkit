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
import time
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

    def test_uninstalled_mise_shim_cannot_hide_an_existing_external_tool(self):
        self.setup_package_fixture()
        shims = self.kit/'mise/shims'; shims.mkdir(parents=True)
        shim = shims/'eza'
        shim.write_text('#!/bin/sh\necho "No version is set for shim" >&2\nexit 1\n'); shim.chmod(0o755)
        self.run_script('setup.sh')
        self.assertNotIn('install cargo:eza', self.calls.read_text())
        self.assertIn('eza\tavailable\texternal:', (self.kit/'state/setup-results.tsv').read_text())

    def test_unsupported_sources_never_invoke_distro_managers_or_shell(self):
        self.setup_package_fixture()
        fakebin = self.root/'fakebin'; fakebin.mkdir()
        marker = self.root/'manager-called'
        for name in ('apt-get', 'dnf', 'yum', 'apk', 'pacman', 'zypper'):
            p = fakebin/name
            p.write_text(f'#!/bin/sh\necho called > "{marker}"\nexit 0\n'); p.chmod(0o755)
        self.env['PATH'] = str(fakebin) + os.pathsep + os.environ['PATH']
        (self.payload/'packages.list').write_text('probe\tkit-probe-missing\tsystem:probe\t-\ncustom\tkit-custom-missing\tcmd:touch bad\t-\n')
        self.run_script('setup.sh', expected=1)
        self.assertIn('probe\tblocked\tsystem:probe', (self.kit/'state/setup-results.tsv').read_text())
        self.assertFalse(marker.exists())
        self.assertFalse((self.kit/'bad').exists())

    def test_brew_success_without_a_working_binary_is_failure(self):
        self.setup_package_fixture()
        fakebin = self.root/'fakebin'; fakebin.mkdir()
        p = fakebin/'brew'; p.write_text('#!/bin/sh\nexit 0\n'); p.chmod(0o755)
        self.env['PATH'] = str(fakebin) + os.pathsep + os.environ['PATH']
        (self.payload/'packages.list').write_text('probe\tkit-probe-missing\tbrew:probe\t-\n')
        self.run_script('setup.sh', expected=1)
        self.assertIn('probe\tfailed\tbrew:probe', (self.kit/'state/setup-results.tsv').read_text())

    def test_macos_cask_uses_homebrew_ownership_without_requiring_a_cli(self):
        self.setup_package_fixture()
        fakebin = self.root/'fakebin'; fakebin.mkdir()
        uname = fakebin/'uname'
        uname.write_text('#!/bin/sh\ncase "$1" in -s) echo Darwin;; *) echo arm64;; esac\n'); uname.chmod(0o755)
        brew = fakebin/'brew'; calls = self.root/'brew-calls'
        brew.write_text(f'#!/bin/sh\necho "$*" >> "{calls}"\n[ "$1" = list ] && exit 0\nexit 99\n'); brew.chmod(0o755)
        self.env['PATH'] = str(fakebin) + os.pathsep + os.environ['PATH']
        (self.payload/'packages.list').write_text('gui\tkit-no-gui-command\tcask:example-app\t-\n')
        self.run_script('setup.sh')
        self.assertIn('gui\tinstalled\tcask:example-app', (self.kit/'state/setup-results.tsv').read_text())
        self.assertEqual(calls.read_text().splitlines(), ['list --cask example-app'])

    def test_setup_does_not_execute_shell_or_editor_configuration(self):
        self.setup_package_fixture()
        marker = self.root/'startup-ran'
        (self.home/'.zshrc').write_text(f'touch "{marker}"\n')
        self.run_script('setup.sh')
        self.assertFalse(marker.exists())
        self.assertNotIn('Lazy', self.calls.read_text())

    def test_external_parent_symlink_is_not_followed(self):
        self.setup_package_fixture()
        outside=self.root/'outside'; outside.mkdir()
        os.symlink(outside,self.home/'linked')
        (self.payload/'externals.list').write_text('linked/repo\thttps://example.invalid/repo\tmain\n')
        self.run_script('setup.sh',expected=1)
        self.assertEqual(list(outside.iterdir()),[])

    def test_shell_environment_preserves_existing_rust_roots(self):
        user_rust = self.root/'user-rust'; user_cargo = self.root/'user-cargo'
        (self.kit/'rustup/toolchains').mkdir(parents=True)
        (self.kit/'cargo').mkdir()
        r = subprocess.run(['bash', '-c', '. "$1"; printf "%s\\n%s\\n" "$RUSTUP_HOME" "$CARGO_HOME"',
                            'bash', str(ASSETS/'kit-env')],
                           env=dict(self.env, RUSTUP_HOME=str(user_rust), CARGO_HOME=str(user_cargo)),
                           capture_output=True, text=True)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(r.stdout.splitlines(), [str(user_rust), str(user_cargo)])

    def test_shell_environment_adds_missing_shims_when_kit_bin_is_already_present(self):
        env = dict(self.env, PATH=str(self.kit/'bin') + os.pathsep + os.environ['PATH'])
        r = subprocess.run(['bash', '-c', '. "$1"; printf "%s" "$PATH"', 'bash', str(ASSETS/'kit-env')],
                           env=env, capture_output=True, text=True)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn(str(self.kit/'mise/shims'), r.stdout.split(os.pathsep))
        self.assertEqual(r.stdout.split(os.pathsep).count(str(self.kit/'bin')), 1)

    def test_shell_environment_does_not_select_an_absent_private_toolchain(self):
        env = dict(self.env)
        env.pop('RUSTUP_HOME', None); env.pop('CARGO_HOME', None)
        r = subprocess.run(['bash', '-c', '. "$1"; printf "%s" "${RUSTUP_HOME:-unchanged}"',
                            'bash', str(ASSETS/'kit-env')], env=env, capture_output=True, text=True)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(r.stdout, 'unchanged')

    def test_failed_binary_is_not_reported_available_or_installed(self):
        self.setup_package_fixture()
        binary = self.home/'.cargo/bin/eza'
        binary.write_text('#!/bin/sh\n echo configuration-broken >&2\n exit 2\n')
        self.run_script('setup.sh', expected=1)
        report = (self.kit/'state/setup-results.tsv').read_text()
        self.assertIn('eza\tfailed\t', report)
        self.assertNotIn('eza\tavailable\t', report)

    def test_binary_with_only_short_help_can_be_used(self):
        self.setup_package_fixture()
        binary = self.home/'.cargo/bin/eza'
        binary.write_text('#!/bin/sh\ncase "$1" in -h) echo usage; exit 0;; *) echo "invalid option" >&2; exit 2;; esac\n')
        self.run_script('setup.sh')
        self.assertIn('eza\tavailable\t', (self.kit/'state/setup-results.tsv').read_text())

    def test_shell_hook_does_not_follow_rc_or_parent_symlinks(self):
        (self.payload/'mode').write_text('remote\n')
        outside = self.root/'outside'; outside.mkdir()
        original = outside/'rc'; original.write_text('original\n')
        os.symlink(original, self.home/'.bashrc')
        os.symlink(outside, self.home/'.fizsh')
        (outside/'.zshrc').write_text('fizsh-original\n')
        self.run_script('hook.sh', 'install', expected=1)
        self.assertEqual(original.read_text(), 'original\n')
        self.assertEqual((outside/'.zshrc').read_text(), 'fizsh-original\n')
        self.assertEqual(sorted(p.name for p in outside.iterdir()), ['.zshrc', 'rc'])
        self.run_script('hook.sh', 'remove', expected=1)
        self.assertEqual(original.read_text(), 'original\n')

    def test_shell_hook_does_not_follow_preexisting_temporary_symlinks(self):
        (self.payload/'mode').write_text('remote\n')
        outside = self.root/'outside'; outside.write_text('original\n')
        os.symlink(outside, self.home/'.bashrc.kit-tmp')
        self.run_script('hook.sh', 'install', expected=1)
        self.assertEqual(outside.read_text(), 'original\n')

    def test_incomplete_setup_lock_is_never_stolen(self):
        self.setup_package_fixture()
        lock = self.kit/'state/setup.lock'; lock.mkdir(parents=True)
        self.run_script('setup.sh', expected=1)
        self.assertTrue(lock.is_dir())
        self.assertFalse(self.calls.exists())

    def test_external_updates_fast_forward_and_preserve_local_commits(self):
        self.setup_package_fixture()
        origin = self.root/'origin'; origin.mkdir()
        env = dict(self.env, GIT_CONFIG_NOSYSTEM='1', GIT_CONFIG_GLOBAL=os.devnull,
                   GIT_AUTHOR_NAME='Kit Test', GIT_AUTHOR_EMAIL='kit@example.invalid',
                   GIT_COMMITTER_NAME='Kit Test', GIT_COMMITTER_EMAIL='kit@example.invalid')
        def git(path, *args):
            r = subprocess.run(['git', '-C', str(path), *args], env=env, capture_output=True, text=True)
            self.assertEqual(r.returncode, 0, r.stdout+r.stderr)
            return r.stdout.strip()
        git(origin, 'init', '-q', '-b', 'main')
        (origin/'file').write_text('first\n'); git(origin, 'add', 'file'); git(origin, 'commit', '-qm', 'first')
        self.env.update(env)
        (self.payload/'externals.list').write_text(f'repo\t{origin}\tmain\n')
        self.run_script('setup.sh')
        checkout = self.home/'repo'
        (origin/'file').write_text('second\n'); git(origin, 'commit', '-qam', 'second')
        self.run_script('setup.sh')
        self.assertEqual(git(checkout, 'rev-parse', 'HEAD'), git(origin, 'rev-parse', 'HEAD'))
        (checkout/'local').write_text('mine\n'); git(checkout, 'add', 'local'); git(checkout, 'commit', '-qm', 'local')
        local = git(checkout, 'rev-parse', 'HEAD')
        (origin/'file').write_text('third\n'); git(origin, 'commit', '-qam', 'third')
        self.run_script('setup.sh', expected=1)
        self.assertEqual(git(checkout, 'rev-parse', 'HEAD'), local)
        self.assertEqual((checkout/'local').read_text(), 'mine\n')

    def test_external_clone_is_bounded_and_noninteractive(self):
        self.setup_package_fixture()
        fakebin = self.root/'fakebin'; fakebin.mkdir()
        git = fakebin/'git'; git.write_text('#!/bin/sh\nsleep 10\n'); git.chmod(0o755)
        self.env['PATH'] = str(fakebin) + os.pathsep + os.environ['PATH']
        self.env['KIT_INSTALL_TIMEOUT'] = '1'
        (self.payload/'externals.list').write_text('repo\thttps://example.invalid/repo\tmain\n')
        started = time.monotonic()
        self.run_script('setup.sh', expected=1)
        self.assertLess(time.monotonic()-started, 5)
        self.assertIn('exceeded 1s', (self.kit/'state/external-install.log').read_text())

    def test_install_runner_logs_live_output_and_preserves_failure(self):
        log=self.root/'install.log'
        r=self.run_script('run-install.sh',str(log),'5','bash','-c','echo progress; echo error >&2; exit 7',expected=7)
        self.assertIn('progress',r.stdout)
        self.assertIn('error',log.read_text())
        self.assertFalse(list(self.root.glob('*.timeout.*')))

    def test_fast_installers_stop_their_watchdog_without_holding_output_open(self):
        fakebin = self.root/'timer-bin'; fakebin.mkdir()
        timer_pids = self.root/'timer-pids'
        timer = fakebin/'sleep'
        # Record only watchdog timers owned by this fixture. exec keeps the
        # identity recorded here when the actual sleep starts.
        timer.write_text(f'#!/bin/sh\necho $$ >> "{timer_pids}"\nexec /bin/sleep "$@"\n')
        timer.chmod(0o755)
        env = dict(self.env, PATH=str(fakebin)+os.pathsep+os.environ['PATH'])
        for i in range(40):
            r = subprocess.run(['bash', str(self.payload/'remote/run-install.sh'),
                                str(self.root/f'fast-{i}.log'), '10', 'true'],
                               env=env, capture_output=True, text=True, timeout=2)
            self.assertEqual(r.returncode, 0, r.stdout+r.stderr)
        pids = timer_pids.read_text().splitlines() if timer_pids.exists() else []
        def running():
            if not pids: return []
            r = subprocess.run(['ps', '-p', ','.join(pids), '-o', 'pid=,stat='], capture_output=True, text=True)
            return [line for line in r.stdout.splitlines() if 'Z' not in line.split()[-1]]
        deadline = time.monotonic()+2
        while running() and time.monotonic() < deadline: time.sleep(.02)
        self.assertEqual(running(), [], 'successful installers left watchdog timers running')

    def test_install_runner_times_out_and_stops_descendants(self):
        log=self.root/'install.log'; marker=self.root/'escaped'
        start=time.monotonic()
        r=self.run_script('run-install.sh',str(log),'1','bash','-c',f'(sleep 3; touch "{marker}") & wait',expected=124)
        self.assertLess(time.monotonic()-start,4,r.stderr)
        self.assertIn('exceeded 1s',log.read_text())
        time.sleep(3)
        self.assertFalse(marker.exists(),'timed out installer left a child running')

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
