use anyhow::Result;
use cued::mcp::{McpConfig, Server, serve};
use cued::paths::Paths;
use serde_json::{Value, json};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};

fn paths(root: &std::path::Path) -> Paths {
    Paths {
        data_dir: root.into(),
        db_file: root.join("db"),
        lock_file: root.join("lock"),
        logs_dir: root.join("logs"),
        daemon_log: root.join("daemon.log"),
        socket_file: root.join("socket"),
        config_file: root.join("config.toml"),
    }
}
fn private_dir(parent: &std::path::Path, name: &str) -> Result<std::path::PathBuf> {
    let path = parent.join(name);
    std::fs::create_dir(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
    Ok(path)
}
fn rpc(id: i32, method: &str, params: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
}
fn init() -> Value {
    rpc(
        1,
        "initialize",
        json!({"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1"}}),
    )
}

#[test]
fn stdio_protocol_exposes_only_five_tools_and_closed_errors_are_actionable() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let server = Server::new(
        paths(dir.path()),
        dir.path().join("mcp.toml"),
        McpConfig::default(),
    );
    let requests = [
        init(),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        rpc(2, "tools/list", json!({})),
        rpc(
            3,
            "tools/call",
            json!({"name":"schedule","arguments":{"action":{"type":"exec","argv":["/bin/true"]},"at":"in 1h"}}),
        ),
        rpc(
            4,
            "tools/call",
            json!({"name":"logs","arguments":{"job":"j1"}}),
        ),
        rpc(
            5,
            "tools/call",
            json!({"name":"approve","arguments":{"job":"j1"}}),
        ),
        rpc(
            6,
            "tools/call",
            json!({"name":"schedule","arguments":{"action":{"type":"notify","title":"hello"},"at":"in 1h","keep_env":["API_KEY"]}}),
        ),
        rpc(
            7,
            "tools/call",
            json!({"name":"schedule","arguments":{"action":{"type":"notify","title":"hello"},"at":"in 1h"}}),
        ),
    ];
    let input = requests
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    let mut out = Vec::new();
    serve(input.as_bytes(), &mut out, &server)?;
    let replies: Vec<Value> = String::from_utf8(out)?
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert_eq!(replies.len(), 7, "notifications get no response");
    assert_eq!(replies[0]["result"]["protocolVersion"], "2025-06-18");
    let names: Vec<_> = replies[1]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["schedule", "list", "show", "cancel", "logs"]);
    for (index, key) in [(2, "exec"), (3, "logs"), (6, "notify")] {
        assert_eq!(replies[index]["result"]["isError"], true);
        let text = replies[index]["result"]["content"][0]["text"]
            .as_str()
            .unwrap();
        assert!(text.contains(&format!("`{key}`")), "{text}");
        assert!(
            text.contains(dir.path().join("mcp.toml").to_str().unwrap()),
            "{text}"
        );
    }
    assert_eq!(replies[4]["error"]["code"], -32602);
    assert!(
        replies[5]["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("unknown field `keep_env`")
    );
    Ok(())
}

#[test]
fn capability_config_rejects_approval_for_reads_and_unknown_fields() {
    for invalid in [
        "read = 'approve'",
        "logs = 'approve'",
        "keep_env = []",
        "exec = 'yes'",
    ] {
        assert!(toml::from_str::<McpConfig>(invalid).is_err(), "{invalid}");
    }
    assert!(
        toml::from_str::<McpConfig>("exec = 'approve'\nnotify = 'open'\nread = 'off'\nlogs = 'on'")
            .is_ok()
    );
}

#[test]
fn real_stdio_process_has_clean_stdout_and_env_override_policy() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let socket_dir = private_dir(dir.path(), "socket")?;
    let path = dir.path().join("custom-mcp.toml");
    std::fs::write(&path, "exec = 'open'\nlogs = 'on'\n")?;
    let mut child = Command::new(env!("CARGO_BIN_EXE_cued"))
        .arg("mcp")
        .env("HOME", dir.path())
        .env("XDG_DATA_HOME", dir.path())
        .env("CUED_SOCKET_DIR", &socket_dir)
        .env("XDG_CONFIG_HOME", dir.path())
        .env("CUED_MCP_CONFIG", &path)
        .env("CUED_MCP_EXEC", "closed")
        .env("CUED_MCP_READ", "off")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    {
        let mut input = child.stdin.take().unwrap();
        writeln!(input, "{}", init())?;
        writeln!(
            input,
            "{}",
            rpc(2, "tools/call", json!({"name":"list","arguments":{}}))
        )?;
        writeln!(
            input,
            "{}",
            rpc(
                3,
                "tools/call",
                json!({"name":"schedule","arguments":{"action":{"type":"exec","argv":["/bin/true"]},"at":"in 1h"}})
            )
        )?;
    }
    let output = child.wait_with_output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let lines: Vec<Value> = String::from_utf8(output.stdout)?
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert_eq!(lines.len(), 3);
    assert!(
        lines[1]["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("`read`")
    );
    assert!(
        lines[2]["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains(path.to_str().unwrap())
    );
    assert!(
        !dir.path().join("cued.sock").exists(),
        "closed capabilities must not contact or spawn a daemon"
    );
    Ok(())
}

#[test]
fn stale_daemon_cannot_silently_drop_approval_and_reports_restart() -> Result<()> {
    use std::io::BufRead;
    use std::os::unix::net::UnixListener;
    let dir = tempfile::tempdir()?;
    let p = paths(dir.path());
    let socket = UnixListener::bind(&p.socket_file)?;
    let fake = std::thread::spawn(move || {
        let (mut stream, _) = socket.accept().unwrap();
        let mut line = String::new();
        std::io::BufReader::new(&stream)
            .read_line(&mut line)
            .unwrap();
        let request: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(request["proto"], 1);
        assert_eq!(request["body"]["cmd"], "submit_definition");
        assert_eq!(request["body"]["require_approval"], true);
        writeln!(
            stream,
            "{}",
            json!({"result":"error","message":"bad request: unknown variant `submit_definition`"})
        )
        .unwrap();
    });
    let config = toml::from_str("exec = 'approve'")?;
    let server = Server::new(p, dir.path().join("mcp.toml"), config);
    let error = server
        .call(
            "schedule",
            json!({"action":{"type":"exec","argv":["/bin/true"]},"at":"in 1h"}),
        )
        .unwrap_err();
    assert!(
        error.to_string().contains("stop the running `cued daemon`"),
        "{error:#}"
    );
    fake.join().unwrap();
    Ok(())
}

#[test]
fn old_submitted_response_does_not_masquerade_as_approval_aware() {
    assert_eq!(cued::proto::PROTO_VERSION, 1);
    assert!(
        serde_json::from_value::<cued::proto::Response>(
            json!({"result":"submitted","job":1,"run":1})
        )
        .is_err()
    );
    let expired = serde_json::to_string(&cued::model::JobStatus::Expired).unwrap();
    assert_eq!(expired, "\"expired\"");
}

struct NoBus;
impl cued::notify::Notifier for NoBus {
    async fn deliver(&self, _: &cued::model::NotifySpec) -> Result<bool> {
        Ok(false)
    }
}
async fn wait_socket(path: &std::path::Path) -> Result<()> {
    for _ in 0..200 {
        if tokio::net::UnixStream::connect(path).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    anyhow::bail!("socket unavailable")
}
async fn call(server: std::sync::Arc<Server>, name: &'static str, args: Value) -> Result<Value> {
    tokio::task::spawn_blocking(move || server.call(name, args)).await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_mcp_daemon_cli_approval_execution_and_bounded_logs() -> Result<()> {
    use cued::model::{ApprovalState, JobId};
    let dir = tempfile::tempdir()?;
    let mut p = paths(dir.path());
    p.data_dir = dir.path().join("cued");
    p.db_file = p.data_dir.join("cued.db");
    p.logs_dir = p.data_dir.join("logs");
    let socket_dir = private_dir(dir.path(), "socket")?;
    p.socket_file = socket_dir.join("cued.sock");
    p.config_file = p.data_dir.join("config.toml");
    std::fs::create_dir_all(&p.data_dir)?;
    let store = cued::store::Store::open(&p.db_file).await?;
    let mut daemon = tokio::spawn(cued::daemon::serve_with(
        p.clone(),
        cued::config::Config::default(),
        store.clone(),
        NoBus,
    ));
    tokio::select! {
        result = wait_socket(&p.socket_file) => result?,
        result = &mut daemon => anyhow::bail!("daemon exited before binding: {result:?}"),
    }
    let server = std::sync::Arc::new(Server::new(
        p.clone(),
        dir.path().join("mcp.toml"),
        toml::from_str("exec = 'approve'\nnotify = 'open'\nlogs = 'on'")?,
    ));
    let marker = dir.path().join("executed");
    let response = call(server.clone(),"schedule",json!({"action":{"type":"exec","argv":["/bin/sh","-c",format!("touch '{}'; printf abcdefghijklmnop",marker.display())]},"at":"in 2s"})).await?;
    assert_eq!(response["pending_approval"], true);
    let id = JobId(response["job"].as_i64().unwrap());
    assert!(!marker.exists());
    assert!(store.latest_run_cursor(id).await.is_err());
    let shown = call(server.clone(), "show", json!({"job":id.to_string()})).await?;
    assert!(
        shown["status"]
            .as_str()
            .unwrap()
            .contains("Pending approval")
    );
    // Exercise the actual interactive CLI, including review then hash-bound request.
    let root = dir.path().to_path_buf();
    let runtime = socket_dir.clone();
    let approval = tokio::task::spawn_blocking(move || -> Result<std::process::Output> {
        let mut child = Command::new(env!("CARGO_BIN_EXE_cued"))
            .args(["approve", &id.to_string()])
            .env("HOME", &root)
            .env("XDG_DATA_HOME", &root)
            .env("XDG_CONFIG_HOME", &root)
            .env("CUED_SOCKET_DIR", runtime)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        child.stdin.take().unwrap().write_all(b"yes\n")?;
        Ok(child.wait_with_output()?)
    })
    .await??;
    assert!(
        approval.status.success(),
        "{}",
        String::from_utf8_lossy(&approval.stderr)
    );
    assert_eq!(
        store.load_job(id).await?.approval.unwrap().state,
        ApprovalState::Approved
    );
    for _ in 0..400 {
        if store.load_job(id).await?.status == cued::model::JobStatus::Done {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(marker.exists());
    assert_eq!(
        store.load_job(id).await?.status,
        cued::model::JobStatus::Done
    );
    assert_eq!(
        store.load_job(id).await?.approval.unwrap().state,
        ApprovalState::Approved
    );
    let logs = call(
        server.clone(),
        "logs",
        json!({"job":id.to_string(),"max_bytes":4}),
    )
    .await?;
    assert_eq!(logs["truncated"], true);
    assert_eq!(logs["attempts"][0]["content"], "mnop");
    assert!(logs["notice"].as_str().unwrap().contains("truncated"));
    // Open notify uses no approval record, despite this server gating exec.
    let notification = call(
        server.clone(),
        "schedule",
        json!({"action":{"type":"notify","title":"open reminder"},"at":"in 1h"}),
    )
    .await?;
    assert_eq!(notification["pending_approval"], false);
    let id = JobId(notification["job"].as_i64().unwrap());
    assert!(store.load_job(id).await?.approval.is_none());
    assert!(store.latest_run_cursor(id).await.is_ok());
    call(server.clone(), "cancel", json!({"job":id.to_string()})).await?;
    daemon.abort();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn due_pending_exec_does_not_run_on_live_daemon_or_restart() -> Result<()> {
    use cued::model::*;
    use jiff::{SignedDuration, Timestamp};
    let dir = tempfile::tempdir()?;
    let p = paths(dir.path());
    let marker = dir.path().join("must-not-exist");
    let store = cued::store::Store::open(&p.db_file).await?;
    let now = Timestamp::now();
    let definition = JobSpec {
        name: Some("pending".into()),
        schedule: Schedule::Once {
            at: now.checked_add(SignedDuration::from_millis(100))?,
        },
        graph: cued::submit::single_shell_graph(vec![
            "touch".into(),
            marker.to_string_lossy().into_owned(),
        ]),
        cwd: "/tmp".into(),
        env: CapturedEnv::default(),
        policies: Policies::default(),
        hooks: Hooks::default(),
    };
    let (id, _, _) = store
        .submit_definition(&definition, &now, JobSource::Mcp, true)
        .await?;
    // Restart with the job overdue; RunAsap must not backfill a pending instant.
    drop(store);
    tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    let store = cued::store::Store::open(&p.db_file).await?;
    let daemon = tokio::spawn(cued::daemon::serve_with(
        p.clone(),
        cued::config::Config::default(),
        store.clone(),
        NoBus,
    ));
    wait_socket(&p.socket_file).await?;
    assert_eq!(store.load_job(id).await?.status, JobStatus::Expired);
    assert!(!marker.exists());
    assert!(store.latest_run_cursor(id).await.is_err());
    assert_eq!(store.fired_count(id).await?, 0);
    // A fresh pending job expires while the daemon is alive too. List drives
    // a pass immediately instead of waiting for the daemon's 30-second tick.
    let mut definition = definition;
    definition.schedule = Schedule::Once {
        at: Timestamp::now().checked_add(SignedDuration::from_millis(100))?,
    };
    let (id, _, _) = store
        .submit_definition(&definition, &Timestamp::now(), JobSource::Mcp, true)
        .await?;
    tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    let server = std::sync::Arc::new(Server::new(
        p,
        dir.path().join("mcp.toml"),
        McpConfig::default(),
    ));
    let list = call(server, "list", json!({"all":true})).await?;
    assert!(
        list["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .all(|j| j["status"] == "expired")
    );
    assert_eq!(store.load_job(id).await?.status, JobStatus::Expired);
    assert!(!marker.exists());
    assert_eq!(store.fired_count(id).await?, 0);
    daemon.abort();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workflows_use_existing_graph_validation_and_action_capabilities() -> Result<()> {
    use cued::model::{ApprovalState, JobId};
    let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR"))?;
    let p = paths(dir.path());
    let store = cued::store::Store::open(&p.db_file).await?;
    let mut daemon = tokio::spawn(cued::daemon::serve_with(
        p.clone(),
        cued::config::Config::default(),
        store.clone(),
        NoBus,
    ));
    tokio::select! {
        result = wait_socket(&p.socket_file) => result?,
        result = &mut daemon => anyhow::bail!("daemon exited before binding: {result:?}"),
    }

    let server = std::sync::Arc::new(Server::new(
        p.clone(),
        dir.path().join("mcp.toml"),
        toml::from_str("exec = 'approve'\nnotify = 'open'")?,
    ));
    let workflow = json!({
        "workflow": {
            "entry": "build",
            "steps": {
                "build": {
                    "action": {"type":"exec","argv":["/bin/true"]},
                    "transitions": [
                        {"when":"succeeded","then":{"goto":{"step":"test"}}},
                        {"when":"failed","then":{"end":{"outcome":"failure"}}}
                    ]
                },
                "test": {"action":{"type":"exec","argv":["/bin/true"]}}
            }
        },
        "at":"in 1h"
    });
    let submitted = call(server.clone(), "schedule", workflow.clone()).await?;
    assert_eq!(submitted["pending_approval"], true);
    let id = JobId(submitted["job"].as_i64().unwrap());
    let job = store.load_job(id).await?;
    assert_eq!(job.approval.unwrap().state, ApprovalState::Pending);
    assert_eq!(job.graph.entry, "build");
    assert_eq!(job.graph.steps.len(), 2);
    assert!(matches!(
        job.graph.steps["build"].transitions[0].when,
        cued::model::Condition::Succeeded
    ));
    assert!(matches!(
        job.graph.steps["build"].transitions[0].then,
        cued::model::Effect::Goto { ref step, .. } if step == "test"
    ));
    assert!(store.latest_run_cursor(id).await.is_err());
    call(server.clone(), "cancel", json!({"job":id.to_string()})).await?;

    let invalid_graph = json!({
        "workflow": {
            "entry": "build",
            "steps": {
                "build": {
                    "action": {"type":"exec","argv":["/bin/true"]},
                    "transitions": [
                        {"when":"succeeded","then":{"goto":{"step":"missing"}}}
                    ]
                }
            }
        },
        "at":"in 1h"
    });
    let error = call(server.clone(), "schedule", invalid_graph)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("goto target \"missing\" doesn't exist")
    );

    // A notify-only graph uses notify policy without requiring exec to open.
    let notify_server = std::sync::Arc::new(Server::new(
        p.clone(),
        dir.path().join("mcp.toml"),
        toml::from_str("exec = 'closed'\nnotify = 'open'")?,
    ));
    let notification = call(
        notify_server.clone(),
        "schedule",
        json!({"workflow":{"entry":"note","steps":{"note":{"action":{"type":"notify","title":"hello"}}}},"at":"in 1h"}),
    )
    .await?;
    assert_eq!(notification["pending_approval"], false);
    call(
        notify_server,
        "cancel",
        json!({"job":notification["job"].to_string().trim_matches('"')}),
    )
    .await?;

    // With both capabilities open, the same graph runs through its success
    // transition into the next step.
    let open_server = std::sync::Arc::new(Server::new(
        p.clone(),
        dir.path().join("mcp.toml"),
        toml::from_str("exec = 'open'\nnotify = 'open'")?,
    ));
    let completed_marker = dir.path().join("workflow-test-step-ran");
    let mut runnable_workflow = workflow.clone();
    runnable_workflow["at"] = json!("in 1s");
    runnable_workflow["workflow"]["steps"]["test"]["action"] =
        json!({"type":"exec","argv":["/usr/bin/touch",completed_marker.to_string_lossy()]});
    let run = call(open_server, "schedule", runnable_workflow).await?;
    assert_eq!(run["pending_approval"], false);
    let run_id = JobId(run["job"].as_i64().unwrap());
    for _ in 0..300 {
        if store.load_job(run_id).await?.status == cued::model::JobStatus::Done {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        store.load_job(run_id).await?.status,
        cued::model::JobStatus::Done
    );
    assert!(completed_marker.exists());

    // Every action in a mixed workflow must be enabled by its own capability.
    let closed_notify = std::sync::Arc::new(Server::new(
        p.clone(),
        dir.path().join("mcp.toml"),
        toml::from_str("exec = 'open'\nnotify = 'closed'")?,
    ));
    let mut mixed_workflow = workflow;
    mixed_workflow["workflow"]["steps"]["notice"] =
        json!({"action":{"type":"notify","title":"done"}});
    let error = call(closed_notify, "schedule", mixed_workflow)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("`notify`"), "{error:#}");

    daemon.abort();
    Ok(())
}
