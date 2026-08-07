use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use orchestrator_state::{Database, GlobalStatePaths, StateEnvironment, WorkspaceId};
use serde_json::json;

use super::{
    EvidenceInspectionInput, IPC_SCHEMA_VERSION, IpcError, IpcRequest, IpcResponse,
    MAX_PENDING_WRITER_ADMISSIONS, RegistrationCommit, RegistrationPreparationInput,
    RegistrationSemantics, WRITER_INGRESS_CAPACITY, WriterBackend, WriterIngress, WriterRequest,
    WriterScheduler, build_writer_runtime, process_writer_request, resolve_registration_admission,
    spawn_writer_thread_with_backend, workspace_activation, writer_loop_with_backend,
};

struct CanonicalTempDir {
    _directory: tempfile::TempDir,
    path: PathBuf,
}

impl CanonicalTempDir {
    fn new() -> std::io::Result<Self> {
        let directory = tempfile::tempdir()?;
        let path = fs::canonicalize(directory.path())?;
        Ok(Self {
            _directory: directory,
            path,
        })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

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
    let temporary = CanonicalTempDir::new()?;
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
async fn five_ready_lanes_create_only_four_active_jobs_then_start_the_fifth()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary = CanonicalTempDir::new()?;
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
    let mut scheduler = WriterScheduler::new(backend);

    let mut replies = Vec::new();
    for index in 0..5 {
        let repository = temporary.path().join(format!("repository-{index}"));
        fs::create_dir_all(&repository)?;
        let (response, reply) = tokio::sync::oneshot::channel();
        assert!(!scheduler.admit(
            &database,
            &paths,
            WriterRequest {
                request: register_request(index, &repository),
                response,
            },
        ));
        replies.push(reply);
    }

    let active_jobs_before_release = scheduler.jobs.len();
    let mut lanes = HashSet::new();
    for _ in 0..4 {
        lanes.insert(
            tokio::time::timeout(Duration::from_secs(2), starts.recv())
                .await?
                .ok_or("preparation start channel closed")?,
        );
    }
    assert_eq!(lanes.len(), 4);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), starts.recv())
            .await
            .is_err(),
        "the fifth preparation started before capacity was released"
    );

    release.release();
    while let Some(completed) = scheduler.jobs.join_next().await {
        let (generation, result) = completed?;
        scheduler.complete_generation(generation, result);
    }
    assert!(
        starts.try_recv().is_ok(),
        "the fifth preparation never started"
    );
    assert_eq!(
        active_jobs_before_release, 4,
        "capacity waiters must remain in a bounded ready queue, not in JoinSet"
    );
    drop(replies);
    Ok(())
}

struct IndividuallyGatedBackend {
    started: tokio::sync::mpsc::UnboundedSender<WorkspaceId>,
    gates: HashMap<WorkspaceId, Arc<ReleaseGate>>,
}

impl WriterBackend for IndividuallyGatedBackend {
    fn prepare_registration(
        &self,
        input: RegistrationPreparationInput,
    ) -> Result<Box<dyn RegistrationCommit>, IpcError> {
        let _ = self.started.send(input.workspace_id);
        self.gates
            .get(&input.workspace_id)
            .ok_or_else(|| IpcError::Protocol("missing workspace gate".to_owned()))?
            .wait();
        Ok(Box::new(NoopCommit))
    }

    fn inspect_evidence(
        &self,
        _input: EvidenceInspectionInput,
    ) -> Result<serde_json::Value, IpcError> {
        Err(IpcError::Protocol(
            "individually gated backend did not expect evidence inspection".to_owned(),
        ))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn global_ready_queue_starts_eligible_lanes_fifo_without_stale_entries()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary = CanonicalTempDir::new()?;
    let paths = GlobalStatePaths::resolve(&StateEnvironment::with_colay_home(
        temporary.path().join("global"),
    )?)?;
    let database = Arc::new(Database::open(&paths.database)?);
    database.migrate_with_backup(&paths.backups)?;
    let mut repositories = Vec::new();
    let mut workspace_ids = Vec::new();
    let mut gates = HashMap::new();
    for index in 0..6 {
        let repository = temporary.path().join(format!("repository-{index}"));
        fs::create_dir_all(&repository)?;
        let workspace_id = database
            .resolve_repository_workspace(&repository)?
            .workspace_id;
        repositories.push(repository);
        workspace_ids.push(workspace_id);
        gates.insert(workspace_id, Arc::new(ReleaseGate::closed()));
    }
    let release_gates = gates.clone();
    let (started, mut starts) = tokio::sync::mpsc::unbounded_channel();
    let mut scheduler = WriterScheduler::new(Arc::new(IndividuallyGatedBackend { started, gates }));
    let mut replies = Vec::new();
    for (index, repository) in repositories.iter().enumerate() {
        let (response, reply) = tokio::sync::oneshot::channel();
        assert!(!scheduler.admit(
            &database,
            &paths,
            WriterRequest {
                request: register_request(index, repository),
                response,
            },
        ));
        replies.push(reply);
    }

    let mut observed = HashSet::new();
    for _ in 0..4 {
        observed.insert(
            starts
                .recv()
                .await
                .ok_or("preparation start channel closed")?,
        );
    }
    assert_eq!(observed, workspace_ids[..4].iter().copied().collect());
    for next_index in 4..6 {
        release_gates[&workspace_ids[next_index - 4]].release();
        let (generation, result) = scheduler
            .jobs
            .join_next()
            .await
            .ok_or("released preparation disappeared")??;
        scheduler.complete_generation(generation, result);
        let started = starts
            .recv()
            .await
            .ok_or("queued preparation did not start")?;
        assert_eq!(started, workspace_ids[next_index]);
        assert!(observed.insert(started));
    }
    assert!(scheduler.ready_generations.is_empty());
    for gate in release_gates.values() {
        gate.release();
    }
    while let Some(completed) = scheduler.jobs.join_next().await {
        let (generation, result) = completed?;
        scheduler.complete_generation(generation, result);
    }
    assert_eq!(observed.len(), 6);
    drop(replies);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_internal_admission_capacity_backpressures_the_bounded_ingress_channel()
-> Result<(), Box<dyn std::error::Error>> {
    const EXPECTED_INTERNAL_ADMISSION_CAPACITY: usize = 64;

    let temporary = CanonicalTempDir::new()?;
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
    let (writer, receiver) = tokio::sync::mpsc::channel(1);
    let writer_database = Arc::clone(&database);
    let writer_paths = paths.clone();
    let writer_task = tokio::spawn(async move {
        writer_loop_with_backend(writer_database, writer_paths, receiver, None, backend).await;
    });

    let (response, reply) = tokio::sync::oneshot::channel();
    drop(reply);
    writer
        .send(WriterRequest {
            request: register_request(0, &repository),
            response,
        })
        .await?;
    tokio::time::timeout(Duration::from_secs(2), starts.recv())
        .await?
        .ok_or("registration preparation did not start")?;

    for index in 1..=EXPECTED_INTERNAL_ADMISSION_CAPACITY {
        let (response, reply) = tokio::sync::oneshot::channel();
        drop(reply);
        writer
            .send(WriterRequest {
                request: IpcRequest {
                    schema_version: IPC_SCHEMA_VERSION,
                    request_id: format!("ordinary-{index}"),
                    workspace_id: None,
                    action: "workspace.selection".to_owned(),
                    payload: json!({}),
                },
                response,
            })
            .await?;
    }

    let overflow_finished = Arc::new(AtomicBool::new(false));
    let overflow_finished_in_task = Arc::clone(&overflow_finished);
    let overflow_writer = writer.clone();
    let overflow_task = tokio::spawn(async move {
        let (response, reply) = tokio::sync::oneshot::channel();
        drop(reply);
        let result = overflow_writer
            .send(WriterRequest {
                request: IpcRequest {
                    schema_version: IPC_SCHEMA_VERSION,
                    request_id: "ordinary-overflow".to_owned(),
                    workspace_id: None,
                    action: "workspace.selection".to_owned(),
                    payload: json!({}),
                },
                response,
            })
            .await;
        overflow_finished_in_task.store(true, Ordering::SeqCst);
        result
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let overflow_was_backpressured = !overflow_finished.load(Ordering::SeqCst);

    release.release();
    drop(writer);
    overflow_task.await??;
    tokio::time::timeout(Duration::from_secs(2), writer_task).await??;
    assert!(
        overflow_was_backpressured,
        "writer drained bounded ingress into unbounded internal admissions"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn internal_capacity_counts_coalesced_and_ordinary_admissions()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary = CanonicalTempDir::new()?;
    let paths = GlobalStatePaths::resolve(&StateEnvironment::with_colay_home(
        temporary.path().join("global"),
    )?)?;
    let database = Arc::new(Database::open(&paths.database)?);
    database.migrate_with_backup(&paths.backups)?;
    let repository = temporary.path().join("repository");
    fs::create_dir_all(&repository)?;
    let (started, _starts) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(ReleaseGate::closed());
    let mut scheduler = WriterScheduler::new(Arc::new(ControlledBackend {
        started,
        release: Arc::clone(&release),
    }));
    let mut replies = Vec::new();
    for index in 0..32 {
        let (response, reply) = tokio::sync::oneshot::channel();
        assert!(!scheduler.admit(
            &database,
            &paths,
            WriterRequest {
                request: register_request(index, &repository),
                response,
            },
        ));
        replies.push(reply);
    }
    for index in 32..MAX_PENDING_WRITER_ADMISSIONS {
        let (response, reply) = tokio::sync::oneshot::channel();
        assert!(!scheduler.admit(
            &database,
            &paths,
            WriterRequest {
                request: ordinary_request(format!("ordinary-{index}")),
                response,
            },
        ));
        replies.push(reply);
    }
    assert_eq!(scheduler.admissions.len(), MAX_PENDING_WRITER_ADMISSIONS);
    assert_eq!(scheduler.generations.len(), 1);
    assert_eq!(scheduler.lanes.len(), 1);
    assert_eq!(scheduler.jobs.len(), 1);

    let (overflow_response, overflow_reply) = tokio::sync::oneshot::channel();
    assert!(!scheduler.admit(
        &database,
        &paths,
        WriterRequest {
            request: register_request(MAX_PENDING_WRITER_ADMISSIONS, &repository),
            response: overflow_response,
        },
    ));
    assert_eq!(scheduler.admissions.len(), MAX_PENDING_WRITER_ADMISSIONS);
    assert_eq!(overflow_reply.await?.outcome["status"], "error");

    release.release();
    while let Some(completed) = scheduler.jobs.join_next().await {
        let (generation, result) = completed?;
        scheduler.complete_generation(generation, result);
    }
    drop(replies);
    Ok(())
}

struct BlockingCommitBackend {
    entered: std::sync::mpsc::Sender<()>,
    release: Arc<ReleaseGate>,
}

impl WriterBackend for BlockingCommitBackend {
    fn prepare_registration(
        &self,
        _input: RegistrationPreparationInput,
    ) -> Result<Box<dyn RegistrationCommit>, IpcError> {
        Ok(Box::new(BlockingCommit {
            entered: self.entered.clone(),
            release: Arc::clone(&self.release),
        }))
    }

    fn inspect_evidence(
        &self,
        _input: EvidenceInspectionInput,
    ) -> Result<serde_json::Value, IpcError> {
        Err(IpcError::Protocol(
            "blocking commit backend did not expect evidence inspection".to_owned(),
        ))
    }
}

struct BlockingCommit {
    entered: std::sync::mpsc::Sender<()>,
    release: Arc<ReleaseGate>,
}

impl RegistrationCommit for BlockingCommit {
    fn commit(
        self: Box<Self>,
        _database: &Database,
        _paths: &GlobalStatePaths,
    ) -> Result<bool, IpcError> {
        let _ = self.entered.send(());
        self.release.wait();
        Ok(false)
    }
}

#[tokio::test(flavor = "current_thread")]
async fn synchronous_commit_does_not_stall_the_calling_runtime()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary = CanonicalTempDir::new()?;
    let paths = GlobalStatePaths::resolve(&StateEnvironment::with_colay_home(
        temporary.path().join("global"),
    )?)?;
    let database = Arc::new(Database::open(&paths.database)?);
    database.migrate_with_backup(&paths.backups)?;
    let repository = temporary.path().join("repository");
    fs::create_dir_all(&repository)?;
    let release = Arc::new(ReleaseGate::closed());
    let (entered, commit_entered) = std::sync::mpsc::channel();
    let backend = Arc::new(BlockingCommitBackend {
        entered,
        release: Arc::clone(&release),
    });
    let (writer, writer_thread) =
        spawn_writer_thread_with_backend(Arc::clone(&database), paths.clone(), None, backend)
            .await?;

    let ticks = Arc::new(AtomicUsize::new(0));
    let heartbeat_running = Arc::new(AtomicBool::new(true));
    let heartbeat_ticks = Arc::clone(&ticks);
    let heartbeat_running_in_task = Arc::clone(&heartbeat_running);
    let heartbeat = tokio::spawn(async move {
        while heartbeat_running_in_task.load(Ordering::SeqCst) {
            heartbeat_ticks.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });
    let observer_ticks = Arc::clone(&ticks);
    let observer_release = Arc::clone(&release);
    let observer = std::thread::spawn(move || -> Result<bool, String> {
        commit_entered
            .recv_timeout(Duration::from_secs(2))
            .map_err(|error| format!("commit did not start: {error}"))?;
        let before = observer_ticks.load(Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(100));
        let after = observer_ticks.load(Ordering::SeqCst);
        observer_release.release();
        Ok(after > before)
    });

    let (response, reply) = tokio::sync::oneshot::channel();
    writer
        .send(WriterRequest {
            request: register_request(0, &repository),
            response,
        })
        .await?;
    let registration = reply.await?;
    heartbeat_running.store(false, Ordering::SeqCst);
    heartbeat.await?;
    let heartbeat_progressed = observer.join().map_err(|_| "commit observer panicked")??;
    drop(writer);
    writer_thread.join().await?;

    assert_eq!(registration.outcome["status"], "ok");
    assert!(
        heartbeat_progressed,
        "synchronous commit stalled the calling Tokio runtime"
    );
    Ok(())
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
enum SynchronousWriterStage {
    Resolution,
    Commit,
    Activation,
    OrdinaryAction,
}

struct SynchronousStageProbe {
    entered: std::sync::mpsc::Sender<SynchronousWriterStage>,
    releases: HashMap<SynchronousWriterStage, Arc<ReleaseGate>>,
    event_log: Arc<Mutex<Vec<&'static str>>>,
}

impl SynchronousStageProbe {
    fn block(&self, stage: SynchronousWriterStage) -> Result<(), IpcError> {
        self.entered
            .send(stage)
            .map_err(|error| IpcError::Protocol(format!("stage observer closed: {error}")))?;
        self.releases
            .get(&stage)
            .ok_or_else(|| IpcError::Protocol(format!("missing release gate for {stage:?}")))?
            .wait();
        Ok(())
    }

    fn record(&self, event: &'static str) {
        self.event_log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(event);
    }
}

struct SynchronousStageBackend {
    probe: Arc<SynchronousStageProbe>,
}

impl WriterBackend for SynchronousStageBackend {
    fn resolve_registration(
        &self,
        database: &Arc<Database>,
        paths: &GlobalStatePaths,
        request: &IpcRequest,
    ) -> Result<(RegistrationPreparationInput, RegistrationSemantics), IpcError> {
        self.probe.block(SynchronousWriterStage::Resolution)?;
        resolve_registration_admission(database, paths, request)
    }

    fn prepare_registration(
        &self,
        _input: RegistrationPreparationInput,
    ) -> Result<Box<dyn RegistrationCommit>, IpcError> {
        Ok(Box::new(SynchronousStageCommit {
            probe: Arc::clone(&self.probe),
        }))
    }

    fn inspect_evidence(
        &self,
        _input: EvidenceInspectionInput,
    ) -> Result<serde_json::Value, IpcError> {
        Err(IpcError::Protocol(
            "synchronous-stage backend did not expect evidence inspection".to_owned(),
        ))
    }

    fn process_ordinary(
        &self,
        database: &Database,
        paths: &GlobalStatePaths,
        request: IpcRequest,
    ) -> IpcResponse {
        if let Err(error) = self.probe.block(SynchronousWriterStage::OrdinaryAction) {
            return IpcResponse::failure(request.request_id, error.to_string());
        }
        process_writer_request(database, paths, request)
    }

    fn publish_activation(
        &self,
        database: &Database,
        request: &IpcRequest,
        response: &IpcResponse,
        sender: &tokio::sync::mpsc::UnboundedSender<super::WorkspaceActivation>,
    ) -> Result<(), IpcError> {
        self.probe.block(SynchronousWriterStage::Activation)?;
        let activation = workspace_activation(database, request, response)?;
        sender
            .send(activation)
            .map_err(|_| IpcError::WriterUnavailable)?;
        self.probe.record("activation");
        Ok(())
    }
}

struct SynchronousStageCommit {
    probe: Arc<SynchronousStageProbe>,
}

impl RegistrationCommit for SynchronousStageCommit {
    fn commit(
        self: Box<Self>,
        _database: &Database,
        _paths: &GlobalStatePaths,
    ) -> Result<bool, IpcError> {
        self.probe.block(SynchronousWriterStage::Commit)?;
        self.probe.record("commit");
        Ok(false)
    }
}

struct RuntimeHeartbeat {
    running: Arc<AtomicBool>,
    ticks: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl RuntimeHeartbeat {
    fn start() -> Self {
        let running = Arc::new(AtomicBool::new(true));
        let ticks = Arc::new(AtomicUsize::new(0));
        let task_running = Arc::clone(&running);
        let task_ticks = Arc::clone(&ticks);
        let task = tokio::spawn(async move {
            while task_running.load(Ordering::SeqCst) {
                task_ticks.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        Self {
            running,
            ticks,
            task,
        }
    }

    async fn stop(self) -> Result<(), tokio::task::JoinError> {
        self.running.store(false, Ordering::SeqCst);
        self.task.await
    }
}

fn spawn_synchronous_stage_observer(
    stages: [SynchronousWriterStage; 4],
    entered: std::sync::mpsc::Receiver<SynchronousWriterStage>,
    releases: HashMap<SynchronousWriterStage, Arc<ReleaseGate>>,
    ticks: Arc<AtomicUsize>,
) -> std::thread::JoinHandle<Result<Vec<bool>, String>> {
    std::thread::spawn(move || {
        let mut progress = Vec::new();
        for expected in stages {
            let observed = entered
                .recv_timeout(Duration::from_secs(2))
                .map_err(|error| format!("{expected:?} did not start: {error}"))?;
            if observed != expected {
                return Err(format!("expected {expected:?}, observed {observed:?}"));
            }
            let before = ticks.load(Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(75));
            let after = ticks.load(Ordering::SeqCst);
            progress.push(after > before);
            releases
                .get(&expected)
                .ok_or_else(|| format!("missing observer gate for {expected:?}"))?
                .release();
        }
        Ok(progress)
    })
}

#[tokio::test(flavor = "current_thread")]
async fn all_synchronous_writer_stages_leave_the_calling_runtime_responsive_and_publish_in_order()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary = CanonicalTempDir::new()?;
    let paths = GlobalStatePaths::resolve(&StateEnvironment::with_colay_home(
        temporary.path().join("global"),
    )?)?;
    let database = Arc::new(Database::open(&paths.database)?);
    database.migrate_with_backup(&paths.backups)?;
    let repository = temporary.path().join("repository");
    fs::create_dir_all(&repository)?;
    let stages = [
        SynchronousWriterStage::Resolution,
        SynchronousWriterStage::Commit,
        SynchronousWriterStage::Activation,
        SynchronousWriterStage::OrdinaryAction,
    ];
    let releases = stages
        .into_iter()
        .map(|stage| (stage, Arc::new(ReleaseGate::closed())))
        .collect::<HashMap<_, _>>();
    let observer_releases = releases.clone();
    let (entered, stage_entered) = std::sync::mpsc::channel();
    let event_log = Arc::new(Mutex::new(Vec::new()));
    let backend = Arc::new(SynchronousStageBackend {
        probe: Arc::new(SynchronousStageProbe {
            entered,
            releases,
            event_log: Arc::clone(&event_log),
        }),
    });
    let (activations, mut activated) = tokio::sync::mpsc::unbounded_channel();
    let (writer, writer_thread) = spawn_writer_thread_with_backend(
        Arc::clone(&database),
        paths.clone(),
        Some(activations),
        backend,
    )
    .await?;

    let heartbeat = RuntimeHeartbeat::start();
    let observer = spawn_synchronous_stage_observer(
        stages,
        stage_entered,
        observer_releases,
        Arc::clone(&heartbeat.ticks),
    );

    let (registration_response, registration_reply) = tokio::sync::oneshot::channel();
    writer
        .send(WriterRequest {
            request: register_request(0, &repository),
            response: registration_response,
        })
        .await?;
    let registration = registration_reply.await?;
    event_log
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push("response");
    assert_eq!(registration.outcome["status"], "ok");
    assert!(activated.try_recv().is_ok(), "activation was not published");

    let (ordinary_response, ordinary_reply) = tokio::sync::oneshot::channel();
    writer
        .send(WriterRequest {
            request: IpcRequest {
                schema_version: IPC_SCHEMA_VERSION,
                request_id: "ordinary-heartbeat".to_owned(),
                workspace_id: None,
                action: "workspace.selection".to_owned(),
                payload: json!({}),
            },
            response: ordinary_response,
        })
        .await?;
    let _ordinary = ordinary_reply.await?;

    let progress = observer
        .join()
        .map_err(|_| "synchronous-stage observer panicked")??;
    heartbeat.stop().await?;
    drop(writer);
    writer_thread.join().await?;

    assert!(
        progress.into_iter().all(std::convert::identity),
        "at least one synchronous writer stage stalled the calling runtime"
    );
    assert_eq!(
        *event_log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        ["commit", "activation", "response"]
    );
    Ok(())
}

#[test]
fn writer_runtime_limits_blocking_workers_to_four() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = build_writer_runtime()?;
    runtime.block_on(async {
        let release = Arc::new(ReleaseGate::closed());
        let (started, mut starts) = tokio::sync::mpsc::unbounded_channel();
        let mut jobs = tokio::task::JoinSet::new();
        for index in 0..5 {
            let started = started.clone();
            let release = Arc::clone(&release);
            jobs.spawn_blocking(move || {
                let _ = started.send(index);
                release.wait();
            });
        }
        drop(started);

        let mut first = HashSet::new();
        for _ in 0..4 {
            first.insert(
                tokio::time::timeout(Duration::from_secs(2), starts.recv())
                    .await?
                    .ok_or("blocking worker start channel closed")?,
            );
        }
        assert_eq!(first.len(), 4);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), starts.recv())
                .await
                .is_err(),
            "writer runtime started a fifth blocking worker"
        );
        release.release();
        let fifth = tokio::time::timeout(Duration::from_secs(2), starts.recv())
            .await?
            .ok_or("fifth blocking worker never started")?;
        assert!(!first.contains(&fifth));
        while let Some(result) = jobs.join_next().await {
            result?;
        }
        Ok::<(), Box<dyn std::error::Error>>(())
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_closes_admission_then_drains_an_already_admitted_preparation()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary = CanonicalTempDir::new()?;
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

struct CapacityBackend {
    resolved: tokio::sync::mpsc::UnboundedSender<WorkspaceId>,
    release: Arc<ReleaseGate>,
    commits: Arc<AtomicUsize>,
}

impl WriterBackend for CapacityBackend {
    fn resolve_registration(
        &self,
        database: &Arc<Database>,
        paths: &GlobalStatePaths,
        request: &IpcRequest,
    ) -> Result<(RegistrationPreparationInput, RegistrationSemantics), IpcError> {
        let resolved = resolve_registration_admission(database, paths, request)?;
        let _ = self.resolved.send(resolved.0.workspace_id);
        Ok(resolved)
    }

    fn prepare_registration(
        &self,
        _input: RegistrationPreparationInput,
    ) -> Result<Box<dyn RegistrationCommit>, IpcError> {
        self.release.wait();
        Ok(Box::new(CapacityCommit {
            commits: Arc::clone(&self.commits),
        }))
    }

    fn inspect_evidence(
        &self,
        _input: EvidenceInspectionInput,
    ) -> Result<serde_json::Value, IpcError> {
        Err(IpcError::Protocol(
            "capacity backend did not expect evidence inspection".to_owned(),
        ))
    }
}

struct CapacityCommit {
    commits: Arc<AtomicUsize>,
}

impl RegistrationCommit for CapacityCommit {
    fn commit(
        self: Box<Self>,
        _database: &Database,
        _paths: &GlobalStatePaths,
    ) -> Result<bool, IpcError> {
        self.commits.fetch_add(1, Ordering::SeqCst);
        Ok(false)
    }
}

async fn fill_internal_admission_capacity(
    temporary: &std::path::Path,
    writer: &WriterIngress,
    resolutions: &mut tokio::sync::mpsc::UnboundedReceiver<WorkspaceId>,
) -> Result<Vec<tokio::sync::oneshot::Receiver<IpcResponse>>, Box<dyn std::error::Error>> {
    let mut replies = Vec::new();
    for index in 0..MAX_PENDING_WRITER_ADMISSIONS {
        let repository = temporary.join(format!("repository-{index}"));
        fs::create_dir_all(&repository)?;
        let (response, reply) = tokio::sync::oneshot::channel();
        writer
            .send(WriterRequest {
                request: register_request(index, &repository),
                response,
            })
            .await?;
        tokio::time::timeout(Duration::from_secs(2), resolutions.recv())
            .await?
            .ok_or("registration was not resolved")?;
        if index == 0 {
            drop(reply);
        } else {
            replies.push(reply);
        }
    }
    Ok(replies)
}

async fn fill_data_ingress(
    writer: &WriterIngress,
) -> Result<Vec<tokio::sync::oneshot::Receiver<IpcResponse>>, Box<dyn std::error::Error>> {
    let mut replies = Vec::new();
    for index in 0..WRITER_INGRESS_CAPACITY {
        let (response, reply) = tokio::sync::oneshot::channel();
        writer
            .send(WriterRequest {
                request: ordinary_request(format!("buffered-{index}")),
                response,
            })
            .await?;
        replies.push(reply);
    }
    Ok(replies)
}

fn ordinary_request(request_id: String) -> IpcRequest {
    IpcRequest {
        schema_version: IPC_SCHEMA_VERSION,
        request_id,
        workspace_id: None,
        action: "workspace.selection".to_owned(),
        payload: json!({}),
    }
}

fn stop_request(request_id: &str) -> IpcRequest {
    IpcRequest {
        schema_version: IPC_SCHEMA_VERSION,
        request_id: request_id.to_owned(),
        workspace_id: None,
        action: "daemon.stop".to_owned(),
        payload: json!({}),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn priority_stop_at_capacity_promptly_rejects_unadmitted_work_and_drains_the_prefix()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary = CanonicalTempDir::new()?;
    let paths = GlobalStatePaths::resolve(&StateEnvironment::with_colay_home(
        temporary.path().join("global"),
    )?)?;
    let database = Arc::new(Database::open(&paths.database)?);
    database.migrate_with_backup(&paths.backups)?;
    let (resolved, mut resolutions) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(ReleaseGate::closed());
    let commits = Arc::new(AtomicUsize::new(0));
    let backend = Arc::new(CapacityBackend {
        resolved,
        release: Arc::clone(&release),
        commits: Arc::clone(&commits),
    });
    let (writer, writer_thread) =
        spawn_writer_thread_with_backend(Arc::clone(&database), paths, None, backend).await?;

    let admitted_replies =
        fill_internal_admission_capacity(temporary.path(), &writer, &mut resolutions).await?;
    let rejected_replies = fill_data_ingress(&writer).await?;

    let overflow_finished = Arc::new(AtomicBool::new(false));
    let overflow_finished_in_task = Arc::clone(&overflow_finished);
    let overflow_writer = writer.clone();
    let overflow_task = tokio::spawn(async move {
        let (response, reply) = tokio::sync::oneshot::channel();
        drop(reply);
        let result = overflow_writer
            .send(WriterRequest {
                request: ordinary_request("blocked-overflow".to_owned()),
                response,
            })
            .await;
        overflow_finished_in_task.store(true, Ordering::SeqCst);
        result
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !overflow_finished.load(Ordering::SeqCst),
        "overflow request was not backpressured"
    );

    let (stop_response, mut stop_reply) = tokio::sync::oneshot::channel();
    writer
        .send(WriterRequest {
            request: stop_request("priority-stop"),
            response: stop_response,
        })
        .await?;
    let (duplicate_stop_response, duplicate_stop_reply) = tokio::sync::oneshot::channel();
    let duplicate_stop = writer
        .send(WriterRequest {
            request: stop_request("duplicate-stop"),
            response: duplicate_stop_response,
        })
        .await;
    drop(duplicate_stop_reply);
    assert!(
        duplicate_stop.is_err(),
        "a second concurrent stop was accepted"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(500), overflow_task)
            .await??
            .is_err(),
        "blocked data sender survived priority stop"
    );
    for reply in rejected_replies {
        let response = tokio::time::timeout(Duration::from_millis(500), reply).await??;
        assert_eq!(response.outcome["status"], "error");
        assert_eq!(
            response.outcome["error"],
            "daemon writer stopped before admitting request"
        );
    }
    assert!(
        matches!(
            stop_reply.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ),
        "stop replied before the admitted prefix drained"
    );

    release.release();
    for reply in admitted_replies {
        let response = tokio::time::timeout(Duration::from_secs(2), reply).await??;
        assert_eq!(response.outcome["status"], "ok");
    }
    let stopped = tokio::time::timeout(Duration::from_secs(2), stop_reply).await??;
    assert_eq!(stopped.outcome["status"], "ok");
    assert_eq!(
        commits.load(Ordering::SeqCst),
        MAX_PENDING_WRITER_ADMISSIONS,
        "stop did not drain every admitted registration, including the dropped receiver"
    );

    drop(writer);
    writer_thread.join().await?;
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
    let temporary = CanonicalTempDir::new()?;
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
    let temporary = CanonicalTempDir::new()?;
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
    let temporary = CanonicalTempDir::new()?;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn different_semantics_for_one_workspace_never_overlap()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary = CanonicalTempDir::new()?;
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
    let (writer, writer_thread) =
        spawn_writer_thread_with_backend(Arc::clone(&database), paths, None, backend).await?;

    let (first_response, first_reply) = tokio::sync::oneshot::channel();
    writer
        .send(WriterRequest {
            request: register_request_with_state(0, &repository, ".colay"),
            response: first_response,
        })
        .await?;
    let workspace_id = tokio::time::timeout(Duration::from_secs(2), starts.recv())
        .await?
        .ok_or("first preparation did not start")?;

    let (second_response, second_reply) = tokio::sync::oneshot::channel();
    writer
        .send(WriterRequest {
            request: register_request_with_state(1, &repository, ".other-colay"),
            response: second_response,
        })
        .await?;
    assert!(
        tokio::time::timeout(Duration::from_millis(100), starts.recv())
            .await
            .is_err(),
        "different semantics overlapped within one workspace lane"
    );

    release.release();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), starts.recv())
            .await?
            .ok_or("second preparation did not start")?,
        workspace_id
    );
    assert_eq!(first_reply.await?.outcome["status"], "ok");
    assert_eq!(second_reply.await?.outcome["status"], "ok");

    drop(writer);
    writer_thread.join().await?;
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
    let temporary = CanonicalTempDir::new()?;
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
    success_started: tokio::sync::mpsc::UnboundedSender<&'static str>,
    success_release: Arc<ReleaseGate>,
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
        let _ = self.success_started.send("success-preparation-started");
        self.success_release.wait();
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
    let temporary = CanonicalTempDir::new()?;
    let paths = GlobalStatePaths::resolve(&StateEnvironment::with_colay_home(
        temporary.path().join("global"),
    )?)?;
    let database = Arc::new(Database::open(&paths.database)?);
    database.migrate_with_backup(&paths.backups)?;
    let repository = temporary.path().join("repository");
    fs::create_dir_all(&repository)?;
    let (drops, mut dropped) = tokio::sync::mpsc::unbounded_channel();
    let (success_starts, mut success_started) = tokio::sync::mpsc::unbounded_channel();
    let success_release = Arc::new(ReleaseGate::closed());
    let (commits, mut committed) = tokio::sync::mpsc::unbounded_channel();
    let backend = Arc::new(ErrorThenSuccessBackend {
        calls: AtomicUsize::new(0),
        drops,
        success_started: success_starts,
        success_release: Arc::clone(&success_release),
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
    let (success_response, mut success_reply) = tokio::sync::oneshot::channel();
    writer
        .send(WriterRequest {
            request: register_request_with_state(1, &repository, ".other-colay"),
            response: success_response,
        })
        .await?;

    let failed = tokio::time::timeout(Duration::from_secs(2), failed_reply).await;
    let dropped_guard = tokio::time::timeout(Duration::from_secs(2), dropped.recv()).await;
    let started = tokio::time::timeout(Duration::from_secs(2), success_started.recv()).await;
    let commit_before_release = committed.try_recv();
    let activation_before_release = activated.try_recv();
    let response_before_release = success_reply.try_recv();
    success_release.release();

    let failed = failed??;
    assert_eq!(failed.outcome["status"], "error");
    assert_eq!(
        dropped_guard?,
        Some("preparation-guard-dropped"),
        "failed preparation did not drop its owned guard"
    );
    assert_eq!(
        started?,
        Some("success-preparation-started"),
        "failed preparation did not advance its workspace lane"
    );
    assert!(matches!(
        commit_before_release,
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    assert!(matches!(
        activation_before_release,
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    assert!(matches!(
        response_before_release,
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));

    let success = tokio::time::timeout(Duration::from_secs(2), success_reply).await??;
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
