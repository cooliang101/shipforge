"""Bounded in-place application publishing. No service or data administration."""
import os, sys, json, tarfile, stat, hashlib, uuid, shutil
from pathlib import Path

MAX_ENTRIES = 100000
META = '.shipforge-deploy'

class ApplicationError(ValueError):
    """Only explicit, fixed application diagnostics may cross the RPC boundary."""

def fail(message):
    raise ApplicationError(message)

def canonical(name):
    if not isinstance(name, str) or not name or len(name.encode()) > 3900:
        fail('Invalid application path')
    if name.startswith('/') or '\\' in name or any(ord(c) < 32 for c in name):
        fail('Invalid application path')
    parts = name.rstrip('/').split('/')
    if any(p in ('', '.', '..') for p in parts) or parts[0] == META:
        fail('Unsafe application path')
    return '/'.join(parts)

def regular(path):
    s = path.lstat()
    if not stat.S_ISREG(s.st_mode) or s.st_nlink != 1:
        fail('Application file is a link or special file')
    return s

def safe_path(root, name):
    parts = canonical(name).split('/')
    p = root
    for part in parts[:-1]:
        p = p / part
        if p.exists() or p.is_symlink():
            if not stat.S_ISDIR(p.lstat().st_mode): fail('Application parent is not a real directory')
    return root.joinpath(*parts)

def digest(path):
    regular(path)
    h = hashlib.sha256()
    with path.open('rb') as f:
        for block in iter(lambda: f.read(1024*1024), b''): h.update(block)
    return h.hexdigest()

def atomic_json(path, value):
    temp = path.with_name(path.name + '.new')
    with temp.open('x', encoding='utf-8') as f:
        os.chmod(temp, 0o600)
        json.dump(value, f, separators=(',', ':'))
        f.flush(); os.fsync(f.fileno())
    os.replace(temp, path)

def read_json(path):
    regular(path)
    if path.stat().st_size > 32*1024*1024: fail('Deployment record exceeds limit')
    return json.loads(path.read_text())

def snapshot(root, scopes):
    files, directories = {}, []
    def visit(name):
        p = safe_path(root, name)
        if not p.exists() and not p.is_symlink(): return
        s = p.lstat()
        if stat.S_ISDIR(s.st_mode):
            directories.append(name)
            for child in sorted(p.iterdir()): visit(name + '/' + child.name)
        else:
            regular(p)
            files[name] = {'sha256': digest(p), 'mode': stat.S_IMODE(s.st_mode), 'size': s.st_size}
        if len(files) + len(directories) > MAX_ENTRIES: fail('Application scope exceeds limit')
    for name in scopes: visit(canonical(name))
    return {'files': files, 'directories': directories}

def archive_payload(path, expected_manifest=None):
    entries, manifest = {}, None
    with tarfile.open(path, 'r:gz') as tar:
        for m in tar:
            name = canonical(m.name)
            if name in entries or (name == 'manifest.json' and manifest is not None): fail('Duplicate archive path')
            if name == 'manifest.json' and not m.isfile(): fail('Invalid manifest entry')
            if m.isdir():
                entries[name] = {'directory': True, 'mode': 0o755, 'size': 0}
            elif m.isfile() and not m.issparse():
                if m.mode & 0o7000: fail('Privileged application file mode')
                if name == 'manifest.json':
                    if m.size > 65536: fail('Oversized manifest')
                    manifest = json.load(tar.extractfile(m))
                else:
                    entries[name] = {'directory': False, 'mode': m.mode & 0o777, 'size': m.size}
            else: fail('Archive links and special entries are forbidden')
            if len(entries) > MAX_ENTRIES: fail('Archive entry limit')
    if not manifest or (expected_manifest is not None and manifest != expected_manifest): fail('Release manifest mismatch')
    for name in entries:
        parent = name.rpartition('/')[0]
        while parent:
            if parent in entries and not entries[parent]['directory']: fail('Archive path collision')
            parent = parent.rpartition('/')[0]
    if not entries: fail('Empty application archive')
    return entries, manifest

def write_previous(root, meta, snap):
    temp = meta / 'previous.new.tar.gz'
    with temp.open('xb') as f:
        os.chmod(temp, 0o600)
        with tarfile.open(fileobj=f, mode='w:gz') as tar:
            for name in snap['directories']:
                item = tarfile.TarInfo(name); item.type = tarfile.DIRTYPE; item.mode = 0o755
                tar.addfile(item)
            for name, info in snap['files'].items():
                p = safe_path(root, name)
                if digest(p) != info['sha256']: fail('Application changed while archiving')
                item = tarfile.TarInfo(name); item.size = info['size']; item.mode = info['mode']
                with p.open('rb') as source: tar.addfile(item, source)
        f.flush(); os.fsync(f.fileno())
    if snapshot(root, sorted({n.split('/')[0] for n in list(snap['files']) + snap['directories']})) != snap:
        fail('Application changed while archiving')
    os.replace(temp, meta / 'previous.tar.gz')

def copy_member(tar, member, root):
    name = canonical(member.name)
    target = safe_path(root, name)
    if member.isdir():
        if (target.exists() or target.is_symlink()) and not stat.S_ISDIR(target.lstat().st_mode): fail('Application directory collision')
        target.mkdir(parents=True, exist_ok=True)
        return
    target.parent.mkdir(parents=True, exist_ok=True)
    safe_path(root, name)
    if target.exists() or target.is_symlink(): regular(target)
    temp = target.with_name('.shipforge-write-' + uuid.uuid4().hex)
    try:
        with temp.open('xb') as out, tar.extractfile(member) as source:
            shutil.copyfileobj(source, out, 1024*1024)
            os.chmod(temp, member.mode & 0o777)
            out.flush(); os.fsync(out.fileno())
        os.replace(temp, target)
    finally:
        if temp.exists(): temp.unlink()

def remove_files(root, names):
    for name in names:
        path = safe_path(root, name)
        if path.exists() or path.is_symlink(): regular(path); path.unlink()

def observation(root, meta, state):
    if state is None: return {'current': None, 'previous': None, 'phase': 'unmanaged'}
    if state['phase'] not in ('stable', 'prepared', 'archived', 'files-applied', 'service-failed', 'publish-failed'):
        fail('Previous operation has no confirmed outcome; inspect before continuing')
    current = state.get('current')
    if state['phase']=='publish-failed':
        if snapshot(root,state['scopes'])!=state['failedSnapshot']: fail('Application changed after failed publishing')
    elif current and snapshot(root, current['scopes']) != current['snapshot']:
        fail('Application files changed outside this deployment')
    def public(release):
        return {k:release[k] for k in ('manifest','sha256','size')} if release else None
    return {'current': public(current), 'previous': public((state.get('previous') or {}).get('release')), 'phase': state['phase']}

def execute(request):
    root = Path(request['root'])
    if not root.is_absolute() or str(root) == root.anchor or '..' in root.parts: fail('Unsafe deployment root')
    for ancestor in [root] + list(root.parents):
        if ancestor.is_symlink(): fail('Deployment root must not follow symlinks')
    op = request['operation']
    meta = root / META
    if meta.is_symlink(): fail('Deployment workspace is a symlink')
    state_path = meta / 'state.json'
    state = read_json(state_path) if state_path.exists() else None
    identity = request['identity']
    if state and (state.get('schema') != 1 or state.get('identity') != identity): fail('Deployment workspace belongs to another target')
    if not state and meta.exists() and any(meta.iterdir()): fail('Unrecognized deployment workspace')
    if op == 'observe': return observation(root, meta, state)
    if op == 'preflight':
        if root.exists() and not root.is_dir(): fail('Target is not a directory')
        parent = root
        while not parent.exists(): parent = parent.parent
        if not os.access(parent, os.W_OK): fail('Deployment directory is not writable')
        observation(root, meta, state)
        for program in request.get('programs',[]):
            if '/' in program and not program.startswith('/'): continue
            if shutil.which(program) is None: fail('Configured service executable is unavailable')
        return {'free': shutil.disk_usage(parent).free}
    if op == 'begin':
        observation(root, meta, state)
        current = state.get('current') if state else None
        if (current['manifest']['version'] if current else None) != request['expected']: fail('Deployment version changed')
        if state and state['phase'] != 'stable': fail('Unfinished prepared operation requires inspection')
        root.mkdir(parents=True, exist_ok=True); meta.mkdir(mode=0o700, exist_ok=True)
        state = state or {'schema': 1, 'identity': identity, 'current': None, 'previous': None}
        state.update(phase='uploading', deployment=request['deployment'], candidate=request['manifest'], expected=request['expected'])
        atomic_json(state_path, state)
        return {'upload': str(meta / 'incoming.tar.gz')}
    if op == 'discard':
        if not state or state.get('deployment') != request.get('deployment') or state['phase'] not in ('uploading','prepared'): return {}
        op = 'abort'
    if not state: fail('Missing deployment state')
    if op not in ('rollback',) and state.get('deployment') != request.get('deployment'): fail('Deployment request changed')
    if op == 'prepare':
        if state['phase'] != 'uploading': fail('Invalid preparation state')
        incoming = meta / 'incoming.tar.gz'
        if incoming.stat().st_size != request['size'] or digest(incoming) != request['sha256']: fail('Upload digest mismatch')
        entries, manifest = archive_payload(incoming, state['candidate'])
        scopes = sorted({name.split('/')[0] for name in entries} | set((state.get('current') or {}).get('scopes', [])))
        snap = snapshot(root, scopes)
        total = sum(i['size'] for i in entries.values()) + sum(i['size'] for i in snap['files'].values())
        if shutil.disk_usage(root).free < total + 16*1024*1024: fail('Insufficient application publishing space')
        state.update(phase='prepared', scopes=scopes, before=snap, package={'sha256':request['sha256'],'size':request['size']})
        atomic_json(state_path, state)
        return {'scopeCount':len(scopes)}
    if op == 'archive':
        if request.get('version',state['candidate']['version']) != state['candidate']['version']: fail('Activation version changed')
        if state['phase'] != 'prepared': fail('Application is not prepared')
        if snapshot(root, state['scopes']) != state['before']: fail('Application changed after preparation')
        write_previous(root, meta, state['before'])
        state['previous'] = {'release':state.get('current'), 'snapshot':state['before'], 'scopes':state['scopes'], 'sha256':digest(meta/'previous.tar.gz'), 'size':(meta/'previous.tar.gz').stat().st_size}
        state['phase']='archived'; atomic_json(state_path,state)
        return {'previousExists': bool(state['before']['files'])}
    if op == 'publish':
        if state['phase'] != 'archived': fail('Previous application archive is not ready')
        if snapshot(root,state['scopes']) != state['before']: fail('Application changed before publishing')
        incoming=meta/'incoming.tar.gz'
        if digest(incoming)!=state['package']['sha256']: fail('Candidate archive changed')
        entries,manifest=archive_payload(incoming,state['candidate'])
        state['phase']='publishing'; atomic_json(state_path,state)
        with tarfile.open(incoming,'r:gz') as tar:
            for m in tar:
                if m.name!='manifest.json': copy_member(tar,m,root)
        remove_files(root,set(state['before']['files'])-set(entries))
        state['current']={'manifest':manifest,'scopes':state['scopes'],'snapshot':snapshot(root,state['scopes']),'sha256':state['package']['sha256'],'size':state['package']['size']}
        state['phase']='files-applied'; atomic_json(state_path,state)
        return {}
    if op == 'phase':
        allowed={'archived':{'service-pending'},'files-applied':{'service-pending','stable'},'service-pending':{'service-failed','service-complete'},'restored':{'service-pending','stable'}}
        new=request['phase']
        if new not in allowed.get(state['phase'],set()): fail('Invalid service phase')
        if new=='service-pending':state['resumePhase']=state['phase']
        if new=='service-complete':new=state.pop('resumePhase')
        state['phase']=new; atomic_json(state_path,state)
        if new=='stable' and (meta/'incoming.tar.gz').exists(): (meta/'incoming.tar.gz').unlink()
        return {}
    if op == 'rollback':
        if state['phase'] not in ('stable','files-applied','service-failed','archived','publish-failed'): fail('Operation outcome is unknown; automatic rollback refused')
        current=state.get('current')
        if (current['manifest']['version'] if current else None)!=request['expected']: fail('Rollback source changed')
        previous=state.get('previous')
        if previous is None: fail('Previous application archive is unavailable')
        prior=previous['release']
        if (prior['manifest']['version'] if prior else None)!=request['desired']: fail('Only previous application rollback is available')
        if state['phase']=='publish-failed':
            if snapshot(root,state['scopes'])!=state['failedSnapshot']:fail('Application changed after failed publishing')
        elif current and snapshot(root,current['scopes'])!=current['snapshot']: fail('Application changed before rollback')
        elif state['phase']=='archived' or (state['phase']=='service-failed' and state.get('resumePhase')=='archived'):
            if snapshot(root,state['scopes'])!=state['before']: fail('Original application changed before rollback')
        archive=meta/'previous.tar.gz'
        if digest(archive)!=previous['sha256']: fail('Previous archive changed')
        # Validate the entire archive before restoring any file.
        with tarfile.open(archive,'r:gz') as tar:
            for m in tar:
                name=canonical(m.name)
                if not (m.isdir() or m.isfile()) or m.issparse(): fail('Unsafe previous archive')
                if name not in previous['snapshot']['files'] and name not in previous['snapshot']['directories']: fail('Previous archive scope changed')
        state.update(phase='restoring',deployment=request['deployment']); atomic_json(state_path,state)
        with tarfile.open(archive,'r:gz') as tar:
            for m in tar: copy_member(tar,m,root)
        present=snapshot(root,previous['scopes'])
        remove_files(root,set(present['files'])-set(previous['snapshot']['files']))
        # Empty directories introduced by this release can be removed without deleting data.
        for name in sorted(set(present['directories'])-set(previous['snapshot']['directories']),key=len,reverse=True):
            p=safe_path(root,name)
            if p.is_dir() and not any(p.iterdir()): p.rmdir()
        if snapshot(root,previous['scopes'])!=previous['snapshot']: fail('Restored application verification failed')
        state.update(current=prior,phase='restored',previous=None); atomic_json(state_path,state)
        return {'previousExists':bool(previous['snapshot']['files'])}
    if op == 'abort':
        if state['phase'] not in ('uploading','prepared'): fail('Cannot discard an applied operation')
        if (meta/'incoming.tar.gz').exists(): regular(meta/'incoming.tar.gz'); (meta/'incoming.tar.gz').unlink()
        state['phase']='stable'; atomic_json(state_path,state)
        return {}
    fail('Unknown deployment operation')

if __name__ == '__main__':
    request=json.loads(sys.argv[1])
    try:
        print(json.dumps(execute(request),separators=(',',':')))
    except Exception as error:
        recoverable=False
        if request.get('operation')=='publish':
            try:
                root=Path(request['root']);path=root/META/'state.json';state=read_json(path)
                if state['phase']=='archived' and state['identity']==request['identity']: recoverable=True
                if state['phase']=='publishing' and state['identity']==request['identity']:
                    state.update(phase='publish-failed',failedSnapshot=snapshot(root,state['scopes']))
                    atomic_json(path,state);recoverable=True
            except Exception:pass
        print(json.dumps({'error':str(error) if isinstance(error,ApplicationError) else 'Remote application operation failed','recoverable':recoverable}))
        sys.exit(1)
