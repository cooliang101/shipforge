"""Exercise the shipped remote executor against disposable local directories."""
import importlib.util, pathlib, tempfile, unittest, json, tarfile, io, hashlib, os
spec=importlib.util.spec_from_file_location('executor',pathlib.Path(__file__).parents[1]/'src/drivers/linux_ssh/inplace.py')
executor=importlib.util.module_from_spec(spec);spec.loader.exec_module(executor)

class InplaceContract(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory();self.root=pathlib.Path(self.temp.name)/'app';self.root.mkdir()
        self.identity={'project':'test','environment':'test','component':'app'}
        self.deployment='one'
    def tearDown(self): self.temp.cleanup()
    def runop(self,op,**args): return executor.execute(dict(root=str(self.root),identity=self.identity,operation=op,deployment=self.deployment,**args))
    def put(self,name,data):
        p=self.root/name;p.parent.mkdir(parents=True,exist_ok=True);p.write_bytes(data)
    def prepare(self,version,files,expected=None):
        manifest={'version':version}
        location=self.runop('begin',manifest=manifest,expected=expected)['upload']
        with tarfile.open(location,'w:gz') as tar:
            for name,content in {'manifest.json':json.dumps(manifest).encode(),**files}.items():
                item=tarfile.TarInfo(name);item.size=len(content);item.mode=0o755 if name=='app' else 0o644
                tar.addfile(item,io.BytesIO(content))
        path=pathlib.Path(location)
        self.runop('prepare',sha256=hashlib.sha256(path.read_bytes()).hexdigest(),size=path.stat().st_size)
    def publish(self,version,files,expected=None):
        self.prepare(version,files,expected);self.runop('archive');self.runop('publish');self.runop('phase',phase='stable')
    def test_existing_directory_and_runtime_data_are_preserved(self):
        for name in ['config.yaml','attachments/movie.mp4','logs/app.log']:self.put(name,b'live-data')
        self.put('app',b'old');self.publish('v1',{'app':b'new'})
        self.assertEqual((self.root/'app').read_bytes(),b'new')
        with tarfile.open(self.root/'.shipforge-deploy/previous.tar.gz') as tar:self.assertEqual(tar.getnames(),['app'])
        for name in ['config.yaml','attachments/movie.mp4','logs/app.log']:self.assertEqual((self.root/name).read_bytes(),b'live-data')
        self.assertFalse((self.root/'current').exists());self.assertFalse((self.root/'releases').exists())
    def test_initial_failure_recovers_original_application(self):
        self.put('app',b'old');self.prepare('v1',{'app':b'new'});self.runop('archive');self.runop('publish')
        self.runop('phase',phase='service-pending');self.runop('phase',phase='service-failed')
        self.assertTrue(self.runop('rollback',expected='v1',desired=None)['previousExists'])
        self.runop('phase',phase='stable');self.assertEqual((self.root/'app').read_bytes(),b'old')
        self.assertIsNone(self.runop('observe')['current'])
    def test_frontend_replaces_old_hashed_assets_and_rollback_removes_new(self):
        self.put('assets/old.js',b'old');self.put('index.html',b'old-index');self.put('uploads/user',b'keep')
        self.publish('v1',{'assets/new.js':b'new','index.html':b'new-index'})
        self.assertFalse((self.root/'assets/old.js').exists())
        self.runop('rollback',expected='v1',desired=None);self.runop('phase',phase='stable')
        self.assertEqual((self.root/'assets/old.js').read_bytes(),b'old');self.assertFalse((self.root/'assets/new.js').exists());self.assertTrue((self.root/'uploads/user').exists())
    def test_only_previous_version_is_retained(self):
        self.publish('v1',{'app':b'one'});self.deployment='two';self.publish('v2',{'app':b'two'},'v1')
        info=self.runop('observe');self.assertEqual(info['previous']['manifest']['version'],'v1')
        self.assertEqual([p.name for p in (self.root/'.shipforge-deploy').glob('*.tar.gz')],['previous.tar.gz'])
        self.runop('rollback',expected='v2',desired='v1');self.runop('phase',phase='stable')
        self.assertEqual((self.root/'app').read_bytes(),b'one')
    def test_unknown_service_result_blocks_recovery_and_new_deploy(self):
        self.prepare('v1',{'app':b'one'});self.runop('archive');self.runop('publish');self.runop('phase',phase='service-pending')
        for op,args in [('observe',{}),('rollback',{'expected':'v1','desired':None}),('begin',{'manifest':{'version':'v2'},'expected':'v1'})]:
            with self.assertRaises(ValueError):self.runop(op,**args)
    def test_drift_stops_before_application_changes(self):
        self.put('app',b'old');self.prepare('v1',{'app':b'one'});self.put('app',b'external')
        with self.assertRaises(ValueError):self.runop('archive')
        self.assertEqual((self.root/'app').read_bytes(),b'external')
    def test_corrupt_previous_archive_blocks_rollback(self):
        self.publish('v1',{'app':b'one'});(self.root/'.shipforge-deploy/previous.tar.gz').write_bytes(b'bad')
        with self.assertRaises(ValueError):self.runop('rollback',expected='v1',desired=None)
        self.assertEqual((self.root/'app').read_bytes(),b'one')
    def test_identity_mismatch_is_not_adopted(self):
        self.publish('v1',{'app':b'one'});self.identity['project']='another'
        with self.assertRaises(ValueError):self.runop('observe')
    def test_traversal_and_reserved_workspace_entries_rejected(self):
        for name in ['../escape','/outside','a/../b','.shipforge-deploy/state.json','a\\b']:
            with self.assertRaises(ValueError):executor.canonical(name)
    def test_upload_digest_failure_can_discard_without_touching_application(self):
        self.put('app',b'old');path=pathlib.Path(self.runop('begin',manifest={'version':'v1'},expected=None)['upload']);path.write_bytes(b'bad')
        with self.assertRaises(ValueError):self.runop('prepare',sha256='bad',size=3)
        self.runop('abort');self.assertEqual((self.root/'app').read_bytes(),b'old')
        self.assertFalse(path.exists())

    def test_stop_and_start_commands_resume_file_phase(self):
        self.put('app',b'old');self.prepare('v1',{'app':b'new'});self.runop('archive')
        for unused in range(2):self.runop('phase',phase='service-pending');self.runop('phase',phase='service-complete')
        self.runop('publish');self.runop('phase',phase='service-pending');self.runop('phase',phase='service-complete');self.runop('phase',phase='stable')
        self.assertEqual((self.root/'app').read_bytes(),b'new')
    def test_discard_is_scoped_and_cannot_remove_applied_application(self):
        self.put('app',b'old');self.prepare('v1',{'app':b'new'})
        self.deployment='other';self.runop('discard');self.assertTrue((self.root/'.shipforge-deploy/incoming.tar.gz').exists())
        self.deployment='one';self.runop('discard');self.assertFalse((self.root/'.shipforge-deploy/incoming.tar.gz').exists())
        self.publish('v2',{'app':b'new'});self.runop('discard');self.assertEqual((self.root/'app').read_bytes(),b'new')
    def test_archive_cannot_activate_a_different_release(self):
        self.put('app',b'old');self.prepare('v1',{'app':b'new'})
        with self.assertRaises(ValueError):self.runop('archive',version='another')
        self.assertFalse((self.root/'.shipforge-deploy/previous.tar.gz').exists())
    def test_linked_application_file_is_never_archived(self):
        outside=pathlib.Path(self.temp.name)/'outside';outside.write_bytes(b'data')
        os.link(outside,self.root/'app')
        with self.assertRaises(ValueError):self.prepare('v1',{'app':b'new'})
        self.assertEqual(outside.read_bytes(),b'data')
    def test_interrupted_file_operation_blocks_blind_retry(self):
        self.prepare('v1',{'app':b'new'});self.runop('archive')
        path=self.root/'.shipforge-deploy/state.json';state=json.loads(path.read_text());state['phase']='publishing';path.write_text(json.dumps(state))
        with self.assertRaises(ValueError):self.runop('rollback',expected=None,desired=None)
        with self.assertRaises(ValueError):self.runop('observe')
    def test_original_drift_after_failed_stop_is_not_overwritten(self):
        self.put('app',b'old');self.prepare('v1',{'app':b'new'});self.runop('archive');self.runop('phase',phase='service-pending');self.runop('phase',phase='service-failed');self.put('app',b'external')
        with self.assertRaises(ValueError):self.runop('rollback',expected=None,desired=None)
        self.assertEqual((self.root/'app').read_bytes(),b'external')

if __name__=='__main__':unittest.main()
