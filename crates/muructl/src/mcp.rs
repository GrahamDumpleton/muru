//! The MCP server for coding agents (design specification section 10.11,
//! ADR 0039): `muructl mcp serve` speaks the Model Context Protocol on
//! standard input and output. Each tool runs the matching muructl command
//! with --json against the same daemon, so what a tool returns is exactly
//! what the command prints, with the command's own checks and errors.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use anyhow::Context as _;
use clap::{Args, Subcommand};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    JsonObject, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
    ToolAnnotations,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt as _};
use serde_json::{Value, json};

use crate::admin::SocketArgs;

include!(concat!(env!("OUT_DIR"), "/guide.rs"));

/// The mcp subcommands.
#[derive(Subcommand)]
pub enum McpCommand {
    /// Serve the Model Context Protocol on standard input and output, for
    /// a coding agent: tools that operate the daemon as muructl's
    /// commands do, and the user guide.
    Serve(ServeArgs),

    /// The skill a project carries for coding agents: print it, install or
    /// update it in the project, or check the project's copy.
    Skill {
        #[command(subcommand)]
        command: crate::skill::SkillCommand,
    },
}

/// What `mcp serve` takes.
#[derive(Args)]
pub struct ServeArgs {
    /// Start the daemon first when none answers, as `muructl up` does.
    #[arg(long)]
    up: bool,

    /// Offer only the tools that read; none that change anything.
    #[arg(long, conflicts_with = "allow")]
    read_only: bool,

    /// Offer only these of the tools that change things, by name; the
    /// tools that read are always offered.
    #[arg(long, value_delimiter = ',', value_name = "TOOL")]
    allow: Vec<String>,

    #[command(flatten)]
    socket: SocketArgs,
}

/// Run an `mcp` subcommand.
pub async fn run(command: McpCommand) -> anyhow::Result<()> {
    match command {
        McpCommand::Serve(args) => serve(args).await,
        McpCommand::Skill { command } => crate::skill::run(command),
    }
}

/// Serve tools to the agent on standard input and output until it closes
/// them. Nothing else may write to standard output meanwhile: muructl's
/// own diagnostics go to standard error, and each command a tool runs has
/// its output captured.
async fn serve(args: ServeArgs) -> anyhow::Result<()> {
    let muructl = std::env::current_exe().context("finding the muructl executable")?;
    let all = specs();

    for name in &args.allow {
        anyhow::ensure!(
            all.iter().any(|spec| spec.changes && spec.name == name),
            "--allow names {name}, which is not a tool that changes anything; those are {}",
            all.iter()
                .filter(|spec| spec.changes)
                .map(|spec| spec.name)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    let tools: Vec<Spec> = all
        .into_iter()
        .filter(|spec| {
            !spec.changes
                || (!args.read_only
                    && (args.allow.is_empty() || args.allow.iter().any(|name| name == spec.name)))
        })
        .collect();
    let server = Server {
        tools: Arc::new(tools),
        connect: Arc::new(args.socket.as_arguments()),
        muructl,
        read_only: args.read_only,
        skill: crate::skill::status(std::path::Path::new(crate::skill::DEFAULT_DIR)),
    };

    // With --up, a daemon is started when none answers, its output kept
    // off standard output, which belongs to the protocol.

    if args.up && !server.answers().await {
        let started = tokio::process::Command::new(&server.muructl)
            .arg("up")
            .args(args.socket.location_arguments())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .status()
            .await
            .context("starting the daemon")?;
        anyhow::ensure!(started.success(), "the daemon did not start");
    }

    let service = server
        .serve(rmcp::transport::stdio())
        .await
        .context("starting the MCP session")?;
    service.waiting().await.context("serving the MCP session")?;
    Ok(())
}

/// The server: the tools it offers and how its commands reach the daemon.
#[derive(Clone)]
struct Server {
    tools: Arc<Vec<Spec>>,
    /// The socket and location options every command it runs is given.
    connect: Arc<Vec<String>>,
    muructl: PathBuf,
    read_only: bool,
    /// How the project's copy of the skill compared at start.
    skill: Value,
}

impl Server {
    /// Whether the daemon answers.
    async fn answers(&self) -> bool {
        tokio::process::Command::new(&self.muructl)
            .arg("status")
            .args(self.connect.iter())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .is_ok_and(|status| status.success())
    }

    /// Run a muructl command with --json and turn what it printed into a
    /// tool result: the JSON on success, the error's sentence on failure.
    async fn command(&self, mut command: Vec<String>) -> CallToolResult {
        command.push("--json".to_owned());
        command.extend(self.connect.iter().cloned());

        let output = match tokio::process::Command::new(&self.muructl)
            .args(&command)
            .stdin(Stdio::null())
            .output()
            .await
        {
            Ok(output) => output,
            Err(error) => return failure(format!("running muructl: {error}")),
        };
        let printed = String::from_utf8_lossy(&output.stdout).trim().to_owned();

        if output.status.success() {
            return CallToolResult::success(vec![ContentBlock::text(printed)]);
        }

        // A failure prints {"error": ...}; anything else (clap refusing
        // the arguments, say) is on standard error.

        let message = serde_json::from_str::<Value>(&printed)
            .ok()
            .and_then(|answer| answer["error"].as_str().map(str::to_owned))
            .unwrap_or_else(|| String::from_utf8_lossy(&output.stderr).trim().to_owned());
        failure(message)
    }

    /// The run_command tool. A secret input given as a value is refused,
    /// so a secret never passes through the agent; the command's output
    /// comes back with its exit status, as an error when that is not 0.
    async fn run_command(&self, arguments: &JsonObject) -> CallToolResult {
        let command = match (specs()
            .into_iter()
            .find(|spec| spec.name == "run_command")
            .expect("the run_command tool")
            .command)(arguments)
        {
            Ok(command) => command,
            Err(message) => return failure(message),
        };

        let given = match pairs(arguments, "inputs") {
            Ok(given) => given,
            Err(message) => return failure(message),
        };

        if !given.is_empty() {
            let listed = self.command(words(&["command", "list", &command[2]])).await;
            let declared = listed
                .content
                .first()
                .and_then(|block| block.as_text())
                .and_then(|text| serde_json::from_str::<Value>(&text.text).ok());

            let secret = declared.as_ref().and_then(|declared| {
                let inputs = declared["commands"]
                    .as_array()?
                    .iter()
                    .find(|listed| listed["name"] == command[3])?["inputs"]
                    .as_array()?
                    .clone();
                given.iter().find_map(|(name, _)| {
                    inputs
                        .iter()
                        .any(|input| input["name"] == *name && input["secret"] == true)
                        .then(|| name.clone())
                })
            });

            if let Some(name) = secret {
                return failure(format!(
                    "the input {name} is secret: give it in secret_inputs as STORE/KEY, an entry \
                     of one of the daemon's secret stores, so its value never passes through here"
                ));
            }
        }

        let mut command = command;
        command.push("--json".to_owned());
        command.extend(self.connect.iter().cloned());

        let output = match tokio::process::Command::new(&self.muructl)
            .args(&command)
            .stdin(Stdio::null())
            .output()
            .await
        {
            Ok(output) => output,
            Err(error) => return failure(format!("running muructl: {error}")),
        };

        command_result(
            &String::from_utf8_lossy(&output.stdout),
            &String::from_utf8_lossy(&output.stderr),
        )
    }

    /// The instructions the initialize response carries: what Muru's
    /// objects are, so the agent picks the right tool.
    fn instructions(&self) -> String {
        let mut text = String::from(INSTRUCTIONS);

        if self.read_only {
            text.push_str("\n\nThis server is read-only: it offers no tool that changes anything.");
        }

        // A project whose skill is missing or older than this muructl's
        // hears about it once, with the command that fixes it.

        if self.skill["state"] != "current" {
            text.push_str("\n\nThe Muru skill: ");
            text.push_str(&crate::skill::describe(&self.skill));
            text.push('.');
        }

        text
    }
}

/// What the agent is told when the session starts.
const INSTRUCTIONS: &str = "\
Muru serves Python web applications (WSGI and ASGI) behind its own edge. \
These tools operate the Muru daemon on this machine through muructl, and \
each returns the --json output of the muructl command it names.

An application is a name with routes; a release is one immutable \
deployment of the application's source with its settings, named \
app@release (shop@r3). Traffic goes to one release at a time, or is split \
between two by a rollout. Only the daemon's reconciler acts: a tool \
records what is wanted and the reconciler makes it so.

- deploy creates a release from a source directory and, once its workers \
are ready, moves traffic to it; the previous release drains. deploy with \
hold starts the release without moving traffic.
- promote moves traffic to a release now; rollback restores the previous \
release; drain takes a release out of traffic and stops it.
- rollout canary shifts traffic to a release step by step, each step judged \
by gates against the release that serves.
- apply makes a declarative file (muru.toml) the desired state; deploy adds \
one release.
- An overlay is a named, temporary change to routes or settings.
- commands lists what an application declares it can run with a release \
(migrations, data fixes, checks); run_command runs one by name, with its \
inputs, and returns its output and exit status.

After a deploy, check the release's state and the events. When something \
fails, read the release's logs, then debug stacks for what the workers are \
doing. The docs tool returns pages of the user guide.";

impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                Implementation::new("muru", env!("CARGO_PKG_VERSION")).with_title("Muru"),
            )
            .with_instructions(self.instructions())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult {
            tools: self.tools.iter().map(Spec::tool).collect(),
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let Some(spec) = self.tools.iter().find(|spec| spec.name == request.name) else {
            return Err(ErrorData::invalid_params(
                format!("no tool named {}", request.name),
                None,
            ));
        };
        let arguments = request.arguments.unwrap_or_default();

        let result = match spec.name {
            "docs" => docs(&arguments),
            "skill_status" => skill_status(),
            "run_command" => self.run_command(&arguments).await,
            _ => match (spec.command)(&arguments) {
                Ok(command) => {
                    let result = self.command(command).await;
                    finish(spec.name, &arguments, result)
                }
                Err(message) => failure(message),
            },
        };

        Ok(result.into())
    }
}

/// A tool: what the agent sees, and the muructl command it runs.
struct Spec {
    name: &'static str,
    description: &'static str,
    /// Whether it changes anything; --read-only and --allow leave these
    /// out.
    changes: bool,
    /// Its arguments' JSON Schema properties, and which are required.
    properties: Value,
    required: &'static [&'static str],
    /// The command line it runs, without --json and the socket options.
    command: fn(&JsonObject) -> Result<Vec<String>, String>,
}

impl Spec {
    /// The tool as the protocol lists it, with its input schema and
    /// whether it reads or changes things.
    fn tool(&self) -> Tool {
        let mut schema = JsonObject::new();
        schema.insert("type".to_owned(), json!("object"));
        schema.insert("properties".to_owned(), self.properties.clone());

        if !self.required.is_empty() {
            schema.insert("required".to_owned(), json!(self.required));
        }

        schema.insert("additionalProperties".to_owned(), json!(false));

        let annotations = ToolAnnotations::new()
            .read_only(!self.changes)
            .destructive(self.changes)
            .idempotent(!self.changes)
            .open_world(false);
        Tool::new(self.name, self.description, Arc::new(schema)).with_annotations(annotations)
    }
}

/// A failed tool call, with the sentence the agent reads.
fn failure(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message.into())])
}

/// An argument as text, if given.
fn text(arguments: &JsonObject, key: &str) -> Option<String> {
    match arguments.get(key)? {
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// An argument that must be given, as text.
fn needed(arguments: &JsonObject, key: &str) -> Result<String, String> {
    text(arguments, key).ok_or_else(|| format!("{key} is required"))
}

/// A boolean argument, false when absent.
fn flag(arguments: &JsonObject, key: &str) -> bool {
    arguments.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// An option and its value, when the argument is given.
fn option(arguments: &JsonObject, key: &str, name: &str) -> Vec<String> {
    text(arguments, key)
        .map(|value| vec![name.to_owned(), value])
        .unwrap_or_default()
}

/// An argument that is an object of strings, as name and value pairs.
fn pairs(arguments: &JsonObject, key: &str) -> Result<Vec<(String, String)>, String> {
    let Some(value) = arguments.get(key) else {
        return Ok(Vec::new());
    };
    let Some(object) = value.as_object() else {
        return Err(format!("{key} is an object of names and values"));
    };

    object
        .iter()
        .map(|(name, value)| match value {
            Value::String(text) => Ok((name.clone(), text.clone())),
            Value::Number(number) => Ok((name.clone(), number.to_string())),
            Value::Bool(flag) => Ok((name.clone(), flag.to_string())),
            _ => Err(format!("{key}.{name} is not a string")),
        })
        .collect()
}

/// Words, all owned.
fn words(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_owned()).collect()
}

/// After a command: a release's output becomes an array of its lines, and
/// metrics keep only the names asked for, so a result stays small.
fn finish(name: &str, arguments: &JsonObject, result: CallToolResult) -> CallToolResult {
    if result.is_error == Some(true) {
        return result;
    }

    let Some(printed) = result
        .content
        .first()
        .and_then(|block| block.as_text())
        .map(|text| text.text.clone())
    else {
        return result;
    };

    let value = match name {
        "logs" => Value::Array(
            printed
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(|line| serde_json::from_str(line).unwrap_or_else(|_| json!(line)))
                .collect(),
        ),
        "metrics" => {
            let Ok(mut answer) = serde_json::from_str::<Value>(&printed) else {
                return result;
            };
            let prefix = text(arguments, "prefix").unwrap_or_default();

            if let Some(samples) = answer["samples"].as_array_mut() {
                samples.retain(|sample| {
                    sample["name"]
                        .as_str()
                        .is_some_and(|name| name.starts_with(&prefix))
                });
            }

            answer
        }
        _ => return result,
    };

    CallToolResult::success(vec![ContentBlock::text(
        serde_json::to_string_pretty(&value).unwrap_or(printed),
    )])
}

/// How many lines of a command's output a tool result keeps, the last.
const COMMAND_OUTPUT_LINES: usize = 500;

/// A command's JSON lines as one result: its output, standard error lines
/// marked, the last lines only when there are many, then how it ended.
/// A refusal (`{"error": ...}`) is the error's sentence.
fn command_result(printed: &str, stderr: &str) -> CallToolResult {
    let messages: Vec<Value> = printed
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();

    let Some(last) = messages.last().filter(|last| last["finished"] == true) else {
        let message = serde_json::from_str::<Value>(printed.trim())
            .ok()
            .and_then(|answer| answer["error"].as_str().map(str::to_owned))
            .unwrap_or_else(|| stderr.trim().to_owned());
        return failure(message);
    };

    let lines: Vec<String> = messages
        .iter()
        .filter(|message| message["finished"] != true)
        .map(|message| {
            let line = message["line"].as_str().unwrap_or_default();

            if message["stderr"] == true {
                format!("stderr: {line}")
            } else {
                line.to_owned()
            }
        })
        .collect();
    let omitted = lines.len().saturating_sub(COMMAND_OUTPUT_LINES);
    let exit_code = last["exit_code"].as_i64().unwrap_or(0);

    let mut answer = json!({
        "release": last["release"],
        "output": lines[omitted..],
        "exit_code": exit_code,
        "message": last["message"],
    });

    if omitted > 0 {
        answer["omitted_lines"] = json!(omitted);
    }

    let text = vec![ContentBlock::text(
        serde_json::to_string_pretty(&answer).unwrap_or_default(),
    )];

    if exit_code == 0 {
        CallToolResult::success(text)
    } else {
        CallToolResult::error(text)
    }
}

/// The skill_status tool: how the project's copy of the skill compares
/// with this muructl's now, as `muructl mcp skill status --json` prints it.
fn skill_status() -> CallToolResult {
    let report = crate::skill::status(std::path::Path::new(crate::skill::DEFAULT_DIR));
    CallToolResult::success(vec![ContentBlock::text(
        serde_json::to_string_pretty(&report).unwrap_or_default(),
    )])
}

/// The docs tool: the guide's pages by name, or one page's text.
fn docs(arguments: &JsonObject) -> CallToolResult {
    let Some(wanted) = text(arguments, "page") else {
        let names: Vec<&str> = PAGES.iter().map(|(name, _)| *name).collect();
        return CallToolResult::success(vec![ContentBlock::text(names.join("\n"))]);
    };
    let wanted = wanted.trim_end_matches(".md");

    match PAGES
        .iter()
        .find(|(name, _)| *name == wanted || name.ends_with(&format!("/{wanted}")))
    {
        Some((_, page)) => CallToolResult::success(vec![ContentBlock::text(*page)]),
        None => failure(format!(
            "no page named {wanted}; call docs without a page for the list"
        )),
    }
}

/// Every tool, in the order the agent sees them: few and shaped like
/// tasks rather than one per API call, since an agent's client puts
/// every description into its context.
fn specs() -> Vec<Spec> {
    vec![
        Spec {
            name: "status",
            description: "The daemon's status: version, listeners, applications and releases. Fails when no daemon answers.",
            changes: false,
            properties: json!({}),
            required: &[],
            command: |_| Ok(words(&["status"])),
        },
        Spec {
            name: "list",
            description: "List one kind of object: apps, releases (of one app, or all), hosts, routes, upstreams, rollouts (of one app, or all) or overlays.",
            changes: false,
            properties: json!({
                "what": {"type": "string", "enum": ["apps", "releases", "hosts", "routes", "upstreams", "rollouts", "overlays"]},
                "app": {"type": "string", "description": "For releases and rollouts: one application only"},
            }),
            required: &["what"],
            command: |arguments| {
                let app: Vec<String> = text(arguments, "app").into_iter().collect();

                Ok(match needed(arguments, "what")?.as_str() {
                    "apps" => words(&["app", "list"]),
                    "releases" => [words(&["release", "list"]), app].concat(),
                    "hosts" => words(&["host", "list"]),
                    "routes" => words(&["route", "list"]),
                    "upstreams" => words(&["upstream", "list"]),
                    "rollouts" => [words(&["rollout", "status"]), app].concat(),
                    "overlays" => words(&["overlay", "list"]),
                    other => return Err(format!("cannot list {other}")),
                })
            },
        },
        Spec {
            name: "release",
            description: "One release in full: its state and message, source, settings, mounts and worker groups, and whether it serves.",
            changes: false,
            properties: json!({"release": {"type": "string", "description": "app@release, such as shop@r3"}}),
            required: &["release"],
            command: |arguments| {
                Ok([
                    words(&["release", "show"]),
                    vec![needed(arguments, "release")?],
                ]
                .concat())
            },
        },
        Spec {
            name: "commands",
            description: "The commands an application declares (migrations, data fixes, checks and the like), with their inputs, which are required or secret, and how each last ran.",
            changes: false,
            properties: json!({"target": {"type": "string", "description": "app, or app@release; the current release when only the app is named"}}),
            required: &["target"],
            command: |arguments| {
                Ok([
                    words(&["command", "list"]),
                    vec![needed(arguments, "target")?],
                ]
                .concat())
            },
        },
        Spec {
            name: "events",
            description: "The daemon's events in order (deploys, state changes, rollouts, worker faults), up to 200 after a sequence number.",
            changes: false,
            properties: json!({"after": {"type": "integer", "description": "Only events after this sequence number; 0 for the oldest kept"}}),
            required: &[],
            command: |arguments| {
                Ok([words(&["events"]), option(arguments, "after", "--after")].concat())
            },
        },
        Spec {
            name: "logs",
            description: "Recent lines of a release's output (prints, logging, tracebacks), or of the access log, as an array.",
            changes: false,
            properties: json!({
                "target": {"type": "string", "description": "app, or app@release; the current release when only the app is named"},
                "lines": {"type": "integer", "description": "How many recent lines (default 100, at most 1000)"},
                "access": {"type": "boolean", "description": "The access log instead of the release's output"},
            }),
            required: &[],
            command: |arguments| {
                let lines = arguments
                    .get("lines")
                    .and_then(Value::as_u64)
                    .unwrap_or(100)
                    .min(1000);
                let mut command = words(&["logs"]);
                command.extend(text(arguments, "target"));
                command.extend(["--lines".to_owned(), lines.to_string()]);

                if flag(arguments, "access") {
                    command.push("--access".to_owned());
                }

                Ok(command)
            },
        },
        Spec {
            name: "debug",
            description: "Look into a release's workers: status (pid, uptime, requests, memory), stacks (each thread's Python stack and the request it serves), tasks (asyncio), memory, or inspect (interpreter, modules, environment).",
            changes: false,
            properties: json!({
                "what": {"type": "string", "enum": ["status", "stacks", "tasks", "memory", "inspect"]},
                "target": {"type": "string", "description": "app, or app@release"},
                "worker": {"type": "integer", "description": "One worker, by index"},
            }),
            required: &["what", "target"],
            command: |arguments| {
                let what = needed(arguments, "what")?;

                if !["status", "stacks", "tasks", "memory", "inspect"].contains(&what.as_str()) {
                    return Err(format!("cannot debug {what}"));
                }

                Ok([
                    words(&["debug"]),
                    vec![what, needed(arguments, "target")?],
                    option(arguments, "worker", "--worker"),
                ]
                .concat())
            },
        },
        Spec {
            name: "scale_status",
            description: "Each worker group's scaling policy, workers, signals, last decision and churn for a release.",
            changes: false,
            properties: json!({"target": {"type": "string", "description": "app, or app@release"}}),
            required: &["target"],
            command: |arguments| {
                Ok([
                    words(&["scale", "status"]),
                    vec![needed(arguments, "target")?],
                ]
                .concat())
            },
        },
        Spec {
            name: "config",
            description: "The desired state as TOML (show, with effective for what active overlays change), or compare a file or directory of TOML files with it (diff).",
            changes: false,
            properties: json!({
                "action": {"type": "string", "enum": ["show", "diff"]},
                "app": {"type": "string", "description": "For show: one application only"},
                "effective": {"type": "boolean", "description": "For show: as the active overlays leave it"},
                "file": {"type": "string", "description": "For diff: the file or directory"},
            }),
            required: &["action"],
            command: |arguments| match needed(arguments, "action")?.as_str() {
                "show" => {
                    let mut command = words(&["config", "show"]);
                    command.extend(text(arguments, "app"));

                    if flag(arguments, "effective") {
                        command.push("--effective".to_owned());
                    }

                    Ok(command)
                }
                "diff" => Ok([
                    words(&["config", "diff", "--file"]),
                    vec![needed(arguments, "file")?],
                ]
                .concat()),
                other => Err(format!("no config action {other}")),
            },
        },
        Spec {
            name: "metrics",
            description: "The node's metrics as samples (name, type, labels, value), optionally only those whose name starts with a prefix such as muru_requests.",
            changes: false,
            properties: json!({"prefix": {"type": "string"}}),
            required: &[],
            command: |_| Ok(words(&["metrics"])),
        },
        Spec {
            name: "websockets",
            description: "The WebSockets open now: counts by endpoint, or each one with list.",
            changes: false,
            properties: json!({
                "app": {"type": "string"},
                "list": {"type": "boolean"},
            }),
            required: &[],
            command: |arguments| {
                let mut command = words(&["websockets"]);
                command.extend(text(arguments, "app"));

                if flag(arguments, "list") {
                    command.push("--list".to_owned());
                }

                Ok(command)
            },
        },
        Spec {
            name: "overlay_preview",
            description: "What activating an overlay now would change, without activating it.",
            changes: false,
            properties: json!({"name": {"type": "string"}}),
            required: &["name"],
            command: |arguments| {
                Ok([
                    words(&["overlay", "preview"]),
                    vec![needed(arguments, "name")?],
                ]
                .concat())
            },
        },
        Spec {
            name: "docs",
            description: "The Muru user guide: without a page, the list of pages (tutorials, howto, explanation, reference); with one, its Markdown.",
            changes: false,
            properties: json!({"page": {"type": "string", "description": "A page name from the list, such as howto/deploy"}}),
            required: &[],
            command: |_| Ok(Vec::new()),
        },
        Spec {
            name: "skill_status",
            description: "Whether the project's Muru skill (.claude/skills/muru/SKILL.md) matches this muructl's: current, missing or out_of_date; `muructl mcp skill install` writes or updates it.",
            changes: false,
            properties: json!({}),
            required: &[],
            command: |_| Ok(Vec::new()),
        },
        Spec {
            name: "deploy",
            description: "Create a release of an application from a source directory and, once ready, move traffic to it; hold starts it without moving traffic. Waits for the release unless wait is false.",
            changes: true,
            properties: json!({
                "app": {"type": "string"},
                "source": {"type": "string", "description": "The source directory (default: the server's current directory)"},
                "entrypoint": {"type": "string", "description": "module:callable, else from the source's [tool.muru] table"},
                "python": {"type": "string", "description": "An interpreter path or a version such as 3.13"},
                "hold": {"type": "boolean"},
                "wait": {"type": "boolean", "description": "Wait until the release is ready or failed (default true)"},
            }),
            required: &["app"],
            command: |arguments| {
                let mut command = words(&["deploy"]);
                command.push(needed(arguments, "app")?);
                command.extend(text(arguments, "source"));
                command.extend(option(arguments, "entrypoint", "--entrypoint"));
                command.extend(option(arguments, "python", "--python"));

                if flag(arguments, "hold") {
                    command.push("--hold".to_owned());
                }

                if arguments.get("wait").and_then(Value::as_bool) == Some(false) {
                    command.push("--no-wait".to_owned());
                }

                Ok(command)
            },
        },
        Spec {
            name: "promote",
            description: "Move all of an application's traffic to a release now, and drain the one that served.",
            changes: true,
            properties: json!({"release": {"type": "string", "description": "app@release"}}),
            required: &["release"],
            command: |arguments| {
                Ok([
                    words(&["rollout", "bluegreen"]),
                    vec![needed(arguments, "release")?],
                ]
                .concat())
            },
        },
        Spec {
            name: "rollback",
            description: "Restore traffic to an application's previous release.",
            changes: true,
            properties: json!({"app": {"type": "string"}}),
            required: &["app"],
            command: |arguments| {
                Ok([words(&["rollback"]), vec![needed(arguments, "app")?]].concat())
            },
        },
        Spec {
            name: "drain",
            description: "Take a release out of traffic and stop its workers once their requests finish.",
            changes: true,
            properties: json!({"release": {"type": "string", "description": "app@release"}}),
            required: &["release"],
            command: |arguments| {
                Ok([words(&["drain"]), vec![needed(arguments, "release")?]].concat())
            },
        },
        Spec {
            name: "rollout",
            description: "Start a canary rollout of a release (steps as percentages, each held then judged by gates), or pause, resume, promote or abort an application's rollout.",
            changes: true,
            properties: json!({
                "action": {"type": "string", "enum": ["canary", "pause", "resume", "promote", "abort"]},
                "release": {"type": "string", "description": "For canary: the candidate, app@release"},
                "app": {"type": "string", "description": "For the other actions"},
                "steps": {"type": "array", "items": {"type": "integer"}, "description": "For canary: percentages, such as [10, 50]"},
                "hold": {"type": "string", "description": "For canary: how long each step holds, such as 2m"},
            }),
            required: &["action"],
            command: |arguments| {
                let action = needed(arguments, "action")?;

                if action != "canary" {
                    return Ok(
                        [words(&["rollout"]), vec![action, needed(arguments, "app")?]].concat(),
                    );
                }

                let mut command = [
                    words(&["rollout", "canary"]),
                    vec![needed(arguments, "release")?],
                ]
                .concat();

                if let Some(steps) = arguments.get("steps").and_then(Value::as_array) {
                    let steps: Vec<String> = steps.iter().map(Value::to_string).collect();
                    command.extend(["--steps".to_owned(), steps.join(",")]);
                }

                command.extend(option(arguments, "hold", "--hold"));
                Ok(command)
            },
        },
        Spec {
            name: "scale",
            description: "Pin a worker group to a count of workers, or unpin it so the autoscaler decides again.",
            changes: true,
            properties: json!({
                "target": {"type": "string", "description": "app, or app@release"},
                "group": {"type": "string", "description": "The worker group (default: default)"},
                "processes": {"type": "integer"},
                "unpin": {"type": "boolean"},
            }),
            required: &["target"],
            command: |arguments| {
                let mut command = [words(&["scale"]), vec![needed(arguments, "target")?]].concat();
                command.extend(option(arguments, "group", "--group"));
                command.extend(option(arguments, "processes", "--processes"));

                if flag(arguments, "unpin") {
                    command.push("--unpin".to_owned());
                }

                Ok(command)
            },
        },
        Spec {
            name: "run_command",
            description: "Run a command the application declares, with its inputs, and return its output and exit status. Give a secret input in secret_inputs as STORE/KEY, an entry of one of the daemon's secret stores, never as a value.",
            changes: true,
            properties: json!({
                "target": {"type": "string", "description": "app, or app@release; the current release when only the app is named"},
                "name": {"type": "string", "description": "The command, as the commands tool lists it"},
                "inputs": {"type": "object", "additionalProperties": {"type": "string"}, "description": "Input values by name"},
                "secret_inputs": {"type": "object", "additionalProperties": {"type": "string"}, "description": "Secret inputs by name, each as STORE/KEY"},
            }),
            required: &["target", "name"],
            command: |arguments| {
                let mut command = words(&["command", "run"]);
                command.push(needed(arguments, "target")?);
                command.push(needed(arguments, "name")?);

                for (key, option) in [("inputs", "--input"), ("secret_inputs", "--secret-input")] {
                    for (name, value) in pairs(arguments, key)? {
                        command.push(option.to_owned());
                        command.push(format!("{name}={value}"));
                    }
                }

                Ok(command)
            },
        },
        Spec {
            name: "restart",
            description: "Replace a release's worker (by index) with a fresh one, gracefully or at once with force; or every worker in turn with all.",
            changes: true,
            properties: json!({
                "target": {"type": "string", "description": "app, or app@release"},
                "worker": {"type": "integer"},
                "all": {"type": "boolean"},
                "force": {"type": "boolean"},
            }),
            required: &["target"],
            command: |arguments| {
                let mut command =
                    [words(&["restart"]), vec![needed(arguments, "target")?]].concat();
                command.extend(option(arguments, "worker", "--worker"));

                for (key, name) in [("all", "--all"), ("force", "--force")] {
                    if flag(arguments, key) {
                        command.push(name.to_owned());
                    }
                }

                Ok(command)
            },
        },
        Spec {
            name: "overlay",
            description: "Activate an overlay (until ended, or for a duration), end it, or promote its changes into the base configuration.",
            changes: true,
            properties: json!({
                "action": {"type": "string", "enum": ["activate", "end", "promote"]},
                "name": {"type": "string"},
                "for": {"type": "string", "description": "For activate: end it after this long, such as 2h"},
            }),
            required: &["action", "name"],
            command: |arguments| {
                let mut command = [
                    words(&["overlay"]),
                    vec![needed(arguments, "action")?, needed(arguments, "name")?],
                ]
                .concat();
                command.extend(option(arguments, "for", "--for"));
                Ok(command)
            },
        },
        Spec {
            name: "maintenance",
            description: "Serve an application's maintenance page at the edge (on), or serve it as usual again (off).",
            changes: true,
            properties: json!({
                "app": {"type": "string"},
                "on": {"type": "boolean"},
            }),
            required: &["app", "on"],
            command: |arguments| {
                let state = if flag(arguments, "on") { "on" } else { "off" };
                Ok([
                    words(&["maintenance", state]),
                    vec![needed(arguments, "app")?],
                ]
                .concat())
            },
        },
        Spec {
            name: "apply",
            description: "Make a declarative TOML file, or a directory of them, the desired state; dry_run reports what would change without changing it.",
            changes: true,
            properties: json!({
                "file": {"type": "string"},
                "dry_run": {"type": "boolean"},
            }),
            required: &["file"],
            command: |arguments| {
                let mut command = [
                    words(&["apply", "--file"]),
                    vec![needed(arguments, "file")?],
                ]
                .concat();

                if flag(arguments, "dry_run") {
                    command.push("--dry-run".to_owned());
                }

                Ok(command)
            },
        },
        Spec {
            name: "log_level",
            description: "Set Python logger levels in a release's workers (as NAME=LEVEL, root as =LEVEL), for a duration or until reset.",
            changes: true,
            properties: json!({
                "target": {"type": "string", "description": "app, or app@release"},
                "levels": {"type": "array", "items": {"type": "string"}, "description": "Such as [\"myapp=DEBUG\"]"},
                "for": {"type": "string", "description": "Revert after this long, such as 10m"},
                "reset": {"type": "boolean"},
            }),
            required: &["target"],
            command: |arguments| {
                let mut command = [
                    words(&["logging", "level"]),
                    vec![needed(arguments, "target")?],
                ]
                .concat();

                if let Some(levels) = arguments.get("levels").and_then(Value::as_array) {
                    command.extend(levels.iter().filter_map(Value::as_str).map(str::to_owned));
                }

                command.extend(option(arguments, "for", "--for"));

                if flag(arguments, "reset") {
                    command.push("--reset".to_owned());
                }

                Ok(command)
            },
        },
        Spec {
            name: "access_log",
            description: "Change which requests the access log writes (on, errors or off) for a duration, for one application and some paths or all, then it reverts on its own; without a mode, list the changes in force; reset puts them back.",
            changes: true,
            properties: json!({
                "mode": {"type": "string", "enum": ["on", "errors", "off"]},
                "app": {"type": "string", "description": "One application; every one when left out"},
                "paths": {"type": "array", "items": {"type": "string"}, "description": "Path patterns such as [\"/api/*\"]; every path when left out"},
                "for": {"type": "string", "description": "How long, such as 10m; needed with a mode"},
                "reset": {"type": "boolean"},
            }),
            required: &[],
            command: |arguments| {
                let mut command = words(&["logging", "access"]);

                if let Some(mode) = arguments.get("mode").and_then(Value::as_str) {
                    command.push(mode.to_owned());
                }

                command.extend(option(arguments, "app", "--app"));

                if let Some(paths) = arguments.get("paths").and_then(Value::as_array) {
                    for path in paths.iter().filter_map(Value::as_str) {
                        command.extend(["--path".to_owned(), path.to_owned()]);
                    }
                }

                command.extend(option(arguments, "for", "--for"));

                if flag(arguments, "reset") {
                    command.push("--reset".to_owned());
                }

                Ok(command)
            },
        },
        Spec {
            name: "reopen_logs",
            description: "Reopen every log file by name, after something outside Muru has renamed them; workers also runs each worker's on_reopen_logs hook.",
            changes: true,
            properties: json!({"workers": {"type": "boolean"}}),
            required: &[],
            command: |arguments| {
                let mut command = words(&["logs", "reopen"]);

                if flag(arguments, "workers") {
                    command.push("--workers".to_owned());
                }

                Ok(command)
            },
        },
        Spec {
            name: "reload_config",
            description: "Re-read the daemon's configuration file and apply what changed.",
            changes: true,
            properties: json!({}),
            required: &[],
            command: |_| Ok(words(&["daemon", "reload"])),
        },
    ]
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{PAGES, specs};

    /// The command line a tool's arguments become.
    fn command(name: &str, arguments: serde_json::Value) -> Result<Vec<String>, String> {
        let specs = specs();
        let spec = specs.iter().find(|spec| spec.name == name).expect("a tool");
        (spec.command)(arguments.as_object().expect("an object"))
    }

    #[test]
    fn tools_become_the_muructl_commands_they_name() {
        assert_eq!(
            command("list", json!({"what": "releases", "app": "shop"})),
            Ok(vec!["release".into(), "list".into(), "shop".into()])
        );
        assert_eq!(
            command(
                "deploy",
                json!({"app": "shop", "source": "/srv/shop", "wait": false})
            ),
            Ok(vec![
                "deploy".into(),
                "shop".into(),
                "/srv/shop".into(),
                "--no-wait".into()
            ])
        );
        assert_eq!(
            command(
                "rollout",
                json!({"action": "canary", "release": "shop@r4", "steps": [10, 50]})
            ),
            Ok(vec![
                "rollout".into(),
                "canary".into(),
                "shop@r4".into(),
                "--steps".into(),
                "10,50".into()
            ])
        );
        assert_eq!(
            command("logs", json!({"target": "shop", "lines": 5000})),
            Ok(vec![
                "logs".into(),
                "shop".into(),
                "--lines".into(),
                "1000".into()
            ])
        );
        assert!(command("debug", json!({"what": "exec", "target": "shop"})).is_err());
        assert_eq!(
            command(
                "run_command",
                json!({"target": "shop@r2", "name": "add-user", "inputs": {"username": "bob"},
                       "secret_inputs": {"password": "local/admin"}})
            ),
            Ok(vec![
                "command".into(),
                "run".into(),
                "shop@r2".into(),
                "add-user".into(),
                "--input".into(),
                "username=bob".into(),
                "--secret-input".into(),
                "password=local/admin".into()
            ])
        );
        assert!(
            command(
                "run_command",
                json!({"target": "shop", "name": "x", "inputs": "a=b"})
            )
            .is_err()
        );
        assert_eq!(
            command("release", json!({})),
            Err("release is required".to_owned())
        );
    }

    #[test]
    fn tool_names_are_unique_and_schemas_name_their_required_arguments() {
        let specs = specs();

        for spec in &specs {
            assert_eq!(
                specs.iter().filter(|other| other.name == spec.name).count(),
                1
            );

            for required in spec.required {
                assert!(
                    spec.properties.get(*required).is_some(),
                    "{} requires {required}",
                    spec.name
                );
            }
        }
    }

    #[test]
    fn a_command_result_keeps_the_output_and_the_ending() {
        let printed = [
            r#"{"release":"r1","line":"migrating","stderr":false,"finished":false}"#,
            r#"{"release":"r1","line":"warning","stderr":true,"finished":false}"#,
            r#"{"release":"r1","line":"","stderr":false,"finished":true,"exit_code":2,"message":"exited with status 2 after 3s"}"#,
        ]
        .join("\n");
        let result = super::command_result(&printed, "");
        assert_eq!(result.is_error, Some(true));

        let text = &result.content[0].as_text().expect("text").text;
        let answer: serde_json::Value = serde_json::from_str(text).expect("JSON");
        assert_eq!(answer["output"], json!(["migrating", "stderr: warning"]));
        assert_eq!(answer["exit_code"], 2);
        assert_eq!(answer["release"], "r1");

        let refused = super::command_result("{\n  \"error\": \"shop needs the input who\"\n}", "");
        assert_eq!(refused.is_error, Some(true));
        assert_eq!(
            refused.content[0].as_text().expect("text").text,
            "shop needs the input who"
        );
    }

    #[test]
    fn the_guide_is_embedded() {
        assert!(
            PAGES
                .iter()
                .any(|(name, text)| *name == "reference/json-output" && text.contains("--json"))
        );
    }

    #[test]
    fn the_tools_reference_matches_the_tools() {
        let page = include_str!("../../../docs/reference/mcp-tools.md");
        let mut listed: Vec<(String, bool)> = page
            .lines()
            .filter_map(|line| line.strip_prefix("| `"))
            .filter_map(|rest| {
                let (name, rest) = rest.split_once("` | ")?;
                let kind = rest.split(" |").next()?;
                Some((name.to_owned(), kind == "changes"))
            })
            .collect();
        let mut tools: Vec<(String, bool)> = specs()
            .iter()
            .map(|spec| (spec.name.to_owned(), spec.changes))
            .collect();
        listed.sort();
        tools.sort();

        assert_eq!(
            listed, tools,
            "docs/reference/mcp-tools.md and the tools disagree"
        );
    }
}
