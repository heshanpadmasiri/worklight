# Worklight scenarios

These flows illustrate the proposed [architecture](architecture.md). Worklight is not implemented yet. Commands are issued automatically by integrations unless identified as user or panel actions. IDs are illustrative; real integrations generate unique invocation/request IDs and derive session identities from the native conversation and launch incarnation. `--request-id` is shown on retry-sensitive boundaries; integrations should reuse an event's ID when retrying.

**All Worklight lifecycle reporters are synchronous: never set `async: true`, use `&`, or queue a report for later.** The examples assume one reporter per event with explicit turn correlation.

All CLI responses go to the integration, not the agent's hook output or the user's prompt. The examples omit output handling, shell hook registration, status restoration, and harness-specific turn correlation. They are flow examples, not complete shell configuration.

## 1. A successful `make test`

The user types:

```sh
make test
```

The zsh pre-execution hook selects this command, creates an invocation ID, retains it in shell memory, and reports:

```sh
worklight start --id build-1 --source-id shell-1 \
  --request-id build-1-start --kind command \
  --label "make test" --cwd "$PWD"
```

Origin is captured from the shell environment. An integration can capture and pass a verified supervising-shell origin at setup instead. While make runs, the record is `running`; the panel may remain closed.

At the next prompt, the hook's first action saves the command's exit status. Before reporting, it checks for a stopped tracked job and the platform stop status. If stopped (including Linux status 148), skip reconciliation; see scenario 10. Otherwise it combines completion reporting and repair:

```sh
worklight shell reconcile --source-id shell-1 \
  --through-activity-id build-1 --request-id build-1-end \
  --exit-code 0
```

Result: `build-1` is finished/succeeded, exit 0, unacknowledged. Any earlier unresolved command from this shell is finished as unknown; later activities and other shells are unaffected. The reporting hook restores the original status and must not break shell behavior if Worklight fails.

Ordinary completion can alternatively use:

```sh
worklight finish build-1 --source-id shell-1 \
  --request-id build-1-end --exit-code 0
```

Use one completion path per event. Prompt reconciliation is preferred because it repairs prior missed reports without an extra invocation.

## 2. Build failure, crash, or missed completion

A failing build uses the same start and completion calls. For example, exit 2:

```sh
worklight shell reconcile --source-id shell-1 \
  --through-activity-id build-1 --request-id build-1-end \
  --exit-code 2
```

The result is failed with exit 2. If make or a child crashes, report the actual status the shell received, without manufacturing a crash classification. Exit 130 is failed/130 unless additional evidence establishes interruption.

If Worklight was unavailable at completion, a retry uses the same request ID and payload. If that report is lost entirely, a later selected command creates `build-2`. Its successful prompt reconciliation reports `build-2`'s status and marks unresolved `build-1` unknown. It never applies build-2's exit code to build-1.

If no later tracked command occurs, the user can explicitly end stale tracking:

```sh
worklight abandon build-1 --reason "Completion report was lost"
```

This does not stop a process. It records an unknown tracking outcome. Suspended/background jobs are outside this foreground reconciliation contract.

## 3. Agent launch and a back-and-forth conversation

### Launch association

The user launches an agent normally, such as `claude`. The zsh integration records it exactly as it records make:

```sh
worklight start --id agent-process-1 --source-id shell-1 \
  --request-id agent-process-1-start --kind command \
  --label "claude" --cwd "$PWD"
```

Before execution, the integration exports the launch association:

```sh
export WORKLIGHT_LAUNCH_ID=agent-process-1
```

The harness and its hook processes inherit this value. This is integration setup around the ordinary launch, not an extra command the user types. Nested harness launches from non-interactive tool shells inherit and deliberately share the outermost tracked launch ID; an explicitly instrumented inner launcher may supply its own ID. On return to the shell prompt, the integration clears the export.

### Session and first prompt

The harness hook ensures a session, with an identity derived from its native session ID and `agent-process-1`:

```sh
worklight session ensure --id session-1 --source-id harness-1 \
  --harness claude --native-session-id native-conversation-1 \
  --process-activity-id agent-process-1 \
  --label "Backend work" --cwd "$PWD"
```

`--process-activity-id` is explicit here for clarity; it defaults to the inherited launch ID. Session state is idle. The panel groups the launch and session into one live agent entry.

On the first prompt:

```sh
worklight start --id turn-1 --source-id harness-1 \
  --request-id prompt-1 --kind agent_turn --session-id session-1 \
  --label "Add endpoint validation" --cwd "$PWD" --supersede unknown
```

For Claude, a synchronous PermissionRequest hook reports the approval wait; observed subsequent tool completion/failure or resumed work clears it. Do not use delayed Notification.permission_prompt as a lifecycle update. If the harness needs permission, resumes, and finishes responding, its hooks report:

```sh
worklight update turn-1 --source-id harness-1 --state waiting_approval
worklight update turn-1 --source-id harness-1 --state running
worklight finish turn-1 --source-id harness-1 --outcome turn_finished
```

For a question requiring a user response, use `waiting_input`. Only report states established by that harness's events. Session state returns to idle after turn completion; the process activity remains running.

### Follow-up and normal exit

The next user message creates a new turn:

```sh
worklight start --id turn-2 --source-id harness-1 \
  --request-id prompt-2 --kind agent_turn --session-id session-1 \
  --label "Cover empty input too" --cwd "$PWD" --supersede unknown
worklight finish turn-2 --source-id harness-1 --outcome turn_finished
```

When the harness emits SessionEnd, report a nonterminal boundary:

```sh
worklight session end session-1 --source-id harness-1 --reason normal
```

The session is now dormant, not closed. If SessionStart/resume arrives before process exit, `session ensure` reactivates the same session ID; see scenario 9.

When the agent executable actually returns (not when its job is suspended), the shell uses the same completion path as make:

```sh
worklight shell reconcile --source-id shell-1 \
  --through-activity-id agent-process-1 \
  --request-id agent-process-1-end --exit-code 0
unset WORKLIGHT_LAUNCH_ID
```

The command records success/0 and is automatically acknowledged because it has a linked harness session. Its open/dormant sessions become closed and completed turn outcomes are preserved. This creates no redundant successful-process notification; unseen turn outcomes still need attention. Session closure and process exit are separate observations: neither a finished turn nor SessionEnd necessarily means the executable has exited.

## 4. Missing SessionStart or user interruption

To handle an absent session-start report, a prompt hook ensures the session and starts the turn in one transaction:

```sh
worklight start --json - <<'JSON'
{
  "id":"turn-2","kind":"agent_turn","source_id":"harness-1",
  "request_id":"prompt-2","session_id":"session-1",
  "label":"Continue the review","cwd":"/projects/app",
  "ensure_session":{
    "id":"session-1","harness":"claude",
    "native_session_id":"native-conversation-1",
    "process_activity_id":"agent-process-1",
    "label":"Backend work","cwd":"/projects/app"
  },
  "supersede":"unknown"
}
JSON
```

If the session exists with matching identity, ensure does not reset it. If an earlier turn is still active because its end event was missed, the operation closes it as unknown and starts turn-2 atomically. An exact retry does not supersede turn-2 or a subsequent turn.

When an integration directly observes interruption, it can report:

```sh
worklight finish turn-1 --source-id harness-1 --outcome interrupted
```

Otherwise, the next start uses `--supersede unknown`; it must not infer interruption solely from the missing Stop. Hooks must correlate events to the correct turn ID. A delayed old-turn completion must never finish the current turn.

## 5. Agent process crash

Suppose session-1 has an active turn when the agent exits unexpectedly and no harness shutdown hook executes. The surviving shell receives a nonzero status, shown here as the illustrative value 1:

```sh
worklight shell reconcile --source-id shell-1 \
  --through-activity-id agent-process-1 \
  --request-id agent-process-1-end --exit-code 1
unset WORKLIGHT_LAUNCH_ID
```

In one transaction:

1. The shell-owned command becomes failed/1.
2. Any open or dormant sessions linked to that process become closed.
3. Their unresolved turns become unknown with reason `process_exited_without_turn_result`.
4. Previously completed turn outcomes remain unchanged.

There is no crash-specific CLI or `session exited` command. Nonzero exit has the same meaning as for make; linking adds cleanup of agent-specific state. A zero exit with an unresolved turn likewise closes that turn as unknown, while recording process success/0.

The shell writes only its own process record. Application logic follows the immutable launch association to close linked sessions; it does not authorize shell-originated running/waiting updates to harness turns. If SessionEnd arrived first, the session was dormant; process completion now closes it and records the exit code without rewriting prior turn results.

If lazy session creation arrives after process completion, the session is created closed and new turn start is rejected. An exact late retry cannot resurrect the process or session.

## 6. Shell/pane loss and a hung process

If the shell and agent disappear together, there may be no completion event. On panel entry:

```sh
worklight maintenance
```

Verified owner disappearance closes tracking as unknown. For location-bound tracking, confirmed loss of the recorded pane/server closes tracking as lost, without asserting actual process death. Unknown/temporarily unavailable provider results do not count as confirmed loss.

If no reliable process or location observation is possible, the panel labels liveness unverified and offers:

```sh
worklight session abandon session-1 --reason "Tracking can no longer be verified"
```

A live but hung process remains active. Silence and elapsed time alone do not establish failure; no timeout policy is implied.

## 7. Render the panel in tmux and jump to work

An illustrative tmux configuration binding is:

```tmux
bind-key Space display-popup -E -w 80% -h 80% \
  'worklight panel --client #{q:client_name}'
```

Press prefix, then Space. tmux expands and shell-quotes the initiating client name before launching Worklight. The implementation must validate this binding on supported tmux versions. Do not infer the client from `TMUX_PANE` inside the popup. See the [tmux format documentation](https://github.com/tmux/tmux/wiki/Formats).

On entry the panel requests:

```sh
worklight maintenance
worklight list --view all
worklight session list --status live
```

Maintenance is throttled; list queries repeat while the panel is visible. The panel groups linked process/session/current-turn records and shows older unseen outcomes separately. Agent process failures remain visible even when the last turn had already finished.

Selecting a build invokes:

```sh
worklight focus build-1 --client "$initiating_client"
worklight acknowledge build-1
```

Only acknowledge automatically after successful navigation if that is the chosen UI policy. Selecting an idle agent invokes:

```sh
worklight session focus session-1 --client "$initiating_client"
```

A closed pane leaves the historical result available but navigation unavailable. Focusing history does not restore old terminal output.

## 8. Trace, dry-run, and 72-hour cleanup

Inspect an operation's stages and durations without altering stdout:

```sh
worklight --trace list --view attention
```

Preview completion against current state without logical database changes or navigation:

```sh
worklight --trace --dry-run finish build-1 \
  --source-id shell-1 --request-id build-1-end --exit-code 2
```

Use this against a currently running illustrative build; known incompatible terminal outcomes still produce a validation error. Preview creates no receipts, primary database, or provider tokens. SQLite may create or maintain WAL/SHM sidecars and locking metadata while reading an existing database; the guarantee is logical-state invariance, not byte-for-byte filesystem invariance. Actual execution must omit `--dry-run`; previews do not reserve future state.

On a later panel opening, due maintenance removes outcomes older than 72 hours regardless of acknowledgment. Active records remain. To preview maintenance explicitly:

```sh
worklight --dry-run maintenance --force
```

Unknown tracking outcomes follow the same retention window from their recorded terminal time. No daemon or scheduled cleanup is required.


## 9. SessionEnd and resume within one live process

This flow covers an internal idle timeout, `/clear`, and `/resume` without asserting that every harness version keeps the same native ID. The launch record `agent-process-1` stays running throughout.

```sh
# Initial session, already linked to the running process.
worklight session ensure --id session-1 --source-id harness-1 \
  --harness codex --native-session-id native-1 \
  --process-activity-id agent-process-1 --label "Backend work" --cwd "$PWD"

# A native SessionEnd boundary occurs, without process exit.
worklight session end session-1 --source-id harness-1 \
  --request-id boundary-1 --reason idle_timeout

# SessionStart with source resume and the same native ID.
worklight session ensure --id session-1 --source-id harness-1 \
  --request-id resume-1 --harness codex --native-session-id native-1 \
  --process-activity-id agent-process-1 --label "Backend work" --cwd "$PWD"

worklight start --id turn-after-resume --source-id harness-1 \
  --request-id prompt-after-resume --kind agent_turn --session-id session-1 \
  --label "Continue" --cwd "$PWD" --supersede unknown
```

`idle_timeout` is an illustrative raw reason; forward what the harness actually emits rather than requiring this spelling. State is open → dormant → open, then running. No new process/session incarnation is needed, receipts remain, and old turns stay terminal. Repeating this cycle works. If SessionStart was missed, turn start reactivates the dormant session itself.

For clear/resume with a changed native ID, derive a new session ID and ensure it under the same process link. With an unchanged native ID, reuse session-1. The implementation must live-test both lifecycle behavior and ID changes on supported harness versions; no live check of Claude `/clear` is claimed here. Worklight does not depend on a particular timeout duration.

A true process exit still closes all its open/dormant sessions. A late SessionStart after that cannot reactivate them. This distinction prevents both permanently blocked conversations and ghost sessions after process death.

## 10. Ctrl-Z and unsupported job control

After the tracked command starts, the user presses Ctrl-Z. The shell returns to a prompt, but the foreground job is stopped, not finished. The pre-prompt integration saves status, checks zsh stopped-job state and the platform's stop-signal status, and **does not call `shell reconcile`**. Linux commonly reports 148; macOS commonly reports 146. Do not use 148 alone as a portable test or as proof of suspension.

The old run remains unresolved. If another selected foreground command later finishes, its bounded prompt reconciliation marks that earlier run unknown, even if the stopped job is still alive. Later `fg`/`bg` completion is not mapped back to the old run. The panel shows an unknown tracking outcome, not success or verified process death. An explicit exit with a stop-like code can conservatively be deferred too.

The same guard applies to an agent launch: do not report process exit or close linked sessions on suspension. This remains a documented limitation; implementing full suspend/resume ownership is outside the first version.

## 11. Nested harnesses and identity without a launch record

An outer harness launched as agent-process-1 starts an inner harness via a non-interactive tool shell. No zsh pre-execution hook is assumed. The inner hooks inherit `WORKLIGHT_LAUNCH_ID=agent-process-1` and ensure a distinct session using their harness/native conversation identity. Both sessions belong to the outermost tracked launch scope. The inner SessionEnd makes its session dormant; the outer process exit ultimately closes both. Worklight cannot claim the inner process's exact exit status from that association.

Outside a tracked launch, verified process identity or an inherited explicit token is preferred. If neither exists, derive both session/source identities consistently from `(machine_id, boot_id, harness, native_session_id)` with the unverified namespace, and pass `identity_scope: conversation` with `--no-process-link`. Same-boot resumes reuse this logical conversation; they do not establish a new process incarnation. Concurrent instances using the same native ID can collide, and incompatible origins must surface a conflict. After explicit abandonment/confirmed tracking loss, a new explicit token is required to create a new incarnation. This fallback is visibly unverified, not full process tracking.

## 12. Delayed notifications, long labels, and quiet WAL preview

A PermissionRequest is followed by tool completion. A delayed `Notification.permission_prompt` then arrives. The Worklight notification integration emits no lifecycle mutation, so the turn remains running. This does not require a `tool_use_id` that the notification schema does not guarantee. Prompts with no supported synchronous event may have reduced waiting-state detail.

For a long prompt or pasted command, use file input when it may exceed the OS argument limit:

```sh
worklight start --id long-build --source-id shell-1 --kind command \
  --request-id long-build-start --label-file /path/to/command-label.txt --cwd "$PWD"
```

Worklight retains a normalized 512-scalar display label with an ellipsis and `LABEL_TRUNCATED` warning, while creating the activity normally. Quoted flag/environment labels and JSON labels follow the same rule; invalid or oversized IDs are rejected instead of truncated.

After all database connections close and SQLite removes WAL sidecars, this must still work in a writable data directory:

```sh
worklight --dry-run maintenance --force
```

SQLite may recreate sidecars to read consistently. Worklight changes no tables, schema, receipts, lease, acknowledgment, or retention timestamp. Verify the same guarantee with concurrent readers/writers; do not assert file timestamp/sidecar invariance.
