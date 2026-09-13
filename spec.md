# Incremental process state storage

## Goals

- Keep SQLite as the shared authority for state written by independent Worklight CLI and TUI processes.
- Keep the TUI's actionable process list—running and completed-but-unacknowledged processes—in memory and render from that list without querying SQLite during drawing.
- Keep completed-and-acknowledged history in SQLite for CLI history queries, but do not load it into a newly opened panel.
- Read immutable process and orchestrator identity data once when a process first enters the TUI list.
- Import later state transitions incrementally.
- Stop polling a field after it reaches a state from which it cannot change again.
- Expose each state transition as one operation that persists the transition and updates the supplied in-memory process. There must be no mutate-then-save workflow.
- Permit each command to update only the fields it owns.
- Keep a process attached to the same concrete orchestrator row for its lifetime while allowing that concrete implementation to refresh its permitted columns.

## Non-goals

- No new orchestrator supertable.
- No flattened union of all orchestrator-specific columns.
- No event/change-log table, persisted reader cursor, notification daemon, background thread, filesystem watcher, or SQLite update hook.
- No new pane-name field or other orchestrator metadata that is not currently stored.
- No process deletion. Supporting deletion would require a retained tombstone or change record and is a separate design.
- No detection of unsupported direct SQL writes. Supported writers are Worklight operations that follow this protocol.
- No pagination. The panel holds the complete actionable set; `get` and `list` continue to expose acknowledged history from SQLite.
- No change to CLI output columns or process display ordering.

## Success criteria

- A TUI draw performs zero storage calls.
- Initial panel loading reads each running or completed-but-unacknowledged process completely once and reads no completed-and-acknowledged process row.
- With the actionable set held constant, panel startup work and memory do not grow with the number of completed-and-acknowledged rows. The initial query is served by a partial index containing only actionable rows, and the synchronization high-water mark comes from a singleton sequence row.
- A newly started process appears in an open panel within the existing one-second synchronization interval.
- A completion is imported once as a completion patch; the process is not subsequently queried for completion again.
- An acknowledgment is imported once as a removal patch; the process leaves the panel and is not subsequently queried by the panel.
- Immutable process fields and immutable orchestrator identity are absent from update statements.
- `start`, `finish`, and `acknowledge` have no generic whole-record persistence path.
- A failed transaction leaves the database and the supplied in-memory object unchanged.
- Concurrent operations retain first-completion-wins and monotonic-acknowledgment semantics.
- The process's selected foreign key, concrete orchestrator variant, and referenced row ID never change after creation.
- Empty synchronization polls are bounded index lookups and do not scan process history or acknowledged history.

# Design

## Runtime constraints

Worklight does not execute the labeled command. `start` and `finish` are separate CLI invocations. An open panel is another long-running OS process. These processes share SQLite but do not share memory.

Consequently:

- SQLite serializes and validates writes;
- each CLI invocation keeps only the process values needed for that command;
- the panel maintains a materialized in-memory view;
- the panel imports external writes through bounded incremental polling because SQLite provides no cross-process change stream;
- in-memory state is updated only from a committed transition or a consistent read snapshot.

## Process ownership and transitions

| Field | Initial writer | Later writer | Legal change |
|---|---|---|---|
| `id` | `start` | none | immutable |
| `label` | `start` | none | immutable |
| `started_at` | `start` | none | immutable |
| `shell_id` / `tmux_id` | `start` | none | exactly one selected for the process lifetime |
| `finished_at` | none | first accepted `finish` | `NULL -> value` once |
| `exit_code` | none | first accepted `finish` | `NULL -> 0..=255` once |
| `acknowledged` | `start` as false | `acknowledge` or successful `focus` | `false -> true` once after completion |
| `created_seq` | `start` | none | one positive global transition sequence |
| `finished_seq` | none | first accepted `finish` | `NULL -> positive sequence` once |
| `acknowledged_seq` | none | first accepted acknowledgment | `NULL -> positive sequence` once |
| displayed state | derived | none | derived from `exit_code` |
| elapsed duration | derived | none | derived from timestamps |

The three revision-marker columns record the transitions that can happen to a process. Separate markers are required because they let the panel request only the data owned by each transition:

- `created_seq` discovers a new process and loads the whole object;
- `finished_seq` imports completion and the orchestrator refresh committed with it;
- `acknowledged_seq` imports only acknowledgment.

A generic `change_seq` would force an acknowledgment poll to reread completion and orchestrator data that are already terminal. The transition-specific columns avoid that.

Database constraints enforce:

```text
created_seq > 0
finished_seq is present exactly when completion is present
acknowledged_seq is present exactly when acknowledged is true
finished_seq > created_seq when present
acknowledged_seq > finished_seq when present
```

`finished_at` and `exit_code` are set together. Acknowledgment never returns to false.

## Orchestrator ownership and transitions

Keep the existing `shells` and `tmux` tables. A process owns exactly one concrete row. Add uniqueness constraints to non-null `processes.shell_id` and `processes.tmux_id` so one child row cannot be shared by two processes.

The following are immutable after process creation:

- which foreign-key column is selected;
- the referenced child-row ID;
- the `Orchestrator` enum variant.

The concrete implementation decides which columns in its existing row can be refreshed by the first accepted completion. There is no generic orchestrator replacement and no cross-variant conversion.

Current variant behavior:

- **Shell:** preserve the shell row ID and `shell_id`; refresh `cwd` from the accepted finishing invocation.
- **Tmux with a valid tmux finishing context:** preserve the tmux row ID and `tmux_id`; refresh `cwd`, `socket`, `server_instance`, and `pane_id` in the existing row.
- **Tmux without a valid tmux finishing context:** preserve the existing tmux row unchanged while still committing process completion.

A valid tmux context requires:

- `$TMUX` with non-empty socket and server-instance components;
- non-empty `$TMUX_PANE`.

Missing, partial, or malformed tmux data is not converted to a shell record when the stored process already owns a tmux row.

Only the first accepted completion currently updates orchestrator columns. Its `finished_seq` therefore also announces the orchestrator patch. Acknowledgment does not reread or update orchestrator data.

A future orchestrator implementation must explicitly define:

1. its immutable row identity and variant;
2. fields refreshed by an accepted completion;
3. how a compatible environment observation is recognized;
4. behavior when no compatible observation exists;
5. validation and application of its typed completion patch.

If a future feature adds orchestrator changes independent of process completion, that feature must add its own transition marker. It must not overload acknowledgment or force completed orchestrator data to be polled repeatedly.

## Actual command scenarios

The following can occur because `start` and `finish` are independent invocations:

- **Shell start, shell finish:** keep the original shell FK and row ID; refresh its `cwd`; finish the process.
- **Shell start, finish invoked from tmux:** keep the original shell FK, row ID, and shell variant; refresh the shell row's `cwd`; finish the process.
- **Tmux start, same tmux context finish:** keep the original tmux FK and row ID; refresh that row in place; finish the process.
- **Tmux start, another valid tmux context finish:** keep the original tmux FK and row ID; refresh that row's columns with the accepted finishing context; finish the process.
- **Tmux start, finish outside tmux or with malformed tmux data:** keep the original tmux FK, row ID, variant, and columns; finish the process.
- **Same-code repeated finish:** return the first committed completion unchanged; do not observe the environment, update the child row, or allocate a sequence.
- **Different-code repeated finish:** return a conflict and change nothing.
- **Concurrent finish:** `BEGIN IMMEDIATE` serializes contenders. The first completion wins. A same-code loser is unchanged; a different-code loser conflicts.

Moving a tmux pane between windows does not require storing a pane display name. Focus already resolves the pane's current display address from stored navigation data.

## Database revision

A revision is the database-wide, monotonically increasing number used by the panel as its incremental synchronization position. It is not process lifecycle state and is never displayed. Each accepted `start`, first `finish`, or first acknowledgment receives the next revision. For example, a process may have `created_seq = 41`, `finished_seq = 52`, and `acknowledged_seq = 68`; a panel at revision 50 queries for transition markers greater than 50 and discovers the completion at 52.

The three process sequence columns share this one revision number space. A singleton row stores the current revision:

```sql
CREATE TABLE storage_state (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    transition_seq INTEGER NOT NULL CHECK (transition_seq >= 0)
);
```

This row is required for the startup constraint: the panel can establish its synchronization high-water mark with one fixed-size lookup rather than consulting indexes whose size grows with acknowledged history. It also makes write-side allocation independent of process-history size.

While holding `BEGIN IMMEDIATE`, an effective writer reads and increments `storage_state.transition_seq` with checked `i64` arithmetic. If it is `i64::MAX`, the operation fails with storage exhaustion. The increment and process transition commit in the same transaction, so a failed transition does not consume a sequence.

SQLite's immediate transaction serializes sequence allocation with every other supported writer. Allocation occurs only after deciding that the operation is effective:

- `start` allocates `created_seq`;
- first completion allocates `finished_seq`;
- first acknowledgment allocates `acknowledged_seq`;
- previews, idempotent repeats, and rejected transitions allocate nothing.

Timestamps are canonicalized to SQLite's millisecond representation before constructing an in-memory committed result. This ensures a later equal-sequence replay is byte-for-byte equivalent to the local state.

## One mutation API

`ProcessRun`, `ShellRecord`, and `TmuxRecord` are loaded state, not caller-editable desired records. Their stored fields are private.

The application exposes one operation for each command. Internally, storage transition methods receive a loaded `ProcessRun`, validate against current SQLite state, commit, and synchronize that same loaded object before returning.

For an effective transition:

1. begin `IMMEDIATE`;
2. read current mutable state and fixed association from SQLite;
3. validate the request against SQLite, not the potentially stale supplied object;
4. lazily obtain any environment observation required by that transition;
5. allocate one sequence;
6. execute only transition-owned updates;
7. construct the canonical typed patch from values that will be stored;
8. validate and apply the patch to a clone of the supplied object;
9. commit SQLite;
10. replace the supplied object with the validated clone using an infallible assignment;
11. return the transition outcome.

If validation, observation, SQL, patch construction, or commit fails, the supplied object is untouched.

For an idempotent transition, storage reads the authoritative current state and catches a stale supplied object up to that state without writing or allocating a sequence. For a preview, storage validates against a read snapshot but does not mutate either SQLite or the supplied object.

There is no `ProcessRun::finish`/`acknowledge` followed by `Storage::save_process`, no generic patch accepted from a caller, and no generic whole-record update.

## Lazy finish observation

Environment observation must occur only when SQLite confirms that the row is still running. Otherwise a repeated completion could fail because the current directory disappeared even though no observation is needed.

The application passes storage an object-safe lazy observer. Storage invokes it inside the effective first-completion path after reading the running row under `BEGIN IMMEDIATE`.

The observation contains no database row ID and cannot replace the stored association:

```text
cwd
optional valid tmux context { socket, server_instance, pane_id }
```

The stored concrete orchestrator converts this observation into its own typed, variant-preserving patch.

## Initial panel materialization

The panel contains only actionable processes: running processes and completed processes not yet acknowledged. Completed-and-acknowledged history remains available to CLI `get` and `list`, but it is not loaded into the panel.

Add a partial panel index:

```sql
CREATE INDEX processes_panel
    ON processes(started_at DESC, id DESC)
    WHERE acknowledged = 0;

CREATE INDEX processes_panel_created
    ON processes(created_seq)
    WHERE acknowledged = 0;

CREATE INDEX processes_panel_finished
    ON processes(finished_seq)
    WHERE acknowledged = 0;
```

Because database constraints require every running process to be unacknowledged, `acknowledged = 0` selects the complete actionable set. These indexes contain no completed-and-acknowledged rows, so their number does not increase the panel startup scan, actionable creation/completion scans, or panel memory use.

The panel performs one initial storage operation before its first draw:

1. attach to the database if it exists;
2. begin one SQLite read transaction;
3. read `storage_state.transition_seq` as high-water `H`, establishing the read snapshot;
4. read processes satisfying `acknowledged = 0`, with their concrete orchestrators, in `started_at DESC, id DESC` order through `processes_panel`;
5. validate and construct the complete actionable-process array;
6. commit the read transaction;
7. publish the array and synchronization cursor `H` together.

An empty or absent database produces an empty array and cursor zero without creating a file. A later synchronization attaches when another Worklight process creates the database.

No partially decoded array is published. Opening the panel acknowledges nothing.

The panel stores:

```text
processes: Vec<ProcessRun>
by_id: HashMap<process id, vector index>
cursor: Revision
selection: process id plus TableState
```

The array remains sorted by the existing order: `started_at DESC, id DESC`.

## Incremental panel synchronization

The existing one-second timer calls one synchronization operation. Drawing never calls it.

For cursor `C`, storage begins one SQLite read transaction and first reads high-water `H`, establishing one snapshot. It then reads three bounded groups from that same transaction:

1. **Created actionable processes**
   - predicate: `C < created_seq <= H AND acknowledged = 0`;
   - payload: complete process and concrete orchestrator;
   - purpose: create a new in-memory object exactly once.

2. **Completions of known processes that remain actionable**
   - predicate: `created_seq <= C AND C < finished_seq <= H` and no acknowledgment after `C`;
   - payload: process ID, `finished_seq`, canonical `finished_at`, exit code, selected child ID/variant for validation, and the concrete orchestrator's completion fields;
   - purpose: move running to finished and refresh the existing nested orchestrator object;
   - a process acknowledged in the same synchronization window is omitted because it will be removed rather than updated.

3. **Acknowledgments of processes known at `C`**
   - predicate: `created_seq <= C AND C < acknowledged_seq <= H`;
   - payload: process ID and `acknowledged_seq` only;
   - purpose: remove the now-terminal process from the panel without rereading completion, orchestrator data, or immutable process fields.

All queries use indexed sequence ranges. New processes are excluded from patch groups because their complete row already contains their latest state. A process created and acknowledged between polls is omitted entirely: it was never actionable at either synchronization boundary and no panel object needs to be created for it.

The snapshot boundary prevents this lost-update race: a process inserted between two autocommit queries cannot be excluded from the creation query while causing the cursor to advance past it in a later query.

### Batch staging and application

Storage decodes the whole snapshot into a `PanelChanges` value before returning. The panel then stages only affected records; it never clones the full history.

Validation rules:

- a new row normally has an unknown ID;
- a new row whose ID already exists after a local write is treated as a replay only if immutable identity and current state are equivalent;
- a completion patch must identify an existing process with the same selected FK, child ID, and orchestrator variant;
- an acknowledgment removal may identify a cached running process when completion and acknowledgment both happened since `C`; the committed acknowledgment sequence is sufficient to remove it because database constraints guarantee acknowledgment follows completion;
- an acknowledgment for an ID already removed by a successful local action is a harmless replay;
- a transition sequence older than the corresponding cached sequence is ignored after identity validation;
- an equal sequence with equivalent state is an idempotent replay;
- an equal sequence with different state is storage corruption;
- a newer sequence may only perform its legal forward transition.

All affected clones and newly constructed rows are validated before the live vector changes. After validation, applying patches and swapping staged objects are infallible.

New rows arrive sorted by `started_at DESC, id DESC`. Merge them with the existing sorted vector in linear time, remove acknowledged IDs in the same pass, and rebuild the ID index once. Completion patches do not affect ordering because the ordering fields are immutable.

Selection is preserved by process ID when that process remains actionable. If the selected process is removed, select the row that takes its previous position, or the preceding last row when the removed process was last. Select the first row only when there was no prior selection and the resulting vector is non-empty.

Only after successful application does the panel advance its cursor to `H`. On storage, decode, validation, or allocation failure, both vector and cursor remain unchanged; the panel shows the error and retries from `C`.

### Local panel transitions

A panel acknowledgment passes the selected loaded object to the same command-level transition operation. On success, that operation commits SQLite and updates the selected object before returning; the panel then removes that object from its actionable array. The panel does not reload any rows.

A local transition does not advance the panel's global cursor. Another writer may already own a lower unseen sequence. The next synchronization imports the full range; the acknowledgment for the already removed ID is a harmless replay.

## Rendering

Rendering reads only the process vector and current wall-clock time.

It computes the visible global row range from terminal height, selected global index, and viewport offset. It formats only that slice. A temporary table state translates the global selected index to the visible slice's relative index; movement and persisted selection remain global. This prevents full-history formatting and allocation on every frame.

## Command details

### Start

- Validate a nonblank label.
- Detect the initial concrete orchestrator from the starting environment.
- Pass the validated label, canonical start time, and detected orchestrator directly to the single storage start operation.
- In one immediate transaction, generate the process ID, allocate `created_seq`, insert the concrete child row, and insert the running process with its fixed FK.
- Canonicalize `started_at` before constructing the committed `ProcessRun`.
- Return the tracking ID only after commit.
- Dry-run validates label and environment but creates no file, schema, row, sequence, in-memory persisted process, or tracking ID.

### Finish

- Reject exit codes outside `0..=255` before storage access.
- Load the process once to create the command's in-memory object.
- In one command-level transition, storage rechecks current mutable state under `BEGIN IMMEDIATE`.
- Missing ID is not found.
- Same-code completion returns unchanged and catches up a stale loaded object; it does not invoke the environment observer.
- Different-code completion conflicts; it does not invoke the environment observer.
- A running process invokes the observer, lets the stored concrete orchestrator derive its permitted patch, allocates `finished_seq`, and atomically writes lifecycle plus permitted existing-child columns.
- It never writes acknowledgment, label, start time, either FK, or either child-row ID.
- Dry-run performs the same validation against a read snapshot but writes nothing and leaves the loaded object unchanged, preserving current CLI output of stored state.

### Acknowledge

- Load the process once for a CLI command, or use the panel's existing loaded object.
- Storage validates current state under the transition operation.
- Missing ID is not found.
- Running is invalid.
- Already acknowledged catches a stale supplied object up to authoritative state without writing or allocating a sequence.
- Otherwise allocate `acknowledged_seq` and update only `acknowledged` and `acknowledged_seq`.
- Dry-run validates but writes nothing and leaves the loaded object unchanged.

### Focus

- Load current storage state at action time; do not navigate from a stale panel object.
- Navigate outside a database write transaction using the loaded concrete orchestrator.
- Failed navigation changes nothing.
- Successful navigation of a process observed as running does not acknowledge it.
- Successful navigation of a process observed as finished invokes the same loaded-object acknowledgment transition.
- Dry-run focus loads current storage state, reports its destination, performs no navigation, and does not acknowledge.
- The focus result carries the authoritative destination used for navigation so TUI errors do not combine an old cached destination with a newer operation.

## Dry-run authority

`SqliteStorage` is the sole authority for whether writes are previews. Remove the separate `dry_run` argument from write-oriented application functions. Their result is derived from storage's transition outcome.

The CLI/TUI may still use the parsed flag to choose whether `focus` performs an external navigation, because navigation is not a storage write. It must not independently decide whether a storage transition commits.

A dry-run storage instance:

- reads an existing database read-only;
- does not create a missing database or directory;
- does not initialize or migrate schema;
- performs transition validation;
- allocates no sequence;
- executes no update;
- does not alter supplied in-memory state.

## Integrity and compatibility

All full and incremental reads reject:

- inconsistent completion columns;
- exit codes outside `0..=255`;
- acknowledgment before completion;
- sequence/lifecycle disagreement;
- non-positive or incorrectly ordered transition sequences;
- missing child rows;
- anything other than exactly one selected child reference;
- duplicate ownership of a child row;
- child variant or row identity changing relative to an existing cached process.

Bump `SCHEMA_VERSION` from 1 to 2. Preserve the repository's existing strict compatibility policy:

- absent or empty databases initialize as version 2 on first real write;
- reads and dry runs do not create databases;
- versionless databases containing tables are rejected;
- version 1 and unknown versions are rejected unchanged;
- this change does not introduce migration machinery.

# API changes

## `src/process.rs`

### Remove

```rust
pub(crate) fn start(
    label: String,
    started_at: SystemTime,
    orchestrator: Orchestrator,
) -> Result<Self, String>;

pub(crate) fn finish(
    &mut self,
    exit_code: u8,
    finished_at: SystemTime,
    orchestrator: Orchestrator,
);

pub(crate) fn acknowledge(&mut self) -> bool;
```

### Add

```rust
/// SQLite synchronization revision. Zero means that no database transition
/// has been observed; persisted transition markers are always positive.
pub(crate) type Revision = i64;

pub(crate) const INITIAL_REVISION: Revision = 0;

```

There is no unpersisted process type. `ProcessRun` represents only a row that has been committed or restored from SQLite.

`ProcessRun` stores `created_seq`, `finished_seq`, and `acknowledged_seq` privately. Restore constructors receive and validate the sequence fields in addition to the existing persisted fields. Add crate-visible sequence accessors required by storage and synchronization.

```rust
pub(crate) fn created_seq(&self) -> Revision;
pub(crate) fn finished_seq(&self) -> Option<Revision>;
pub(crate) fn acknowledged_seq(&self) -> Option<Revision>;
```

Add typed cache patches:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompletionPatch {
    pub(crate) id: String,
    pub(crate) sequence: Revision,
    pub(crate) finished_at: SystemTime,
    pub(crate) exit_code: u8,
    pub(crate) orchestrator: OrchestratorCompletionPatch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AcknowledgmentPatch {
    pub(crate) id: String,
    pub(crate) sequence: Revision,
}

pub(crate) fn apply_completion(
    &mut self,
    patch: &CompletionPatch,
) -> Result<(), String>;

pub(crate) fn apply_acknowledgment(
    &mut self,
    patch: &AcknowledgmentPatch,
) -> Result<(), String>;
```

Both application methods validate completely before mutating. Older equivalent state is ignored, equal sequence requires equivalent state, and only a newer legal transition changes the object.

Existing read-only accessors and derived-state methods remain.

## `src/orchestrator.rs`

Make `ShellRecord` and `TmuxRecord` fields private. Add validated restore constructors and crate-visible serialization/navigation accessors for currently stored fields.

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EnvironmentObservation {
    cwd: PathBuf,
    tmux: Option<TmuxObservation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TmuxObservation {
    socket: String,
    server_instance: String,
    pane_id: String,
}

pub(crate) fn observe(env: &dyn Environment) -> Result<EnvironmentObservation, String>;
```

`observe` directly rejects empty required tmux fields rather than relying on a particular `Environment` implementation to filter them.

Add the object-safe lazy observation boundary:

```rust
pub(crate) trait FinishObserver {
    fn observe(&mut self) -> Result<EnvironmentObservation, String>;
}
```

Application code may implement this with a closure adapter around `Environment`.

Add typed, variant-preserving completion patches:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OrchestratorCompletionPatch {
    Shell {
        id: String,
        cwd: PathBuf,
    },
    Tmux {
        id: String,
        cwd: PathBuf,
        socket: String,
        server_instance: String,
        pane_id: String,
    },
}

pub(crate) fn completion_patch(
    &self,
    observation: &EnvironmentObservation,
) -> OrchestratorCompletionPatch;

pub(crate) fn apply_completion(
    &mut self,
    patch: &OrchestratorCompletionPatch,
) -> Result<(), String>;
```

The patch always contains the existing variant and row ID. `apply_completion` validates fully before mutation.

Existing `detect`, navigation, and read-only display methods remain.

## `src/storage.rs`

### Remove

```rust
fn save_process(&mut self, process: &ProcessRun) -> Result<(), StorageError>;
```

### Add transition errors and outcomes

```rust
#[derive(Debug)]
pub(crate) enum TransitionError {
    NotFound(String),
    Invalid(String),
    Environment(String),
    Storage(StorageError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MutationStatus {
    Applied,
    Unchanged,
    Preview,
}

pub(crate) enum InsertOutcome {
    Committed(ProcessRun),
    Preview,
}
```

`TransitionError` maps directly to the corresponding `AppError` category.

### Change `Storage`

```rust
pub(crate) trait Storage {
    fn start_process(
        &mut self,
        label: String,
        started_at: SystemTime,
        orchestrator: Orchestrator,
    ) -> Result<InsertOutcome, TransitionError>;

    fn finish_process(
        &mut self,
        process: &mut ProcessRun,
        exit_code: u8,
        finished_at: SystemTime,
        observer: &mut dyn FinishObserver,
    ) -> Result<MutationStatus, TransitionError>;

    fn acknowledge_process(
        &mut self,
        process: &mut ProcessRun,
    ) -> Result<MutationStatus, TransitionError>;

    fn get_process(&self, id: &str) -> Result<Option<ProcessRun>, StorageError>;
    fn list_all_process(&self) -> Result<Vec<ProcessRun>, StorageError>;
    fn list_active_process(&self) -> Result<Vec<ProcessRun>, StorageError>;

    fn load_panel_snapshot(&self) -> Result<PanelSnapshot, StorageError>;
    fn load_panel_changes(
        &self,
        after: Revision,
    ) -> Result<PanelChanges, StorageError>;
}
```

`finish_process` and `acknowledge_process` validate against SQLite and mutate the supplied object only after a successful commit. `Unchanged` still catches a stale supplied object up to authoritative current state. `Preview` leaves it unchanged.

```rust
pub(crate) struct PanelSnapshot {
    pub(crate) processes: Vec<ProcessRun>,
    pub(crate) cursor: Revision,
}

pub(crate) struct PanelChanges {
    pub(crate) new_processes: Vec<ProcessRun>,
    pub(crate) completions: Vec<CompletionPatch>,
    pub(crate) acknowledgments: Vec<AcknowledgmentPatch>,
    pub(crate) cursor: Revision,
}
```

Both panel methods return only fully decoded data from one SQLite read transaction. `PanelSnapshot.processes` and `PanelChanges.new_processes` contain only rows with `acknowledged = false`; `PanelChanges.acknowledgments` are removal records, not requests to reread acknowledged rows.

## `src/storage/sqlite.rs`

Set `SCHEMA_VERSION` to 2. Add the singleton `storage_state` table initialized to sequence zero, the three transition sequence columns, the actionable partial indexes, and unique ownership constraints for `shell_id` and `tmux_id`.

Remove `merge`, `update_existing`, replacement child insertion during finish, cross-kind FK switching, and deletion of the original child row.

Add private helpers backed only by the singleton `storage_state` row:

```rust
fn current_revision(tx: &Transaction<'_>) -> Result<Revision, StorageError>;
fn next_revision(tx: &Transaction<'_>) -> Result<Revision, StorageError>;

fn commit_start(
    tx: &Transaction<'_>,
    label: String,
    started_at: SystemTime,
    orchestrator: Orchestrator,
) -> Result<ProcessRun, TransitionError>;

fn commit_finish(
    tx: &Transaction<'_>,
    process: &ProcessRun,
    exit_code: u8,
    finished_at: SystemTime,
    observer: &mut dyn FinishObserver,
) -> Result<(MutationStatus, Option<CompletionPatch>, Option<AcknowledgmentPatch>), TransitionError>;

fn commit_acknowledgment(
    tx: &Transaction<'_>,
    process: &ProcessRun,
) -> Result<(MutationStatus, Option<CompletionPatch>, Option<AcknowledgmentPatch>), TransitionError>;

fn read_panel_snapshot(
    tx: &Transaction<'_>,
    high_water: Revision,
) -> Result<PanelSnapshot, StorageError>;

fn read_panel_changes(
    tx: &Transaction<'_>,
    after: Revision,
    high_water: Revision,
) -> Result<PanelChanges, StorageError>;
```

The public storage operation starts the transaction, reads high-water first to establish the snapshot, calls the matching private reader, commits, and only then returns the snapshot/batch.

For local mutations, `commit_finish` and `commit_acknowledgment` may return catch-up patches for transitions already committed by another process. The public method applies all returned patches to a clone before commit, commits, and then replaces the supplied object.

Canonicalize all persisted timestamps through `to_millis`/`from_millis` before constructing committed objects or patches.

Dry-run transition implementations use read transactions only and never initialize schema, allocate sequences, execute child updates, or mutate supplied objects.

## `src/app.rs`

Remove the `dry_run` argument from `start`; storage's `InsertOutcome` is authoritative:

```rust
pub(crate) fn start(
    storage: &mut dyn Storage,
    label: &str,
    env: &dyn Environment,
) -> Result<Option<String>, AppError>;
```

Keep CLI-facing transitions, implemented with one loaded object and one storage transition:

```rust
pub(crate) fn finish(
    storage: &mut dyn Storage,
    id: &str,
    exit_code: i32,
    env: &dyn Environment,
) -> Result<ProcessRun, AppError>;

pub(crate) fn acknowledge(
    storage: &mut dyn Storage,
    id: &str,
) -> Result<ProcessRun, AppError>;
```

Add the panel path that uses an object already in memory:

```rust
pub(crate) fn acknowledge_loaded(
    storage: &mut dyn Storage,
    process: &mut ProcessRun,
) -> Result<MutationStatus, AppError>;
```

`finish` wraps `env` in a lazy `FinishObserver`. `acknowledge` loads immutable data once and passes the object to storage. Neither function rereads after the transition.

Change focus result to carry the authoritative destination used by the operation:

```rust
pub(crate) struct Focused {
    pub(crate) process: ProcessRun,
    pub(crate) target: String,
    pub(crate) destination: String,
}
```

Real and preview focus both load current storage state at action time. Preview remains a separate no-navigation application path selected by CLI/TUI because it controls an external side effect rather than storage commit behavior.

## `src/tui.rs`

Replace `Panel::refresh` with:

```rust
impl Panel {
    fn load(&mut self, storage: &dyn Storage);
    fn synchronize(&mut self, storage: &dyn Storage);
    fn acknowledge_selected(&mut self, storage: &mut dyn Storage);
    fn apply_changes(&mut self, changes: PanelChanges) -> Result<(), String>;
    fn visible_range(&self, available_rows: usize) -> Range<usize>;
}
```

Add:

```rust
by_id: HashMap<String, usize>,
cursor: Revision,
selected_id: Option<String>,
```

`load` runs once before the first draw. `synchronize` runs on the existing timer. `acknowledge_selected` calls `app::acknowledge_loaded`, removes the committed acknowledged process from the actionable array, repairs selection, performs no refresh, and does not move the cursor. `apply_changes` stages only affected rows and performs an infallible final merge/removal.

`draw` formats only `visible_range` and translates global selection to a relative table state.

## `src/cli.rs`

Update the `app::start` call for the removed `dry_run` argument. No command syntax or output format changes.

Real and preview focus use authoritative state returned by the application operation.

## `src/tests/support.rs`

Replace fixture construction through `save_process` with explicit start/finish/acknowledge fixture helpers. Fixtures must assign deterministic valid transition sequences without exposing a production generic save API.

## `README.md`

Update behavior that currently says finish can switch the saved orchestrator:

- the selected concrete orchestrator row and FK are fixed at start;
- compatible finish observations refresh permitted columns in that existing row;
- incompatible observations do not switch variants;
- the panel synchronizes transition ranges once per second instead of reloading all rows.

# Tests

## Process and orchestrator state

- `ProcessRun` cannot represent an unpersisted process or a process without a positive `created_seq`.
- Restored processes reject invalid or incorrectly ordered transition sequences.
- Identity, label, start time, selected FK, child row ID, and orchestrator variant cannot change through patches.
- Running accepts one matching completion patch.
- Finished rejects a different completion and cannot return to running.
- Pending acknowledgment accepts one acknowledgment patch only after completion.
- Acknowledged cannot return to pending.
- Older equivalent patches are ignored.
- Equal-sequence equivalent patches are ignored.
- Equal-sequence different content is rejected as corruption.
- Timestamps with sub-millisecond input replay equivalently after canonicalization.
- Shell completion patch preserves row ID and updates only shell fields.
- Tmux completion patch preserves row ID and variant and updates its permitted fields.
- Cross-row and cross-variant orchestrator patches are rejected before mutation.

## Transition storage

- Start atomically inserts one process and one concrete child with a unique positive `created_seq`.
- Two processes cannot own one shell or tmux row.
- Missing-database finish and acknowledgment return not-found without creating a file or schema.
- First completion atomically writes lifecycle, `finished_seq`, and allowed child columns.
- Completion never writes acknowledgment, immutable process fields, either FK, or either child ID.
- Shell-to-shell and shell-finish-from-tmux retain raw `shell_id` and row ID.
- Tmux finish from same and different valid tmux contexts retains raw `tmux_id` and row ID while updating the existing row.
- Tmux finish outside tmux, with partial tmux data, or with empty required fields retains raw FK, row ID, and prior tmux columns.
- No cross-kind finish inserts a row in the other child table or deletes the original child row.
- Same-code repeated finish returns unchanged, allocates no sequence, and does not invoke an observer that fails or panics.
- Different-code repeated finish conflicts, allocates no sequence, and does not invoke the observer.
- Environment failure during first completion changes no row, sequence, child field, or supplied object.
- Concurrent first completions preserve the winner; losers allocate no sequence.
- Acknowledgment of running fails without allocation.
- First acknowledgment updates only acknowledgment columns.
- Repeated acknowledgment allocates nothing and catches up a stale supplied object.
- A stale running object acknowledged after another process finishes receives completion and acknowledgment state after commit.
- A stale pending object catches up when SQLite is already acknowledged.
- Concurrent starts, finishes, and acknowledgments receive unique increasing global sequences.
- Sequence exhaustion returns an explicit error and changes nothing.
- Child refresh and process completion roll back together.
- Commit failure leaves the supplied object unchanged.
- Dry-run validates transitions, invokes no unnecessary observer, allocates nothing, writes nothing, and leaves supplied objects unchanged.

## Initial and incremental reads

- Initial actionable rows and cursor come from one snapshot.
- Initial loading excludes every completed-and-acknowledged row.
- Initial ordering remains `started_at DESC, id DESC`.
- Empty and absent databases return cursor zero; reads create nothing.
- A panel opened before database creation discovers the later database.
- Empty incremental poll uses sequence indexes and returns no process rows.
- Creation range returns complete records.
- Completion range returns no label or start-time columns.
- Acknowledgment range returns only ID and sequence.
- A known process finishing and then being acknowledged between polls produces only an acknowledgment removal; completion and orchestrator columns are not reread for an object that is leaving the panel.
- A process created and acknowledged between polls is omitted and no in-memory object is constructed for it.
- Multiple transitions coalesce without losing the actionable-set result.
- A write after snapshot high-water appears on the next poll.
- No insertion can occur between range queries and be skipped when cursor advances.
- Cursor advances to snapshot high-water only after all groups decode and apply successfully.
- Decode or integrity failure leaves cache and cursor untouched.
- Older local replays are ignored while cursor advances.
- Equal revision with differing content is rejected.
- A local transition never advances the global cursor and therefore cannot hide an earlier external transition.

## Panel

- Opening and synchronizing acknowledge nothing.
- Drawing performs no storage access.
- Drawing formats only the visible process range.
- Initial selected row is the newest.
- New rows merge in linear time and preserve `started_at DESC, id DESC`.
- Existing patches do not reorder rows.
- Selection remains on the same process ID after insertion and patching.
- External starts and finishes appear within one synchronization interval; external acknowledgment removes the process within that interval.
- Local acknowledgment commits and removes the selected object without refreshing the list.
- Removing the selected object selects its successor, or its predecessor when no successor exists.
- A local transition error leaves the selected object in the list unchanged and displays the error.
- Synchronization failure preserves the previous list and cursor and retries later.
- Dry-run panel synchronization observes external writes, while local acknowledgment and focus preview write nothing.
- Focus preview uses storage state newer than the cached panel row.

## Focus and navigation

- Focus reloads authoritative state at action time.
- Successful navigation acknowledges a process observed as finished.
- Successful navigation does not acknowledge a process observed as running.
- Invalid arguments, shell destination, missing pane, restarted server, and tmux command failure change nothing.
- Dry-run focus performs no navigation or acknowledgment.
- TUI error reporting uses the destination loaded for that focus operation, not an older cached destination.

## Schema, direct fixtures, and benchmarks

- Fresh schema reports `user_version = 2`.
- A valid version-1 database is rejected unchanged.
- Unknown and versionless nonempty databases are rejected unchanged.
- Concurrent first writers initialize one valid version-2 database with exactly one `storage_state` row at sequence zero before the first transition allocation.
- Schema constraints reject sequence/lifecycle mismatch and duplicate child ownership.
- Every effective transition increments the singleton sequence and its process marker atomically; failed and idempotent transitions leave both unchanged.
- Existing direct SQL fixtures supply globally unique transition sequences.
- Benchmark fixture generation uses one sequence allocator across active and completed batches.
- The initial panel query uses `processes_panel`; actionable creation/completion queries use their partial indexes; acknowledgment uses its sequence index; high-water reads access only `storage_state`.
- Startup benchmarks hold the actionable set constant while varying completed-and-acknowledged history through one million rows and verify that rows read, objects allocated, and panel memory remain constant, with no history-size latency regression beyond an explicitly measured fixed tolerance.
- Synchronization benchmarks cover empty polls and creation/completion/acknowledgment batches at existing history sizes through one million acknowledged rows.
- A noninteractive rendering benchmark confirms frame work is proportional to visible rows rather than total actionable history.
