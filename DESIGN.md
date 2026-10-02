# cued design

cued is a Linux per-user scheduler implemented in Rust. A single binary provides
the CLI, daemon, and stdio MCP server. This document describes the alpha's
execution contracts; the schema and protocol are not yet stable interfaces.

## 1. Purpose

Schedule a command or reminder, inspect it later, and retain its state across
restarts. Workflows extend that model with conditional steps and durable waits.
The daemon runs with the user's privileges and needs no privileged system service.

## 2. Jobs, runs, and actions

A job stores a schedule, graph, execution context, policies, and notification
hooks. Each firing creates a run. Each visit to a step records an attempt.
Actions execute an argv vector or enqueue a desktop notification.

Job lifecycle and approval are separate fields. Active, paused, done, cancelled,
and expired describe lifecycle. Pending or approved describes a definition that
requires approval. A pending job cannot create runs, claim steps, or consume its
recurrence count. Terminal jobs release their names.

### 2.1 Execution context

Submission captures the working directory and environment. Defaults apply before
job and step overrides. Execution uses the captured values rather than the
daemon's current environment. Scheduling decisions compare stored UTC instants
with the clock; suspend and clock changes can make work overdue.

### 2.2 Command execution

Argv commands execute directly. Shell strings use `sh -c`. Standard input is
closed; stdout and stderr are captured in per-attempt logs. Each command starts
in its own process group. Cancellation and timeout send TERM, wait the configured
grace period, then send KILL to the group, even if its leader has exited.
Output-draining tasks have bounded shutdown and are explicitly aborted if needed.

### 2.3 Process ownership

The executor tracks claimed attempts and their process groups. Pause allows an
already claimed step to finish; cancellation first commits terminal state, then
signals owned processes. A child that escapes its process group is outside this
mechanism. SIGKILL of the daemon can leave children alive, so recovery cannot
promise exactly-once command effects.

## 3. Workflows

### 3.1 Graph model

A graph has an entry step and named steps. A step contains an action, optional
execution overrides, and ordered transitions. Conditions include success,
failure, timeout, exit status, output matching, and an unconditional fallback.
An effect ends the run or moves to another step, optionally after a wait.

### 3.2 Evaluation and timing

The first matching transition wins. Relative waits resolve once and persist their
target instant. A restart does not restart the delay. Step visit limits and run
deadlines bound loops; graph validation rejects unsafe cycles and missing targets.
Run deadlines continue to elapse while paused or held.

### 3.3 Durable state and write ordering

Runs retain their cursor: waiting, running, held, or done. A step claim commits
before process creation. Completion records the attempt and next cursor together.
The current attempt and approval are checked before starting work; a stale
completion cannot advance a newer attempt. Failed writes receive bounded retries,
with unresolved work held rather than silently replayed.

These rules make database transitions durable. They cannot atomically commit an
external command's effects with SQLite. Run identifiers are not reused after GC.

### 3.4 Reconciliation

An overdue wait follows `missed_wait`: `run_asap` by default, or `abandon`.
An interrupted running step follows `on_interrupt`: `hold` by default, `fail`,
or `retry`. Automatic retry requires the step's `restart_safe` declaration.
Human `continue` and `retry` controls remain subject to lifecycle and approval
checks. Resume does not backfill every missed recurring firing.

### 3.5 Notifications and lifecycle hooks

Lifecycle hooks enqueue notifications; they do not execute commands. Notification
actions succeed when durably enqueued, independently of desktop delivery.
Delivery locates the user's session bus and retries unavailable transport.

An acknowledgement that cannot yet be persisted is retained in an in-memory
ledger so the daemon does not immediately send the same popup again. Calls can
remain pending after the inline acknowledgement timeout, but have a total timeout.
GC removes corresponding delivery tasks when it removes queued rows.

A daemon crash loses the in-memory ledger. Delivery followed by a crash before
recording acknowledgement can produce a duplicate. Retention can remove an
undelivered notification with its history; the queue is not an unlimited archive.

### 3.6 Workflow trust

Workflow commands run as the daemon's user. Graph validation and approval prevent
accidental execution but do not sandbox commands or protect against that same
user modifying local state.

## 4. Recurrence

### 4.1 Schedules

A schedule is a one-shot instant, an anchored elapsed interval, or a calendar
rule in an IANA time zone. Recurring schedules may have an end time or count.
The next firing is stored durably rather than reconstructed from retained history.

### 4.2 Firing semantics

`catch_up = "run_once"` creates one catch-up run after downtime; `skip` advances
past missed firings. `overlap = "skip"` drops a firing while a run remains live;
`queue` coalesces overlap into one queued firing. Runs do not execute in parallel
within a job. Approval gates firing transactions, including count updates.

## 5. Components

The CLI and MCP server submit canonical job definitions to the same daemon.
SQLite stores definitions, scheduling state, runs, attempts, and notifications.
Logs live beside the store. The executor handles processes; the scheduler handles
durable transitions and reconciliation.

### 5.1 IPC

Local IPC uses a Unix socket, JSON messages, and kernel peer credentials. Clients
and daemon must belong to the same user. Protocol version remains 1 for this
unreleased alpha; incompatible or stale daemon responses produce a restart
instruction. There is no network listener.

A client that wants to follow changes, such as a status window, sends
`Subscribe` instead of polling. The daemon replies `Subscribed`, then sends
`Changed` after each commit that `list`, `show`, or `logs` could see. A notice
names nothing: the subscriber refetches what it shows, so the read requests
stay the one source of data. Commits that land close together, or while a
notice is still being written, arrive as one. Nothing is sent while nothing
changes, and a subscriber's own refetches change nothing: the expiry and
exhaustion sweeps those reads run write only when they find something. The
connection takes no further requests; closing it, or sending another line,
ends the stream.

Notices come from SQLite's hooks on the single writer connection, not from
each write path, so a new write cannot forget to send one. A change is told
once the writer is released, because the commit hook runs before the commit
completes and a subscriber told from there could read the old state. Rolled
back changes, and §3.5 delivery bookkeeping that no inspection view reads,
are not told. A failed commit can produce a notice with nothing behind it,
which costs a refetch; the design accepts that rather than ever missing one.

An upgrade waits for request handlers to finish (§5.2), but not for a
subscription, which would never finish.
The exec closes the stream, and the subscriber reconnects to the new image
and fetches everything again, as it does after any reconnect: notices are
not stored, and a subscriber learns of what happened while it was away only
by refetching.

### 5.2 Daemon lifecycle

Clients start a daemon on demand. A data-directory lock prevents competing
writers; startup checks ownership before replacing a stale socket.

All launchers derive the default socket from the UID:
`/run/user/<uid>/cued.sock`, falling back to
`~/.local/share/cued/run/cued.sock` if the standard private runtime directory is
unavailable. An inherited `XDG_RUNTIME_DIR` does not change the endpoint.
`CUED_SOCKET_DIR` explicitly selects an isolated deployment.

`cued upgrade` replaces the daemon's image without a restart. It first runs the
installed binary with `--version`, refusing one that can't run. The daemon then stops
popping due work, waits for claimed steps, firings, and request handlers to
finish, then stops accepting and re-executes the path it started from. The PID,
both locks, and the listening socket cross the exec, so supervisors see no
restart, no second daemon can win the lock in between, and clients connecting
meanwhile queue on the socket. The new image adopts the inherited descriptors
only after checking that they are this deployment's lock files and socket.
Then it reconciles as at any startup. If the drain outlasts its wait, the upgrade
is abandoned unless forced; a forced upgrade interrupts steps as shutdown does.
If the exec fails, the daemon re-executes its own running image instead, and
the client, which checks the image actually running, reports the failure. A stop
signal during the upgrade ends it; the daemon exits instead of re-executing. Waits
are capped at 24 hours. The
upgrade request and its replies keep a frozen shape outside the protocol
version check, so a newer CLI can always ask an older daemon to upgrade.

### 5.3 Store

SQLite uses WAL, a serialized writer, and a bounded reader pool. The initial
migration defines the alpha schema. Durable attempt identity fences stale work.
The store and log directories are private to the user; captured environment
values are stored as data, without encryption.

## 6. Authoring

### 6.1 CLI

`at`, `remind`, `every`, and `chain` build the same graphs accepted by `submit`.
`show --toml` exports the authoring format, including interval anchors and
explicit transitions, so an export can be edited and resubmitted.

### 6.2 TOML

A workflow file can use simple transition sugar or ordered `[[step.transition]]`
entries. For example:

```toml
name = "build-and-test"
at = "9am tomorrow"
entry = "build"

[[step]]
id = "build"
run = ["make", "build"]
on.success = { goto = "test" }
on.fail = { end = "failure" }

[[step]]
id = "test"
run = ["make", "test"]
```

String commands use a shell; arrays preserve argv boundaries. Job defaults can
set the working directory, deadline, and recovery policy; steps can override
execution settings. Exported definitions can contain captured environment values.

### 6.3 Validation

Submission validates the entry, targets, actions, regular expressions, scheduling
bounds, and loop limits before persistence. MCP applies additional capability
checks to every graph step, including unreachable steps.

## 7. Security and approval

### 7.1 Threat model

Protect one user's jobs from other local users and prevent accidental execution
through an AI client. Same-user processes and intentionally executed commands
are trusted with that user's privileges.

### 7.2 Privileges

The daemon does not change execution identity. A command can still exercise any
privileges available to its user, including separately configured elevation.
Approval is not an operating-system security boundary.

### 7.3 Socket authentication

The daemon verifies peer credentials supplied by the kernel, rather than a UID
asserted in the request payload.

### 7.4 Filesystem permissions

Socket and data directories are private and ownership is checked. XDG data and
config locations remain configurable; the default socket location is deliberately
independent of a launcher's optional runtime environment.

### 7.5 Environment

Submission captures environment values after applying a configurable denylist.
The default patterns are `*_TOKEN`, `*_SECRET`, `*_KEY`, `*PASSWORD*`, and
`*_CREDENTIALS`. Name matching cannot identify every secret.

The CLI summary lists captured and stripped names. Explicit `show --json` exposes
actual captured values for human troubleshooting. MCP responses omit captured
names, values, and stripping metadata. Model-visible previews, notifications,
and MCP log output redact known captured values; this cannot detect every secret
a command might produce. MCP cannot request environment overrides or `keep_env`.

### 7.6 MCP capabilities and approval

The stdio server exposes `schedule`, `list`, `show`, `cancel`, and `logs`.
Scheduling accepts one action or a workflow graph, a time, and optional recurrence.
There are no MCP approval, installation, daemon, GC, recovery, or change
subscription tools.

`mcp.toml` selects `open`, `approve`, or `closed` independently for exec and notify.
Both default to closed. Read access defaults to on; logs default to off. Each
gated call reloads its configuration. Cancellation remains available.
Environment overrides and the configuration path are documented in the README.

Every action in a graph follows its capability policy. A closed action rejects
the graph. Any action requiring approval makes the whole definition pending.
A human reviews it through `cued approve`; approval binds a hash of the canonical
stored definition, including execution context. The hash is internal and omitted
from ordinary CLI and MCP output. Approval does not freeze referenced files.

One-shot approval must commit before the scheduled instant. At or after that
instant, a pending job expires. Recurring pending jobs expire seven days after
creation. The approval transaction checks the clock after obtaining the writer
lock, so expiry cannot be bypassed by delaying a transaction or waiting for a
sweep. Expiry stores its time and reason and releases the job name.

All durable claim, start, reconciliation, resume, and retry paths enforce approval.
Pending jobs do not create runs or spend firing counts. Expired records remain
subject to retention even if they never created a run.

## 8. Installation

### 8.1 Persistence backends

Setup offers supported systemd user and cron `@reboot` backends. Linger allows
a user service to survive logout and start at boot; enabling it can require
administrator authorization. Cron supplies startup without supervision.

### 8.2 Setup ownership

Installation records its backend so status and uninstall can inspect and remove
the installation. Setup is a human CLI operation, outside MCP capabilities.

`cued uninstall` removes everything cued put on the account: the backend, then
the daemon, then the data directory and socket. Removing the backend first means
systemd stops a supervised daemon itself and nothing restarts it. The client
then signals the daemon found on the socket at that moment, by pidfd where the
kernel supports it, so a pid reused since the prompt is never signalled. It
waits as long as that process lives, since shutdown takes up to the slowest
step's `kill_grace`. It deletes nothing
until it holds both daemon locks, which proves no daemon remains and keeps an
auto-spawned one from opening the store mid-delete. It never auto-spawns.
Recursive deletes are limited to real directories named `cued`, so an unusual
XDG value can make uninstall refuse but cannot widen what it deletes. Config is
user-authored and goes only with `--purge`; linger and the binary belong to
others and are only reported. Deletion needs interactive confirmation or `--yes`.

### 8.3 Implementation

Persistence backends share probing, installation, status, and removal operations
in `persist.rs`. They launch the same daemon used by on-demand startup.

## 9. Time

Instants are UTC timestamps. Parsing and display use time zones at the boundary;
calendar recurrence retains its zone because civil time is part of the rule.
Durations represent elapsed time. Wall-clock jumps can make targets overdue or
delay them until the clock reaches the target again.

### 9.1 Grammar

The parser accepts relative durations, civil dates and times, and calendar rules.
Relative input resolves once at submission. `--zone` selects the interpretation
zone. Duration schedules and calendar schedules are distinct forms.

### 9.2 Calendar rules

Daily, weekly, and monthly-by-date rules follow the stored zone. A nonexistent
spring-forward time shifts forward by the gap; an ambiguous fall-back time uses
its first occurrence. Calendar recurrence therefore differs from a fixed elapsed
interval across daylight-saving changes.

## 10. Operations

### 10.1 Configuration

`$XDG_CONFIG_HOME/cued/config.toml`, defaulting to `~/.config/cued/config.toml`,
sets policy, environment, and retention defaults. Built-ins are overridden by
configuration, then job and step settings where supported. Default interruption
handling is hold, catch-up is run-once, overlap is skip, missed waits run ASAP,
and process termination grace is ten seconds.

### 10.2 Retention and GC

Defaults are 30 days and 20 runs per job. A terminal run is eligible when it is
older than the age limit or exceeds the retained count. Live and held runs are
protected. Selection and deletion share a writer transaction, in bounded batches,
so concurrent recovery cannot turn a chosen row into live work before deletion.
Logs are removed after committed row deletion. Partial cleanup reports committed
progress; notification transport state is reconciled with remaining durable rows.
Expired jobs without runs age from their durable expiry timestamp.

### 10.3 Inspection

CLI list, show, logs, and JSON output support inspection. Run and attempt records
retain outcome and timing information. MCP exports a restricted projection of
that state; bounded log tails report truncation. A list entry's latest run
carries when it started and, while it is live, the step it is at.

`cued-gui` is a desktop status window over the same requests, in its own crate
so the CLI and daemon never build its graphics stack. It is a client like the
CLI: it holds a §5.1 subscription and refetches the list and the selected
job's latest run on each notice, reading a step's log file directly, as `cued
logs` does. While nothing changes it makes no requests and does not redraw;
the only timers are a running step's log, re-read each second while it is on
screen, and elapsed times counting up while a run is shown. It starts a daemon
when none is running as the window opens, from a `cued` binary it locates, and
never afterwards: a daemon stopped on purpose stays stopped.

A run is shown as its plan. The attempts come first, in the order they ran,
each with the transition it took, so a branch or a loop reads as the path the
run actually followed. A running step lists the transitions it may take, in
evaluation order. The steps still reachable from the run's cursor follow,
nearest first; a waiting run's next step says when it starts. Steps the run
can no longer reach are marked not reached: the branches it didn't take. The
workflow comes from `show` and is fetched once per job, since a definition
never changes; each attempt's `outcome_edge` names the transition taken.

The same plan can be drawn as the workflow: a box per step, in columns so each
step sits right of every step that can lead to it, with an arrow per pair of
steps a goto joins, labeled with its conditions. A depth-first walk from the
entry, edges in evaluation order, finds the edges that lead back; those loop
arrows run along lanes under the boxes and through the gaps between columns,
so they cross arrows but never a box. A step whose only way on is back sits
below the main line. The edges the run took are bold, the rest faint, and the
boxes carry the plan's states. Loops are counted within the run's current
epoch, as `max_visits` is: a box shows its visits ("×3 of 5" against its
limit, "×3" without one), an arrow taken more than once carries a "×N"
badge, and both turn the warning color once a step has used more than half
its allowed visits. Each attempt's `epoch` comes with it, so a run rewound by
`cued retry` counts only its current pass. The drawing shrinks to fit the pane, down to
70%, and scrolls beyond that. End transitions draw no arrow; the list says
where a run ended.

The window offers continue, retry, pause, resume, and cancel, sending the same
requests as the CLI verbs; cancel asks first. Each button names what it acts
on, and the run's come first: continue and retry act on the latest run ("Continue
from ship", "Retry run"), pause, resume, and cancel on the job ("Pause job"). A
held run's buttons are set off as what needs doing. Pause stays in place but is
dimmed, its reason on hover, when the job is already paused or its run is held,
since nothing new starts then anyway. Otherwise only the verbs that can apply
to the job as listed are shown, and the daemon's refusal is reported when one
doesn't.
It has no approve button: approval binds a definition a person has reviewed
(§7.6), which stays a terminal review of the exact text. `cued-gui
--install-desktop` writes a freedesktop launcher entry and icons under
`$XDG_DATA_HOME`, marked as generated; it never replaces an entry it didn't
write, and `cued uninstall` removes only a generated one.

`cued wait` (and `--wait` on `at`, `chain`, and `submit`) blocks until a run
settles, meaning done, failed, held, cancelled, or missed, and exits with a
status that encodes the outcome. A held run counts as settled because it
needs a decision. `Skipped` rows are passed over: they record firings that
didn't execute, and they are written with ids above the run they deferred
to. Exit codes avoid 1 (cued error) and 2 (usage). A `--wait` whose wait
fails after submission has its own code, so a retry doesn't resubmit.

It polls the daemon about once a second, by job id after the first request.
When the reply says nothing can change on its own until later (a waiting
run's wake time, or the job's next firing), it sleeps toward that instead, up
to 30 seconds at a time, so commands such as cancel are still noticed. On a
paused job, a step that is executing is waited out, since it finishes
regardless; anything that would need the job to move (the next step, a run
not yet started or created) is refused, since a paused job starts none of
it. `--wait` asks the daemon whether it would answer before submitting, so
a refused wait never leaves a job behind. A status reply that arrives after
`--timeout` counts as a timeout; an error reply is passed through whenever
it arrives. `--run N` looks up that exact row, including a Skipped one; the
reply's highest run id tells a run not created yet from one that is gone.
Polls are reads and never take the writer; a lapsed approval is the
scheduler's to expire, and while one is pending the waiter naps toward its
deadline. The daemon includes step results in the reply only for
a settled run, so reporting needs no further requests. The waiter rides out
up to a minute of the daemon being unreachable, which covers a restart or an
upgrade that keeps the wire protocol. Across one that changes it, the old
waiter can't read the new daemon, so it stops with an error and the run
carries on; `cued wait` from the new binary picks it up. Each exchange (connect, request, reply) has one absolute bound:
10 seconds, or less if `--timeout` ends sooner. The connect, every write, and
every read get only what is left of it, so neither a full accept queue nor a
reply arriving piece by piece can stretch it. The connect is nonblocking,
because a blocking connect waits without limit on a full queue. A reply that
arrives after `--timeout`, and a run that settles after it, count as a
timeout, and the pause between polls is clamped to the deadline.
With no daemon running, `wait` exits 1 rather than time out, since nothing
would finish the run. A bad `--timeout` is a usage error (2).

It is meant as the single shell command an agent restricted to MCP may run
(§7.6), so it reports no more than MCP would: the outcome under `read`,
per-step exit codes and timing under `logs`, and never captured output or
environment. The daemon enforces both switches on every poll, from its own
environment, because the waiter's shell is exactly where an override could
be planted. For this check the settings only tighten: `off` in any policy
file or in the environment wins, and nothing reopens it. For the same reason
`wait` never auto-spawns a daemon: one started from the waiter's shell would
inherit that shell's environment. The daemon can't see a policy set only in
an MCP server's launch environment, so `cued mcp` warns at startup when that
makes `wait` looser than the server. MCP itself has no wait tool, because a
blocking call would hold the client's session.

## 11. Tests

Automated tests cover scheduling, recurrence, approval boundaries, workflow
execution, restart reconciliation, cancellation, notifications, startup races,
and retention. Process-level tests use isolated stores and sockets. The optional
`test-hooks` feature adds fault-injection points and must be omitted from normal
installations. Campaign reports and temporary stress tooling are not shipped.

## 12. Limitations

External effects and desktop delivery are not exactly-once. Process groups do
not contain commands that deliberately escape them. Retention is bounded history,
not an audit archive. The alpha makes no compatibility promise for stored data,
wire messages, or CLI exit codes. Broader calendar syntax and additional delivery
channels remain future work.
