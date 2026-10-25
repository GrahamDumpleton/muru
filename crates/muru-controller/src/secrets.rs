//! Secret stores (design specification section 10.8): a release's
//! environment refers to a secret by store and key, and the daemon
//! resolves the value when the release starts, from a directory of files
//! readable only by the daemon, from the daemon's own environment, or
//! from a command. Values are held only as long as needed and never
//! recorded, exported, logged or passed on a command line.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// Where a store's values come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// A directory of files, one per key.
    File(PathBuf),
    /// Variables of the daemon's own environment.
    Env,
    /// A command that prints the value for a key, with `{key}` in its
    /// arguments replaced by the key.
    Exec {
        /// The command and its arguments.
        command: Vec<String>,
        /// How long it may take.
        timeout: Duration,
    },
}

/// A named store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Store {
    /// The store's name, as references use it.
    pub name: String,
    /// Where its values come from.
    pub kind: Kind,
}

impl Store {
    /// The stores a daemon's `[secrets.store.NAME]` tables declare; a
    /// file store without a path lives under `secrets_dir`.
    pub fn from_document(
        document: &muru_config::Document,
        secrets_dir: &Path,
    ) -> anyhow::Result<Vec<Self>> {
        let Some(secrets) = &document.secrets else {
            return Ok(Vec::new());
        };
        let mut stores = Vec::new();

        for (name, table) in &secrets.store {
            anyhow::ensure!(
                crate::model::valid_name(name),
                "[secrets.store.{name}]: not a valid store name"
            );

            let kind = match table.kind.as_str() {
                "file" => Kind::File(
                    table
                        .path
                        .as_ref()
                        .map(PathBuf::from)
                        .unwrap_or_else(|| secrets_dir.join(name)),
                ),
                "env" => Kind::Env,
                "exec" => {
                    anyhow::ensure!(
                        !table.command.is_empty(),
                        "[secrets.store.{name}]: an exec store needs a command"
                    );
                    Kind::Exec {
                        command: table.command.clone(),
                        timeout: match &table.timeout {
                            Some(text) => muru_config::parse_duration(text).map_err(|error| {
                                anyhow::anyhow!("[secrets.store.{name}] timeout: {error}")
                            })?,
                            None => Duration::from_secs(5),
                        },
                    }
                }
                other => anyhow::bail!(
                    "[secrets.store.{name}]: kind must be file, env or exec, not {other:?}"
                ),
            };

            stores.push(Self {
                name: name.clone(),
                kind,
            });
        }

        Ok(stores)
    }

    /// The value for a key, or why there is none; the message never
    /// contains a value.
    pub async fn resolve(&self, key: &str) -> Result<String, String> {
        match &self.kind {
            Kind::File(directory) => {
                if key.is_empty() || key.contains('/') || key.starts_with('.') {
                    return Err(format!("secret {}/{key}: not a valid key", self.name));
                }

                match tokio::fs::read_to_string(directory.join(key)).await {
                    Ok(text) => Ok(text.trim_end_matches(['\n', '\r']).to_owned()),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(format!(
                        "secret {}/{key}: not found in {}",
                        self.name,
                        directory.display()
                    )),
                    Err(error) => Err(format!("secret {}/{key}: {error}", self.name)),
                }
            }
            Kind::Env => std::env::var(key).map_err(|_| {
                format!(
                    "secret {}/{key}: not in the daemon's environment",
                    self.name
                )
            }),
            Kind::Exec { command, timeout } => {
                let arguments: Vec<String> = command
                    .iter()
                    .skip(1)
                    .map(|argument| argument.replace("{key}", key))
                    .collect();
                let output = tokio::time::timeout(
                    *timeout,
                    tokio::process::Command::new(&command[0])
                        .args(&arguments)
                        .stdin(std::process::Stdio::null())
                        .output(),
                )
                .await
                .map_err(|_| {
                    format!(
                        "secret {}/{key}: the command did not finish in {} s",
                        self.name,
                        timeout.as_secs()
                    )
                })?
                .map_err(|error| {
                    format!(
                        "secret {}/{key}: the command could not run: {error}",
                        self.name
                    )
                })?;

                if !output.status.success() {
                    return Err(format!(
                        "secret {}/{key}: the command failed with {}",
                        self.name, output.status
                    ));
                }

                Ok(String::from_utf8_lossy(&output.stdout)
                    .trim_end_matches(['\n', '\r'])
                    .to_owned())
            }
        }
    }
}

/// Write a secret's value to a file only its owner can read, for
/// delivery as a file (section 10.8).
pub fn write_private(path: &Path, value: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        std::fs::set_permissions(parent, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
    }

    let _ = std::fs::remove_file(path);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o400)
        .open(path)?;
    file.write_all(value.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn file_and_exec_stores_resolve_and_say_why_not() {
        let dir = std::env::temp_dir().join(format!("muru-secrets-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("db"), "hunter2\n").unwrap();
        let file = Store {
            name: "local".to_owned(),
            kind: Kind::File(dir.clone()),
        };
        assert_eq!(file.resolve("db").await.unwrap(), "hunter2");
        let missing = file.resolve("other").await.unwrap_err();
        assert!(
            missing.starts_with("secret local/other: not found"),
            "{missing}"
        );
        assert!(file.resolve("../etc").await.is_err());

        let exec = Store {
            name: "cmd".to_owned(),
            kind: Kind::Exec {
                command: vec![
                    "sh".to_owned(),
                    "-c".to_owned(),
                    "echo value-{key}".to_owned(),
                ],
                timeout: Duration::from_secs(5),
            },
        };
        assert_eq!(exec.resolve("api").await.unwrap(), "value-api");
        let failing = Store {
            name: "cmd".to_owned(),
            kind: Kind::Exec {
                command: vec!["sh".to_owned(), "-c".to_owned(), "exit 3".to_owned()],
                timeout: Duration::from_secs(5),
            },
        };
        assert!(failing.resolve("api").await.unwrap_err().contains("failed"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
