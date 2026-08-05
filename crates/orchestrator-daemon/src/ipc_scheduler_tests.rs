use std::{
    collections::{HashMap, HashSet},
    fs,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use orchestrator_state::{Database, GlobalStatePaths, StateEnvironment, WorkspaceId};
use serde_json::json;

use super::{
    EvidenceInspectionInput, IPC_SCHEMA_VERSION, IpcError, IpcRequest, RegistrationCommit,
    RegistrationPreparationInput, WriterBackend, WriterRequest, writer_loop_with_backend,
};

struct ReleaseGate {
    released: Mutex<bool>,
    wake: Condvar,
}

impl ReleaseGate {
    const fn closed() -> Self {
        Self {
            released: Mutex::new(false),
            wake: Condvar::new(),
        }
    }

    fn wait(&self) {
        let released = self
            .released
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _released = self
            .wake
            .wait_while(released, |released| !*released)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }

    fn release(&self) {
        *self
            .released
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        self.wake.notify_all();
    }
}

struct ControlledBackend {
    started: tokio::sync::mpsc::UnboundedSender<WorkspaceId>,
    release: Arc<ReleaseGate>,
}

impl WriterBackend for ControlledBackend {
    fn prepare_registration(
        &self,
        input: RegistrationPreparationInput,
    ) -> Result<Box<dyn RegistrationCommit>, IpcError> {
        let _ = self.started.send(input.workspace_id);
        self.release.wait();
        Ok(Box::new(NoopCommit))
    }

    fn inspect_evidence(
        &self,
        _input: EvidenceInspectionInput,
    ) -> Result<serde_json::Value, IpcError> {
        Err(IpcError::Protocol(
            "controlled backend did not expect evidence inspection".to_owned(),
        ))
    }
}

struct NoopCommit;

impl RegistrationCommit for NoopCommit {
    fn commit(
        self: Box<Self>,
        _database: &Database,
        _paths: &GlobalStatePaths,
    ) -> Result<bool, IpcError> {
        Ok(false)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn four_distinct_workspace_lanes_overlap_blocking_preparation_before_release()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary = tempfile::tempdir()?;
    let paths = GlobalStatePaths::resolve(&StateEnvironment::with_colay_home(
        temporary.path().join("global"),
    )?)?;
    let database = Arc::new(Database::open(&paths.database)?);
    database.migrate_with_backup(&paths.backups)?;
    let (started, mut starts) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(ReleaseGate::closed());
    let backend = Arc::new(ControlledBackend {
        started,
        release: Arc::clone(&release),
    });
    let (writer, receiver) = tokio::sync::mpsc::channel(16);
    let writer_database = Arc::clone(&database);
    let writer_paths = paths.clone();
    let writer_task = tokio::spawn(async move {
        writer_loop_with_backend(writer_database, writer_paths, receiver, None, backend).await;
    });

    let mut replies = Vec::new();
    for index in 0..4 {
        let repository = temporary.path().join(format!("repository-{index}"));
        fs::create_dir_all(&repository)?;
        let (response, reply) = tokio::sync::oneshot::channel();
        writer
            .send(WriterRequest {
                request: register_request(index, &repository),
                response,
            })
            .await?;
        replies.push(reply);
    }

    let mut lanes = HashSet::new();
    for _ in 0..4 {
        lanes.insert(
            tokio::time::timeout(Duration::from_secs(2), starts.recv())
                .await?
                .ok_or("preparation start channel closed")?,
        );
    }
    assert_eq!(lanes.len(), 4);
    release.release();
    for reply in replies {
        let response = tokio::time::timeout(Duration::from_secs(2), reply).await??;
        assert_eq!(response.outcome["status"], "ok");
    }

    drop(writer);
    tokio::time::timeout(Duration::from_secs(2), writer_task).await??;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_closes_admission_then_drains_an_already_admitted_preparation()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary = tempfile::tempdir()?;
    let paths = GlobalStatePaths::resolve(&StateEnvironment::with_colay_home(
        temporary.path().join("global"),
    )?)?;
    let database = Arc::new(Database::open(&paths.database)?);
    database.migrate_with_backup(&paths.backups)?;
    let repository = temporary.path().join("repository");
    fs::create_dir_all(&repository)?;
    let (started, mut starts) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(ReleaseGate::closed());
    let backend = Arc::new(ControlledBackend {
        started,
        release: Arc::clone(&release),
    });
    let (writer, receiver) = tokio::sync::mpsc::channel(16);
    let writer_database = Arc::clone(&database);
    let writer_paths = paths.clone();
    let writer_task = tokio::spawn(async move {
        writer_loop_with_backend(writer_database, writer_paths, receiver, None, backend).await;
    });

    let (registration_response, registration_reply) = tokio::sync::oneshot::channel();
    writer
        .send(WriterRequest {
            request: register_request(0, &repository),
            response: registration_response,
        })
        .await?;
    tokio::time::timeout(Duration::from_secs(2), starts.recv())
        .await?
        .ok_or("registration preparation did not start")?;
    let (stop_response, stop_reply) = tokio::sync::oneshot::channel();
    writer
        .send(WriterRequest {
            request: IpcRequest {
                schema_version: IPC_SCHEMA_VERSION,
                request_id: "stop".to_owned(),
                workspace_id: None,
                action: "daemon.stop".to_owned(),
                payload: json!({}),
            },
            response: stop_response,
        })
        .await?;

    let closed = tokio::time::timeout(Duration::from_millis(500), writer.closed()).await;
    if closed.is_err() {
        release.release();
        drop(writer);
        let _ = writer_task.await;
        return Err("writer kept admitting work after daemon.stop was admitted".into());
    }
    let (late_response, _late_reply) = tokio::sync::oneshot::channel();
    assert!(
        writer
            .send(WriterRequest {
                request: register_request(1, &repository),
                response: late_response,
            })
            .await
            .is_err(),
        "writer accepted work after stop admission"
    );

    release.release();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), registration_reply)
            .await??
            .outcome["status"],
        "ok"
    );
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), stop_reply)
            .await??
            .outcome["status"],
        "ok"
    );
    drop(writer);
    tokio::time::timeout(Duration::from_secs(2), writer_task).await??;
    Ok(())
}

struct EvidenceBackend {
    events: tokio::sync::mpsc::UnboundedSender<&'static str>,
    registration_release: Arc<ReleaseGate>,
}

impl WriterBackend for EvidenceBackend {
    fn prepare_registration(
        &self,
        _input: RegistrationPreparationInput,
    ) -> Result<Box<dyn RegistrationCommit>, IpcError> {
        let _ = self.events.send("registration-started");
        self.registration_release.wait();
        Ok(Box::new(NoopCommit))
    }

    fn inspect_evidence(
        &self,
        _input: EvidenceInspectionInput,
    ) -> Result<serde_json::Value, IpcError> {
        let _ = self.events.send("evidence-started");
        Ok(json!({"coordinated": true}))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn evidence_lookup_waits_for_the_same_workspace_lane_without_blocking_runtime()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary = tempfile::tempdir()?;
    let paths = GlobalStatePaths::resolve(&StateEnvironment::with_colay_home(
        temporary.path().join("global"),
    )?)?;
    let database = Arc::new(Database::open(&paths.database)?);
    database.migrate_with_backup(&paths.backups)?;
    let repository = temporary.path().join("repository");
    fs::create_dir_all(&repository)?;
    let (events, mut observed) = tokio::sync::mpsc::unbounded_channel();
    let registration_release = Arc::new(ReleaseGate::closed());
    let backend = Arc::new(EvidenceBackend {
        events,
        registration_release: Arc::clone(&registration_release),
    });
    let (writer, receiver) = tokio::sync::mpsc::channel(16);
    let writer_database = Arc::clone(&database);
    let writer_paths = paths.clone();
    let writer_task = tokio::spawn(async move {
        writer_loop_with_backend(writer_database, writer_paths, receiver, None, backend).await;
    });

    let (registration_response, registration_reply) = tokio::sync::oneshot::channel();
    writer
        .send(WriterRequest {
            request: register_request(0, &repository),
            response: registration_response,
        })
        .await?;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), observed.recv())
            .await?
            .ok_or("registration did not start")?,
        "registration-started"
    );
    let (evidence_response, evidence_reply) = tokio::sync::oneshot::channel();
    writer
        .send(WriterRequest {
            request: IpcRequest {
                schema_version: IPC_SCHEMA_VERSION,
                request_id: "evidence".to_owned(),
                workspace_id: None,
                action: "workspace.doctor.lookup".to_owned(),
                payload: json!({
                    "repository": repository,
                    "legacy_state_dir": ".colay",
                    "legacy_source_fingerprint": "sealed-fingerprint",
                }),
            },
            response: evidence_response,
        })
        .await?;
    assert!(
        tokio::time::timeout(Duration::from_millis(100), observed.recv())
            .await
            .is_err(),
        "evidence inspection collided with an active registration preparation"
    );

    registration_release.release();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), registration_reply)
            .await??
            .outcome["status"],
        "ok"
    );
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), observed.recv())
            .await?
            .ok_or("evidence inspection did not start")?,
        "evidence-started"
    );
    let evidence = tokio::time::timeout(Duration::from_secs(2), evidence_reply).await??;
    assert_eq!(evidence.outcome["status"], "ok");
    assert_eq!(evidence.outcome["data"]["coordinated"], true);

    drop(writer);
    tokio::time::timeout(Duration::from_secs(2), writer_task).await??;
    Ok(())
}

struct OrderedBackend {
    gates: HashMap<String, Arc<ReleaseGate>>,
    started: tokio::sync::mpsc::UnboundedSender<String>,
    ordinary_response_observed: Arc<AtomicBool>,
}

impl WriterBackend for OrderedBackend {
    fn prepare_registration(
        &self,
        input: RegistrationPreparationInput,
    ) -> Result<Box<dyn RegistrationCommit>, IpcError> {
        let label = input
            .source
            .root
            .parent()
            .and_then(std::path::Path::file_name)
            .and_then(std::ffi::OsStr::to_str)
            .ok_or_else(|| IpcError::Protocol("test repository label is invalid".to_owned()))?
            .to_owned();
        let _ = self.started.send(label.clone());
        self.gates
            .get(&label)
            .ok_or_else(|| IpcError::Protocol(format!("missing test gate for {label}")))?
            .wait();
        Ok(Box::new(OrderedCommit {
            label,
            ordinary_response_observed: Arc::clone(&self.ordinary_response_observed),
        }))
    }

    fn inspect_evidence(
        &self,
        _input: EvidenceInspectionInput,
    ) -> Result<serde_json::Value, IpcError> {
        Err(IpcError::Protocol("evidence was not expected".to_owned()))
    }
}

struct OrderedCommit {
    label: String,
    ordinary_response_observed: Arc<AtomicBool>,
}

impl RegistrationCommit for OrderedCommit {
    fn commit(
        self: Box<Self>,
        _database: &Database,
        _paths: &GlobalStatePaths,
    ) -> Result<bool, IpcError> {
        if self.label == "repository-b" && !self.ordinary_response_observed.load(Ordering::SeqCst) {
            return Err(IpcError::Protocol(
                "later registration committed before the interleaved writer response flushed"
                    .to_owned(),
            ));
        }
        Ok(false)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn out_of_order_preparations_commit_around_an_ordinary_action_in_admission_order()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary = tempfile::tempdir()?;
    let paths = GlobalStatePaths::resolve(&StateEnvironment::with_colay_home(
        temporary.path().join("global"),
    )?)?;
    let database = Arc::new(Database::open(&paths.database)?);
    database.migrate_with_backup(&paths.backups)?;
    let repository_a = temporary.path().join("repository-a");
    let repository_b = temporary.path().join("repository-b");
    fs::create_dir_all(&repository_a)?;
    fs::create_dir_all(&repository_b)?;
    let gate_a = Arc::new(ReleaseGate::closed());
    let gate_b = Arc::new(ReleaseGate::closed());
    let ordinary_response_observed = Arc::new(AtomicBool::new(false));
    let (started, mut starts) = tokio::sync::mpsc::unbounded_channel();
    let backend = Arc::new(OrderedBackend {
        gates: HashMap::from([
            ("repository-a".to_owned(), Arc::clone(&gate_a)),
            ("repository-b".to_owned(), Arc::clone(&gate_b)),
        ]),
        started,
        ordinary_response_observed: Arc::clone(&ordinary_response_observed),
    });
    let (writer, receiver) = tokio::sync::mpsc::channel(16);
    let writer_database = Arc::clone(&database);
    let writer_paths = paths.clone();
    let writer_task = tokio::spawn(async move {
        writer_loop_with_backend(writer_database, writer_paths, receiver, None, backend).await;
    });

    let (a_response, a_reply) = tokio::sync::oneshot::channel();
    writer
        .send(WriterRequest {
            request: register_request(0, &repository_a),
            response: a_response,
        })
        .await?;
    assert_eq!(starts.recv().await, Some("repository-a".to_owned()));
    let (ordinary_response, ordinary_reply) = tokio::sync::oneshot::channel();
    writer
        .send(WriterRequest {
            request: IpcRequest {
                schema_version: IPC_SCHEMA_VERSION,
                request_id: "ordinary".to_owned(),
                workspace_id: None,
                action: "workspace.selection".to_owned(),
                payload: json!({}),
            },
            response: ordinary_response,
        })
        .await?;
    let ordinary_observed = Arc::clone(&ordinary_response_observed);
    let ordinary_task = tokio::spawn(async move {
        let response = ordinary_reply.await;
        ordinary_observed.store(true, Ordering::SeqCst);
        response
    });
    let (b_response, mut b_reply) = tokio::sync::oneshot::channel();
    writer
        .send(WriterRequest {
            request: register_request(1, &repository_b),
            response: b_response,
        })
        .await?;
    assert_eq!(starts.recv().await, Some("repository-b".to_owned()));

    gate_b.release();
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut b_reply)
            .await
            .is_err(),
        "later ready preparation bypassed the admission-order prefix"
    );
    gate_a.release();
    assert_eq!(a_reply.await?.outcome["status"], "ok");
    assert_eq!(ordinary_task.await??.outcome["status"], "error");
    assert_eq!(b_reply.await?.outcome["status"], "ok");

    drop(writer);
    writer_task.await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn path_aliases_share_one_workspace_lane_and_one_preparation_generation()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary = tempfile::tempdir()?;
    let paths = GlobalStatePaths::resolve(&StateEnvironment::with_colay_home(
        temporary.path().join("global"),
    )?)?;
    let database = Arc::new(Database::open(&paths.database)?);
    database.migrate_with_backup(&paths.backups)?;
    let repository = temporary.path().join("repository");
    fs::create_dir_all(&repository)?;
    let alias = repository.join(".");
    let (started, mut starts) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(ReleaseGate::closed());
    let backend = Arc::new(ControlledBackend {
        started,
        release: Arc::clone(&release),
    });
    let (writer, receiver) = tokio::sync::mpsc::channel(16);
    let writer_database = Arc::clone(&database);
    let writer_paths = paths.clone();
    let writer_task = tokio::spawn(async move {
        writer_loop_with_backend(writer_database, writer_paths, receiver, None, backend).await;
    });

    let (first_response, first_reply) = tokio::sync::oneshot::channel();
    writer
        .send(WriterRequest {
            request: register_request(0, &repository),
            response: first_response,
        })
        .await?;
    let lane = starts
        .recv()
        .await
        .ok_or("first preparation did not start")?;
    let (second_response, second_reply) = tokio::sync::oneshot::channel();
    writer
        .send(WriterRequest {
            request: register_request(1, &alias),
            response: second_response,
        })
        .await?;
    assert!(
        tokio::time::timeout(Duration::from_millis(100), starts.recv())
            .await
            .is_err(),
        "path alias started a second same-workspace preparation"
    );
    release.release();
    let first = first_reply.await?;
    let second = second_reply.await?;
    assert_eq!(first.outcome["status"], "ok");
    assert_eq!(second.outcome["status"], "ok");
    assert_eq!(
        first.outcome["data"]["workspace_id"],
        second.outcome["data"]["workspace_id"]
    );
    assert_eq!(first.outcome["data"]["workspace_id"], lane.to_string());

    drop(writer);
    writer_task.await?;
    Ok(())
}

struct ReplayBackend {
    committed: Arc<Mutex<HashSet<WorkspaceId>>>,
    durable_commits: tokio::sync::mpsc::UnboundedSender<WorkspaceId>,
}

impl WriterBackend for ReplayBackend {
    fn prepare_registration(
        &self,
        input: RegistrationPreparationInput,
    ) -> Result<Box<dyn RegistrationCommit>, IpcError> {
        Ok(Box::new(ReplayCommit {
            workspace_id: input.workspace_id,
            committed: Arc::clone(&self.committed),
            durable_commits: self.durable_commits.clone(),
        }))
    }

    fn inspect_evidence(
        &self,
        _input: EvidenceInspectionInput,
    ) -> Result<serde_json::Value, IpcError> {
        Err(IpcError::Protocol("evidence was not expected".to_owned()))
    }
}

struct ReplayCommit {
    workspace_id: WorkspaceId,
    committed: Arc<Mutex<HashSet<WorkspaceId>>>,
    durable_commits: tokio::sync::mpsc::UnboundedSender<WorkspaceId>,
}

impl RegistrationCommit for ReplayCommit {
    fn commit(
        self: Box<Self>,
        _database: &Database,
        _paths: &GlobalStatePaths,
    ) -> Result<bool, IpcError> {
        let inserted = self
            .committed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(self.workspace_id);
        if inserted {
            let _ = self.durable_commits.send(self.workspace_id);
        }
        Ok(inserted)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropped_response_receiver_does_not_cancel_commit_and_retry_is_replay_safe()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary = tempfile::tempdir()?;
    let paths = GlobalStatePaths::resolve(&StateEnvironment::with_colay_home(
        temporary.path().join("global"),
    )?)?;
    let database = Arc::new(Database::open(&paths.database)?);
    database.migrate_with_backup(&paths.backups)?;
    let repository = temporary.path().join("repository");
    fs::create_dir_all(&repository)?;
    let committed = Arc::new(Mutex::new(HashSet::new()));
    let (durable_commits, mut committed_workspaces) = tokio::sync::mpsc::unbounded_channel();
    let backend = Arc::new(ReplayBackend {
        committed: Arc::clone(&committed),
        durable_commits,
    });
    let (writer, receiver) = tokio::sync::mpsc::channel(16);
    let writer_database = Arc::clone(&database);
    let writer_paths = paths.clone();
    let writer_task = tokio::spawn(async move {
        writer_loop_with_backend(writer_database, writer_paths, receiver, None, backend).await;
    });

    let (abandoned_response, abandoned_reply) = tokio::sync::oneshot::channel();
    drop(abandoned_reply);
    writer
        .send(WriterRequest {
            request: register_request(0, &repository),
            response: abandoned_response,
        })
        .await?;
    let workspace_id = tokio::time::timeout(Duration::from_secs(2), committed_workspaces.recv())
        .await?
        .ok_or("dropped response cancelled the durable commit")?;

    let (retry_response, retry_reply) = tokio::sync::oneshot::channel();
    writer
        .send(WriterRequest {
            request: register_request(1, &repository),
            response: retry_response,
        })
        .await?;
    let retry = retry_reply.await?;
    assert_eq!(retry.outcome["status"], "ok");
    assert_eq!(
        retry.outcome["data"]["workspace_id"],
        workspace_id.to_string()
    );
    assert_eq!(retry.outcome["data"]["imported_legacy_state"], false);
    assert!(committed_workspaces.try_recv().is_err());
    assert_eq!(
        committed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len(),
        1
    );

    drop(writer);
    writer_task.await?;
    Ok(())
}

struct DropProbe(tokio::sync::mpsc::UnboundedSender<&'static str>);

impl Drop for DropProbe {
    fn drop(&mut self) {
        let _ = self.0.send("preparation-guard-dropped");
    }
}

struct ErrorThenSuccessBackend {
    calls: AtomicUsize,
    drops: tokio::sync::mpsc::UnboundedSender<&'static str>,
    commits: tokio::sync::mpsc::UnboundedSender<&'static str>,
}

impl WriterBackend for ErrorThenSuccessBackend {
    fn prepare_registration(
        &self,
        _input: RegistrationPreparationInput,
    ) -> Result<Box<dyn RegistrationCommit>, IpcError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            let _guard = DropProbe(self.drops.clone());
            return Err(IpcError::Protocol(
                "injected preparation failure".to_owned(),
            ));
        }
        Ok(Box::new(MarkerCommit(self.commits.clone())))
    }

    fn inspect_evidence(
        &self,
        _input: EvidenceInspectionInput,
    ) -> Result<serde_json::Value, IpcError> {
        Err(IpcError::Protocol("evidence was not expected".to_owned()))
    }
}

struct MarkerCommit(tokio::sync::mpsc::UnboundedSender<&'static str>);

impl RegistrationCommit for MarkerCommit {
    fn commit(
        self: Box<Self>,
        _database: &Database,
        _paths: &GlobalStatePaths,
    ) -> Result<bool, IpcError> {
        let _ = self.0.send("commit-complete");
        Ok(true)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preparation_error_drops_guards_advances_lane_and_success_waits_for_commit_activation()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary = tempfile::tempdir()?;
    let paths = GlobalStatePaths::resolve(&StateEnvironment::with_colay_home(
        temporary.path().join("global"),
    )?)?;
    let database = Arc::new(Database::open(&paths.database)?);
    database.migrate_with_backup(&paths.backups)?;
    let repository = temporary.path().join("repository");
    fs::create_dir_all(&repository)?;
    let (drops, mut dropped) = tokio::sync::mpsc::unbounded_channel();
    let (commits, mut committed) = tokio::sync::mpsc::unbounded_channel();
    let backend = Arc::new(ErrorThenSuccessBackend {
        calls: AtomicUsize::new(0),
        drops,
        commits,
    });
    let (activations, mut activated) = tokio::sync::mpsc::unbounded_channel();
    let (writer, receiver) = tokio::sync::mpsc::channel(16);
    let writer_database = Arc::clone(&database);
    let writer_paths = paths.clone();
    let writer_task = tokio::spawn(async move {
        writer_loop_with_backend(
            writer_database,
            writer_paths,
            receiver,
            Some(activations),
            backend,
        )
        .await;
    });

    let (failed_response, failed_reply) = tokio::sync::oneshot::channel();
    writer
        .send(WriterRequest {
            request: register_request_with_state(0, &repository, ".colay"),
            response: failed_response,
        })
        .await?;
    let (success_response, success_reply) = tokio::sync::oneshot::channel();
    writer
        .send(WriterRequest {
            request: register_request_with_state(1, &repository, ".other-colay"),
            response: success_response,
        })
        .await?;

    let failed = failed_reply.await?;
    assert_eq!(failed.outcome["status"], "error");
    assert_eq!(
        dropped.recv().await,
        Some("preparation-guard-dropped"),
        "failed preparation did not drop its owned guard"
    );
    assert!(committed.try_recv().is_err());
    assert!(activated.try_recv().is_err());

    let success = success_reply.await?;
    assert_eq!(success.outcome["status"], "ok");
    assert_eq!(success.outcome["data"]["imported_legacy_state"], true);
    assert_eq!(committed.try_recv(), Ok("commit-complete"));
    let activation = activated
        .try_recv()
        .map_err(|error| format!("response arrived before activation enqueue: {error}"))?;
    assert_eq!(
        activation.workspace_id.to_string(),
        success.outcome["data"]["workspace_id"]
    );

    drop(writer);
    writer_task.await?;
    Ok(())
}

fn register_request(index: usize, repository: &std::path::Path) -> IpcRequest {
    register_request_with_state(index, repository, ".colay")
}

fn register_request_with_state(
    index: usize,
    repository: &std::path::Path,
    state_dir: &str,
) -> IpcRequest {
    IpcRequest {
        schema_version: IPC_SCHEMA_VERSION,
        request_id: format!("register-{index}"),
        workspace_id: None,
        action: "workspace.register".to_owned(),
        payload: json!({
            "repository": repository,
            "state_dir": state_dir,
            "explicit_config": null,
        }),
    }
}
