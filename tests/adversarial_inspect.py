#!/usr/bin/env python3
"""Offline inspection must never initialize state, run setup, or hide malformed input."""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import shutil
import sys

spec=importlib.util.spec_from_file_location('kit_harness',Path(__file__).with_name('run.py'))
h=importlib.util.module_from_spec(spec);spec.loader.exec_module(h)

def snapshot(root):
    result={}
    for p in [root,*sorted(root.rglob('*'))]:
        st=p.lstat();rel=str(p.relative_to(root))
        content=os.readlink(p) if p.is_symlink() else p.read_bytes() if p.is_file() else None
        result[rel]=(st.st_mode,st.st_mtime_ns,content)
    return result

def dry_run_first_machine_does_not_bootstrap_state_or_completions(sb):
    m=h.setup(sb,{'config':'same\n'},add=['config'])
    shutil.rmtree(m.state)
    m.repo_write('scripts/probe.sh','touch "$HOME/hook-ran"\n')
    m.repo_write('packages.json',json.dumps({'missing':{'mac':'aqua:o/missing@1.2.3','linux':'aqua:o/missing@1.2.3'}}))
    before=snapshot(m.home);head=m.head()
    result=m.kit('sync','--dry-run','--json')
    report=json.loads(result.out)
    h.ok(report['files']==[])
    h.ok(any(row['name']=='missing' and row['action'].startswith('install ') for row in report['packages']))
    h.ok(report['scripts'] and not m.exists('hook-ran'))
    h.ok(snapshot(m.home)==before,'dry-run changed HOME, index, or state')
    h.ok(m.head()==head)
    h.ok(not sb.calls('mise') and not sb.calls('ssh'),sb.calls())

def dry_run_requires_a_real_merge_base(sb):
    m=h.setup(sb,{'config':'base\n'},add=['config'])
    m.write('config','here\n');m.repo_write('home/config','repo\n')
    (m.state/'base/config').unlink()
    before=snapshot(m.home)
    report=json.loads(m.kit('sync','--dry-run','--json',code=1).out)
    h.ok(report['files'][0]['blocked'])
    h.has(report['files'][0]['action'],'no usable text merge base')
    h.ok(snapshot(m.home)==before)

def doctor_before_init_is_actionable_and_read_only(sb):
    m=sb.machine();before=snapshot(m.home)
    report=json.loads(m.kit('doctor','--json',code=1).out)
    h.ok(any(c['id']=='repository' and c['level']=='error' for c in report['checks']))
    h.ok(snapshot(m.home)==before)

def malformed_metadata_is_structured_and_never_replaced(sb):
    m=h.setup(sb)
    for path,data in [('rules.json','[{"path":"config","on":["linux"],"regex":[["(","x"]]}]'),('packages.json','{"broken": 42}'),('dirs.json','["../escape"]')]:
        previous=(m.src/path).read_bytes();m.repo_write(path,data)
        before=snapshot(m.home)
        for command in [('doctor','--json'),('sync','--dry-run','--json')]:
            report=json.loads(m.kit(*command,code=1).out)
            h.ok(any(c['level']=='error' for c in report['checks']))
            h.ok(snapshot(m.home)==before,command)
        (m.src/path).write_bytes(previous)

def malformed_sync_state_is_structured(sb):
    m=h.setup(sb);(m.state/'synced.json').write_text('{broken')
    before=snapshot(m.home)
    for command in [('doctor','--json'),('sync','--dry-run','--json')]:
        report=json.loads(m.kit(*command,code=1).out)
        h.ok(any(c['level']=='error' for c in report['checks']))
        h.ok(snapshot(m.home)==before)

def linux_casks_are_blocked_by_preview(sb):
    if h.PLAT!='linux':return
    m=h.setup(sb);m.repo_write('packages.json',json.dumps({'app':{'linux':'cask:app'}}))
    report=json.loads(m.kit('sync','--dry-run','--json',code=1).out)
    h.ok(any(r['action']=='blocked: macOS-only cask' for r in report['packages']))

def preview_modes_cannot_approve_code_or_mutate_silently(sb):
    m=h.setup(sb);before=snapshot(m.home)
    for command in [('sync','--json'),('sync','--dry-run','--trust')]:
        m.kit(*command,code=2)
        h.ok(snapshot(m.home)==before)

def preview_cannot_execute_git_fsmonitor_or_clean_filters(sb):
    m=h.setup(sb,{'config':'base\n'},add=['config'])
    m.git('add','-A');m.git('commit','-qm','baseline')
    monitor=m.write('monitor.sh','#!/bin/sh\ntouch "$HOME/monitor-ran"\n');monitor.chmod(0o755)
    cleaner=m.write('cleaner.sh','#!/bin/sh\ntouch "$HOME/filter-ran"\ncat\n');cleaner.chmod(0o755)
    m.git('config','core.fsmonitor',str(monitor));m.git('config','filter.probe.clean',str(cleaner))
    m.repo_write('.gitattributes','home/config filter=probe\n')
    m.repo_write('home/config','changed\n')
    before=snapshot(m.home)
    report=json.loads(m.kit('sync','--dry-run','--json').out)
    h.ok(not m.exists('monitor-ran') and not m.exists('filter-ran'))
    h.ok(report['saved'],'preview should expose saved changes')
    h.ok(snapshot(m.home)==before)


def invalid_legacy_recipe_is_a_json_diagnostic(sb):
    m=h.setup(sb);m.repo_write('packages.json',json.dumps({'bad':{'mac':'system:--bad','linux':'system:--bad'}}))
    for command in [('doctor','--json'),('sync','--dry-run','--json')]:
        report=json.loads(m.kit(*command,code=1).out)
        h.ok(any(c['level']=='error' for c in report['checks']))

def metadata_symlinks_to_special_files_are_rejected_without_reading(sb):
    if not hasattr(os,'mkfifo'):return
    m=h.setup(sb);fifo=sb.root/'fifo';os.mkfifo(fifo)
    (m.src/'packages.json').unlink();(m.src/'packages.json').symlink_to(fifo)
    for command in [('doctor','--json'),('sync','--dry-run','--json')]:
        report=json.loads(m.kit(*command,code=1).out)
        h.ok(any(c['level']=='error' and 'regular JSON' in c['message'] for c in report['checks']))

TESTS=[metadata_symlinks_to_special_files_are_rejected_without_reading,preview_cannot_execute_git_fsmonitor_or_clean_filters,invalid_legacy_recipe_is_a_json_diagnostic,dry_run_first_machine_does_not_bootstrap_state_or_completions,dry_run_requires_a_real_merge_base,
       doctor_before_init_is_actionable_and_read_only,malformed_metadata_is_structured_and_never_replaced,
       malformed_sync_state_is_structured,linux_casks_are_blocked_by_preview,preview_modes_cannot_approve_code_or_mutate_silently]
if __name__=='__main__':
    ap=argparse.ArgumentParser();ap.add_argument('--kit',default='target/debug/kit');args=ap.parse_args()
    h.KIT=[str(Path(args.kit).resolve())];failed=0
    for test in TESTS:
        name,status,elapsed,detail=h.run_one(test);print(f'{status} {name} ({elapsed:.2f}s)')
        if detail:print(detail)
        failed+=status=='FAIL'
    sys.exit(bool(failed))
