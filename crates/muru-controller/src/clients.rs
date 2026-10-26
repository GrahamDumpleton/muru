//! Client classes and rate limits for the edge (design specification
//! sections 4.14 and 4.15), from the daemon's own configuration file and,
//! after its, from applied files. A class's list files are read relative
//! to the daemon file's directory and read again when they change; a
//! limit that starts refusing requests is an event.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use muru_config::{ClientClass, RateLimit};
use muru_proto::edge::v1 as pb;

use crate::store::Store;

/// How long a limit must have refused nothing before refusing again is a
/// new ratelimit.enforcing event.
const QUIET_FOR: Duration = Duration::from_secs(60);

/// The daemon file's classes and limits, with the directory its list
/// files are relative to.
#[derive(Debug, Clone, Default)]
pub struct FileRules {
    /// The `[[client_class]]` rules, in order.
    pub classes: Vec<ClientClass>,
    /// The `[rate_limit.NAME]` tables.
    pub limits: BTreeMap<String, RateLimit>,
    /// The daemon file's directory.
    pub directory: PathBuf,
}

impl FileRules {
    /// The names the file defines, for checking what applied files name.
    pub fn defined(&self) -> crate::config::Defined {
        crate::config::Defined {
            rate_limits: self.limits.keys().cloned().collect(),
            client_classes: self
                .classes
                .iter()
                .map(|class| class.name.clone())
                .collect(),
        }
    }

    /// The classes and limits for the edge: the file's, then the applied
    /// files' from `store`, a name the file defines staying the file's.
    /// An error is the file's own; when the applied ones cannot be read,
    /// they are left out with a warning.
    pub fn policies(&self, store: &Store) -> Result<ClientPolicies, String> {
        let alone = ClientPolicies::resolve(&self.classes, &self.limits, &self.directory)?;
        let applied = store.client_classes().unwrap_or_default();
        let applied_limits = store.rate_limits().unwrap_or_default();

        if applied.is_empty() && applied_limits.is_empty() {
            return Ok(alone);
        }

        let mut classes = self.classes.clone();
        let mut limits = self.limits.clone();

        for (_, class) in applied {
            if classes.iter().any(|defined| defined.name == class.name) {
                tracing::warn!(class = %class.name, "the daemon's file defines an applied client class too; its rule stands");
            } else {
                classes.push(class);
            }
        }

        for (name, limit) in applied_limits {
            match limits.entry(name) {
                Entry::Occupied(entry) => {
                    tracing::warn!(limit = %entry.key(), "the daemon's file defines an applied rate limit too; its table stands");
                }
                Entry::Vacant(entry) => {
                    entry.insert(limit);
                }
            }
        }

        match ClientPolicies::resolve(&classes, &limits, &self.directory) {
            Ok(policies) => Ok(policies),
            Err(error) => {
                tracing::warn!(%error, "the applied client classes and rate limits are left out");
                Ok(alone)
            }
        }
    }
}

/// The classes and limits as the edge gets them, with the list files
/// they were read from.
#[derive(Debug, Default)]
pub struct ClientPolicies {
    classes: Vec<ClientClass>,
    directory: PathBuf,
    /// The classification rules for the snapshot.
    pub classes_message: Vec<pb::ClientClass>,
    /// The rate limits for the snapshot.
    pub limits_message: Vec<pb::RateLimit>,
    lists: Vec<(PathBuf, Option<SystemTime>)>,
}

impl ClientPolicies {
    /// Check the classes and limits and read the classes' list files,
    /// relative to `directory`.
    pub fn resolve(
        classes: &[ClientClass],
        limits: &BTreeMap<String, RateLimit>,
        directory: &Path,
    ) -> Result<Self, String> {
        let mut limits_message = Vec::new();

        for (name, limit) in limits {
            let settings = muru_config::check_rate_limit(name, limit)?;
            limits_message.push(pb::RateLimit {
                name: name.clone(),
                classes: settings.classes,
                key: settings.key,
                per_second: settings.per_second,
                burst: settings.burst,
                action: settings.action,
                max_delay_ms: settings.max_delay.as_millis() as u64,
                ipv4_prefix: u32::from(settings.ipv4_prefix),
                ipv6_prefix: u32::from(settings.ipv6_prefix),
                headers: settings.headers,
            });
        }

        let mut policies = Self {
            classes: classes.to_vec(),
            directory: directory.to_path_buf(),
            classes_message: Vec::new(),
            limits_message,
            lists: Vec::new(),
        };
        policies.read_classes()?;
        Ok(policies)
    }

    /// Read the list files again when one has changed since they were
    /// read; the changed files when any had.
    pub fn refresh(&mut self) -> Result<Vec<PathBuf>, String> {
        let changed: Vec<PathBuf> = self
            .lists
            .iter()
            .filter(|(path, modified)| modified_time(path) != *modified)
            .map(|(path, _)| path.clone())
            .collect();

        if !changed.is_empty() {
            self.read_classes()?;
        }

        Ok(changed)
    }

    fn read_classes(&mut self) -> Result<(), String> {
        let mut messages = Vec::new();
        let mut lists = Vec::new();

        for class in &self.classes {
            muru_config::check_client_class(class)?;
            let mut user_agents = class.user_agent.clone();
            let mut addresses = class.addresses.clone();

            for (list, into) in [
                (&class.user_agent_list, &mut user_agents),
                (&class.address_list, &mut addresses),
            ] {
                if let Some(list) = list {
                    let path = self.directory.join(list);
                    lists.push((path.clone(), modified_time(&path)));
                    into.extend(read_list(&path).map_err(|error| {
                        format!("client_class {}: {}: {error}", class.name, path.display())
                    })?);
                }
            }

            messages.push(pb::ClientClass {
                name: class.name.clone(),
                user_agents,
                user_agent_regexes: class.user_agent_regex.clone(),
                addresses,
                headers: class
                    .headers
                    .iter()
                    .map(|(name, pattern)| (name.to_ascii_lowercase(), pattern.clone()))
                    .collect(),
                cookies: class.cookies.clone().into_iter().collect(),
                verify_domains: class.verify_domains.clone(),
            });
        }

        // The edge's own parsing checks the addresses and expressions.

        muru_edge::classify::Classifier::from_messages(&messages)?;
        self.classes_message = messages;
        self.lists = lists;
        Ok(())
    }
}

fn modified_time(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
}

/// A list file's entries: one a line, blank lines and lines starting
/// with # left out.
fn read_list(path: &Path) -> std::io::Result<Vec<String>> {
    Ok(std::fs::read_to_string(path)?
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect())
}

/// Which limits are refusing or holding requests, from the edge's
/// counters, so that a limit starting to do so is an event.
#[derive(Debug, Default)]
pub struct Enforcement {
    seen: HashMap<String, (f64, Option<Instant>)>,
}

impl Enforcement {
    /// The limits that began refusing or delaying requests since the
    /// last look, after a quiet minute or more, with how many they have
    /// refused or delayed since.
    pub fn observe(
        &mut self,
        samples: &[muru_edge::metrics::Sample],
        now: Instant,
    ) -> Vec<(String, f64)> {
        let mut totals: HashMap<String, f64> = HashMap::new();

        for sample in samples {
            if sample.name != "muru_rate_limited_total" {
                continue;
            }

            let label = |name: &str| {
                sample
                    .labels
                    .iter()
                    .find(|(key, _)| key == name)
                    .map(|(_, value)| value.clone())
                    .unwrap_or_default()
            };

            if label("action") != "logged" {
                *totals.entry(label("limit")).or_default() += sample.value;
            }
        }

        let mut began = Vec::new();

        for (limit, total) in totals {
            let (previous, last) = self.seen.get(&limit).copied().unwrap_or((0.0, None));

            if total > previous {
                if last.is_none_or(|last| now.saturating_duration_since(last) >= QUIET_FOR) {
                    began.push((limit.clone(), total - previous));
                }

                self.seen.insert(limit, (total, Some(now)));
            } else {
                self.seen.insert(limit, (total, last));
            }
        }

        began.sort_by(|a, b| a.0.cmp(&b.0));
        began
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_files_are_read_and_read_again_when_they_change() {
        let directory = tempfile::tempdir().unwrap();
        let list = directory.path().join("ai.txt");
        std::fs::write(&list, "# AI crawlers\nGPTBot\n\n  ClaudeBot  \n").unwrap();
        let classes = vec![ClientClass {
            name: "ai_crawler".to_owned(),
            user_agent: vec!["CCBot".to_owned()],
            user_agent_list: Some("ai.txt".to_owned()),
            ..ClientClass::default()
        }];
        let mut policies =
            ClientPolicies::resolve(&classes, &BTreeMap::new(), directory.path()).unwrap();
        assert_eq!(
            policies.classes_message[0].user_agents,
            vec!["CCBot", "GPTBot", "ClaudeBot"]
        );
        assert!(policies.refresh().unwrap().is_empty());

        // A modification time that differs is a change, whichever way.

        std::fs::write(&list, "Bytespider\n").unwrap();
        let earlier = SystemTime::now() - Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(&list)
            .unwrap()
            .set_modified(earlier)
            .unwrap();
        assert_eq!(policies.refresh().unwrap(), vec![list]);
        assert_eq!(
            policies.classes_message[0].user_agents,
            vec!["CCBot", "Bytespider"]
        );
    }

    #[test]
    fn a_missing_list_or_bad_address_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let missing = vec![ClientClass {
            name: "partner".to_owned(),
            address_list: Some("partners.txt".to_owned()),
            ..ClientClass::default()
        }];
        let error =
            ClientPolicies::resolve(&missing, &BTreeMap::new(), directory.path()).unwrap_err();
        assert!(error.contains("partners.txt"), "{error}");

        let bad = vec![ClientClass {
            name: "partner".to_owned(),
            addresses: vec!["300.1.1.1/8".to_owned()],
            ..ClientClass::default()
        }];
        assert!(ClientPolicies::resolve(&bad, &BTreeMap::new(), directory.path()).is_err());
    }

    #[test]
    fn a_limit_beginning_to_refuse_is_noticed_once_until_it_has_been_quiet() {
        let sample = |action: &str, value: f64| muru_edge::metrics::Sample {
            name: "muru_rate_limited_total".to_owned(),
            labels: vec![
                ("limit".to_owned(), "crawlers".to_owned()),
                ("client_class".to_owned(), "ai_crawler".to_owned()),
                ("action".to_owned(), action.to_owned()),
            ],
            value,
        };
        let mut enforcement = Enforcement::default();
        let start = Instant::now();

        assert!(
            enforcement
                .observe(&[sample("logged", 5.0)], start)
                .is_empty()
        );
        assert_eq!(
            enforcement.observe(&[sample("rejected", 3.0)], start),
            vec![("crawlers".to_owned(), 3.0)]
        );
        assert!(
            enforcement
                .observe(&[sample("rejected", 9.0)], start + Duration::from_secs(1))
                .is_empty()
        );
        assert!(
            enforcement
                .observe(&[sample("rejected", 9.0)], start + Duration::from_secs(120))
                .is_empty()
        );
        assert_eq!(
            enforcement.observe(
                &[sample("rejected", 10.0)],
                start + Duration::from_secs(121)
            ),
            vec![("crawlers".to_owned(), 1.0)]
        );
    }
}
