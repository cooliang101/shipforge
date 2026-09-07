//! Real PM2 only in the runner's attested, disposable Node/OpenSSH containers.
use super::*;
use shipforge::config::{ServiceCheck, ServiceConfig};

fn setup(destinations: &[DestinationKey]) -> ProjectSetup {
    let mut setup = project_setup(destinations);
    for (name, target) in &mut setup.environments.get_mut("acceptance").unwrap().components {
        target.destination = destinations[0].clone();
        target.health = None;
        target.after.clear();
        target.service = Some(ServiceConfig {
            start: vec![vec![
                "node".into(),
                "service.cjs".into(),
                "start".into(),
                name.to_string(),
            ]],
            update: vec![vec![
                "node".into(),
                "service.cjs".into(),
                "update".into(),
                name.to_string(),
            ]],
            restore: vec![vec![
                "node".into(),
                "service.cjs".into(),
                "restore".into(),
                name.to_string(),
            ]],
            stop: vec![vec![
                "node".into(),
                "service.cjs".into(),
                "stop".into(),
                name.to_string(),
            ]],
            check: Some(ServiceCheck::Command {
                argv: vec!["node".into(), "check.cjs".into()],
            }),
        });
    }
    setup
}

const SERVICE: &str = r"
const fs = require('node:fs');
const path = require('node:path');
const cp = require('node:child_process');
if (process.cwd() !== __dirname) throw Error('wrong version cwd');
const [phase, name] = process.argv.slice(2);
const id = 'shipforge-fixture-' + name;
const run = args => cp.execFileSync('/usr/local/bin/pm2', args, {encoding:'utf8', timeout:20000});
// The first PM2 invocation prints daemon bootstrap output, not just JSON.
run(['ping']);
const exists = JSON.parse(run(['jlist'])).some(p => p.name === id);
if (exists) run(['delete', id]);
if (phase === 'stop') process.exit(0);
// Deleting before starting makes the absolute script path unambiguous. A bare
// pm2 restart <name> could preserve a previous version's pm_exec_path.
run(['start', path.join(__dirname, 'worker.cjs'), '--name', id, '--cwd', __dirname, '--no-autorestart']);
if (fs.readFileSync('mode.txt', 'utf8') === 'command-fail') process.exit(7);
";

const WORKER: &str = r"
const fs = require('node:fs');
if (fs.readFileSync('mode.txt', 'utf8') !== 'unhealthy') {
  fs.writeFileSync('ready.json', JSON.stringify({pid:process.pid, directory:__dirname}));
}
setInterval(() => {}, 1000);
";

const CHECK: &str = r"
const fs = require('node:fs');
const ready = JSON.parse(fs.readFileSync('ready.json', 'utf8'));
process.kill(ready.pid, 0);
if (ready.directory !== __dirname || fs.readlinkSync('/proc/' + ready.pid + '/cwd') !== __dirname) process.exit(1);
";

fn payload(fixture: &Fixture, name: &str, mode: &str) {
    fixture.payload(name, "healthy");
    let root = fixture.project.join(name);
    for (file, text) in [
        ("service.cjs", SERVICE),
        ("worker.cjs", WORKER),
        ("check.cjs", CHECK),
        ("mode.txt", mode),
    ] {
        std::fs::write(root.join(file), text).unwrap();
    }
}

async fn verify_process(fixture: &Fixture, name: &str, release: Option<&ReleaseRef>) {
    let bytes = fixture
        .remote("frontend", "/usr/local/bin/pm2", &["jlist"])
        .await;
    let entries: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
    let id = format!("shipforge-fixture-{name}");
    let found = entries
        .iter()
        .find(|entry| entry["name"].as_str() == Some(&id));
    if let Some(release) = release {
        let found = found.expect("fixture process exists");
        let directory = format!(
            "/srv/shipforge-acceptance/{name}/releases/{}",
            release.version
        );
        assert_eq!(found["pm2_env"]["pm_cwd"], directory);
        assert_eq!(
            found["pm2_env"]["pm_exec_path"],
            format!("{directory}/worker.cjs")
        );
        assert_eq!(found["pm2_env"]["status"], "online");
        let readiness = fixture
            .remote(name, "cat", &[&format!("{directory}/ready.json")])
            .await;
        let readiness: serde_json::Value = serde_json::from_slice(&readiness).unwrap();
        assert_eq!(readiness["directory"], directory);
        assert_eq!(readiness["pid"], found["pid"]);
    } else {
        assert!(found.is_none(), "undeployed fixture process must be absent");
    }
}

#[tokio::test]
#[ignore = "requires ServiceCommands runner with disposable Node/PM2/OpenSSH fixtures"]
async fn real_pm2_versions_failure_recovery_and_component_isolation() {
    tokio::time::timeout(Duration::from_secs(600), run())
        .await
        .expect("PM2 acceptance ten-minute deadline");
}

async fn run() {
    let fixture = Fixture::with_setup(setup).await;
    payload(&fixture, "frontend", "healthy");
    payload(&fixture, "backend", "healthy");
    let first = fixture.deploy(&["frontend"], false).await;
    assert_eq!(
        first.deployment.state,
        DeploymentState::Succeeded,
        "{first:#?}"
    );
    let first_versions = fixture.observed().await;
    verify_process(
        &fixture,
        "frontend",
        first_versions[&component("frontend")].as_ref(),
    )
    .await;
    verify_process(&fixture, "backend", None).await;
    assert!(first_versions[&component("backend")].is_none());
    let joint = fixture.deploy(&["frontend", "backend"], false).await;
    assert_eq!(joint.deployment.state, DeploymentState::Succeeded);
    let stable = fixture.observed().await;
    for name in ["frontend", "backend"] {
        verify_process(&fixture, name, stable[&component(name)].as_ref()).await;
    }
    for mode in ["unhealthy", "command-fail"] {
        payload(&fixture, "backend", mode);
        let failed = fixture.deploy(&["backend"], false).await;
        assert_eq!(failed.deployment.state, DeploymentState::Failed);
        assert!(failed.compensation_failures.is_empty());
        assert_eq!(fixture.observed().await, stable);
        for name in ["frontend", "backend"] {
            verify_process(&fixture, name, stable[&component(name)].as_ref()).await;
        }
    }
    payload(&fixture, "backend", "healthy");
    let newer = fixture.deploy(&["frontend", "backend"], false).await;
    assert_eq!(newer.deployment.state, DeploymentState::Succeeded);
    fixture.rollback(&newer.deployment.id, &stable).await;
    for name in ["frontend", "backend"] {
        verify_process(&fixture, name, stable[&component(name)].as_ref()).await;
    }
    let absent = BTreeMap::from([(component("frontend"), None), (component("backend"), None)]);
    fixture.rollback(&joint.deployment.id, &absent).await;
    for name in ["frontend", "backend"] {
        verify_process(&fixture, name, None).await;
    }
    payload(&fixture, "backend", "unhealthy");
    let failed_first = fixture.deploy(&["backend"], false).await;
    assert_eq!(failed_first.deployment.state, DeploymentState::Failed);
    assert_eq!(fixture.observed().await, absent);
    verify_process(&fixture, "backend", None).await;
    let history = HistoryStore::open(&fixture.history).unwrap();
    let snapshots = history.component_snapshots(&first.deployment.id).unwrap();
    assert_eq!(snapshots.len(), 1);
    assert_eq!(
        snapshots[0].target_snapshot.as_ref().unwrap()["service"]["start"][0][0],
        "node"
    );
}
