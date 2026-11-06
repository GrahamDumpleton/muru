//! Commands for the project here (design specification section 9.12):
//! what a development run, `muructl serve` and muructl on its own run a
//! declared or a one-off command with. Each takes the project's commands
//! from its file, the interpreter and runtime library the application is
//! served with, and the environment its workers have.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use clap::Args;
use muru_controller::commands::{CommandRequest, Interpreter, Prepared, Record};
use muru_controller::logs::Logs;
use muru_controller::model::CommandSpec;

/// What a command of the project runs with.
#[derive(Debug, Clone)]
pub struct Context {
    /// The application, as a run's log names it.
    pub app: String,
    /// The release that serves, as the log names it; empty for a command
    /// run on its own, where there is none.
    pub release: String,
    /// The project directory, which the command runs in.
    pub project: PathBuf,
    /// The interpreter.
    pub python: PathBuf,
    /// The runtime library's directory, when the environment's own
    /// `muru` is not used (section 8.3).
    pub pythonpath: Option<PathBuf>,
    /// The project's environment variables (section 10.8), over those
    /// the process already has.
    pub env: Vec<(String, String)>,
    /// The project settings file read in place of the one beside the
    /// code, when one is named (section 13.9).
    pub file: Option<PathBuf>,
}

/// The commands the project declares, read from its file as it is now,
/// or from `file` in its place, and checked as a release's are when it is
/// created.
pub fn declared(project: &Path, file: Option<&Path>) -> Result<Vec<CommandSpec>, String> {
    let Some((path, table)) =
        muru_config::load_settings(project, file).map_err(|error| error.to_string())?
    else {
        return Ok(Vec::new());
    };

    muru_controller::config::resolve_commands(&table.commands)
        .map_err(|error| format!("{}: {error}", path.display()))
}

/// What the project's application is called: its entrypoint's package,
/// as a development run names it, else the directory.
pub fn label(project: &Path, entrypoint: Option<&str>) -> String {
    match entrypoint {
        Some(entrypoint) => crate::app_name(entrypoint).replace('_', "-"),
        None => project.file_name().map_or_else(
            || "default".to_owned(),
            |name| name.to_string_lossy().into_owned(),
        ),
    }
}

/// The project's commands as `muructl command list` shows them, with no
/// application or release to name, for a command run on its own.
pub fn listed(
    project: &Path,
    file: Option<&Path>,
) -> Result<muru_proto::admin::v1::ListCommandsResponse, String> {
    let entrypoint = muru_config::load_settings(project, file)
        .map_err(|error| error.to_string())?
        .and_then(|(_, table)| table.entrypoint);

    Ok(muru_proto::admin::v1::ListCommandsResponse {
        app: label(project, entrypoint.as_deref()),
        release: String::new(),
        commands: declared(project, file)?
            .iter()
            .map(|command| muru_controller::commands::info(command, None))
            .collect(),
    })
}

/// What the commands of the project are called in a refusal.
fn subject(context: &Context) -> String {
    format!("the project in {}", context.project.display())
}

/// A command ready to start: the request checked against the project's
/// declarations, with the interpreter, environment and directory the
/// application is served with. A secret store is the daemon's, so an
/// input named as an entry of one is refused.
pub fn prepare(context: &Context, request: CommandRequest) -> Result<Prepared, String> {
    let declared_command = match &request {
        CommandRequest::OneOff(argv) if argv.is_empty() => {
            return Err("no command was given".to_owned());
        }
        CommandRequest::OneOff(_) => None,
        CommandRequest::Declared {
            name,
            inputs,
            secret_inputs,
        } => {
            if let Some(input) = secret_inputs.keys().next() {
                return Err(format!(
                    "the input {input} names a secret store entry, and secret stores are the \
                     daemon's; give its value on standard input with --input-stdin {input}"
                ));
            }

            if context.file.is_none()
                && !muru_config::PROJECT_FILES
                    .iter()
                    .any(|name| context.project.join(name).is_file())
            {
                return Err(format!(
                    "{} has no pyproject.toml or muru.toml to declare {name} in; to run a \
                     command of a release under the daemon, name its application first",
                    context.project.display()
                ));
            }

            let commands = declared(&context.project, context.file.as_deref())?;
            let command =
                muru_controller::commands::find(&commands, name, &subject(context))?.clone();
            let values = muru_controller::commands::input_values(&command, inputs)?;
            Some((command, inputs.clone(), values))
        }
    };

    let env = muru_controller::commands::environment(
        context.env.clone(),
        &Interpreter {
            python: &context.python,
            runtime: context.pythonpath.as_deref(),
            pycache: None,
            no_bytecode: false,
        },
        &context.app,
        &context.release,
        &context.project,
    );

    let prepared = Prepared {
        app: context.app.clone(),
        release: context.release.clone(),
        label: "exec".to_owned(),
        argv: Vec::new(),
        env,
        cwd: context.project.clone(),
        identity: None,
        log: None,
        timeout: None,
        declared: None,
        exclusive: false,
        inputs: String::new(),
        secrets: None,
    };

    match (declared_command, request) {
        (Some((command, given, values)), _) => muru_controller::commands::declared(
            prepared,
            &command,
            &given,
            &values,
            &context.python,
        ),
        (None, CommandRequest::OneOff(argv)) => Ok(Prepared {
            argv: muru_controller::commands::with_interpreter(argv, &context.python),
            ..prepared
        }),
        (None, CommandRequest::Declared { .. }) => {
            unreachable!("a declared command was found above")
        }
    }
}

/// The record of a command run through a development run or `muructl
/// serve`: its output joins the application's, on the terminal and in
/// what `muructl logs` shows, and its events are printed, as the run's
/// others are.
pub struct Printed {
    /// The run's recent output.
    pub logs: Logs,
}

impl Record for Printed {
    fn output(&self, label: &str, line: &str, stderr: bool) {
        let time = SystemTime::now();
        let text = format!("[command {label}] {line}");
        self.logs.push(time, &text);

        let stream = if stderr {
            muru_common::log::Stream::Stderr
        } else {
            muru_common::log::Stream::Stdout
        };
        muru_common::log::relayed(stream, time, &text);
    }

    fn event(&self, kind: &str, _subject: &str, _level: &'static str, message: &str) {
        muru_common::muru_println!("{kind}: {message}");
    }
}

/// Start a command of the project and stream what it writes, for the
/// admin socket of a development run or `muructl serve`.
pub fn run(
    context: &Context,
    request: CommandRequest,
    record: Arc<dyn Record>,
) -> Result<tokio::sync::mpsc::Receiver<muru_proto::admin::v1::CommandOutput>, String> {
    muru_controller::commands::run(prepare(context, request)?, record)
}

/// How a command finds the project when it runs on its own: the few
/// settings `muructl run` and `muructl serve` take for the same thing.
#[derive(Debug, Clone, Default, Args)]
pub struct ProjectArgs {
    /// The project directory, when no application is named (default:
    /// MURU_SERVE_CHDIR, else the current directory).
    #[arg(long, value_name = "DIR")]
    pub chdir: Option<PathBuf>,

    /// A project settings file used in place of the pyproject.toml or
    /// muru.toml beside the code, as muructl serve takes it (also
    /// MURU_SERVE_CONFIG).
    #[arg(long, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// The interpreter a command run on its own uses (default: as
    /// muructl serve and muructl run find it).
    #[arg(long, value_name = "PATH or VERSION")]
    pub python: Option<String>,

    /// Where the image holds the runtime library, for a command run on
    /// its own (default: /opt/muru when it holds one).
    #[arg(long, value_name = "DIR")]
    pub runtime_dir: Option<PathBuf>,
}

impl ProjectArgs {
    /// The flag given, if any, for a refusal that names it.
    pub fn given(&self) -> Option<&'static str> {
        if self.chdir.is_some() {
            Some("--chdir")
        } else if self.config.is_some() {
            Some("--config")
        } else if self.python.is_some() {
            Some("--python")
        } else if self.runtime_dir.is_some() {
            Some("--runtime-dir")
        } else {
            None
        }
    }

    /// The project directory the flags and the environment name, as a
    /// run records its own.
    pub fn project(&self) -> PathBuf {
        let project = self
            .chdir
            .clone()
            .or_else(|| std::env::var_os("MURU_SERVE_CHDIR").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("."));
        project.canonicalize().unwrap_or(project)
    }

    /// The project settings file the flag or the environment names, in
    /// place of the one beside the code.
    pub fn file(&self) -> Option<PathBuf> {
        crate::serve::settings_file(self.config.as_deref(), &|name: &str| {
            std::env::var(name).ok()
        })
    }
}

/// What a command run on its own runs with: the project, interpreter,
/// environment and runtime library found as `muructl serve` and `muructl
/// run` find them. The image's runtime directory is used when a setting
/// names it or it holds a library; otherwise the library comes from
/// where a development run takes it.
pub async fn own(args: &ProjectArgs) -> anyhow::Result<Context> {
    let settings = crate::serve::command_settings(
        args.chdir.as_deref(),
        args.config.as_deref(),
        args.python.as_deref(),
        args.runtime_dir.as_deref(),
        |name| std::env::var(name).ok(),
    )?;

    let found = muru_config::load_settings(&settings.project, settings.file.as_deref())?;
    anyhow::ensure!(
        settings.project.is_dir(),
        "{} is not a directory",
        settings.project.display()
    );

    let project = settings
        .project
        .canonicalize()
        .unwrap_or_else(|_| settings.project.clone());
    let table = found.map(|(_, table)| table).unwrap_or_default();

    let uv = muru_provision::Uv::find(None, None);
    let isolation = muru_provision::Isolation::default();

    let python = muru_provision::resolve(&muru_provision::Inputs {
        mode: muru_provision::Mode::Development,
        flag: settings.python.as_deref(),
        table: table.python.as_deref(),
        project: &project,
        previous: None,
        always: false,
        uv: uv.as_ref(),
        isolation: &isolation,
    })?;

    let image = settings.runtime_dir.clone().or_else(|| {
        let default = PathBuf::from(crate::serve::DEFAULT_RUNTIME_DIR);
        default.join("muru").is_dir().then_some(default)
    });

    // The environment's own library needs neither the image's directory
    // nor a cache, which an arbitrary user in a container may not have.

    let library = match (crate::serve::environment_library(&python), &image) {
        (Some(own), _) => own,
        (None, Some(dir)) => crate::serve::runtime_library(&python, dir)?,
        (None, None) => crate::provision::runtime(&python.probe).await?,
    };

    let env = table
        .env
        .iter()
        .map(|(name, value)| match value {
            muru_config::EnvValue::Plain(text) => Ok((name.clone(), text.clone())),
            muru_config::EnvValue::Secret(_) => Err(anyhow::anyhow!(
                "[tool.muru.env] {name}: secrets are resolved by the daemon; a command run for \
                 the project cannot deliver them"
            )),
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    Ok(Context {
        app: label(&project, table.entrypoint.as_deref()),
        release: String::new(),
        project,
        python: python.python.clone(),
        pythonpath: library.pythonpath.clone(),
        env,
        file: settings.file,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn context(project: &Path) -> Context {
        Context {
            app: "shop".to_owned(),
            release: "r3".to_owned(),
            project: project.to_path_buf(),
            python: PathBuf::from("/venv/bin/python"),
            pythonpath: Some(PathBuf::from("/opt/muru")),
            env: vec![("SETTINGS".to_owned(), "shop.settings".to_owned())],
            file: None,
        }
    }

    fn project(commands: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("a directory");
        std::fs::write(
            dir.path().join("muru.toml"),
            format!("entrypoint = 'shop:application'\n{commands}"),
        )
        .expect("the project file");
        dir
    }

    fn value<'a>(prepared: &'a Prepared, name: &str) -> Option<&'a str> {
        prepared
            .env
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    #[test]
    fn a_declared_command_runs_with_what_the_application_is_served_with() {
        let dir = project(
            "[commands.migrate]\nrun = ['python', 'manage.py', 'migrate', '{target}']\n\
             inputs.target = { default = 'head' }\ntimeout = '30m'\nexclusive = true\n",
        );
        let prepared = prepare(
            &context(dir.path()),
            CommandRequest::Declared {
                name: "migrate".to_owned(),
                inputs: BTreeMap::new(),
                secret_inputs: BTreeMap::new(),
            },
        )
        .expect("prepared");

        assert_eq!(
            prepared.argv,
            ["/venv/bin/python", "manage.py", "migrate", "head"]
        );
        assert_eq!(prepared.cwd, dir.path());
        assert_eq!(prepared.log, None);
        assert_eq!(prepared.timeout, Some(std::time::Duration::from_secs(1800)));
        assert!(prepared.exclusive);
        assert_eq!(value(&prepared, "SETTINGS"), Some("shop.settings"));
        assert_eq!(value(&prepared, "MURU_APP"), Some("shop"));
        assert_eq!(value(&prepared, "MURU_RELEASE"), Some("r3"));
        assert_eq!(value(&prepared, "MURU_COMMAND"), Some("migrate"));
        assert!(
            value(&prepared, "PYTHONPATH").is_some_and(|path| path.starts_with("/opt/muru")),
            "{:?}",
            prepared.env
        );
    }

    #[test]
    fn a_command_run_on_its_own_names_no_release() {
        let dir = project("");
        let mut context = context(dir.path());
        context.release = String::new();
        let prepared = prepare(
            &context,
            CommandRequest::OneOff(vec!["python3".to_owned(), "-V".to_owned()]),
        )
        .expect("prepared");

        assert_eq!(prepared.argv, ["/venv/bin/python", "-V"]);
        assert_eq!(value(&prepared, "MURU_RELEASE"), None);
        assert_eq!(
            value(&prepared, "MURU_RELEASE_DIR"),
            Some(dir.path().to_str().expect("a path"))
        );
    }

    #[test]
    fn what_cannot_be_run_says_why() {
        let dir = project("[commands.backup]\ncall = 'shop.ops:backup'\n");
        let run = |name: &str, secret: Option<&str>| {
            prepare(
                &context(dir.path()),
                CommandRequest::Declared {
                    name: name.to_owned(),
                    inputs: BTreeMap::new(),
                    secret_inputs: secret
                        .map(|input| (input.to_owned(), "vault/key".to_owned()))
                        .into_iter()
                        .collect(),
                },
            )
            .expect_err("refused")
        };

        let missing = run("migrate", None);
        assert!(
            missing.contains("has no command named migrate; it declares backup"),
            "{missing}"
        );
        assert!(missing.starts_with("the project in "), "{missing}");

        let secret = run("backup", Some("token"));
        assert!(secret.contains("--input-stdin token"), "{secret}");

        std::fs::write(
            dir.path().join("muru.toml"),
            "[commands.Bad]\nrun = ['x']\n",
        )
        .expect("the project file");
        let invalid = run("backup", None);
        assert!(invalid.contains("muru.toml: commands.Bad"), "{invalid}");
    }
}
