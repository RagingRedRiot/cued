# cued

A per-user scheduler for Linux. Run commands, send desktop reminders, and build
workflows with conditional transitions and waits that survive daemon restarts.
The CLI and stdio MCP server share the same daemon and job store.

Early alpha: commands, storage, and protocols may change without compatibility
support. Interrupted commands can require human review; cued does not guarantee
exactly-once external effects.

## Install

Build and install with Rust and Cargo from this checkout:

```sh
cargo install --path . --locked
cued setup
```

`setup` offers the persistence backends available on your machine: a systemd user
service, systemd with linger, or cron `@reboot`. Linger can require administrator
approval. Cron starts the daemon after reboot but does not supervise it.
Without setup, the first client starts the daemon on demand.
Use `cued setup --status` to inspect the installation and
`cued setup --uninstall` to remove it.

## Upgrade

Install the new build over the old one, then move the running daemon onto it:

```sh
cargo install --path . --locked
cued upgrade
```

The daemon stops starting new steps, lets running ones finish, and re-executes
the installed binary in place. Its PID, socket, persistence backend, and store
are kept. Work that came due during the drain runs as soon as the new build is
up. If steps are still running after `--wait` (default `10m`), the upgrade is
abandoned and nothing changes; `--force` interrupts them instead, and they
reconcile per `on_interrupt` as after any restart.

## Uninstall

```sh
cued uninstall          # add --purge to remove ~/.config/cued too
cargo uninstall cued
```

`uninstall` lists what it will delete and asks first: it removes the
persistence backend, stops the daemon (terminating any running steps), and
deletes the store, all job history, logs, and the socket. Your config files are
kept unless you pass `--purge`. The binary belongs to Cargo, so remove it with
`cargo uninstall`. Off a terminal, `--yes` is required. To keep a job, export it
first with `cued show ID --toml`.

## Use

```sh
cued at "9am tomorrow" -- ./backup.sh
cued remind "1h" "stretch"
cued every "30m" -- ./sync.sh
cued chain "./build.sh" --then "./test.sh"
cued submit flow.toml
cued list
cued show j1
cued logs j1 -f
cued wait j1
cued cancel j1
```

`wait` blocks until a job's run ends and exits with its outcome:

| Exit | Meaning |
| --- | --- |
| 0 | done |
| 3 | failed |
| 4 | held: needs `cued continue` or `cued retry` |
| 5 | ended: cancelled, missed, expired, or out of runs |
| 6 | `--wait` only: the job was submitted, but waiting on it failed |
| 124 | `--timeout` ran out |
| 1 | cued itself failed, including no daemon running |
| 2 | usage error, including a bad `--timeout` |

It waits for the run in progress, or else the next one to fire, and reports
a held run right away. A job that finished its schedule answers with its
last run. A cancelled or expired job has ended (5), whatever its last run
did; the summary still names that run. Skipped firings (overlap or
catch-up) are records, not runs, so `wait` passes over them. `--run N`
means exactly that run: a skipped one is reported as itself (5), and one not
created yet is waited for while the job can still fire.

On a paused job, `wait` still waits out a step that is executing, since that
step finishes regardless, and `wait --run N` still reports a run that has
already settled. Anything else would need the job to move, so `wait` refuses
it (exit 1): a run between steps or not yet started, or a run not yet
created. That includes plain `cued wait` once the latest run has settled,
since it would then be waiting for the next one. A paused job starts none of
those until `cued resume`. While the next firing is far off, `wait`
sleeps toward it instead of polling every second, waking at least every 30
seconds, so a cancel or retry is noticed within that.

`wait` never starts a daemon: with none running it exits 1 rather than
wait, since nothing would finish the run. It rides out a restart or upgrade
of up to a minute, and says whether the daemon is gone or running but not
answering. A socket it can't use at all (permissions, say) fails at once
with the real cause.

`at`, `chain`, and `submit` accept `--wait` to submit and wait in one
command. Like every flag of `at` and `every`, it goes before the time:
`cued at --wait "in 1h" ./backup.sh`. Everything after the time is the
command, so cued refuses a flag of its own typed there (exit 2) rather than
pass it to the command; after `--` it is passed on, with a note. It is the plain form, with no `--timeout` or `--json`; scripts
should submit, then run `cued wait`. If the daemon would refuse the wait
(MCP `read` off), `--wait` says so before submitting, and nothing is created.
Exit 6 is for a wait that fails after the job exists; it tells a retrying
wrapper not to submit again, since `cued wait ID` picks the job up.

`pause` stops new work while allowing a claimed step to finish. `resume` allows
work to proceed again. A run held after an interruption can be inspected, then
continued or retried with `cued continue ID` or `cued retry ID`.

Jobs capture their working directory and environment at submission. `show ID`
lists captured and stripped variable names; `show ID --json` deliberately exposes
captured values for local troubleshooting. `show ID --toml` exports an editable
workflow. See `cued --help` and [the design](DESIGN.md) for scheduling, workflow,
and recovery semantics.

## MCP

Configure an AI client to launch `cued` with arguments `["mcp"]`. It serves
stdio only and exposes exactly `schedule`, `list`, `show`, `cancel`, and `logs`.
Scheduling accepts either one action (`exec` argv or `notify` title/body) or a
workflow graph of those actions, at one time and optionally recurring. Recovery
controls remain CLI work.

The server reads `~/.config/cued/mcp.toml` (`$XDG_CONFIG_HOME/cued/mcp.toml`):

```toml
exec = "approve"    # open | approve | closed; default closed
notify = "open"     # open | approve | closed; default closed
read = "on"         # list/show; on | off
logs = "off"        # captured output; on | off
```

Override the path with `CUED_MCP_CONFIG`, or values with `CUED_MCP_EXEC`,
`CUED_MCP_NOTIFY`, `CUED_MCP_READ`, and `CUED_MCP_LOGS`. These settings belong
only to the MCP server; the daemon's `config.toml` is unchanged. Closed tools
name the setting and resolved file needed to enable them. Capability checks re-read the file, so edits apply to a running server.

Example `schedule` arguments:

```json
{"action":{"type":"exec","argv":["/home/me/bin/backup"]},"at":"in 1h","every":"24h","count":7}
```

For reminders, use `{"type":"notify","title":"Stretch","body":"Take a break"}`
as the action. Calendar recurrence such as `"every":"day 09:00"` carries its
own time and omits `at`; `zone` selects an IANA time zone. Relative times are
resolved once, at submission. Environment is captured under the normal secrets
denylist for execution, but MCP cannot see captured environment variable names
or values; this is intentional, to keep host configuration and secrets out of
model-visible responses. A human troubleshooting a job can run `cued show ID`
to see captured and denylist-stripped variable names, or `cued show ID --json`
to inspect the actual captured values locally. MCP cannot request `keep_env`
or arbitrary overrides.

For dependent commands, pass a `workflow` instead of `action`. `entry` names
the first step; `steps` is an object keyed by step id. Each step has an `action`
and an optional ordered `transitions` array. A transition has `when` and `then`;
the first matching condition wins. Conditions use cued's graph forms such as
`"succeeded"`, `"failed"`, `"timed_out"`, `"always"`, or `{"exit_eq":0}`.
Effects use `{"goto":{"step":"test"}}` or
`{"end":{"outcome":"failure"}}`. For example:

```json
{
  "workflow": {
    "entry": "build",
    "steps": {
      "build": {
        "action": {"type":"exec","argv":["make","build"]},
        "transitions": [
          {"when":"succeeded","then":{"goto":{"step":"test"}}},
          {"when":"failed","then":{"goto":{"step":"notify_failure"}}}
        ]
      },
      "test": {
        "action": {"type":"exec","argv":["make","test"]},
        "transitions": [
          {"when":"failed","then":{"goto":{"step":"notify_failure"}}},
          {"when":"succeeded","then":{"end":{"outcome":"success"}}}
        ]
      },
      "notify_failure": {
        "action": {"type":"notify","title":"Build failed","body":"Check cued logs"},
        "transitions": [
          {"when":"always","then":{"end":{"outcome":"failure"}}}
        ]
      }
    }
  },
  "at":"in 1h"
}
```

Every exec step uses the `exec` capability and every notify step uses
`notify`; mixed workflows require both capabilities to be enabled. If either
capability is set to `approve`, the entire workflow waits for approval. Graphs
go through cued's normal validation, including target and loop checks.

A pending job does nothing until a human runs `cued approve j7`, reviews the
stored definition, and confirms. Cancel denies it. One-shot approval must
arrive before its scheduled instant; recurring pending approval expires after
seven days. Expiry releases the name and retains the audit record under normal
retention. Approval lasts across recurring firings, binds actual stored fields
including captured environment, and does not freeze referenced file contents.
List/show omit captured environment and approval hashes, and show approval and expiry status.

`logs` is separately disabled by default because it sends captured output to
the client. When enabled it returns a bounded tail (16 KiB by default, at most
64 KiB via `max_bytes`) and reports truncation. The approval mechanism is a
same-user guardrail, not a security boundary against deliberate CLI use.

### Waiting on a job

MCP has no wait tool. A blocking tool call would tie up the session and run
into client timeouts. Waiting is the CLI's `cued wait` instead, meant for an
agent's background shell (in Claude Code, a background Bash command). The
agent schedules a workflow, starts `cued wait j7` in the background, and ends
its turn. cued runs the steps, conditions, and waits. The agent spends no
tokens or context until the run ends. Then `wait` exits, the client wakes the
agent, and it reads a few lines: the outcome, the steps that ran with their
exit codes when the policy allows them, and an exit status it can act on.

This is deliberate. If you use the MCP server to limit what a model can do,
you can allowlist exactly one shell command, `cued wait` (in Claude Code, a
permission rule such as `Bash(cued wait:*)`), and still give the agent cheap,
durable waits. `wait` changes nothing, and it reports no more than MCP would:

- **The outcome** (status, failure reason, end time) is job status, governed
  by `read`. With `read = "off"`, the daemon refuses every `wait` poll,
  including `--wait` on a submitting command.
- **Each step's exit code and timing** are what MCP's `logs` tool exposes,
  so the daemon sends them only while `logs` is on. `logs` is off by default,
  so by default `wait` reports the outcome alone, and anyone, you included,
  reads step results with `cued logs`.
- **Captured output, environment, and the commands a job runs** are never
  printed.

The daemon enforces this, not the client, and re-reads the policy on every
poll. It uses its own environment, and for `wait` the settings only tighten:
`off` in its default `mcp.toml`, in the file named by its `CUED_MCP_CONFIG`,
or in its `CUED_MCP_READ` / `CUED_MCP_LOGS` wins, and nothing turns a switch
back on. Nothing set in the waiter's shell counts, and `wait` never starts a
daemon, which would inherit that shell's environment. So set the policy in
`mcp.toml`. An override passed only in the MCP server's launch config
reaches the daemon only if that server happened to start it. When that
leaves `wait` looser than the server, `cued mcp` prints a warning at
startup.

## Reliability and retention

Jobs and workflow positions are stored in SQLite. After downtime, recurring jobs
can catch up once or skip missed firings; they do not replay every missed interval.
Overlapping firings are skipped or coalesced into one queued run.

Notifications are queued durably until delivery or retention cleanup. A crash
between desktop delivery and recording its acknowledgement can cause a duplicate.
A process surviving a daemon crash can also continue its external work; the default
recovery policy holds interrupted runs for review instead of retrying them.

`cued gc` prunes retained history and logs. Defaults keep up to 20 runs per job
and remove terminal runs older than 30 days. Active and held runs are protected.

## Development

```sh
cargo test --release
cargo clippy --release --all-targets -- -D warnings
```

The cross-user access test (DESIGN.md §7.1) needs Docker. It runs the daemon
as two real UIDs in a locked-down container and checks that the owner is served
and the other user is refused:

```sh
cargo build --locked && scripts/security/run.sh target/debug/cued
```

CI runs it with fmt, clippy and the test suite (`.github/workflows/ci.yml`).
`controls.yml` fails a pull request that changes workflows, the access fixture,
or existing tests, until a maintainer applies the `reviewed-controls` label to
that revision.

The optional `test-hooks` feature enables fault injection for tests. Leave it off
in installations used for real jobs.

Licensed under the [MIT license](LICENSE).
