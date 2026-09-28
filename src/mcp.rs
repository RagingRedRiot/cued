//! Stdio-only MCP front end. Capability policy belongs here, never in the daemon.
//! Protocol: https://modelcontextprotocol.io/specification/2025-06-18
use std::collections::BTreeMap;
use std::io::{BufRead, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::Config;
use crate::model::{
    Action, Condition, Effect, Graph, Hooks, Job, JobSource, JobSpec, Policies, Schedule, Step,
    Transition,
};
use crate::paths::Paths;
use crate::proto::{RequestBody, Response};
use crate::{client, submit, timeparse};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Open,
    Approve,
    #[default]
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Switch {
    On,
    Off,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpConfig {
    pub exec: Capability,
    pub notify: Capability,
    pub read: Switch,
    pub logs: Switch,
}
impl Default for McpConfig {
    fn default() -> Self {
        Self {
            exec: Capability::Closed,
            notify: Capability::Closed,
            read: Switch::On,
            logs: Switch::Off,
        }
    }
}
impl McpConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let mut config = match std::fs::read_to_string(path) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        for key in ["exec", "notify", "read", "logs"] {
            let name = format!("CUED_MCP_{}", key.to_uppercase());
            if let Ok(value) = std::env::var(&name) {
                let parsed = Value::String(value);
                match key {
                    "exec" => config.exec = serde_json::from_value(parsed).with_context(|| name)?,
                    "notify" => {
                        config.notify = serde_json::from_value(parsed).with_context(|| name)?
                    }
                    "read" => config.read = serde_json::from_value(parsed).with_context(|| name)?,
                    "logs" => config.logs = serde_json::from_value(parsed).with_context(|| name)?,
                    _ => unreachable!(),
                }
            }
        }
        Ok(config)
    }
    fn action(&self, key: &str, path: &Path) -> Result<bool> {
        let capability = match key {
            "exec" => self.exec,
            "notify" => self.notify,
            _ => unreachable!(),
        };
        ensure!(
            capability != Capability::Closed,
            "capability closed: change `{key}` in {} to `open` or `approve` (env override CUED_MCP_{})",
            path.display(),
            key.to_uppercase()
        );
        Ok(capability == Capability::Approve)
    }
    fn read(&self, key: &str, path: &Path) -> Result<()> {
        let switch = match key {
            "read" => self.read,
            "logs" => self.logs,
            _ => unreachable!(),
        };
        ensure!(
            switch == Switch::On,
            "capability closed: change `{key}` in {} to `on` (env override CUED_MCP_{})",
            path.display(),
            key.to_uppercase()
        );
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleArgs {
    action: Option<ScheduleAction>,
    workflow: Option<WorkflowArgs>,
    /// One instant; optional only when a calendar recurrence carries its time.
    at: Option<String>,
    every: Option<String>,
    zone: Option<String>,
    name: Option<String>,
    cwd: Option<String>,
    count: Option<u32>,
    until: Option<String>,
}
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum ScheduleAction {
    Exec {
        argv: Vec<String>,
    },
    Notify {
        title: String,
        #[serde(default)]
        body: String,
    },
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkflowArgs {
    entry: String,
    steps: BTreeMap<String, WorkflowStep>,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkflowStep {
    action: ScheduleAction,
    #[serde(default)]
    transitions: Vec<WorkflowTransition>,
    timeout: Option<String>,
    max_visits: Option<u32>,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkflowTransition {
    when: Condition,
    then: Effect,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reference {
    job: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    #[serde(default)]
    all: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LogsArgs {
    job: String,
    run: Option<i64>,
    step: Option<String>,
    attempt: Option<u32>,
    #[serde(default = "default_limit")]
    max_bytes: usize,
}
fn default_limit() -> usize {
    16 * 1024
}

pub struct Server {
    paths: Paths,
    config_path: PathBuf,
    /// None: re-read the policy for every call, so revoking or granting a
    /// capability in the file applies without restarting a long-lived client.
    config: Option<McpConfig>,
}
impl Server {
    /// A server with a fixed policy (embedding and tests).
    pub fn new(paths: Paths, config_path: PathBuf, config: McpConfig) -> Self {
        Self {
            paths,
            config_path,
            config: Some(config),
        }
    }
    /// A server that loads `config_path` (and env overrides) on every call.
    /// An unreadable or invalid policy fails that call closed.
    pub fn from_file(paths: Paths, config_path: PathBuf) -> Self {
        Self {
            paths,
            config_path,
            config: None,
        }
    }
    fn policy(&self) -> Result<std::borrow::Cow<'_, McpConfig>> {
        Ok(match &self.config {
            Some(config) => std::borrow::Cow::Borrowed(config),
            None => std::borrow::Cow::Owned(McpConfig::load(&self.config_path)?),
        })
    }
    fn request(&self, body: RequestBody) -> Result<Response> {
        match client::request(&self.paths, body)? {
            Response::Error { message } => bail!("{message}"),
            Response::ProtoMismatch { .. } => bail!(
                "daemon protocol differs — stop the running `cued daemon` and rerun; the next command respawns it"
            ),
            response => Ok(response),
        }
    }
    fn job(&self, reference: String) -> Result<Job> {
        match self.request(RequestBody::Show { job: reference })? {
            Response::JobDetail { job, .. } => Ok(*job),
            _ => bail!("unexpected daemon reply — stop the running `cued daemon` and rerun"),
        }
    }
    pub fn call(&self, name: &str, args: Value) -> Result<Value> {
        match name {
            "schedule" => {
                let policy = self.policy()?;
                let args: ScheduleArgs = serde_json::from_value(args)?;
                let spec = schedule_spec(args, &Config::load(&self.paths.config_file)?)?;
                let mut pending = false;
                for key in graph_capabilities(&spec.graph) {
                    pending |= policy.action(key, &self.config_path)?;
                }
                match self.request(RequestBody::SubmitDefinition {
                    spec: Box::new(spec),
                    source: JobSource::Mcp,
                    require_approval: pending,
                })? {
                    Response::Submitted {
                        job,
                        run,
                        pending_approval,
                    } => {
                        ensure!(
                            pending == pending_approval,
                            "daemon did not honor approval policy — stop the running `cued daemon` and rerun"
                        );
                        Ok(
                            json!({"job":job, "run":run, "pending_approval":pending_approval,
                            "status": if pending {"Pending approval"} else {"active"},
                            "approval_command":pending.then(|| format!("cued approve {job}"))}),
                        )
                    }
                    _ => bail!("unexpected submission reply"),
                }
            }
            "list" => {
                self.policy()?.read("read", &self.config_path)?;
                let args: ListArgs = serde_json::from_value(args)?;
                match self.request(RequestBody::List { all: args.all })? {
                    Response::JobList { jobs } => {
                        let mut items = Vec::new();
                        for entry in jobs {
                            let job = self.job(entry.id.to_string())?;
                            items.push(
                                json!({"definition":mcp_job(&job)?, "status":job.display_status(),
                                "next_at":entry.next_at, "last_run":entry.last_run}),
                            );
                        }
                        Ok(json!({"jobs":items}))
                    }
                    _ => bail!("unexpected list reply"),
                }
            }
            "show" => {
                self.policy()?.read("read", &self.config_path)?;
                let args: Reference = serde_json::from_value(args)?;
                match self.request(RequestBody::Show { job: args.job })? {
                    Response::JobDetail {
                        job,
                        next_fire_at,
                        queued_at,
                    } => Ok(json!({
                        "definition": mcp_job(&job)?, "status":job.display_status(),
                        "next_fire_at":next_fire_at, "queued_at":queued_at,
                        "approval_scope":"Stored schedule and execution definition; referenced files can change."
                    })),
                    _ => bail!("unexpected show reply"),
                }
            }
            "cancel" => {
                // Denial must remain available even with all capabilities closed.
                let args: Reference = serde_json::from_value(args)?;
                match self.request(RequestBody::Cancel { job: args.job })? {
                    response @ Response::JobCancelled { .. } => Ok(serde_json::to_value(response)?),
                    _ => bail!("unexpected cancel reply"),
                }
            }
            "logs" => {
                self.policy()?.read("logs", &self.config_path)?;
                let args: LogsArgs = serde_json::from_value(args)?;
                ensure!(
                    (1..=65536).contains(&args.max_bytes),
                    "max_bytes must be 1..65536"
                );
                let definition = self.job(args.job.clone())?;
                match self.request(RequestBody::Logs {
                    job: args.job,
                    run: args.run,
                    step: args.step,
                    attempt: args.attempt,
                })? {
                    Response::LogManifest { job, run, attempts } => {
                        let mut remaining = args.max_bytes;
                        let mut output = Vec::new();
                        let mut truncated = false;
                        // Tail newest attempts first, sharing a total byte budget.
                        for attempt in attempts.into_iter().rev() {
                            if remaining == 0 {
                                truncated = true;
                                break;
                            }
                            let path =
                                self.paths
                                    .step_log(job, run, &attempt.step, attempt.attempt);
                            let (text, cut) = tail(&path, remaining)?;
                            remaining = remaining.saturating_sub(text.len());
                            truncated |= cut;
                            output.push(json!({"attempt":attempt, "content":text}));
                        }
                        output.reverse();
                        for attempt in &mut output {
                            redact_strings(&mut attempt["content"], &secret_values(&definition));
                        }
                        let value = json!({"job":job,"run":run,"attempts":output,"truncated":truncated,
                            "max_bytes":args.max_bytes,"notice":"Tail of newest attempts; output may be truncated. Captured environment values are redacted."});
                        Ok(value)
                    }
                    _ => bail!("unexpected logs reply"),
                }
            }
            _ => bail!("unknown tool {name:?}"),
        }
    }
}

fn schedule_spec(args: ScheduleArgs, config: &Config) -> Result<JobSpec> {
    let zone = timeparse::resolve_zone(args.zone.as_deref())?;
    let now = timeparse::now_in(&zone);
    let mut schedule = match args.every {
        Some(every) => submit::every_schedule(&every, args.at.as_deref(), &now)?,
        None => {
            ensure!(
                args.count.is_none() && args.until.is_none(),
                "count/until require recurrence"
            );
            Schedule::Once {
                at: timeparse::parse_instant(
                    args.at.as_deref().context("at is required for a one-off")?,
                    &now,
                )?,
            }
        }
    };
    match &mut schedule {
        Schedule::Every { count, until, .. } | Schedule::Calendar { count, until, .. } => {
            *count = args.count;
            *until = args
                .until
                .as_deref()
                .map(|at| timeparse::parse_instant(at, &now))
                .transpose()?;
        }
        _ => {}
    }
    let graph = match (args.action, args.workflow) {
        (Some(action), None) => match action {
            ScheduleAction::Exec { argv } => submit::single_shell_graph(argv),
            ScheduleAction::Notify { title, body } => submit::single_notify_graph(title, body),
        },
        (None, Some(workflow)) => workflow_graph(workflow)?,
        (Some(_), Some(_)) => bail!("provide either `action` or `workflow`, not both"),
        (None, None) => bail!("provide `action` or `workflow`"),
    };
    let cwd = args
        .cwd
        .map(PathBuf::from)
        .unwrap_or(std::env::current_dir()?);
    ensure!(
        cwd.is_absolute() && cwd.is_dir(),
        "cwd must be an existing absolute directory"
    );
    let spec = JobSpec {
        name: args.name,
        schedule,
        graph,
        cwd: cwd.to_string_lossy().into_owned(),
        env: submit::capture_env(&config.env.deny, &[]),
        policies: Policies {
            missed_wait: config.policy.missed_wait,
            on_interrupt: config.policy.on_interrupt,
            catch_up: config.policy.catch_up,
            overlap: config.policy.overlap,
            deadline: None,
        },
        hooks: Hooks::default(),
    };
    submit::validate(&spec)?;
    Ok(spec)
}

fn workflow_graph(workflow: WorkflowArgs) -> Result<Graph> {
    let mut steps = BTreeMap::new();
    for (id, step) in workflow.steps {
        let action = match step.action {
            ScheduleAction::Exec { argv } => Action::Shell { argv },
            ScheduleAction::Notify { title, body } => Action::Notify { title, body },
        };
        let transitions = step
            .transitions
            .into_iter()
            .map(|transition| Transition {
                when: transition.when,
                then: transition.then,
            })
            .collect();
        let timeout = step
            .timeout
            .as_deref()
            .map(timeparse::parse_duration)
            .transpose()?;
        steps.insert(
            id,
            Step {
                action,
                cwd: None,
                env: None,
                timeout,
                kill_grace: None,
                transitions,
                max_visits: step.max_visits,
                restart_safe: false,
                missed_wait: None,
            },
        );
    }
    Ok(Graph {
        entry: workflow.entry,
        steps,
    })
}

/// Each action used by a workflow must be enabled by its matching capability.
/// Any configured approval requirement gates the complete definition.
fn graph_capabilities(graph: &Graph) -> Vec<&'static str> {
    let uses_exec = graph
        .steps
        .values()
        .any(|step| matches!(step.action, Action::Shell { .. }));
    let uses_notify = graph
        .steps
        .values()
        .any(|step| matches!(step.action, Action::Notify { .. }));
    let mut capabilities = Vec::with_capacity(2);
    if uses_exec {
        capabilities.push("exec");
    }
    if uses_notify {
        capabilities.push("notify");
    }
    capabilities
}

fn secret_values(job: &Job) -> Vec<String> {
    // Hide all captured values, including names outside the denylist. Longest
    // first prevents a short value masking only a prefix of a longer secret.
    let mut values: Vec<String> = job
        .env
        .vars
        .values()
        .chain(
            job.graph
                .steps
                .values()
                .filter_map(|s| s.env.as_ref())
                .flat_map(|env| env.values()),
        )
        .filter(|value| !value.is_empty())
        .cloned()
        .collect();
    values.sort_by_key(|v| std::cmp::Reverse(v.len()));
    values.dedup();
    values
}
fn redact_strings(value: &mut Value, secrets: &[String]) {
    match value {
        Value::String(text) => {
            // Single pass: never scan inserted redaction markers again.
            let mut redacted = String::new();
            let mut rest = text.as_str();
            while !rest.is_empty() {
                if let Some(secret) = secrets
                    .iter()
                    .find(|secret| rest.starts_with(secret.as_str()))
                {
                    redacted.push_str("[redacted]");
                    rest = &rest[secret.len()..];
                } else {
                    let c = rest.chars().next().expect("nonempty");
                    redacted.push(c);
                    rest = &rest[c.len_utf8()..];
                }
            }
            *text = redacted;
        }
        Value::Array(items) => {
            for item in items {
                redact_strings(item, secrets);
            }
        }
        Value::Object(items) => {
            for item in items.values_mut() {
                redact_strings(item, secrets);
            }
        }
        _ => {}
    }
}
pub fn redacted_job(job: &Job) -> Result<Value> {
    let mut value = serde_json::to_value(job)?;
    // Redact payload strings, not lifecycle enum spellings, timestamps or names
    // of environment variables: the local CLI approval preview can use them.
    let secrets = secret_values(job);
    if let Some(approval) = value["approval"].as_object_mut() {
        approval.remove("definition_hash");
    }
    for key in ["name", "cwd", "graph", "hooks"] {
        redact_strings(&mut value[key], &secrets);
    }
    if let Some(vars) = value["env"]["vars"].as_object_mut() {
        for val in vars.values_mut() {
            *val = json!("[redacted]");
        }
    }
    if let Some(steps) = value["graph"]["steps"].as_object_mut() {
        for step in steps.values_mut() {
            if let Some(env) = step["env"].as_object_mut() {
                for val in env.values_mut() {
                    *val = json!("[redacted]");
                }
            }
        }
    }
    Ok(value)
}

/// MCP has no use for captured environment keys or values. Keep the local CLI
/// approval preview's redacted representation separate from model-visible data.
fn mcp_job(job: &Job) -> Result<Value> {
    let mut value = redacted_job(job)?;
    if let Some(object) = value.as_object_mut() {
        object.remove("env");
    }
    if let Some(steps) = value["graph"]["steps"].as_object_mut() {
        for step in steps.values_mut() {
            if let Some(object) = step.as_object_mut() {
                object.remove("env");
            }
        }
    }
    Ok(value)
}
fn tail(path: &Path, limit: usize) -> Result<(String, bool)> {
    let mut file =
        std::fs::File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let size = file.metadata()?.len();
    let start = size.saturating_sub(limit as u64);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(limit as u64).read_to_end(&mut bytes)?;
    Ok((String::from_utf8_lossy(&bytes).into_owned(), start > 0))
}

pub fn tools() -> Value {
    let reference = json!({"type":"object","properties":{"job":{"type":"string"}},"required":["job"],"additionalProperties":false});
    let action_schema = json!({"oneOf":[
        {"type":"object","required":["type","argv"],"additionalProperties":false,"properties":{"type":{"const":"exec"},"argv":{"type":"array","items":{"type":"string"},"minItems":1}}},
        {"type":"object","required":["type","title"],"additionalProperties":false,"properties":{"type":{"const":"notify"},"title":{"type":"string"},"body":{"type":"string"}}}
    ]});
    let condition_schema = json!({"oneOf":[
        {"type":"string","enum":["always","succeeded","failed","timed_out"]},
        {"type":"object","minProperties":1,"maxProperties":1,"additionalProperties":false,"properties":{
            "exit_eq":{"type":"integer"},"exit_ne":{"type":"integer"},"exit_in":{"type":"array","items":{"type":"integer"}},
            "stdout":{"type":"object","minProperties":1,"maxProperties":1,"additionalProperties":false,"properties":{"contains":{"type":"string"},"regex":{"type":"string"}}},
            "stderr":{"type":"object","minProperties":1,"maxProperties":1,"additionalProperties":false,"properties":{"contains":{"type":"string"},"regex":{"type":"string"}}},
            "all":{"type":"array","items":{"oneOf":[{"type":"string","enum":["always","succeeded","failed","timed_out"]},{"type":"object"}]}}
        }}
    ]});
    let effect_schema = json!({"oneOf":[
        {"type":"object","required":["goto"],"additionalProperties":false,"properties":{"goto":{"type":"object","required":["step"],"additionalProperties":false,"properties":{"step":{"type":"string"},"after":{"oneOf":[{"type":"null"},{"type":"object"}]}}}}},
        {"type":"object","required":["end"],"additionalProperties":false,"properties":{"end":{"type":"object","required":["outcome"],"additionalProperties":false,"properties":{"outcome":{"enum":["success","failure"]}}}}}
    ]});
    let workflow_schema = json!({"type":"object","required":["entry","steps"],"additionalProperties":false,"properties":{
        "entry":{"type":"string"},"steps":{"type":"object","minProperties":1,"additionalProperties":{"type":"object","required":["action"],"additionalProperties":false,"properties":{
            "action":action_schema.clone(),
            "timeout":{"type":"string"},"max_visits":{"type":"integer","minimum":1},
            "transitions":{"type":"array","items":{"type":"object","required":["when","then"],"additionalProperties":false,"properties":{"when":condition_schema,"then":effect_schema}}}
        }}}
    }});
    json!([
        {"name":"schedule","description":"Schedule one exec argv or notification, or a workflow graph of such steps with ordered transitions. A workflow uses the exec capability when it contains exec steps and the notify capability when it contains notify steps; either may require approval. Transition order is significant and the first matching condition wins. No environment overrides.",
         "inputSchema":{"type":"object","additionalProperties":false,"oneOf":[{"required":["action"],"not":{"required":["workflow"]}},{"required":["workflow"],"not":{"required":["action"]}}],"properties":{
            "action":action_schema,"workflow":workflow_schema,
            "at":{"type":"string","description":"One instant, e.g. in 1h or 2026-10-01 09:00; required except calendar recurrence"},
            "every":{"type":"string","description":"Optional recurrence: 30m, day 09:00, mon,wed,fri 9am"},
            "name":{"type":"string"},"zone":{"type":"string"},"cwd":{"type":"string"},
            "count":{"type":"integer","minimum":1},"until":{"type":"string"}}}},
        {"name":"list","description":"List definitions with lifecycle and approval state; captured environment is omitted.","inputSchema":{"type":"object","properties":{"all":{"type":"boolean"}},"additionalProperties":false}},
        {"name":"show","description":"Inspect a definition and its approval or expiry; captured environment is omitted.","inputSchema":reference},
        {"name":"cancel","description":"Cancel a job or deny a pending approval.","inputSchema":reference},
        {"name":"logs","description":"Read a bounded tail of captured output. Separately disabled by default.","inputSchema":{"type":"object","required":["job"],"additionalProperties":false,"properties":{
            "job":{"type":"string"},"run":{"type":"integer"},"step":{"type":"string"},"attempt":{"type":"integer","minimum":1},"max_bytes":{"type":"integer","minimum":1,"maximum":65536,"default":16384}}}}
    ])
}
fn rpc_error(id: Value, code: i32, message: impl ToString) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message.to_string()}})
}

pub fn serve(input: impl BufRead, mut output: impl Write, server: &Server) -> Result<()> {
    let mut initialized = false;
    for line in input.lines() {
        let line = line?;
        let message: Value = match serde_json::from_str(&line) {
            Ok(message) => message,
            Err(_) => {
                writeln!(output, "{}", rpc_error(Value::Null, -32700, "Parse error"))?;
                output.flush()?;
                continue;
            }
        };
        let id = message.get("id").cloned();
        let method = message["method"].as_str();
        if id.is_none() && method.is_some_and(|m| m.starts_with("notifications/")) {
            continue;
        }
        let id = id.unwrap_or(Value::Null);
        let response = if message["jsonrpc"] != "2.0"
            || method.is_none()
            || !(id.is_string() || id.is_number())
        {
            rpc_error(id, -32600, "Invalid Request")
        } else {
            let result = match method.unwrap_or_default() {
                "initialize" => {
                    initialized = true;
                    Some(Ok(
                        json!({"protocolVersion":"2025-06-18","capabilities":{"tools":{}},
                        "serverInfo":{"name":"cued","version":env!("CARGO_PKG_VERSION")}}),
                    ))
                }
                "ping" => Some(Ok(json!({}))),
                "tools/list" if initialized => Some(Ok(json!({"tools":tools()}))),
                "tools/call" if initialized => {
                    let name = message["params"]["name"].as_str().unwrap_or("");
                    if !["schedule", "list", "show", "cancel", "logs"].contains(&name) {
                        Some(Err((-32602, "Unknown tool")))
                    } else {
                        let args = message["params"]
                            .get("arguments")
                            .cloned()
                            .unwrap_or(json!({}));
                        let (text, error) = match server.call(name, args) {
                            Ok(value) => (serde_json::to_string(&value)?, false),
                            Err(error) => (format!("{error:#}"), true),
                        };
                        Some(Ok(
                            json!({"content":[{"type":"text","text":text}],"isError":error}),
                        ))
                    }
                }
                _ if !initialized => Some(Err((-32002, "Initialize the MCP connection first"))),
                _ => None,
            };
            match result {
                Some(Ok(result)) => json!({"jsonrpc":"2.0","id":id,"result":result}),
                Some(Err((code, message))) => rpc_error(id, code, message),
                None => rpc_error(id, -32601, "Method not found"),
            }
        };
        writeln!(output, "{response}")?;
        output.flush()?;
    }
    Ok(())
}

pub fn run() -> Result<()> {
    let paths = Paths::resolve()?;
    let config_path = std::env::var_os("CUED_MCP_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| paths.config_file.with_file_name("mcp.toml"));
    // Refuse to start on a bad policy; afterwards each call re-reads it.
    McpConfig::load(&config_path)?;
    let server = Server::from_file(paths, config_path);
    serve(std::io::stdin().lock(), std::io::stdout().lock(), &server)
}
