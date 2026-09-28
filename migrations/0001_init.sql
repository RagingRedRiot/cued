-- Initial schema (DESIGN.md §5.3).
-- Definitions are documents: graph / schedule / env / policies are JSON on
-- the jobs row. Runtime state is relational, with the cursor flattened into
-- queryable columns so reconciliation (§3.4) is a plain query.

CREATE TABLE jobs (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,  -- rendered "j<n>"
    name          TEXT,                               -- unique among live jobs (partial index below)
    status        TEXT    NOT NULL,                   -- active | paused | done | cancelled | expired
    schedule      TEXT    NOT NULL,                   -- JSON: model::Schedule
    next_fire_at  TEXT,                               -- §4.2 re-arm target; NULL = no future firing
    queued_at     TEXT,                               -- §4.2 overlap=Queue: THE one pending firing
                                                      -- (a single slot — later firings coalesce into it)
    graph         TEXT    NOT NULL,                   -- JSON: model::Graph (validated at submit, §6.3)
    cwd           TEXT    NOT NULL,
    env           TEXT    NOT NULL,                   -- JSON: model::CapturedEnv (§7.5)
    policies      TEXT    NOT NULL,                   -- JSON: model::Policies + hooks
    created_at    TEXT    NOT NULL,                   -- RFC 3339 UTC (§5.3)
    -- §4.1: firings consumed, ever. A durable counter rather than a sum over
    -- runs, because the §4.2 queue slot coalesces many firings into one row
    -- and §10.2's GC deletes rows — a derived count would undercount the
    -- first and *fall* under the second.
    fired         INTEGER NOT NULL DEFAULT 0,
    approval      TEXT,                               -- JSON: Approval; NULL = ungated
    source        TEXT NOT NULL DEFAULT 'cli',         -- label only; never policy
    expired_at    TEXT,                               -- authorization deadline, UTC
    expiry_reason TEXT,                               -- JSON: ExpiryReason
    -- §5.3: the highest run id ever handed out. `MAX(runs.id)` alone falls
    -- when §10.2's GC prunes a job's newest runs, and the next firing would
    -- then reuse an id — one `j7.r3` naming two runs, and the log directory
    -- GC removes for the old one belonging to the new one.
    run_seq       INTEGER NOT NULL DEFAULT 0
);

-- §2: name is unique among *live* jobs only; reusable once the holder is done.
CREATE UNIQUE INDEX jobs_live_name
    ON jobs(name)
    WHERE name IS NOT NULL AND status IN ('active', 'paused');

CREATE INDEX jobs_next_fire ON jobs(next_fire_at) WHERE next_fire_at IS NOT NULL;

CREATE TABLE runs (
    job_id        INTEGER NOT NULL REFERENCES jobs(id),
    id            INTEGER NOT NULL,                   -- per-job sequence: rendered "j7.r3"
    scheduled_for TEXT    NOT NULL,                   -- the firing instant this run represents
    started_at    TEXT,
    ended_at      TEXT,
    status        TEXT    NOT NULL,                   -- pending | running | waiting | held | done
                                                      -- | failed | missed | skipped | cancelled
    -- model::Cursor, flattened (§3.3, §5.3)
    cursor_kind   TEXT    NOT NULL,                   -- waiting | running | held | done
    cursor_step   TEXT,
    cursor_at     TEXT,                               -- waiting: the frozen absolute target (§3.2)
    held_reason   TEXT,                               -- interrupted | …
    fail_reason   TEXT,                               -- deadline | max_visits | … (§3.2)
    -- one Skipped row covers a coalesced range of missed firings (§4.2)
    skipped_from  TEXT,
    skipped_count INTEGER,
    -- §3.4: `cued retry` rewinds in place; the epoch is the rewind
    -- generation — attempts keep appending, visit counters count within it.
    epoch         INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (job_id, id)
);

-- Startup reconciliation is exactly this query (§3.4).
CREATE INDEX runs_reconcile ON runs(cursor_kind)
    WHERE cursor_kind IN ('waiting', 'running', 'held');

-- GC prunes by age (§10.2).
CREATE INDEX runs_gc ON runs(ended_at) WHERE ended_at IS NOT NULL;

-- "Does this job have a live run?" is asked on every firing, close, queue
-- drain, retry and cancel (§4.2). Without this it walks the job's whole
-- retained history through the primary key; with it, only the live rows.
-- Queries must spell the predicate exactly `cursor_kind != 'done'` for the
-- planner to prove the partial index applies.
CREATE INDEX runs_live ON runs(job_id, id) WHERE cursor_kind != 'done';

CREATE TABLE step_runs (
    job_id       INTEGER NOT NULL,
    run_id       INTEGER NOT NULL,
    step_id      TEXT    NOT NULL,
    attempt      INTEGER NOT NULL,                    -- keeps counting across manual retries (§3.4)
    started_at   TEXT    NOT NULL,
    ended_at     TEXT,
    exit_code    INTEGER,
    timed_out    INTEGER NOT NULL DEFAULT 0,
    outcome_edge INTEGER,                             -- which transition matched; NULL = derived End (§3.2)
    epoch        INTEGER NOT NULL DEFAULT 0,          -- runs.epoch at attempt start
    PRIMARY KEY (job_id, run_id, step_id, attempt),
    FOREIGN KEY (job_id, run_id) REFERENCES runs(job_id, id)
);

-- §3.5: the durable notification queue. Delivery is best-effort and retried;
-- rows with delivered_at IS NULL are picked up on a slow tick.
CREATE TABLE notifications (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    job_id       INTEGER NOT NULL REFERENCES jobs(id),
    run_id       INTEGER,
    title        TEXT    NOT NULL,
    body         TEXT    NOT NULL,
    created_at   TEXT    NOT NULL,
    delivered_at TEXT,
    -- The acknowledging server's unique bus name and the ID it returned; the
    -- ID means nothing outside that server lifetime. NULL = no receipt.
    delivery_server TEXT,
    delivery_id     INTEGER
);

CREATE INDEX notifications_undelivered ON notifications(id) WHERE delivered_at IS NULL;

-- §10.2 deletes a pruned run's notifications by (job, run); without an
-- index each of those deletes scanned the whole table.
CREATE INDEX notifications_run ON notifications(job_id, run_id);

CREATE TRIGGER jobs_definition_approval
AFTER UPDATE OF name, schedule, graph, cwd, env, policies ON jobs
WHEN OLD.approval IS NOT NULL AND (
    OLD.name IS NOT NEW.name OR OLD.schedule != NEW.schedule OR OLD.graph != NEW.graph
    OR OLD.cwd != NEW.cwd OR OLD.env != NEW.env OR OLD.policies != NEW.policies)
BEGIN
    UPDATE jobs SET approval = json_set(OLD.approval, '$.state', 'pending', '$.approved_at', NULL)
    WHERE id = NEW.id;
END;

-- Every daemon pass can cheaply find only the jobs whose deadline matters.
CREATE INDEX jobs_pending_approval ON jobs(id)
    WHERE json_extract(approval, '$.state') = 'pending'
      AND status IN ('active', 'paused');
