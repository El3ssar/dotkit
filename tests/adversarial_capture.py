#!/usr/bin/env python3
"""Capture the installed source, never guess ownership or test source builds by installing."""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import shlex
import sys

spec=importlib.util.spec_from_file_location('kit_harness',Path(__file__).with_name('run.py'))
h=importlib.util.module_from_spec(spec);spec.loader.exec_module(h)

def provider(sb, entries, extra=''):
    sb.stub('curl',code=22);sb.stub('wget',code=1)
    registry=json.dumps(entries)
    lock=shlex.quote(sys.executable)+' '+shlex.quote(str(h.HERE/'mise_lock_fixture.py'))
    sb.stub('mise',script=f'''case "$1" in
registry) printf '%s\\n' {shlex.quote(registry)};;
lock) {lock};;
{extra}
ls) echo '{{}}';;
latest|use|install) exit 91;;
esac
''')

def entry(name='probe',backends=None,binary=None):
    return {'short':name,'backends':backends or [f'cargo:{name}',f'aqua:owner/{name}'],'bins':[binary or name]}

def cargo(m,name='probe',binary='probe',version='1.0.0'):
    h.installed_cargo_fixture(m,name,binary,version)
    return m.home/'.cargo/bin'/binary

def brew(sb,m,name='probe',binary='probe',version='1.0.0'):
    root=sb.root/'Cellar'/name/version
    (root/'bin').mkdir(parents=True,exist_ok=True)
    path=root/'bin'/binary;path.write_text('#!/bin/sh\ntouch "$HOME/should-not-run"\nexit 0\n');path.chmod(0o755)
    (root/'INSTALL_RECEIPT.json').write_text(json.dumps({'source':{'tap':'homebrew/core','spec':'stable'},'used_options':[]}))
    active=sb.fakebin/binary;active.unlink(missing_ok=True);active.symlink_to(path)
    sb.stub('brew',script=f'case "$1" in --prefix) echo {shlex.quote(str(sb.root))};; *) exit 92;; esac\n')
    return path

def no_installs(sb):
    h.ok(not any(' use ' in c or ' install ' in c or ' upgrade ' in c or ' latest ' in c for c in sb.calls()),sb.calls())

def cargo_capture_preserves_actual_version_without_installing(sb):
    m=h.setup(sb);binary=cargo(m);provider(sb,[entry()])
    before=binary.read_bytes()
    r=m.kit('add','probe');p=h.pkgs(m)['probe']
    h.ok(p[h.PLAT]=='cargo:probe@1.0.0' and p[h.OTHER]=='aqua:owner/probe@1.0.0',p)
    h.ok(p['observed'][h.PLAT]['version']=='1.0.0' and p['bin']=='probe',p)
    h.has(r.out,'Cargo 1.0.0','nothing installed or upgraded')
    h.ok(binary.read_bytes()==before and not m.exists('should-not-run'))
    no_installs(sb)
    old=(m.src/'packages.json').read_bytes()
    h.has(m.kit('add','probe').out,'already tracked')
    h.ok((m.src/'packages.json').read_bytes()==old)

def brew_capture_records_receipt_and_verified_counterpart(sb):
    m=h.setup(sb);brew(sb,m);provider(sb,[entry()])
    m.kit('add','probe');p=h.pkgs(m)['probe']
    h.ok(p[h.PLAT]=='brew:probe' and p[h.OTHER]=='aqua:owner/probe@1.0.0',p)
    h.ok(p['observed'][h.PLAT]['provider']=='Homebrew',p)
    h.ok(not m.exists('should-not-run'));no_installs(sb)

def active_path_decides_between_duplicate_brew_and_cargo(sb):
    m=h.setup(sb);cargo(m);brew(sb,m);provider(sb,[entry()])
    m.kit('add','probe');h.ok(h.pkgs(m)['probe'][h.PLAT]=='brew:probe')
    m.envvars['PATH']=str(m.home/'.cargo/bin')+os.pathsep+m.envvars['PATH']
    m.kit('add','probe');h.ok(h.pkgs(m)['probe'][h.PLAT]=='cargo:probe@1.0.0')
    no_installs(sb)

def unowned_executable_is_not_mistaken_for_an_installed_package(sb):
    m=h.setup(sb);sb.stub('probe',code=0);provider(sb,[entry()])
    before=(m.src/'packages.json').read_bytes()
    r=m.kit('add','probe',code=1)
    h.has(r.err,'could not prove an installed','nothing recorded')
    h.ok((m.src/'packages.json').read_bytes()==before);no_installs(sb)

def missing_binary_target_leaves_manifest_unchanged(sb):
    m=h.setup(sb);cargo(m);provider(sb,[entry()]);before=(m.src/'packages.json').read_bytes()
    target='linux-arm64' if h.OTHER=='linux' else 'macos-arm64'
    r=m.kit('add','probe',code=1,env={'KIT_TEST_MISSING_PLATFORM':target})
    h.has(r.err,'no verified','Package list unchanged')
    h.ok((m.src/'packages.json').read_bytes()==before);no_installs(sb)

def cargo_on_one_os_is_not_proof_of_a_source_build_on_the_other(sb):
    m=h.setup(sb);cargo(m);provider(sb,[entry(backends=['cargo:probe'])])
    r=m.kit('add','probe',code=1);h.has(r.err,'no verified')
    h.ok(h.pkgs(m)=={});no_installs(sb)

def multiple_captures_are_saved_only_if_every_counterpart_succeeds(sb):
    m=h.setup(sb);cargo(m);cargo(m,'second','second')
    # Cargo receipts contain both installations.
    receipt=m.home/'.cargo/.crates2.json';d=json.loads(receipt.read_text())
    d['installs']['probe 1.0.0 (registry+https://github.com/rust-lang/crates.io-index)']={'bins':['probe']};receipt.write_text(json.dumps(d))
    provider(sb,[entry(),entry('second',['cargo:second'])])
    m.kit('add','probe','second',code=1)
    h.ok(h.pkgs(m)=={},'first item was saved before second failed');no_installs(sb)

def custom_cargo_features_and_git_sources_are_not_silently_lost(sb):
    m=h.setup(sb);cargo(m);provider(sb,[entry()]);receipt=m.home/'.cargo/.crates2.json'
    d=json.loads(receipt.read_text());next(iter(d['installs'].values()))['features']=['custom'];receipt.write_text(json.dumps(d))
    h.has(m.kit('add','probe',code=1).err,'custom Cargo build features')
    h.installed_cargo_fixture(m,'probe','probe',source='git+https://example.invalid/repo#abc123')
    h.has(m.kit('add','probe',code=1).err,'Git/private registry')
    h.ok(h.pkgs(m)=={});no_installs(sb)

def cargo_command_alias_is_resolved_from_real_receipt(sb):
    m=h.setup(sb);cargo(m,'ripgrep','rg','15.2.0');provider(sb,[entry('ripgrep',['aqua:BurntSushi/ripgrep','cargo:ripgrep'],'rg')])
    m.kit('add','ripgrep');p=h.pkgs(m)['ripgrep']
    h.ok(p[h.PLAT]=='cargo:ripgrep@15.2.0' and p['bin']=='rg',p);no_installs(sb)

def mise_fixture(sb,m,active=True,options=None):
    data=Path(m.envvars['XDG_DATA_HOME'])/'mise';root=data/'installs/probe/1.0.0';(root/'bin').mkdir(parents=True)
    binary=root/'bin/probe';binary.write_text('#!/bin/sh\ntouch "$HOME/should-not-run"\n');binary.chmod(0o755)
    shims=data/'shims';shims.mkdir();(shims/'probe').write_text('#!/bin/sh\ntouch "$HOME/should-not-run"\n');(shims/'probe').chmod(0o755)
    m.envvars['PATH']=str(shims)+os.pathsep+m.envvars['PATH']
    record=json.dumps({'probe':[{'active':active,'installed':True,'version':'1.0.0','install_path':str(root)}]})
    info=json.dumps({'backend':'aqua:owner/probe','tool_options':options or {'os':None,'install_env':{},'lazy_bins':[]}})
    provider(sb,[entry()],extra=f'''ls) printf '%s\\n' {shlex.quote(record)};;
which) echo {shlex.quote(str(binary))};;
tool) printf '%s\\n' {shlex.quote(info)};;''')

def mise_capture_proves_active_install_path_and_preserves_backend(sb):
    m=h.setup(sb);mise_fixture(sb,m)
    m.kit('add','probe');p=h.pkgs(m)['probe']
    h.ok(p[h.PLAT]=='aqua:owner/probe@1.0.0' and p[h.OTHER]==p[h.PLAT],p)
    h.ok(not m.exists('should-not-run'));no_installs(sb)

def inactive_mise_install_is_not_captured(sb):
    m=h.setup(sb);mise_fixture(sb,m,active=False)
    m.kit('add','probe',code=1);h.ok(h.pkgs(m)=={});no_installs(sb)

def custom_mise_install_options_are_not_silently_lost(sb):
    m=h.setup(sb);mise_fixture(sb,m,options={'exe':'special'})
    h.has(m.kit('add','probe',code=1).err,'custom mise install options')
    h.ok(h.pkgs(m)=={});no_installs(sb)

def special_file_receipts_never_block_capture(sb):
    m=h.setup(sb);binary=m.write('.cargo/bin/probe','#!/bin/sh\nexit 0\n');binary.chmod(0o755)
    os.mkfifo(m.home/'.cargo/.crates2.json');provider(sb,[entry()])
    m.kit('add','probe',code=1,timeout=5);h.ok(h.pkgs(m)=={})

def bare_add_never_bootstraps_or_provisions_linux_environment(sb):
    m=h.setup(sb);sb.unstub('mise');sb.unstub('brew');cargo(m);(m.home/'.local/kit/bin/mise').unlink()
    before=(m.src/'packages.json').read_bytes();m.kit('add','probe',code=1)
    h.ok((m.src/'packages.json').read_bytes()==before)
    h.ok(not m.exists('.local/kit/bin/mise') and not (m.state/'bootstrap.sh').exists())
    no_installs(sb)

def crate_release_metadata_can_supply_a_verified_upstream_counterpart(sb):
    m=h.setup(sb);cargo(m);provider(sb,[entry(backends=['cargo:probe'])])
    metadata={'version':{'crate':'probe','num':'1.0.0','bin_names':['probe'],'repository':'https://github.com/owner/probe'}}
    sb.stub('curl',script="printf '%s\\n' "+shlex.quote(json.dumps(metadata))+"\n")
    m.kit('add','probe');p=h.pkgs(m)['probe']
    h.ok(p[h.PLAT]=='cargo:probe@1.0.0' and p[h.OTHER]=='github:owner/probe@1.0.0',p)
    no_installs(sb)

def unrelated_catalog_identity_or_crate_metadata_cannot_replace_the_program(sb):
    m=h.setup(sb);cargo(m);provider(sb,[entry(backends=['cargo:some-other-crate','aqua:wrong/tool'])])
    m.kit('add','probe',code=1);h.ok(h.pkgs(m)=={})
    metadata={'version':{'crate':'probe','num':'9.0.0','bin_names':['probe'],'repository':'https://github.com/wrong/tool'}}
    sb.stub('curl',script="printf '%s\\n' "+shlex.quote(json.dumps(metadata))+"\n")
    m.kit('add','probe',code=1);h.ok(h.pkgs(m)=={});no_installs(sb)

def command_alias_reuses_the_canonical_package_entry(sb):
    m=h.setup(sb);cargo(m,'ripgrep','rg','15.2.0');provider(sb,[entry('ripgrep',['cargo:ripgrep','aqua:owner/ripgrep'],'rg')])
    m.kit('add','rg');m.kit('add','ripgrep')
    h.ok(list(h.pkgs(m))==['ripgrep'],h.pkgs(m));no_installs(sb)

def capture_replaces_stale_linux_binary_alias_and_keeps_same_version(sb):
    m=h.setup(sb);cargo(m);provider(sb,[entry()])
    m.repo_write('packages.json',json.dumps({'probe':{'mac':'cargo:probe@0.9.0','linux':'aqua:owner/probe@0.9.0','bin':'probe','linux_bin':'old-command'}}))
    m.kit('add','probe');p=h.pkgs(m)['probe']
    h.ok('linux_bin' not in p and p[h.PLAT]=='cargo:probe@1.0.0' and p[h.OTHER]=='aqua:owner/probe@1.0.0',p)
    no_installs(sb)

def manifest_changes_during_checks_are_never_overwritten(sb):
    m=h.setup(sb);cargo(m);provider(sb,[entry()])
    script=(sb.stubcfg/'mise.sh').read_text()
    change=json.dumps({'edited-during-check':{h.PLAT:'cargo:edited@1.0.0'}})
    (sb.stubcfg/'mise.sh').write_text('if [ "$1" = lock ]; then printf \'%s\\n\' '+shlex.quote(change)+' > "$KIT_SOURCE/packages.json"; fi\n'+script)
    r=m.kit('add','probe',code=1)
    h.has(r.err,'changed while checking counterparts')
    h.ok(h.pkgs(m)==json.loads(change));no_installs(sb)


def captured_brew_update_does_not_switch_to_duplicate_cargo(sb):
    m=h.setup(sb);cargo(m);brew(sb,m);provider(sb,[entry()])
    m.kit('add','probe')
    sb.stub('brew',script='case "$1" in --prefix) echo '+shlex.quote(str(sb.root))+';; outdated) echo \'{"formulae":[]}\';; *) exit 92;; esac\n')
    r=m.kit('packages','update','--check','probe')
    h.has(r.out,'up to date (Homebrew)')
    h.ok(not any('cargo search' in c or 'cargo install' in c for c in sb.calls()),sb.calls())


def changed_ownership_requires_recapture_before_update(sb):
    m=h.setup(sb);cargo(m);provider(sb,[entry()]);m.kit('add','probe')
    brew(sb,m)
    r=m.kit('packages','update','probe',code=1)
    h.has(r.err,'installed source changed since capture')
    no_installs(sb)


TESTS=[v for k,v in list(globals().items()) if callable(v) and k not in ('provider','entry','cargo','brew','no_installs','mise_fixture') and not k.startswith('_') and getattr(v,'__module__',None)==__name__]
if __name__=='__main__':
    parser=argparse.ArgumentParser();parser.add_argument('--kit',default='target/debug/kit');args=parser.parse_args()
    h.KIT=[str(Path(args.kit).resolve())];failures=0
    for test in TESTS:
        name,status,elapsed,detail=h.run_one(test);print(f'{status} {name} ({elapsed:.2f}s)',flush=True)
        if detail:print(detail,flush=True)
        failures+=status=='FAIL'
    sys.exit(bool(failures))
