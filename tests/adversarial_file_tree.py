#!/usr/bin/env python3
"""Real PTY tree navigation and staged conflict resolution in isolated homes."""
import argparse
import sys
from pathlib import Path
sys.path.insert(0,str(Path(__file__).resolve().parent))
import run as h

MARKERS='header\n<<<<<<< this machine\nlocal\n=======\nrepo\n>>>>>>> repo\ntail\n'

def conflict_home(sb):
    m=h.setup(sb,{'one.conf':'header\nrepo\ntail\n'},['one.conf'])
    m.write('one.conf',MARKERS)
    return m

def sections_can_be_resolved_saved_and_undone(sb):
    m=conflict_home(sb)
    raw=h.package_dialogue(m,b'/one.conf\rm1v\r\r\r',command='files',size=(130,34))
    h.has(raw,'Resolve','Section')
    expected='header\nlocal\ntail\n'
    h.ok(m.read('one.conf')==expected and m.repo_read('home/one.conf')==expected)
    h.ok(any(p.read_text()==MARKERS for p in (m.state/'backups').rglob('one.conf')))
    m.kit('undo');h.ok(m.read('one.conf')==MARKERS,'undo lost original conflict file')

def cancel_resolution_or_review_writes_nothing(sb):
    m=conflict_home(sb)
    for keys in [b'/one.conf\rm1qq',b'/one.conf\rm1\r\rqq']:
        h.package_dialogue(m,keys,command='files')
        h.ok(m.read('one.conf')==MARKERS and m.repo_read('home/one.conf')=='header\nrepo\ntail\n')
        h.ok(not list((m.state/'backups').rglob('one.conf')))

def undecided_sections_cannot_be_queued(sb):
    m=conflict_home(sb)
    raw=h.package_dialogue(m,b'/one.conf\rm\rqq',command='files')
    h.has(raw,'file changes cancelled')
    h.ok(m.read('one.conf')==MARKERS)

def multiple_sections_allow_mixed_choices_and_keep_context(sb):
    m=conflict_home(sb)
    m.write('one.conf',MARKERS+'middle\n<<<<<<< this machine\nleft\n=======\nright\n>>>>>>> repo\nend\n')
    h.package_dialogue(m,b'/one.conf\rm12\r\r\r',command='files',size=(60,20))
    h.ok(m.read('one.conf')=='header\nlocal\ntail\nmiddle\nright\nend\n',m.read('one.conf'))

def malformed_markers_are_refused_without_writes(sb):
    m=conflict_home(sb);text=MARKERS.replace('>>>>>>> repo\n','');m.write('one.conf',text)
    raw=h.package_dialogue(m,b'/one.conf\rmq',command='files')
    h.has(raw,'file changes cancelled');h.ok(m.read('one.conf')==text)

def changed_during_resolution_is_never_overwritten(sb):
    m=conflict_home(sb)
    raw=h.package_dialogue(m,b'/one.conf\rm2\r\r\r',command='files',expected=1,
        on_ready=lambda:m.write('one.conf',MARKERS+'new edit\n'))
    h.has(raw,'changed while the manager was open')
    h.ok(m.read('one.conf')==MARKERS+'new edit\n')
    h.ok(m.repo_read('home/one.conf')=='header\nrepo\ntail\n')

def whole_local_choice_is_explicitly_reviewed_for_two_sided_changes(sb):
    m=h.setup(sb,{'one.conf':'base\n'},['one.conf'])
    m.write('one.conf','local\n');m.repo_write('home/one.conf','remote\n')
    h.package_dialogue(m,b'/one.conf\rs\rqq',command='files')
    h.ok(m.repo_read('home/one.conf')=='remote\n')
    h.package_dialogue(m,b'/one.conf\rs\r\r',command='files')
    h.ok(m.repo_read('home/one.conf')=='local\n')

def two_sided_edits_can_merge_before_sync_writes_markers(sb):
    m=h.setup(sb,{'one.conf':'head\nbase\ntail\n'},['one.conf'])
    m.write('one.conf','head\nlocal\ntail\n');m.repo_write('home/one.conf','head\nremote\ntail\n')
    h.package_dialogue(m,b'/one.conf\rm3\r\r\r',command='files')
    h.ok(m.read('one.conf')=='head\nlocal\nremote\ntail\n',m.read('one.conf'))
    h.ok(not list(m.state.glob('merge-*')),'temporary merge files left behind')

def folder_selection_includes_collapsed_files_and_cancel_is_safe(sb):
    files={f'.config/demo/file-{i:02}.conf':f'{i}\n' for i in range(20)}
    m=h.setup(sb,files,['.config/demo']);before=(m.src/'.kitignore').read_bytes()
    raw=h.package_dialogue(m,b'C p/file-00\r\tqq',command='files')
    h.has(raw,'20 selected','hidden','REVIEW ALL PENDING ACTIONS')
    h.ok((m.src/'.kitignore').read_bytes()==before)
    h.ok(len(h.json.loads(m.kit('files','--json').out))==20)

def only_tracked_files_are_in_the_tree(sb):
    m=h.setup(sb,{'.config/demo/one.conf':'base\n'},['.config/demo/one.conf'])
    m.write('.config/demo/not-tracked.conf','secret\n')
    raw=h.package_dialogue(m,b'Oq',command='files')
    h.has(raw,'one.conf');h.hasnt(raw,'not-tracked.conf')

def secret_files_never_enter_the_merge_editor(sb):
    m=h.setup(sb,{'one.conf':'repo\n'},['one.conf'])
    text=MARKERS.replace('local','-----BEGIN PRIVATE KEY-----\nprivatepayload')
    m.write('one.conf',text)
    raw=h.package_dialogue(m,b'/one.conf\rmq',command='files',size=(130,34))
    h.has(raw,'file changes cancelled');h.hasnt(raw,'privatepayload')
    h.ok(m.read('one.conf')==text)

TESTS=[v for k,v in list(globals().items()) if callable(v) and getattr(v,'__module__',None)==__name__ and k!='conflict_home']
if __name__=='__main__':
    p=argparse.ArgumentParser();p.add_argument('--kit',default='target/debug/kit');args=p.parse_args();h.KIT=h.resolve_kit(args.kit)
    failed=0
    for test in TESTS:
        name,status,elapsed,detail=h.run_one(test);print(f'{status} {name} ({elapsed:.2f}s)',flush=True)
        if detail:print(detail,flush=True)
        failed+=status=='FAIL'
    sys.exit(bool(failed))
