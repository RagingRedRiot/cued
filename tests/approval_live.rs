//! Follow-up live checks (testing/followup/approval-expiry): the real `cued mcp`
//! stdio process and a real auto-spawned `cued daemon`, in a private HOME/XDG
//! tree with a private socket directory and a dead private D-Bus address, so
//! nothing here can reach the host daemon or desktop notification service.
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const VISIBLE_NAME: &str = "FOLLOWUP_VISIBLE_NAME_Q7";
const VISIBLE_VALUE: &str = "followup-value-Z9";
const STRIPPED_NAME: &str = "FOLLOWUP_API_TOKEN";
const STRIPPED_VALUE: &str = "token-value-K3";

struct Sandbox {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Sandbox {
    fn new() -> Result<Self> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().to_path_buf();
        for sub in ["", "home", "data", "config", "sock", "run"] {
            let path = root.join(sub);
            std::fs::create_dir_all(&path)?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(Self { _dir: dir, root })
    }
    fn policy(&self) -> PathBuf {
        self.root.join("config/cued/mcp.toml")
    }
    fn write_policy(&self, text: &str) -> Result<()> {
        std::fs::create_dir_all(self.policy().parent().unwrap())?;
        // Write-then-rename so a concurrently reading MCP call never sees half a file.
        let temp = self.root.join("mcp.toml.tmp");
        std::fs::write(&temp, text)?;
        std::fs::rename(temp, self.policy())?;
        Ok(())
    }
    /// A deterministic environment: no inherited host variables at all.
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cued"));
        command
            .args(args)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.root.join("home"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_RUNTIME_DIR", self.root.join("run"))
            .env("CUED_SOCKET_DIR", self.root.join("sock"))
            // Private and deliberately dead: delivery fails, rows stay queued.
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path={}", self.root.join("run/no-bus").display()),
            )
            .env(VISIBLE_NAME, VISIBLE_VALUE)
            .env(STRIPPED_NAME, STRIPPED_VALUE);
        command
    }
    fn cued(&self, args: &[&str], stdin: &str) -> Result<std::process::Output> {
        let mut child = self
            .command(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        child.stdin.take().unwrap().write_all(stdin.as_bytes())?;
        Ok(child.wait_with_output()?)
    }
    fn show_json(&self, job: &str) -> Result<Value> {
        let output = self.cued(&["show", job, "--json"], "")?;
        anyhow::ensure!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(serde_json::from_slice(&output.stdout)?)
    }
    fn db(&self) -> PathBuf {
        self.root.join("data/cued/cued.db")
    }
    fn mcp(&self, overrides: &[(&str, &str)]) -> Result<Mcp> {
        let mut command = self.command(&["mcp"]);
        for (name, value) in overrides {
            command.env(name, value);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sent, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if sent.send(line).is_err() {
                    break;
                }
            }
        });
        let mut mcp = Mcp {
            child,
            stdin,
            lines,
            next: 1,
            transcript: Vec::new(),
        };
        let init = mcp.rpc(
            "initialize",
            json!({"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"followup","version":"1"}}),
        )?;
        anyhow::ensure!(init["result"]["protocolVersion"] == "2025-06-18");
        Ok(mcp)
    }
    fn daemon_pid(&self) -> Option<i32> {
        let want = format!("HOME={}", self.root.join("home").display());
        for entry in std::fs::read_dir("/proc").ok()?.flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
                continue;
            };
            let (Ok(environ), Ok(comm)) = (
                std::fs::read(entry.path().join("environ")),
                std::fs::read_to_string(entry.path().join("comm")),
            ) else {
                continue;
            };
            if comm.trim() == "cued"
                && environ.split(|b| *b == 0).any(|v| v == want.as_bytes())
                && std::fs::read_to_string(entry.path().join("cmdline"))
                    .is_ok_and(|cmd| cmd.contains("daemon"))
            {
                return Some(pid);
            }
        }
        None
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        // Only ever the daemon serving this private HOME.
        if let Some(pid) = self.daemon_pid() {
            unsafe { libc::kill(pid, libc::SIGTERM) };
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline && unsafe { libc::kill(pid, 0) } == 0 {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

struct Mcp {
    child: Child,
    stdin: ChildStdin,
    lines: mpsc::Receiver<std::io::Result<String>>,
    next: i64,
    /// Every raw line the model would see, for whole-session leak checks.
    transcript: Vec<String>,
}

impl Mcp {
    fn rpc(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next;
        self.next += 1;
        writeln!(
            self.stdin,
            "{}",
            json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
        )?;
        self.stdin.flush()?;
        let line = self
            .lines
            .recv_timeout(Duration::from_secs(30))
            .context("MCP reply timed out")??;
        self.transcript.push(line.clone());
        let value: Value = serde_json::from_str(&line)?;
        anyhow::ensure!(value["id"] == id, "out-of-order reply {value}");
        Ok(value)
    }
    /// (tool text parsed as JSON when possible, isError)
    fn tool(&mut self, name: &str, arguments: Value) -> Result<(Value, bool)> {
        let reply = self.rpc("tools/call", json!({"name":name,"arguments":arguments}))?;
        let result = &reply["result"];
        let text = result["content"][0]["text"]
            .as_str()
            .with_context(|| format!("no text in {reply}"))?;
        let error = result["isError"].as_bool().context("isError")?;
        let value = serde_json::from_str(text).unwrap_or(Value::String(text.into()));
        Ok((value, error))
    }
    fn ok(&mut self, name: &str, arguments: Value) -> Result<Value> {
        let (value, error) = self.tool(name, arguments.clone())?;
        if error {
            bail!("{name} {arguments} failed: {value}");
        }
        Ok(value)
    }
    fn err(&mut self, name: &str, arguments: Value) -> Result<String> {
        let (value, error) = self.tool(name, arguments.clone())?;
        if !error {
            bail!("{name} {arguments} unexpectedly succeeded: {value}");
        }
        Ok(value.as_str().unwrap_or_default().to_string())
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn job_ref(value: &Value) -> String {
    format!("j{}", value["job"].as_i64().expect("job id"))
}

fn wait_for(what: &str, timeout: Duration, mut check: impl FnMut() -> Result<bool>) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if check()? {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    bail!("timed out waiting for {what}")
}

fn assert_no_leak(transcript: &[String]) {
    for line in transcript {
        for forbidden in [
            VISIBLE_NAME,
            VISIBLE_VALUE,
            STRIPPED_NAME,
            STRIPPED_VALUE,
            "definition_hash",
            "\"stripped\"",
        ] {
            assert!(
                !line.contains(forbidden),
                "MCP output exposed {forbidden:?}: {line}"
            );
        }
    }
}

fn graphs() -> Vec<(&'static str, Value, bool, bool)> {
    // (label, schedule arguments, uses exec, uses notify)
    vec![
        (
            "exec",
            json!({"action":{"type":"exec","argv":["/bin/true"]}}),
            true,
            false,
        ),
        (
            "notify",
            json!({"action":{"type":"notify","title":"matrix"}}),
            false,
            true,
        ),
        (
            "mixed",
            json!({"workflow":{"entry":"a","steps":{
                "a":{"action":{"type":"exec","argv":["/bin/true"]},"transitions":[{"when":"always","then":{"goto":{"step":"b"}}}]},
                "b":{"action":{"type":"notify","title":"done"}}}}}),
            true,
            true,
        ),
        // Reachable only through `cued retry --from x`, never by the entry.
        (
            "unreachable-exec",
            json!({"workflow":{"entry":"n","steps":{
                "n":{"action":{"type":"notify","title":"entry"}},
                "x":{"action":{"type":"exec","argv":["/bin/true"]}}}}}),
            true,
            true,
        ),
        // A branch that ordinary transitions will not take (notify succeeds).
        (
            "conditional-exec",
            json!({"workflow":{"entry":"n","steps":{
                "n":{"action":{"type":"notify","title":"entry"},"transitions":[{"when":"failed","then":{"goto":{"step":"x"}}}]},
                "x":{"action":{"type":"exec","argv":["/bin/true"]}}}}}),
            true,
            true,
        ),
    ]
}

/// Every open/approve/closed combination against every graph shape, through the
/// real stdio process and daemon. The policy file uses the documented spelling
/// (`open | approve | closed`); half the matrix uses env overrides instead.
#[test]
fn policy_matrix_gates_whole_graphs_including_unreachable_exec() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let levels = ["open", "approve", "closed"];
    let mut created = Vec::new();
    for (index, exec) in levels.iter().enumerate() {
        for notify in levels {
            let via_env = index % 2 == 1;
            let mut mcp = if via_env {
                sandbox.write_policy("exec = 'closed'\nnotify = 'closed'\n")?;
                sandbox.mcp(&[("CUED_MCP_EXEC", exec), ("CUED_MCP_NOTIFY", notify)])?
            } else {
                sandbox.write_policy(&format!("exec = '{exec}'\nnotify = '{notify}'\n"))?;
                sandbox.mcp(&[])?
            };
            for (label, mut args, uses_exec, uses_notify) in graphs() {
                args["at"] = json!("in 1h");
                let needed: Vec<(&str, &str)> =
                    [("exec", *exec, uses_exec), ("notify", notify, uses_notify)]
                        .into_iter()
                        .filter(|(_, _, used)| *used)
                        .map(|(key, level, _)| (key, level))
                        .collect();
                let context = format!("exec={exec} notify={notify} graph={label}");
                if let Some((key, _)) = needed.iter().find(|(_, level)| *level == "closed") {
                    let message = mcp.err("schedule", args)?;
                    assert!(
                        message.contains(&format!("`{key}`"))
                            && message.contains("capability closed"),
                        "{context}: {message}"
                    );
                    continue;
                }
                let gated = needed.iter().any(|(_, level)| *level == "approve");
                let response = mcp.ok("schedule", args)?;
                assert_eq!(response["pending_approval"], gated, "{context}");
                let job = job_ref(&response);
                let stored = sandbox.show_json(&job)?;
                if gated {
                    assert_eq!(stored["approval"]["state"], "pending", "{context}");
                    assert_eq!(response["approval_command"], format!("cued approve {job}"));
                } else {
                    assert!(stored["approval"].is_null(), "{context}");
                }
                created.push(job);
            }
            assert_no_leak(&mcp.transcript);
        }
    }
    // Error replies created nothing: exactly the successful submissions exist.
    let listed = sandbox.cued(&["list", "--all", "--json"], "")?;
    let listed: Value = serde_json::from_slice(&listed.stdout)?;
    assert_eq!(listed.as_array().map(Vec::len), Some(created.len()));
    // Denial stays available with every capability closed.
    sandbox.write_policy("exec = 'closed'\nnotify = 'closed'\nread = 'off'\n")?;
    let mut closed = sandbox.mcp(&[])?;
    for job in &created {
        closed.ok("cancel", json!({"job":job}))?;
    }
    assert_no_leak(&closed.transcript);
    Ok(())
}

/// Policy is read per call, so a user who revokes (or grants) a capability in
/// mcp.toml does not need to find and restart a long-lived MCP process for the
/// change to apply. An unreadable policy fails closed.
#[test]
fn policy_file_changes_apply_to_a_running_mcp_process() -> Result<()> {
    let sandbox = Sandbox::new()?;
    sandbox.write_policy("exec = 'open'\n")?;
    let mut mcp = sandbox.mcp(&[])?;
    let exec = json!({"action":{"type":"exec","argv":["/bin/true"]},"at":"in 1h"});
    let open = mcp.ok("schedule", exec.clone())?;
    assert_eq!(open["pending_approval"], false);

    sandbox.write_policy("exec = 'closed'\n")?;
    let message = mcp.err("schedule", exec.clone())?;
    assert!(message.contains("`exec`"), "{message}");

    sandbox.write_policy("exec = 'approve'\n")?;
    let gated = mcp.ok("schedule", exec.clone())?;
    assert_eq!(gated["pending_approval"], true);

    sandbox.write_policy("exec = 'sometimes'\n")?;
    let message = mcp.err("schedule", exec.clone())?;
    assert!(message.contains("mcp.toml"), "{message}");

    sandbox.write_policy("read = 'off'\n")?;
    assert!(mcp.err("list", json!({}))?.contains("`read`"));

    // Submission-time policy is recorded on the job: tightening the policy
    // later does not retroactively gate a job accepted as open.
    assert!(sandbox.show_json(&job_ref(&open))?["approval"].is_null());
    let listed = sandbox.cued(&["list", "--all", "--json"], "")?;
    let listed: Value = serde_json::from_slice(&listed.stdout)?;
    assert_eq!(listed.as_array().map(Vec::len), Some(2));
    assert_no_leak(&mcp.transcript);
    Ok(())
}

/// No MCP output — results, previews, logs, or error text — carries a captured
/// variable name, value, stripped name, or definition hash. The local CLI still
/// shows names (summary) and actual values (`--json`).
#[test]
fn mcp_never_exposes_environment_or_hash_while_cli_keeps_both_views() -> Result<()> {
    let sandbox = Sandbox::new()?;
    sandbox.write_policy("exec = 'open'\nnotify = 'approve'\nlogs = 'on'\n")?;
    // The script, not the model-visible argv, reads the variable.
    let script = sandbox.root.join("probe.sh");
    std::fs::write(
        &script,
        format!(
            "printf 'value=%s|' \"${VISIBLE_NAME}\"\nprintf 'token=%s|' \"${STRIPPED_NAME}\"\nprintf done\nexit 3\n"
        ),
    )?;
    let mut mcp = sandbox.mcp(&[])?;
    let exec = mcp.ok(
        "schedule",
        json!({"action":{"type":"exec","argv":["/bin/sh", script.to_str().unwrap()]},"at":"in 1s","name":"probe"}),
    )?;
    let job = job_ref(&exec);
    wait_for("the probe to finish", Duration::from_secs(30), || {
        Ok(sandbox.show_json(&job)?["status"] == "done")
    })?;
    let pending = mcp.ok(
        "schedule",
        json!({"action":{"type":"notify","title":"gated","body":"review"},"at":"in 1h","name":"gated"}),
    )?;
    let pending_job = job_ref(&pending);

    let logs = mcp.ok("logs", json!({"job":job}))?;
    let content = logs["attempts"][0]["content"].as_str().unwrap();
    assert!(
        content.starts_with("value=[redacted]|token=|done"),
        "{content}"
    );
    let shown = mcp.ok("show", json!({"job":pending_job}))?;
    assert_eq!(shown["definition"]["approval"]["state"], "pending");
    assert!(shown["definition"].get("env").is_none());
    mcp.ok("list", json!({"all":true}))?;
    mcp.ok("show", json!({"job":job}))?;
    mcp.rpc("tools/list", json!({}))?;
    // Error paths.
    mcp.err("show", json!({"job":"j999"}))?;
    let unknown = mcp.ok("logs", json!({"job":job,"step":"nope"}))?;
    assert_eq!(unknown["attempts"], json!([]));
    mcp.err("logs", json!({"job":job,"max_bytes":0}))?;
    mcp.err("logs", json!({"job":pending_job}))?;
    mcp.err("cancel", json!({"job":job}))?;
    mcp.err("cancel", json!({"job":"j999"}))?;
    mcp.err(
        "schedule",
        json!({"action":{"type":"notify","title":"dup"},"at":"in 1h","name":"gated"}),
    )?;
    mcp.err(
        "schedule",
        json!({"action":{"type":"exec","argv":["/bin/true"]},"at":"in 1h","cwd":"relative"}),
    )?;
    mcp.err(
        "schedule",
        json!({"action":{"type":"exec","argv":["/bin/true"]},"at":"in 1h","env":{"A":"B"}}),
    )?;
    mcp.ok("cancel", json!({"job":pending_job}))?;
    assert_no_leak(&mcp.transcript);

    // CLI summary names captured and stripped variables but no values.
    let human = sandbox.cued(&["show", &job], "")?;
    let human = String::from_utf8(human.stdout)?;
    assert!(
        human.contains(VISIBLE_NAME) && human.contains(STRIPPED_NAME),
        "{human}"
    );
    assert!(!human.contains(VISIBLE_VALUE) && !human.contains(STRIPPED_VALUE));
    // CLI --json carries the actual captured value, never the stripped one or a hash.
    let json = sandbox.cued(&["show", &job, "--json"], "")?;
    let json = String::from_utf8(json.stdout)?;
    assert!(json.contains(VISIBLE_VALUE), "{json}");
    assert!(!json.contains(STRIPPED_VALUE));
    let json = sandbox.cued(&["show", &pending_job, "--json"], "")?;
    let json = String::from_utf8(json.stdout)?;
    assert!(
        json.contains("\"approval\"") && !json.contains("definition_hash"),
        "{json}"
    );
    Ok(())
}

/// A one-shot pending at its instant expires; the approval window is closed at
/// equality, nothing runs, and the name is free for an immediate resubmission —
/// without any list/show call to trigger a sweep first.
#[test]
fn live_one_shot_expires_at_its_instant_and_releases_its_name() -> Result<()> {
    let sandbox = Sandbox::new()?;
    sandbox.write_policy("exec = 'approve'\n")?;
    let marker = sandbox.root.join("must-not-exist");
    let mut mcp = sandbox.mcp(&[])?;
    let scheduled = mcp.ok(
        "schedule",
        json!({"action":{"type":"exec","argv":["/usr/bin/touch", marker.to_str().unwrap()]},"at":"in 2s","name":"expiring"}),
    )?;
    let job = job_ref(&scheduled);
    let at = sandbox.show_json(&job)?["schedule"]["once"]["at"]
        .as_str()
        .unwrap()
        .parse::<jiff::Timestamp>()?;
    while jiff::Timestamp::now() <= at {
        std::thread::sleep(Duration::from_millis(20));
    }
    // Directly after the deadline — no list/show sweep, and the daemon's own
    // 30-second tick has not come round.
    let replacement = mcp.ok(
        "schedule",
        json!({"action":{"type":"exec","argv":["/bin/true"]},"at":"in 1h","name":"expiring"}),
    )?;
    assert_ne!(job_ref(&replacement), job);
    let shown = mcp.ok("show", json!({"job":job}))?;
    assert_eq!(shown["status"], "expired");
    assert_eq!(shown["definition"]["expiry_reason"], "scheduled_at_passed");
    assert_eq!(
        shown["definition"]["expired_at"]
            .as_str()
            .unwrap()
            .parse::<jiff::Timestamp>()?,
        at
    );
    let approve = sandbox.cued(&["approve", &job], "y\n")?;
    assert!(!approve.status.success());
    std::thread::sleep(Duration::from_millis(500));
    assert!(!marker.exists());
    mcp.ok("cancel", json!({"job":job_ref(&replacement)}))?;
    assert_no_leak(&mcp.transcript);
    Ok(())
}

/// After a human approval, a definition changed underneath (with or without the
/// re-pend trigger) never executes on the live daemon; no side effect happens.
#[test]
fn live_tampered_approved_definitions_never_execute() -> Result<()> {
    let sandbox = Sandbox::new()?;
    sandbox.write_policy("exec = 'approve'\n")?;
    let mut mcp = sandbox.mcp(&[])?;
    let mut jobs = Vec::new();
    for label in ["trigger", "no-trigger"] {
        let approved = sandbox.root.join(format!("{label}-approved-argv"));
        let scheduled = mcp.ok(
            "schedule",
            json!({"action":{"type":"exec","argv":["/usr/bin/touch", approved.to_str().unwrap()]},"at":"in 3s","name":label}),
        )?;
        let job = job_ref(&scheduled);
        let approve = sandbox.cued(&["approve", &job], "y\n")?;
        assert!(
            approve.status.success(),
            "{}",
            String::from_utf8_lossy(&approve.stderr)
        );
        jobs.push((label, job, approved));
    }
    let tampered = sandbox.root.join("tampered-argv");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let pool =
            sqlx::SqlitePool::connect(&format!("sqlite://{}", sandbox.db().display())).await?;
        let graph = serde_json::to_string(&cued::submit::single_shell_graph(vec![
            "/usr/bin/touch".into(),
            tampered.to_string_lossy().into_owned(),
        ]))?;
        for (label, job, _) in &jobs {
            let id: i64 = job[1..].parse()?;
            if *label == "no-trigger" {
                sqlx::query("DROP TRIGGER IF EXISTS jobs_definition_approval")
                    .execute(&pool)
                    .await?;
            }
            sqlx::query("UPDATE jobs SET graph = ? WHERE id = ?")
                .bind(&graph)
                .bind(id)
                .execute(&pool)
                .await?;
        }
        pool.close().await;
        anyhow::Ok(())
    })?;
    std::thread::sleep(Duration::from_secs(5));
    assert!(!tampered.exists(), "a tampered definition executed");
    for (label, job, approved) in &jobs {
        assert!(!approved.exists(), "{label}: old argv ran after tamper");
        let stored = sandbox.show_json(job)?;
        let status = mcp.ok("show", json!({"job":job}))?["status"].clone();
        if *label == "trigger" {
            // Re-pended by the trigger; its one-shot instant then passed while
            // pending, so it expired rather than running late.
            assert_eq!(stored["approval"]["state"], "pending", "{label}");
            assert_eq!(status, "expired");
            assert_eq!(stored["expiry_reason"], "scheduled_at_passed");
        } else {
            // Marker still says approved, but the canonical hash no longer matches.
            assert_eq!(stored["approval"]["state"], "approved", "{label}");
            assert_eq!(status, "active (approved)");
        }
        let attempts: Value =
            serde_json::from_slice(&sandbox.cued(&["logs", job, "--json"], "")?.stdout)
                .unwrap_or(Value::Null);
        assert!(
            attempts["attempts"].as_array().is_none_or(Vec::is_empty),
            "{label}: an attempt was claimed: {attempts}"
        );
        if *label == "trigger" {
            mcp.err("cancel", json!({"job":job}))?;
        } else {
            mcp.ok("cancel", json!({"job":job}))?;
        }
    }
    assert_no_leak(&mcp.transcript);
    Ok(())
}

/// A pending recurrence whose instants keep arriving never creates a run or
/// spends its count on the live daemon; approval then starts it normally.
#[test]
fn live_pending_recurrence_spends_nothing_until_approved() -> Result<()> {
    let sandbox = Sandbox::new()?;
    sandbox.write_policy("exec = 'approve'\n")?;
    let marker = sandbox.root.join("recurring-ran");
    let mut mcp = sandbox.mcp(&[])?;
    let scheduled = mcp.ok(
        "schedule",
        json!({"action":{"type":"exec","argv":["/usr/bin/touch", marker.to_str().unwrap()]},"every":"1s","count":2,"name":"tick"}),
    )?;
    let job = job_ref(&scheduled);
    std::thread::sleep(Duration::from_secs(3));
    // Drive passes: list/show sweep, and a submission wakes the scheduler.
    mcp.ok("list", json!({}))?;
    assert!(!marker.exists());
    let fired = |sandbox: &Sandbox| -> Result<i64> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async {
            let pool =
                sqlx::SqlitePool::connect(&format!("sqlite://{}?mode=ro", sandbox.db().display()))
                    .await?;
            let fired: i64 = sqlx::query_scalar("SELECT fired FROM jobs WHERE id = ?")
                .bind(job[1..].parse::<i64>()?)
                .fetch_one(&pool)
                .await?;
            pool.close().await;
            anyhow::Ok(fired)
        })
    };
    assert_eq!(fired(&sandbox)?, 0);
    let approve = sandbox.cued(&["approve", &job], "y\n")?;
    assert!(
        approve.status.success(),
        "{}",
        String::from_utf8_lossy(&approve.stderr)
    );
    wait_for(
        "the approved recurrence to run",
        Duration::from_secs(20),
        || Ok(marker.exists()),
    )?;
    wait_for(
        "the capped recurrence to finish",
        Duration::from_secs(20),
        || Ok(sandbox.show_json(&job)?["status"] == "done"),
    )?;
    assert_eq!(fired(&sandbox)?, 2);
    assert_no_leak(&mcp.transcript);
    Ok(())
}
