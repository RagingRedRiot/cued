---
name: cued
description: Schedule commands and desktop reminders with cued, a durable per-user scheduler, and wait on them without polling. Use when a command should run later, on a recurring schedule, or as a multi-step workflow with conditional branches and timed delays; when the user says "schedule", "run this at", "remind me", "every N minutes", "chain these", or "let me know when it finishes"; and whenever a session needs to wait on a cued job. Covers `cued submit` TOML authoring, `cued wait` in a background shell, and job inspection.
---

# cued

cued is a per-user scheduler for Linux. A daemon runs jobs from a SQLite
store, so schedules, workflow positions, and timed delays survive restarts.
This skill is everything a session needs to drive it from the CLI.

## Choosing a form

| Need | Use |
| --- | --- |
| One command at one time | `cued at [--name N] "<time>" -- <argv...>` |
| A desktop reminder | `cued remind "<time>" "<title>"` |
| A recurring command | `cued every [--name N] "<spec>" -- <argv...>` |
| A linear sequence, stop on failure | `cued chain "<cmd>" --then "<cmd>" [--then-after 15m "<cmd>"]` |
| Branches, output checks, notifications, loops | a TOML file and `cued submit FILE` |

cued's own flags go **before** the time or spec. Everything after the time is
the command. After `--` the command is argv, executed directly. A single
quoted string without `--` runs under `sh -c`.

Other useful flags: `remind --every "<spec>"` makes a reminder recurring;
`chain --on-fail continue` keeps going past a failed link; `--zone IANA`
reads the times as that zone instead of the machine's (the default);
`submit --at`/`--every` override the file's schedule.

A job runs later, unattended, as the user, with the user's environment.
Before scheduling anything that deletes, deploys, sends, or spends, confirm the
exact command and time with the user. cued is alpha software and does not
guarantee exactly-once execution.

## Submitting

1. Write the TOML to a temporary or scratch location. cued stores the
   definition at submit, so the file isn't needed afterwards. Files the job
   *reads at run time* must live somewhere that survives until the job runs.
2. Make sure `at` is still in the future. An explicitly dated past instant is
   rejected; a bare time of day (`"9am"`) rolls to its next occurrence.
3. Run `cued submit FILE`. It validates the whole graph (entry, goto targets,
   loop bounds, unknown keys) before anything is stored. There is no dry run.
4. Report the job id (`jN`) and the resolved schedule cued echoes back.

Jobs submitted through the CLI are live at once. `cued approve` only gates
jobs submitted through cued's MCP server, so a CLI job never needs approval,
and `pending`/`waiting` in `cued list` is not an approval hold.

The job captures the **working directory and environment of the shell that
submitted it**. Submit from the directory the steps expect, or set `cwd` in
the file. Variables matching the secrets denylist are stripped; `--keep-env
VAR` keeps one (CLI forms only). See [Secrets and sensitive
data](#secrets-and-sensitive-data) before using either.

## TOML reference

```toml
name  = "nightly-build"        # unique among live jobs; usable in place of jN
at    = "2026-06-25 09:00"     # or "in 90m", "9am tomorrow"
zone  = "America/New_York"     # IANA zone the file's times are written in
entry = "build"                # required when there is more than one step
# every = "6h" | "day 09:00" | "weekdays 9am" | "mon,wed,fri 17:30" | "month on 1,15 at 9am"
# until = "<instant>"  count = 7
# catch_up = "run_once" | "skip"      (after downtime; default run_once)
# overlap  = "skip" | "queue"         (firing while a run is live; default skip)

on_hold    = { title = "nightly-build held", body = "cued show nightly-build" }
on_failure = { title = "nightly-build failed" }
# on_success, on_missed take the same { title, body } shape

[defaults]
cwd = "/path/to/project"
env = { RUST_LOG = "info" }    # overlaid on the captured environment
deadline = "2h"                # bounds the whole run, loops included
on_interrupt = "hold"          # hold | fail | retry (retry acts as hold unless the step sets restart_safe)
missed_wait = "run_asap"       # run_asap | abandon

[[step]]
id = "build"
run = ["make", "build"]        # array = argv; a string = sh -c
timeout = "20m"
on.success = { goto = "test" }
on.fail    = { goto = "notify_fail" }

[[step]]
id = "test"
run = "make test 2>&1 | tee test.log"
timeout = "30m"
[[step.transition]]            # ordered; the first match wins
when = { all = ["succeeded", { stdout_matches = '(?m)^0 failed$' }] }
then = { goto = "done" }
[[step.transition]]
when = "always"
then = { goto = "notify_fail" }

[[step]]
id = "done"
notify = { title = "Build passed", body = "cued logs nightly-build" }
on.always = { end = "success" }

[[step]]
id = "notify_fail"
notify = { title = "Build failed", body = "cued logs nightly-build" }
on.always = { end = "failure" }
```

**Step keys:** `id`, exactly one of `run` or `notify`, then optional `cwd`,
`env`, `timeout`, `kill_grace`, `max_visits`, `restart_safe`, `missed_wait`,
and transitions. Set `restart_safe = true` only on a step that is harmless to
run again from the start after an interruption, such as a read-only check or
an idempotent build.

**Transitions.** Use the shorthand `on.success`, `on.fail`, `on.timeout`, and
`on.always`, or ordered `[[step.transition]]` entries with `when` and `then`.

- Condition words: `"succeeded"`, `"failed"`, `"timed_out"`, `"always"`.
- Condition tables: `{ exit = 0 }`, `{ exit_ne = 0 }`, `{ exit_in = [0, 3] }`,
  `{ stdout_contains = "..." }`, `{ stdout_matches = '...' }`, the same two for
  `stderr`, and `{ all = [ ... ] }` to require several conditions.
- A step killed by a signal has no exit code. It matches only `failed`,
  `timed_out`, or `always`.
- If no transition matches, the run ends: success on exit 0, failure
  otherwise.

**Effects:**

- `{ goto = "step" }`, optionally with a delay before the next step:
  `after = "15m"`, `after = { until = "<instant>" }`, or a backoff
  `after = { start = "1m", factor = 2.0, max = "30m" }`.
- `{ end = "success" }` or `{ end = "failure" }`. An `end` takes no `after`.

Delays are durable. A restart does not reset a delay that is already counting
down.

**Loops.** Every back edge needs a bound: `max_visits` on a step in the loop,
or a job `deadline`. Submit rejects an unbounded cycle.

**Gotchas:**

- Write regexes as TOML literal strings (`'...'`) so backslashes survive.
  Use `(?m)^...$` to anchor on a whole line of output.
- An invalid regex is not rejected at submit. It silently never matches, so
  test patterns before relying on them.
- Unknown keys are errors, so a misspelled key fails at submit instead of
  being ignored.
- Times use `s m h d w` durations (`1h30m`), ISO dates, or month names. There
  is no month or year duration unit; use a calendar rule instead.
- To start from an existing job, run `cued show ID --toml`. It prints an
  editable file that resubmits as-is.

## Waiting on a job

Never wait by sleeping, by polling `cued list`, or by running `cued logs -f`
or `--wait` in the foreground. Instead:

1. Submit the job (without `--wait`).
2. Start `cued wait jN` as a **background** shell command. In Claude Code,
   that is the Bash tool with `run_in_background: true`.
3. End the turn. The session costs nothing while cued runs the job, and the
   harness wakes the session when `wait` exits.

Start a background wait whenever the task depends on the job's outcome or the
user wants to hear when it finishes. For a job that won't fire for days and
that nobody is waiting on, report the id and schedule and skip the wait.

`wait` waits for the run in progress, or else the next run to fire. On a
recurring job it therefore returns after the next firing, not at the end of
the schedule. Use `--run N` to wait for a specific run, and `--timeout 2h` to
give up after a set time. `wait` never starts a daemon.

| Exit | Meaning | Next move |
| --- | --- | --- |
| 0 | done | read `cued logs jN` if the output matters |
| 3 | failed | `cued logs jN` to find the failing step |
| 4 | held (interrupted mid-step) | show the user `cued show jN`; continuing or retrying is their call |
| 5 | ended: cancelled, missed, expired, or out of runs | report it |
| 124 | `--timeout` ran out | the job is still running; wait again if needed |
| 1 | cued error, including no daemon running | report the message |
| 2 | usage error | fix the command |
| 6 | `--wait` only: submitted, but the wait failed | do **not** resubmit; `cued wait jN` |

`wait` reports what cued's MCP policy allows, read from the daemon's
`~/.config/cued/mcp.toml`. By default (`read = "on"`, `logs = "off"`) it
prints the outcome only, without per-step exit codes. With `logs = "on"` it
adds each step's exit code and timing. It never prints captured output or
environment, so read step output with `cued logs jN`.

## Inspecting and controlling jobs

| Command | Purpose |
| --- | --- |
| `cued list [--all] [--json]` | Live jobs and recently ended runs; held runs are always shown |
| `cued show ID [--toml]` | Definition, schedule, status, and captured env *names* |
| `cued logs ID [--run N] [--step S] [--attempt N] [--json]` | Captured output of the latest run, or a chosen run, step, or attempt |
| `cued cancel ID` | Stop re-arming and terminate any live run |
| `cued pause ID` / `cued resume ID` | Start nothing new / re-arm to the next future instant |
| `cued continue ID` | Resume a held run where it parked |
| `cued retry ID [--from STEP]` | Re-run a held or finished run |

`ID` is `jN` or, for an active or paused job, its name. Ended jobs need `jN`.

Cancelling, retrying, and continuing change what runs. Do them only when the
user asks. A held run means a step was interrupted and its external effects
are unknown, so the user decides whether to continue or retry it.

Leave these to the user unless they ask for that exact action:

- `cued approve` is a human review step. It is interactive, and an agent must
  never confirm it on the user's behalf.
- `cued uninstall` deletes the store, all job history, and logs.
  `cued upgrade --force` interrupts running steps. `cued gc` prunes history.
  `cued setup` installs or removes system services.
- `~/.config/cued/mcp.toml`, `config.toml`, and the `CUED_MCP_*` variables
  are the user's policy. If `wait` is refused because MCP `read` is off,
  report it. Don't edit the policy to get around it.

## Secrets and sensitive data

Anything cued prints in a session lands in the model's context and the
transcript. Keep secrets out of both.

- **Never run `cued show --json`.** It prints the captured environment's
  values, secrets included. Plain `cued show` lists variable names only.
- **Never write a secret's value into a job file.** An explicit `env` entry
  in `[defaults]` or a step overrides the denylist. It is stored in plain text
  in cued's database, and a step's `env` is printed by `cued show --toml`.
  Have the step read the secret at run time (from a file, a secret manager, or
  a variable the user's environment already provides).
- **The denylist matches names, not values.** The default strips
  `*_TOKEN`, `*_SECRET`, `*_KEY`, `*PASSWORD*`, and `*_CREDENTIALS`. Anything
  else is captured and stored, such as `DATABASE_URL`, `*_PAT`, or a
  credential-bearing `*_URL`. If the shell holds secrets under other names,
  submit from a cleaner shell, or tell the user they will be stored.
- Use `--keep-env` only when the user asks for that variable to be kept.
- **`cued logs` returns raw command output**, which can include credentials,
  personal data, or customer data. Read only the run and step you need,
  prefer `--step` over the whole run, and don't paste output beyond what
  answers the question.
- The commands and arguments in a job are stored and shown by `cued show`,
  so don't pass secrets as command-line arguments either.

## If cued's MCP tools are what you have

cued's MCP server exposes `schedule`, `list`, `show`, `cancel`, and `logs`,
gated by the user's `mcp.toml`. A job scheduled with `exec` or `notify` set to
`approve` waits until a human runs `cued approve jN`, so tell the user it is
pending. The MCP workflow schema spells conditions and effects differently
from the TOML file (for example `{"exit_eq":0}` and
`{"goto":{"step":"test"}}`), so follow the tool's schema, not this file's
TOML. Waiting is still `cued wait` in a background shell; MCP has no wait
tool.

## Troubleshooting

- **No daemon running.** `cued list` and `cued submit` start one on demand.
  `cued setup --status` shows whether a persistence backend keeps the daemon
  alive across logout and reboot.
- **A step can't find a file or tool.** Check the captured working directory
  and environment with `cued show ID`. The job runs with what the submitting
  shell had, not with a login shell's setup.
- **A branch never fires.** Check the regex (an invalid one never matches),
  the transition order (the first match wins), and whether the step was
  signal-killed, which leaves no exit code to match.
