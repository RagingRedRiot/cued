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
cued cancel j1
```

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
