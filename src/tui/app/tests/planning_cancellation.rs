use std::sync::mpsc;

use tokio::sync::Notify;

use super::*;

#[derive(Debug, Default)]
struct ControlledPlanningGateway {
    started: AtomicBool,
    cancelled: AtomicBool,
    completed: AtomicBool,
    executed: AtomicBool,
    release: Notify,
}

#[async_trait(?Send)]
impl TuiDeploymentGateway for ControlledPlanningGateway {
    async fn plan(
        &self,
        selection: DeploymentSelection,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> Result<DeploymentPlan, String> {
        if !self.started.swap(true, Ordering::SeqCst) {
            cancellation.cancelled().await;
            self.cancelled.store(true, Ordering::SeqCst);
            self.release.notified().await;
        }
        self.completed.store(true, Ordering::SeqCst);
        // Deliberately return a valid plan even after cancellation. The UI must
        // enforce cancellation independently of cooperative gateway behavior.
        Ok(DeploymentPlan {
            activation_order: selection.components.iter().cloned().collect(),
            selection,
            entries: Vec::new(),
            git: crate::application::GitWorktreeState::NotRepository,
            git_metadata: crate::application::GitMetadata::default(),
        })
    }

    async fn execute(
        &self,
        _: DeploymentPlan,
        _: &dyn EventSink,
        _: &tokio_util::sync::CancellationToken,
    ) -> Result<DeploymentReport, String> {
        self.executed.store(true, Ordering::SeqCst);
        Err("Execution is forbidden in this cancellation regression.".into())
    }
}

fn fixture() -> (TempDir, App, Arc<ControlledPlanningGateway>) {
    let directory = tempdir().unwrap();
    std::fs::write(
        directory.path().join("shipforge.yaml"),
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/docs/examples/shipforge.yaml"
        )),
    )
    .unwrap();
    let crate::config::ProjectConfigState::Loaded(config) =
        crate::config::load(directory.path()).unwrap()
    else {
        panic!("expected fixture config");
    };
    let mut app = App::new(
        directory.path().join("projects.yaml"),
        directory.path().join("destinations.yaml"),
        directory.path(),
    )
    .unwrap();
    let gateway = Arc::new(ControlledPlanningGateway::default());
    app.deployment_gateway = gateway.clone();
    app.open_deployment(directory.path().to_owned(), config);
    (directory, app, gateway)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_real_planning_worker_discards_late_success_before_allowing_a_retry() {
    for help in [false, true] {
        let (directory, mut app, gateway) = fixture();
        let Screen::DeploySelection(original) = app.screen.clone() else {
            panic!("expected selection");
        };
        app.enter_primary();
        tokio::time::timeout(Duration::from_secs(3), async {
            while !gateway.started.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("real planning thread must start");
        let Screen::DeploymentPlanning { request_id, .. } = app.screen else {
            panic!("expected planning");
        };
        if help {
            app.handle_key(key(KeyCode::F(1)));
        }
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        if help {
            app.handle_key(key(KeyCode::Esc));
        }
        app.handle_key(key(KeyCode::Esc));
        app.handle_key(key(KeyCode::Char('c')));
        app.poll_background();
        assert!(
            matches!(&app.screen, Screen::DeploymentPlanning { cancellation, .. } if cancellation.is_cancelled())
        );
        assert!(!gateway.completed.load(Ordering::SeqCst));
        assert!(!gateway.executed.load(Ordering::SeqCst));
        gateway.release.notify_one();
        wait_for_screen(&mut app, |screen| {
            matches!(screen, Screen::DeploySelection(_))
        })
        .await;
        assert!(gateway.cancelled.load(Ordering::SeqCst));
        assert!(gateway.completed.load(Ordering::SeqCst));
        let Screen::DeploySelection(restored) = &app.screen else {
            unreachable!()
        };
        assert_eq!(restored.root, original.root);
        assert_eq!(restored.config, original.config);
        assert_eq!(restored.selected, original.selected);
        assert_eq!(restored.environment_cursor, original.environment_cursor);
        assert_eq!(
            restored.component_cursor,
            deployment_components(restored).len() - 1
        );
        assert!(
            app.message
                .as_deref()
                .unwrap()
                .contains("completed plan was discarded")
        );
        app.enter_primary();
        let Screen::DeploymentPlanning {
            request_id: new_id, ..
        } = &app.screen
        else {
            panic!("expected explicit retry");
        };
        assert_ne!(*new_id, request_id);
        wait_for_screen(&mut app, |screen| {
            matches!(screen, Screen::DeploymentReview { .. })
        })
        .await;
        assert!(!gateway.executed.load(Ordering::SeqCst));
        assert!(!directory.path().join("history.sqlite3").exists());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_cancels_and_joins_the_real_planning_worker() {
    let (_directory, mut app, gateway) = fixture();
    app.enter_primary();
    tokio::time::timeout(Duration::from_secs(3), async {
        while !gateway.started.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("real planning thread must start");

    let (returned_tx, returned_rx) = mpsc::channel();
    let shutdown = std::thread::spawn(move || {
        app.shutdown();
        returned_tx.send(()).unwrap();
        app
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        while !gateway.cancelled.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("shutdown must propagate cancellation to planning");
    assert!(matches!(
        returned_rx.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));

    gateway.release.notify_one();
    returned_rx
        .recv_timeout(Duration::from_secs(3))
        .expect("shutdown must return after the planning worker exits");
    let app = shutdown.join().unwrap();
    assert!(app.deployment_plan_task.is_none());
    assert!(matches!(app.screen, Screen::DeploySelection(_)));
    assert!(gateway.completed.load(Ordering::SeqCst));
}
