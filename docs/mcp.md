# MCP server

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

## Waiting on a job

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
