# Chat-First TUI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the fixed five-panel dashboard flow with a reconnectable, responsive chat workspace that can inspect multiple tasks, preserve an explicit composer target, submit durable session messages, and keep existing administration controls reachable.

**Architecture:** Add a focused `chat` module tree to `orchestrator-tui`; its presentation-only model, reducer, responsive layout, renderer, and terminal runtime do not depend on persistence or provider crates. Add idempotent session-command processing to the repository daemon and a CLI-owned workspace driver that translates SQLite/domain records into TUI-safe snapshots. Keep the existing dashboard API as the administration compatibility surface while Phase 2 uses the daemon for durable session/message commands and existing typed task controls for current single-task execution.

**Tech Stack:** Rust 1.95, Ratatui 0.29, Crossterm 0.29, Tokio 1, SQLite/rusqlite, Serde, existing Colay domain/state/process redaction contracts.

## Global Constraints

- Preserve the provider-neutral, I/O-free `orchestrator-domain` boundary and the one-way dependency direction.
- Use only official provider CLIs; Phase 2 tests must not start provider inference and must use fake binaries where provider fixtures are required.
- Do not add a socket, HTTP service, MCP server, telemetry, or other network control plane.
- Selecting a task changes the viewed task and inspector only; it must never silently change `ComposerTarget`.
- The composer defaults to `orchestrator` and visibly shows `orchestrator`, a single task, or `all running` before submission.
- At widths `>= 110`, render three panes; `80..=109`, use an inspector overlay; `60..=79`, render one switchable primary view; `< 60`, render safe compact status without accepting mutations.
- Do not rely on color alone; every state and focus indication has a symbol, border, or text label.
- Poll durable projection changes every 100-250 milliseconds and make committed daemon changes observable within 500 milliseconds in tests.
- Bound the visible timeline to paged batches and prove that a 1,000-message session can render without loading unbounded artifacts.
- Preserve the current dashboard controls and provider/profile editors as an administration compatibility surface.
- Phase 2 does not claim graph planning, dependency scheduling, parallel execution, task instruction delivery, or integration; palette entries for later phases are typed and visibly unavailable rather than executing a partial substitute.

---

## File Structure

| File | Responsibility |
| --- | --- |
| `crates/orchestrator-tui/src/chat/mod.rs` | Public chat workspace facade and re-exports |
| `crates/orchestrator-tui/src/chat/model.rs` | Presentation-safe snapshots, targets, actions, feedback, cursors |
| `crates/orchestrator-tui/src/chat/layout.rs` | Pure responsive layout classification and rectangles |
| `crates/orchestrator-tui/src/chat/state.rs` | Focus, selection, scrolling, composer, overlays, reducer |
| `crates/orchestrator-tui/src/chat/input.rs` | Key/command parsing into typed reducer effects |
| `crates/orchestrator-tui/src/chat/render.rs` | Three-pane, overlay, compact, composer rendering |
| `crates/orchestrator-tui/src/chat/runtime.rs` | Terminal guard, polling loop, backend driver boundary |
| `crates/orchestrator-daemon/src/commands.rs` | Idempotent create-session/append-message command execution |
| `crates/orchestrator-state/src/client_commands.rs` | Command lookup plus claim/replay helpers used by daemon and CLI |
| `crates/orchestrator-state/src/sessions.rs` | Message pagination and exact-message reconciliation reads |
| `crates/orchestrator-cli/src/chat_tui.rs` | SQLite-backed `WorkspaceDriver`, daemon bootstrap, snapshot mapping |
| `crates/orchestrator-cli/src/app.rs` | Dispatch chat TUI and retain legacy administration action application |
| `crates/orchestrator-cli/tests/chat_tui_reconnect.rs` | Process/state reconnect and daemon-survival contract |

---

## Task 1: Presentation-Safe Chat Workspace Model

**Files:**
- Create: `crates/orchestrator-tui/src/chat/mod.rs`
- Create: `crates/orchestrator-tui/src/chat/model.rs`
- Modify: `crates/orchestrator-tui/src/lib.rs`

**Interfaces:**
- Produces: `WorkspaceSnapshot`, `WorkspaceCursor`, `ComposerTarget`, `WorkspaceAction`, `TaskControlIntent`, `ActionFeedback`, and display row types.
- Consumes: only `serde` and owned presentation-safe strings.

- [ ] **Step 1: Write model serialization and invariant tests**

Add tests proving the default target is orchestrator, task targets survive JSON round-trip, daemon connectivity has explicit online/stale/offline states, and a snapshot rejects duplicate task keys or an inspector whose task is absent.

```rust
#[test]
fn composer_target_round_trip_is_explicit() -> Result<(), serde_json::Error> {
    let target = ComposerTarget::Task("task-03".to_owned());
    let json = serde_json::to_string(&target)?;
    assert_eq!(serde_json::from_str::<ComposerTarget>(&json)?, target);
    assert_eq!(ComposerTarget::default(), ComposerTarget::Orchestrator);
    Ok(())
}
```

- [ ] **Step 2: Run the focused tests and observe failure**

Run: `cargo test -p orchestrator-tui chat::model`

Expected: FAIL because the chat module and types do not exist.

- [ ] **Step 3: Implement the model contracts**

Define the following public contracts and `WorkspaceSnapshot::validate`:

```rust
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceCursor {
    pub message_ordinal: i64,
    pub event_sequence: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "target", content = "id", rename_all = "snake_case")]
pub enum ComposerTarget {
    #[default]
    Orchestrator,
    Task(String),
    AllRunning,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonConnectivity { Online, Stale, Offline }

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceSnapshot {
    pub repository: String,
    pub session_id: String,
    pub session_title: String,
    pub session_state: String,
    pub daemon: DaemonConnectivity,
    pub running_count: usize,
    pub blocked_count: usize,
    pub tasks: Vec<TaskSummary>,
    pub messages: Vec<TimelineEntry>,
    pub attention: Vec<AttentionItem>,
    pub inspector: Option<TaskInspector>,
    pub cursor: WorkspaceCursor,
    pub read_only_reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceAction {
    SubmitMessage { target: ComposerTarget, content: String },
    RequestTaskControl { task_id: String, intent: TaskControlIntent },
    OpenAdministration,
    Quit,
}
```

Use concise row types for task state symbol/label, timeline role/kind/state/content, attention severity, and inspector provider/profile/dependencies/files/tests. Reject blank IDs/content and snapshots with inconsistent selected inspector data.

- [ ] **Step 4: Run model tests**

Run: `cargo test -p orchestrator-tui chat::model`

Expected: PASS.

- [ ] **Step 5: Commit the model boundary**

```text
git add crates/orchestrator-tui/src/chat crates/orchestrator-tui/src/lib.rs
git commit -m "feat: define chat workspace presentation model"
```

## Task 2: Responsive Layout Contract

**Files:**
- Create: `crates/orchestrator-tui/src/chat/layout.rs`
- Modify: `crates/orchestrator-tui/src/chat/mod.rs`

**Interfaces:**
- Consumes: `ratatui::layout::Rect` and the active narrow view.
- Produces: `LayoutMode`, `PrimaryView`, `WorkspaceLayout`, and `compute_layout`.

- [ ] **Step 1: Write exact width-boundary tests**

```rust
#[test]
fn responsive_thresholds_are_exact() {
    assert_eq!(compute_layout(Rect::new(0, 0, 110, 30), PrimaryView::Conversation).mode, LayoutMode::Wide);
    assert_eq!(compute_layout(Rect::new(0, 0, 109, 30), PrimaryView::Conversation).mode, LayoutMode::Medium);
    assert_eq!(compute_layout(Rect::new(0, 0, 80, 30), PrimaryView::Conversation).mode, LayoutMode::Medium);
    assert_eq!(compute_layout(Rect::new(0, 0, 79, 30), PrimaryView::Conversation).mode, LayoutMode::Narrow);
    assert_eq!(compute_layout(Rect::new(0, 0, 60, 20), PrimaryView::Conversation).mode, LayoutMode::Narrow);
    assert_eq!(compute_layout(Rect::new(0, 0, 59, 20), PrimaryView::Conversation).mode, LayoutMode::TooSmall);
}
```

Also prove every non-compact mode reserves one header row and three composer rows, and no computed rectangle escapes the terminal area at 60x20.

- [ ] **Step 2: Run and observe the missing layout API**

Run: `cargo test -p orchestrator-tui chat::layout`

Expected: FAIL.

- [ ] **Step 3: Implement pure layout computation**

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutMode { Wide, Medium, Narrow, TooSmall }

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PrimaryView { Tasks, #[default] Conversation, Inspector }

pub struct WorkspaceLayout {
    pub mode: LayoutMode,
    pub header: Rect,
    pub task_graph: Option<Rect>,
    pub conversation: Option<Rect>,
    pub inspector: Option<Rect>,
    pub composer: Option<Rect>,
    pub compact_status: Option<Rect>,
}
```

Use 26/49/25 percentage columns in wide mode, 31/69 in medium mode, and one content rectangle in narrow mode. `TooSmall` exposes only `compact_status` and never a composer.

- [ ] **Step 4: Run layout tests and commit**

Run: `cargo test -p orchestrator-tui chat::layout`

Expected: PASS.

```text
git add crates/orchestrator-tui/src/chat/layout.rs crates/orchestrator-tui/src/chat/mod.rs
git commit -m "feat: compute responsive chat workspace layouts"
```

## Task 3: Interaction State and Explicit Target Reducer

**Files:**
- Create: `crates/orchestrator-tui/src/chat/state.rs`
- Create: `crates/orchestrator-tui/src/chat/input.rs`
- Modify: `crates/orchestrator-tui/src/chat/mod.rs`

**Interfaces:**
- Consumes: `WorkspaceSnapshot`, `ComposerTarget`, and Crossterm `KeyCode`/modifiers.
- Produces: `WorkspaceState::handle_key`, `UiEffect`, overlay state, focus traversal, task selection, command parsing.

- [ ] **Step 1: Write reducer tests for navigation and safety**

Cover:

- selecting `task-03` updates `selected_task` but leaves `composer_target == Orchestrator`;
- `Ctrl+T` opens a target picker and an explicit selection changes the target;
- `@task-03 message` and `@all message` override only that submission, not the stored target;
- `Tab`/`BackTab`, `j`/`k`, quick switch, overview, log, help, and Escape follow the approved bindings;
- `Ctrl+Space` produces pause/resume only for the selected task;
- blank messages never produce an action;
- compact mode produces no mutation action;
- `/tasks`, `/plan`, `/approve`, `/pause`, `/resume`, `/cancel`, `/handover`, `/retry`, `/checkpoint`, and `/provider` parse to typed effects; Phase 3+ actions return explicit `ActionFeedback::Unavailable`.

```rust
#[test]
fn task_selection_never_retargets_composer() {
    let mut state = WorkspaceState::default();
    state.select_task(Some("task-03".to_owned()));
    assert_eq!(state.selected_task(), Some("task-03"));
    assert_eq!(state.composer_target(), &ComposerTarget::Orchestrator);
}
```

- [ ] **Step 2: Run and observe failure**

Run: `cargo test -p orchestrator-tui chat::state chat::input`

Expected: FAIL.

- [ ] **Step 3: Implement reducer and command parser**

```rust
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FocusPane { Tasks, #[default] Conversation, Inspector, Composer }

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Overlay {
    TaskSwitcher,
    Overview,
    FullLog,
    TargetPicker,
    CommandPalette,
    Help,
    Inspector,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UiEffect {
    None,
    Dispatch(WorkspaceAction),
    Feedback(ActionFeedback),
    Redraw,
}
```

Store task selection and composer target in separate fields. Parse mention prefixes into a one-shot `(ComposerTarget, content)` value. Clamp selections and scroll offsets whenever a refreshed snapshot removes rows. Preserve composer text, target, selection, and scroll across refreshes.

- [ ] **Step 4: Run reducer tests and commit**

Run: `cargo test -p orchestrator-tui chat::state chat::input`

Expected: PASS.

```text
git add crates/orchestrator-tui/src/chat/state.rs crates/orchestrator-tui/src/chat/input.rs crates/orchestrator-tui/src/chat/mod.rs
git commit -m "feat: add chat workspace navigation and composer state"
```

## Task 4: Chat-First Renderer and Snapshot Coverage

**Files:**
- Create: `crates/orchestrator-tui/src/chat/render.rs`
- Modify: `crates/orchestrator-tui/src/chat/mod.rs`

**Interfaces:**
- Consumes: `WorkspaceSnapshot`, `WorkspaceState`, and `WorkspaceLayout`.
- Produces: `render_workspace(frame, snapshot, state)`.

- [ ] **Step 1: Write TestBackend render contracts**

At 160x40 and 110x30 assert visible header, `TASK GRAPH`, `CONVERSATION`, `INSPECTOR`, `ATTENTION`, target label, state symbols, and focused border marker. At 100x30 assert inspector is absent until its overlay opens. At 70x24 assert only the selected primary view plus composer. At 59x20 assert the compact safety message and absence of composer input.

Add a 1,000-message fixture with a 100-row visible page and assert render completes with a `more messages` marker and folded tool summaries.

```rust
fn rendered_text(width: u16, height: u16, snapshot: &WorkspaceSnapshot, state: &WorkspaceState) -> Result<String, io::Error> {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend)?;
    terminal.draw(|frame| render_workspace(frame, snapshot, state))?;
    Ok(terminal.backend().buffer().content().iter().map(Cell::symbol).collect())
}
```

- [ ] **Step 2: Run and observe failure**

Run: `cargo test -p orchestrator-tui chat::render`

Expected: FAIL.

- [ ] **Step 3: Implement renderer**

Render:

- header: repository, session, counts, and textual daemon state;
- task pane: state symbol plus title, dependency/wait text, selected marker, attention list;
- conversation: role/task prefix, message kind/state, wrapped redacted content, folded tool rows;
- inspector: selected task state, provider/profile, elapsed/progress, dependencies, worktree, files, tests;
- composer: `to: [target]`, input, and send hint;
- overlays: switcher, overview, log, target picker, palette, help, and medium inspector.

Use symbols plus labels (`* RUNNING`, `! BLOCKED`, `o DONE`, `- WAITING`) so monochrome output remains complete. Never render raw artifact bodies.

- [ ] **Step 4: Run renderer tests and commit**

Run: `cargo test -p orchestrator-tui chat::render`

Expected: PASS.

```text
git add crates/orchestrator-tui/src/chat/render.rs crates/orchestrator-tui/src/chat/mod.rs
git commit -m "feat: render responsive chat-first workspace"
```

## Task 5: Persistent Terminal Runtime and Driver Boundary

**Files:**
- Create: `crates/orchestrator-tui/src/chat/runtime.rs`
- Modify: `crates/orchestrator-tui/src/chat/mod.rs`
- Modify: `crates/orchestrator-tui/src/lib.rs`

**Interfaces:**
- Produces: `WorkspaceDriver`, `DriverError`, `run_workspace`, and a public chat facade.
- Consumes: renderer/reducer and the existing terminal guard behavior.

- [ ] **Step 1: Write fake-driver runtime tests**

Use a scripted event source and fake terminal control to prove refresh at 200ms, dispatch without leaving the workspace, feedback propagation, offline read-only mutation suppression, Quit, and restoration after draw/read/driver failure. Reuse the current raw-mode/alternate-screen failure tests rather than weakening them.

```rust
pub trait WorkspaceDriver {
    fn refresh(&mut self, cursor: &WorkspaceCursor) -> Result<WorkspaceSnapshot, DriverError>;
    fn dispatch(&mut self, action: WorkspaceAction) -> Result<ActionFeedback, DriverError>;
}
```

- [ ] **Step 2: Run and observe failure**

Run: `cargo test -p orchestrator-tui chat::runtime`

Expected: FAIL.

- [ ] **Step 3: Implement the runtime**

Keep one alternate-screen/raw-mode guard for the whole session. Poll input at no more than 50ms while scheduling snapshot refreshes every 200ms. Dispatch typed actions through the driver, preserve `WorkspaceState`, refresh immediately after a successful dispatch, and render driver errors as non-destructive feedback. When `read_only_reason` is present or layout is too small, allow navigation/quit but reject mutations locally.

```rust
pub fn run_workspace<D: WorkspaceDriver>(driver: &mut D) -> Result<(), TuiError> {
    run_workspace_with(driver, CrosstermTerminalControl, CrosstermEvents)
}
```

- [ ] **Step 4: Run all TUI tests and commit**

Run: `cargo test -p orchestrator-tui`

Expected: PASS, including legacy dashboard tests.

```text
git add crates/orchestrator-tui/src/chat crates/orchestrator-tui/src/lib.rs
git commit -m "feat: run reconnectable chat workspace loop"
```

## Task 6: Idempotent Daemon Session Command Processing

**Files:**
- Create: `crates/orchestrator-daemon/src/commands.rs`
- Modify: `crates/orchestrator-daemon/src/lib.rs`
- Modify: `crates/orchestrator-state/src/client_commands.rs`
- Modify: `crates/orchestrator-state/src/sessions.rs`
- Modify: `crates/orchestrator-domain/src/session.rs`

**Interfaces:**
- Produces: typed `CreateSessionCommandPayload`, `AppendMessageCommandPayload`, `MessageRedactor`, `process_next_client_command`, command lookup, and exact projection reconciliation.
- Consumes: Phase 1 command claim/complete/fail, session/message persistence, heartbeat loop.

- [ ] **Step 1: Write processor and crash-replay tests**

Prove create-session and append-message processing, redaction before persistence, task target preservation, command outcome completion, malformed payload failure, missing target failure, and recovery when a crash occurs after projection insertion but before command completion. The replay must recognize an exact existing session/message and complete the command without duplicating it; a mismatched existing projection fails closed.

```rust
pub trait MessageRedactor: Send + Sync {
    fn redact(&self, value: &str) -> String;
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppendMessageCommandPayload {
    pub message_id: MessageId,
    pub content: String,
}
```

- [ ] **Step 2: Run and observe failure**

Run: `cargo test -p orchestrator-daemon commands`

Expected: FAIL.

- [ ] **Step 3: Implement processor semantics**

Add `Database::load_client_command`, `Database::load_message`, and exact-match helpers. Claim one command, deserialize by typed action, normalize and redact content, apply the projection, append the corresponding audit event, and complete with a concise outcome. On errors, fail the command with a redacted outcome. Keep `StopDaemon` in the heartbeat path and never blindly process a stale claimed stop.

Add a 100ms `command_poll_interval` to `DaemonSettings`; select between cancellation, heartbeat ticks, and command ticks. Retain the current `serve` API with an identity-redactor compatibility wrapper for tests, and add:

```rust
pub async fn serve_with_commands(
    database: &Database,
    instance_id: DaemonInstanceId,
    pid: u32,
    cancellation: CancellationToken,
    settings: DaemonSettings,
    redactor: &dyn MessageRedactor,
) -> Result<DaemonExit, DaemonError>;
```

- [ ] **Step 4: Run daemon/state/domain tests and commit**

Run: `cargo test -p orchestrator-domain session && cargo test -p orchestrator-state client_commands sessions && cargo test -p orchestrator-daemon`

Expected: PASS.

```text
git add crates/orchestrator-domain/src/session.rs crates/orchestrator-state/src/client_commands.rs crates/orchestrator-state/src/sessions.rs crates/orchestrator-daemon/src
git commit -m "feat: process durable session chat commands"
```

## Task 7: SQLite Workspace Projection Reads

**Files:**
- Create: `crates/orchestrator-state/src/workspace.rs`
- Modify: `crates/orchestrator-state/src/lib.rs`
- Modify: `crates/orchestrator-state/src/sessions.rs`

**Interfaces:**
- Produces: `WorkspaceReadRequest`, `WorkspaceProjection`, `WorkspaceTask`, `WorkspaceAttention`, and `Database::read_workspace_projection`.
- Consumes: sessions/messages/tasks/attempts/worktrees/checkpoints/verifications and daemon status.

- [ ] **Step 1: Write projection and pagination tests**

Seed two sessions, 1,000 messages, running/blocked/completed tasks, attempts, worktree metadata, and verification rows. Prove session isolation, ascending timeline order, `before_ordinal` pagination, selected-task inspector mapping, attention derivation, latest provider/profile selection, and a bounded SQL query result of at most the requested limit.

```rust
pub struct WorkspaceReadRequest {
    pub session_id: SessionId,
    pub selected_task_id: Option<TaskId>,
    pub before_ordinal: Option<i64>,
    pub message_limit: usize,
    pub task_limit: usize,
}
```

- [ ] **Step 2: Run and observe failure**

Run: `cargo test -p orchestrator-state workspace`

Expected: FAIL.

- [ ] **Step 3: Implement bounded read projection**

Build the projection from read-only queries under one database lock. Limit messages to `1..=200`, tasks to `1..=100`, return `has_older_messages`, and use the existing relational IDs rather than parsing display strings. Phase 2 lists recent repository tasks because session-task graph membership begins in Phase 3; document this field as `recent_tasks` in the state type and map it to the TUI's task pane.

- [ ] **Step 4: Run state tests and commit**

Run: `cargo test -p orchestrator-state workspace sessions`

Expected: PASS.

```text
git add crates/orchestrator-state/src/workspace.rs crates/orchestrator-state/src/sessions.rs crates/orchestrator-state/src/lib.rs
git commit -m "feat: read bounded chat workspace projections"
```

## Task 8: CLI Workspace Driver, Daemon Bootstrap, and Reconnect

**Files:**
- Create: `crates/orchestrator-cli/src/chat_tui.rs`
- Create: `crates/orchestrator-cli/tests/chat_tui_reconnect.rs`
- Modify: `crates/orchestrator-cli/src/main.rs`
- Modify: `crates/orchestrator-cli/src/app.rs`
- Modify: `crates/orchestrator-cli/src/daemon.rs`
- Modify: `crates/orchestrator-cli/Cargo.toml`
- Modify: `crates/orchestrator-cli/src/args.rs`

**Interfaces:**
- Produces: CLI `SqliteWorkspaceDriver` and chat-first `colay tui` behavior.
- Consumes: `run_workspace`, Phase 1 daemon start/status, Phase 2 command processor, redactor, and workspace projection reads.

- [ ] **Step 1: Write driver and process-level reconnect tests**

Test with an isolated repository and `COLAY_HOME`:

- `colay tui` help describes chat workspace rather than five-panel dashboard;
- driver startup initializes state and starts a healthy daemon;
- no session creates one via an idempotent command and waits for completion;
- submitting a message persists redacted content within 500ms;
- selecting a task does not alter the persisted composer target in a scripted fake terminal run;
- closing a scripted TUI leaves daemon online;
- a second driver restores session/messages/selected task from SQLite;
- stale/offline daemon makes mutations read-only until restart;
- administration action invokes the preserved legacy dashboard adapter;
- test cleanup stops the daemon and leaves no child.

Expose a test-fixture-only scripted input entry point rather than trying to automate a real terminal.

- [ ] **Step 2: Run and observe failure**

Run: `cargo test -p colay --features test-fixtures chat_tui`

Expected: FAIL.

- [ ] **Step 3: Implement CLI driver and dispatch**

Make daemon start reusable:

```rust
pub(crate) async fn ensure_started(
    repository: &Path,
    config: &RootConfig,
    explicit_config: Option<&Path>,
) -> Result<DaemonStatus>;
```

`SqliteWorkspaceDriver` owns repository paths, database, active session ID, selected task, redactor, and the existing dashboard snapshot builder. `refresh` calls the bounded state projection and maps only redacted strings. `SubmitMessage` creates a UUID-v7 message ID and command ID/idempotency key, writes the durable command, and lets the daemon process it. Task controls use the existing typed control records; later-phase palette actions return explicit unavailable feedback. `OpenAdministration` exits to the legacy dashboard once, applies its typed action through existing app functions, and resumes the chat workspace with the same session and composer state.

Update the hidden daemon serve path to call `serve_with_commands` with the configured process redactor. Do not add provider execution to the daemon in Phase 2.

- [ ] **Step 4: Run CLI/TUI reconnect tests and commit**

Run: `cargo test -p colay --features test-fixtures chat_tui daemon && cargo test -p orchestrator-tui`

Expected: PASS with no child process remaining after each test.

```text
git add crates/orchestrator-cli crates/orchestrator-tui Cargo.lock
git commit -m "feat: connect chat-first tui to durable sessions"
```

## Task 9: Phase 2 Documentation, Accessibility, and Verification Gate

**Files:**
- Modify: `README.md`
- Modify: `docs/architecture.md`
- Modify: `docs/operations.md`
- Modify: `docs/testing.md`
- Modify: `docs/threat-model.md`
- Create: `docs/superpowers/audits/2026-07-21-chat-first-tui-phase2.md`

**Interfaces:**
- Documents: chat workspace navigation, explicit targeting, reconnect behavior, responsive modes, current Phase 2 limitations, and verification evidence.

- [ ] **Step 1: Add documentation contract assertions**

Extend CLI/TUI tests so README/help must contain the primary chat TUI, `Ctrl+T`, `/tasks`, daemon reconnect behavior, and the explicit statement that graph planning/parallel execution arrive in later phases.

- [ ] **Step 2: Update user and operator documentation**

Document the three-pane text layout, bindings, overlays, target safety, narrow-terminal behavior, message redaction, durable command flow, daemon dependency, administration compatibility, and troubleshooting for offline/stale/read-only mode. State clearly that recent tasks are shown before Phase 3 graph membership exists.

- [ ] **Step 3: Run focused gates**

```text
cargo fmt --all -- --check
cargo test -p orchestrator-tui
cargo test -p orchestrator-daemon
cargo test -p orchestrator-state workspace sessions client_commands
cargo test -p colay --features test-fixtures chat_tui daemon
git diff --check
```

Expected: every command exits 0.

- [ ] **Step 4: Run repository-wide gates**

```text
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

Expected: both exit 0; all provider-facing tests use fake binaries.

- [ ] **Step 5: Record the Phase 2 completion audit**

Record direct evidence for wide/medium/narrow/compact layouts, monochrome labels, pane traversal, task quick switch, target preservation, command palette, bounded 1,000-message rendering, durable redacted message submission, disconnect/reconnect, daemon survival, read-only stale mode, legacy administration access, terminal restoration, no network listener, no provider inference, and full gates. Missing direct evidence keeps Phase 2 open.

- [ ] **Step 6: Commit docs and audit**

```text
git add README.md docs/architecture.md docs/operations.md docs/testing.md docs/threat-model.md docs/superpowers/audits/2026-07-21-chat-first-tui-phase2.md
git commit -m "docs: describe chat-first tui workspace"
```

## Execution Checkpoints

- **Checkpoint A — after Task 3:** review model purity, exact layout thresholds, and the no-silent-retarget reducer invariant.
- **Checkpoint B — after Task 6:** review terminal restoration and idempotent crash-replay command handling before connecting persistence to the UI.
- **Checkpoint C — after Task 8:** run scripted disconnect/reconnect and inspect that the daemon survives while no provider process starts.
- **Checkpoint D — after Task 9:** review the audit and full workspace gates before starting Phase 3.

## Self-Review

- Every Phase 2 deliverable in the umbrella spec maps to Tasks 1-9: responsive shell, timeline, task switching, explicit target, attention, palette, administration, daemon bootstrap, reconnect, accessibility, and performance bounds.
- Later-phase `/plan`, `/approve`, `/retry`, and integration behavior is explicitly typed but unavailable; the plan does not claim partial graph scheduling.
- Domain/state/TUI/CLI type names are consistent across producer and consumer tasks.
- The plan contains no skipped implementation placeholders and every task has a failing test, implementation, passing test, and commit step.
