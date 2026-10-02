# Using cued

The commands at a glance; `cued --help` and `cued COMMAND --help` have the
full options.

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

## Waiting on a job

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

## Pausing and recovery

`pause` stops new work while allowing a claimed step to finish. `resume` allows
work to proceed again. A run held after an interruption can be inspected, then
continued or retried with `cued continue ID` or `cued retry ID`.

## Environment and export

Jobs capture their working directory and environment at submission. `show ID`
lists captured and stripped variable names; `show ID --json` deliberately exposes
captured values for local troubleshooting. `show ID --toml` exports an editable
workflow. See `cued --help` and [the design](../DESIGN.md) for scheduling, workflow,
and recovery semantics.
