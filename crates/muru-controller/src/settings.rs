//! What the daemon takes from its configuration file (design specification
//! section 10.3 and appendix C), built the same way when the daemon
//! starts and when `muructl daemon reload` or SIGHUP reads the file
//! again (section 13.3), so the two never read it differently. The daemon
//! diffs a fresh `NodeSettings` against what it runs with and applies the
//! difference.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use muru_proto::edge::v1 as pb;

/// Settings a command line gives in place of the file's: `muructl daemon
/// run --listen`, `--metrics`, `--dashboard` and `--user`. They hold
/// across reloads, so a daemon started on `--listen` keeps that listener
/// whatever the file says.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Overrides {
    /// Serve plain HTTP on this address alone.
    pub listen: Option<SocketAddr>,
    /// Serve metrics here.
    pub metrics: Option<SocketAddr>,
    /// Serve the dashboard here.
    pub dashboard: Option<SocketAddr>,
    /// The service user.
    pub user: Option<String>,
}

/// The daemon's settings from its file, checked, with the overrides laid
/// over them.
#[derive(Debug, Clone)]
pub struct NodeSettings {
    /// The listeners, each with whether it carries TLS; the first is the
    /// main one.
    pub listeners: Vec<(SocketAddr, bool)>,
    /// The `[limits]` table with the defaults for the rest (section 4.6).
    pub limits: muru_edge::Limits,
    /// Section 6.13: the listeners that limit cleartext HTTP/2 to some
    /// clients, with those clients.
    pub h2c: Vec<pb::H2cPolicy>,
    /// Where the Prometheus exposition is served, or nowhere.
    pub metrics: Option<SocketAddr>,
    /// Where the dashboard and JSON gateway are served, or nowhere.
    pub dashboard: Option<SocketAddr>,
    /// Whether a dashboard session may be given the actions.
    pub dashboard_actions: bool,
    /// The webhooks events are delivered to (section 11.5).
    pub sinks: Vec<crate::sinks::Webhook>,
    /// The overlays' signed webhook (section 9.10), or none.
    pub overlay_webhook: Option<crate::triggers::Webhook>,
    /// The secret stores (section 10.8).
    pub secrets: Vec<crate::secrets::Store>,
    /// Log rotation (section 11.3), or none for external rotation.
    pub rotation: Option<crate::rotation::Policy>,
    /// Section 13.3: what SIGTERM means, and how a shutdown runs.
    pub shutdown: crate::shutdown::Policy,
    /// The service user named by the `[daemon]` table or `--user`
    /// (ADR 0026); `muru` when none is named.
    pub service_user: Option<String>,
    /// Whether `muructl debug exec` may run code in workers (section
    /// 11.7).
    pub debug_exec: bool,
    /// The node's budget (section 7.4).
    pub budget: crate::budget::Budget,
    /// The `[tools]` table (section 8.4): uv and interpreter installs.
    pub tools: muru_config::Tools,
    /// The `[proxy]` table as the edge takes it (section 4.7).
    pub proxy: Option<pb::Proxy>,
    /// The `[compression]` table as the edge takes it (section 4.17).
    pub compression: pb::Compression,
    /// The node-wide `[error_pages]` table as the edge takes it (section
    /// 4.12).
    pub error_pages: Vec<pb::ErrorPage>,
    /// The node-wide `[error_override]` table as the edge takes it.
    pub error_override: Option<pb::ErrorOverride>,
    /// The file's client classes and rate limits (sections 4.14 and
    /// 4.15), with the directory their list files are relative to.
    pub client_rules: crate::clients::FileRules,
}

impl NodeSettings {
    /// Read and check the file at `path`, which need not exist, with the
    /// overrides laid over it. A file store without a path lives under
    /// `secrets_dir`.
    pub fn read(path: &Path, secrets_dir: &Path, overrides: &Overrides) -> Result<Self, String> {
        let document = match std::fs::read_to_string(path) {
            Ok(text) => muru_config::Document::parse(&text)
                .map_err(|error| format!("{}: {error}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                muru_config::Document::default()
            }
            Err(error) => return Err(format!("reading {}: {error}", path.display())),
        };

        Self::from_document(&document, path, secrets_dir, overrides)
    }

    /// The settings of a parsed file, checked, with the overrides laid
    /// over it.
    pub fn from_document(
        document: &muru_config::Document,
        path: &Path,
        secrets_dir: &Path,
        overrides: &Overrides,
    ) -> Result<Self, String> {
        let listeners = match overrides.listen {
            Some(listen) => vec![(listen, false)],
            None => listeners_from(document)?,
        };

        let proxy = document.proxy.as_ref().map(crate::daemon::proxy_message);

        // A [proxy] table the edge would refuse is refused here.

        if let Some(proxy) = &proxy {
            muru_edge::proxy::Trust::from_snapshot(proxy)
                .map_err(|error| format!("[proxy]: {error}"))?;
        }

        let compression = crate::daemon::compression_message(document.compression.as_ref())
            .map_err(|error| format!("[compression]: {error}"))?;
        let (error_pages, error_override) = crate::daemon::error_tables_message(
            &document.error_pages,
            document.error_override.as_ref(),
        )?;
        let daemon = document.daemon.as_ref();
        let budget =
            crate::budget::Budget::from_table(daemon.and_then(|table| table.budget.as_ref()))
                .map_err(|error| format!("[daemon.budget]: {error}"))?;
        let secrets = crate::secrets::Store::from_document(document, secrets_dir)
            .map_err(|error| format!("{error:#}"))?;
        let rotation = crate::rotation::Policy::from_table(document.logs.as_ref())
            .map_err(|error| format!("{error:#}"))?;
        let shutdown = crate::shutdown::Policy::from_table(
            document.shutdown.as_ref(),
            crate::shutdown::is_main_process(),
        )
        .map_err(|error| format!("[shutdown]: {error}"))?;
        let overlay_webhook = document
            .overlay_webhook
            .as_ref()
            .map(crate::triggers::Webhook::from_config)
            .transpose()
            .map_err(|error| format!("[overlay_webhook]: {error}"))?;

        Ok(Self {
            listeners,
            limits: limits_from(document)?,
            h2c: h2c_from(document)?,
            metrics: metrics_from(document, overrides.metrics)?,
            dashboard: dashboard_from(document, overrides.dashboard)?,
            dashboard_actions: document
                .dashboard
                .as_ref()
                .is_some_and(|dashboard| dashboard.actions),
            sinks: sinks_from(document)?,
            overlay_webhook,
            secrets,
            rotation,
            shutdown,
            service_user: overrides
                .user
                .clone()
                .or_else(|| daemon.and_then(|table| table.user.clone())),
            debug_exec: daemon.and_then(|table| table.debug_exec).unwrap_or(false),
            budget,
            tools: document.tools.clone().unwrap_or_default(),
            proxy,
            compression,
            error_pages,
            error_override,
            client_rules: crate::clients::FileRules {
                classes: document.client_class.clone(),
                limits: document.rate_limit.clone(),
                directory: config_directory(path),
            },
        })
    }
}

/// The directory the daemon's file is in, which its list files are
/// relative to.
pub fn config_directory(path: &Path) -> PathBuf {
    path.parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

/// The listeners of the file, or plain HTTP on 127.0.0.1:8080.
fn listeners_from(document: &muru_config::Document) -> Result<Vec<(SocketAddr, bool)>, String> {
    if document.listener.is_empty() {
        return Ok(vec![(
            "127.0.0.1:8080".parse().expect("a literal address"),
            false,
        )]);
    }

    let mut listeners = Vec::new();

    for listener in &document.listener {
        let address = listener
            .address
            .parse()
            .map_err(|_| format!("listener address {:?} is not IP:PORT", listener.address))?;
        listeners.push((address, listener.tls));
    }

    Ok(listeners)
}

/// The metrics address: the override, else the [metrics] table's listen
/// address, else none.
fn metrics_from(
    document: &muru_config::Document,
    flag: Option<SocketAddr>,
) -> Result<Option<SocketAddr>, String> {
    if flag.is_some() {
        return Ok(flag);
    }

    let listen = document
        .metrics
        .as_ref()
        .and_then(|metrics| metrics.listen.as_deref());

    match listen {
        Some(address) => Ok(Some(address.parse().map_err(|_| {
            format!("[metrics] listen = {address:?} is not an address such as 127.0.0.1:9464")
        })?)),
        None => Ok(None),
    }
}

/// The dashboard address: the override, else the [dashboard] table's
/// listen address, else none; loopback only, as section 10.7 has it.
fn dashboard_from(
    document: &muru_config::Document,
    flag: Option<SocketAddr>,
) -> Result<Option<SocketAddr>, String> {
    let address = match flag {
        Some(address) => Some(address),
        None => document
            .dashboard
            .as_ref()
            .and_then(|dashboard| dashboard.listen.as_deref())
            .map(|address| {
                address.parse::<SocketAddr>().map_err(|_| {
                    format!(
                        "[dashboard] listen = {address:?} is not an address such as 127.0.0.1:8090"
                    )
                })
            })
            .transpose()?,
    };

    if let Some(address) = address
        && !address.ip().is_loopback()
    {
        return Err(format!(
            "the dashboard is only served on loopback addresses, not {address}"
        ));
    }

    Ok(address)
}

/// The event sinks of the file, checked, with their secrets read.
fn sinks_from(document: &muru_config::Document) -> Result<Vec<crate::sinks::Webhook>, String> {
    let mut sinks: Vec<crate::sinks::Webhook> = Vec::new();

    for sink in document.events.iter().flat_map(|events| &events.sink) {
        let webhook = crate::sinks::Webhook::from_config(sink)
            .map_err(|error| format!("[[events.sink]]: {error}"))?;

        if sinks.iter().any(|seen| seen.name == webhook.name) {
            return Err(format!(
                "[[events.sink]]: two sinks are named {}",
                webhook.name
            ));
        }

        sinks.push(webhook);
    }

    Ok(sinks)
}

/// Section 6.13: the listeners that limit cleartext HTTP/2, checked.
fn h2c_from(document: &muru_config::Document) -> Result<Vec<pb::H2cPolicy>, String> {
    let mut policies = Vec::new();

    for listener in &document.listener {
        let Some(allowed) = &listener.h2c else {
            continue;
        };

        if listener.tls {
            return Err(format!(
                "listener {}: h2c is for a listener without TLS, where HTTP/2 is cleartext",
                listener.address
            ));
        }

        muru_edge::proxy::Networks::parse(allowed)
            .map_err(|error| format!("listener {}: h2c: {error}", listener.address))?;
        policies.push(pb::H2cPolicy {
            listener: listener.address.clone(),
            allowed: allowed.clone(),
        });
    }

    Ok(policies)
}

/// The edge limits from the [limits] table, with the defaults for the
/// rest.
fn limits_from(document: &muru_config::Document) -> Result<muru_edge::Limits, String> {
    let mut limits = muru_edge::Limits::default();
    let Some(table) = &document.limits else {
        return Ok(limits);
    };

    if let Some(size) = &table.max_body_size {
        limits.max_body_size = muru_config::parse_size(size)?;
    }

    if let Some(size) = &table.max_header_bytes {
        limits.max_header_bytes = usize::try_from(muru_config::parse_size(size)?)
            .map_err(|_| format!("[limits] max_header_bytes = {size:?} is too large"))?;
    }

    if let Some(count) = table.max_headers {
        limits.max_headers = count;
    }

    if let Some(count) = table.max_connections {
        if count == 0 {
            return Err("[limits] max_connections must be at least 1".to_owned());
        }

        limits.max_connections = count;
    }

    if let Some(count) = table.max_connections_per_client {
        limits.max_connections_per_client = count;
    }

    if let Some(count) = table.max_requests_per_connection {
        limits.max_requests_per_connection = count;
    }

    if let Some(size) = &table.spool_quota {
        limits.spool_quota = muru_config::parse_size(size)?;
    }

    if let Some(count) = table
        .http2
        .as_ref()
        .and_then(|http2| http2.max_concurrent_streams)
    {
        if count == 0 {
            return Err("[limits.http2] max_concurrent_streams must be at least 1".to_owned());
        }

        limits.http2_max_concurrent_streams = count;
    }

    for (value, field) in [
        (&table.header_read_timeout, &mut limits.header_read_timeout),
        (&table.queue_timeout, &mut limits.queue_timeout),
        (&table.request_deadline, &mut limits.request_deadline),
        (&table.body_idle_timeout, &mut limits.body_idle_timeout),
        (
            &table.response_idle_timeout,
            &mut limits.response_idle_timeout,
        ),
        (&table.keep_alive_timeout, &mut limits.keep_alive_timeout),
        (
            &table.keep_alive_timeout_proxy,
            &mut limits.keep_alive_timeout_proxy,
        ),
    ] {
        if let Some(text) = value {
            *field = muru_config::parse_duration(text)?;
        }
    }

    Ok(limits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(text: &str) -> Result<NodeSettings, String> {
        let document = muru_config::Document::parse(text).expect("a parsable document");
        NodeSettings::from_document(
            &document,
            Path::new("/etc/muru/muru.toml"),
            Path::new("/var/lib/muru/secrets"),
            &Overrides::default(),
        )
    }

    #[test]
    fn an_empty_file_gives_the_defaults() {
        let settings = settings("").unwrap();
        assert_eq!(
            settings.listeners,
            vec![("127.0.0.1:8080".parse().unwrap(), false)]
        );
        assert_eq!(settings.limits.max_body_size, 10 * 1024 * 1024);
        assert!(settings.sinks.is_empty());
        assert!(settings.secrets.is_empty());
        assert!(settings.metrics.is_none());
        assert!(!settings.dashboard_actions);
        assert_eq!(settings.client_rules.directory, PathBuf::from("/etc/muru"));
    }

    #[test]
    fn the_tables_are_read_and_checked() {
        let settings = settings(
            "[[listener]]\naddress = '0.0.0.0:80'\n[[listener]]\naddress = '0.0.0.0:443'\ntls = true\n\
             [limits]\nmax_body_size = '1MB'\nqueue_timeout = '2s'\n\
             [dashboard]\nlisten = '127.0.0.1:8090'\nactions = true\n",
        )
        .unwrap();
        assert_eq!(settings.listeners.len(), 2);
        assert!(settings.listeners[1].1);
        assert_eq!(settings.limits.max_body_size, 1024 * 1024);
        assert_eq!(settings.limits.queue_timeout.as_secs(), 2);
        assert_eq!(settings.dashboard, Some("127.0.0.1:8090".parse().unwrap()));
        assert!(settings.dashboard_actions);
    }

    #[test]
    fn overrides_win_over_the_file() {
        let document = muru_config::Document::parse(
            "[[listener]]\naddress = '0.0.0.0:80'\n[daemon]\nuser = 'web'\n",
        )
        .unwrap();
        let settings = NodeSettings::from_document(
            &document,
            Path::new("muru.toml"),
            Path::new("secrets"),
            &Overrides {
                listen: Some("127.0.0.1:9000".parse().unwrap()),
                user: Some("svc".to_owned()),
                ..Overrides::default()
            },
        )
        .unwrap();
        assert_eq!(
            settings.listeners,
            vec![("127.0.0.1:9000".parse().unwrap(), false)]
        );
        assert_eq!(settings.service_user.as_deref(), Some("svc"));
    }

    #[test]
    fn mistakes_are_refused_by_table() {
        let refused = |text: &str| settings(text).unwrap_err();
        assert!(refused("[[listener]]\naddress = 'eighty'\n").contains("listener address"));
        assert!(refused("[limits]\nmax_connections = 0\n").contains("[limits] max_connections"));
        assert!(refused("[dashboard]\nlisten = '0.0.0.0:8090'\n").contains("loopback"));
        assert!(
            refused("[[listener]]\naddress = '0.0.0.0:443'\ntls = true\nh2c = ['loopback']\n")
                .contains("h2c is for a listener without TLS")
        );
        assert!(
            refused(
                "[[events.sink]]\nname = 'ops'\nurl = 'http://127.0.0.1:1/'\nsecret = 'x'\n\
                 [[events.sink]]\nname = 'ops'\nurl = 'http://127.0.0.1:2/'\nsecret = 'x'\n"
            )
            .contains("two sinks are named ops")
        );
        assert!(refused("[proxy]\ntrusted = ['not an address']\n").starts_with("[proxy]"));
    }
}
