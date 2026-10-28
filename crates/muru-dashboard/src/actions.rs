//! Signing in to the daemon's dashboard and its mutating actions (design
//! specification section 10.7). Every page under the daemon needs a
//! session. A person signs in with a single-use link that `muructl
//! dashboard login` asks the daemon for over the admin socket, so whoever
//! may use the socket may look, and act when the link was asked for with
//! actions; the link gives the browser a session cookie, and every action
//! is a POST carrying the session's own token, from a page of the same
//! origin. A script is given a session's id to send as a bearer token.

use std::collections::HashMap;
use std::io::Read as _;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long a login link may wait to be opened.
pub const CODE_LIFETIME: Duration = Duration::from_secs(300);

/// How long a session lasts once signed in.
const SESSION_LIFETIME: Duration = Duration::from_secs(12 * 3600);

/// The cookie that carries the session.
pub const SESSION_COOKIE: &str = "muru_dashboard";

/// Something a person may do from the dashboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Turn an application's maintenance mode on or off (section 4.12).
    Maintenance {
        /// The application.
        app: String,
        /// On or off.
        on: bool,
    },
    /// Move an application's traffic to a release now (section 9.2).
    Promote {
        /// The application.
        app: String,
        /// The release.
        release: String,
    },
    /// Restore traffic to an application's previous release.
    Rollback {
        /// The application.
        app: String,
    },
    /// Take a release out of traffic and stop it.
    Drain {
        /// The application.
        app: String,
        /// The release.
        release: String,
    },
    /// Pin a worker group's count, or let the autoscaler decide again
    /// (section 7.4).
    Scale {
        /// The application.
        app: String,
        /// The release.
        release: String,
        /// The worker group.
        group: String,
        /// The count to pin, or none to unpin.
        processes: Option<u32>,
    },
    /// Replace one worker with a fresh one (section 7.6), at once with
    /// `force`.
    Restart {
        /// The application.
        app: String,
        /// The release.
        release: String,
        /// The worker's index.
        worker: u32,
        /// Kill it rather than let it finish.
        force: bool,
    },
}

impl Action {
    /// The action a form names, from its path's last segment and its
    /// fields, or why the form is not one.
    pub fn from_form(name: &str, fields: &HashMap<String, String>) -> Result<Self, String> {
        let field = |key: &str| fields.get(key).cloned().unwrap_or_default();
        let named = |key: &str| -> Result<String, String> {
            let value = field(key);

            if value.is_empty() {
                return Err(format!("the form needs a {key}"));
            }

            Ok(value)
        };

        Ok(match name {
            "maintenance" => Self::Maintenance {
                app: named("app")?,
                on: field("state") == "on",
            },
            "promote" => Self::Promote {
                app: named("app")?,
                release: named("release")?,
            },
            "rollback" => Self::Rollback { app: named("app")? },
            "drain" => Self::Drain {
                app: named("app")?,
                release: named("release")?,
            },
            "scale" => Self::Scale {
                app: named("app")?,
                release: named("release")?,
                group: named("group")?,
                processes: if field("unpin") == "on" {
                    None
                } else {
                    Some(
                        named("processes")?
                            .parse()
                            .map_err(|_| "processes must be a whole number".to_owned())?,
                    )
                },
            },
            "restart" => Self::Restart {
                app: named("app")?,
                release: named("release")?,
                worker: named("worker")?
                    .parse()
                    .map_err(|_| "worker must be an index".to_owned())?,
                force: field("force") == "on",
            },
            _ => return Err(format!("there is no action {name}")),
        })
    }
}

/// Carries out an action for a session, named by a short label for the
/// audit trail, and says what happened or why not.
pub type ActionSink =
    Arc<dyn Fn(Action, String) -> crate::BoxFuture<Result<String, String>> + Send + Sync>;

/// A signed-in browser or script.
#[derive(Debug, Clone)]
pub struct Session {
    /// The token its forms carry.
    pub csrf: String,
    /// A short label for the audit trail: the start of the session's id.
    pub label: String,
    /// Whether it may act, or only look.
    pub actions: bool,
    expires: Instant,
}

/// Login codes and sessions, shared between the admin API, which issues
/// the codes, and the dashboard, which redeems them.
#[derive(Debug, Default)]
pub struct Sessions {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    /// Each code's expiry and whether its session may act.
    codes: HashMap<String, (Instant, bool)>,
    sessions: HashMap<String, Session>,
}

impl Sessions {
    /// No codes and no sessions.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// A single-use login code, good for five minutes, for a session
    /// that may act when `actions` is true.
    pub fn issue_code(&self, actions: bool) -> String {
        let code = random_token();
        let mut inner = self.inner.lock().expect("sessions poisoned");
        let now = Instant::now();
        inner.codes.retain(|_, (expires, _)| *expires > now);
        inner
            .codes
            .insert(code.clone(), (now + CODE_LIFETIME, actions));
        code
    }

    /// Redeem a login code for a new session's id, once.
    pub fn redeem(&self, code: &str) -> Option<(String, Session)> {
        let mut inner = self.inner.lock().expect("sessions poisoned");
        let now = Instant::now();

        match inner.codes.remove(code) {
            Some((expires, actions)) if expires > now => Some(inner.open(actions, now)),
            _ => None,
        }
    }

    /// A new session at once, for a script to name as a bearer token.
    pub fn issue_token(&self, actions: bool) -> String {
        let mut inner = self.inner.lock().expect("sessions poisoned");
        inner.open(actions, Instant::now()).0
    }

    /// The live session a cookie value names.
    pub fn session(&self, id: &str) -> Option<Session> {
        let inner = self.inner.lock().expect("sessions poisoned");
        inner
            .sessions
            .get(id)
            .filter(|session| session.expires > Instant::now())
            .cloned()
    }
}

impl Inner {
    fn open(&mut self, actions: bool, now: Instant) -> (String, Session) {
        self.sessions.retain(|_, session| session.expires > now);
        let id = random_token();
        let session = Session {
            csrf: random_token(),
            label: id[..8].to_owned(),
            actions,
            expires: now + SESSION_LIFETIME,
        };
        self.sessions.insert(id.clone(), session.clone());
        (id, session)
    }
}

/// Whether two tokens are equal, compared in time that does not depend
/// on where they differ.
pub fn same_token(given: &str, expected: &str) -> bool {
    given.len() == expected.len()
        && given
            .bytes()
            .zip(expected.bytes())
            .fold(0u8, |difference, (a, b)| difference | (a ^ b))
            == 0
}

/// 32 random bytes from the kernel, as hex.
fn random_token() -> String {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut bytes))
        .expect("the kernel's random source is readable");
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The fields of a form body, decoded.
pub fn form_fields(body: &[u8]) -> HashMap<String, String> {
    let decode = |text: &str| -> String {
        let text = text.replace('+', " ");
        let bytes = text.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut index = 0;

        while index < bytes.len() {
            if bytes[index] == b'%'
                && let Some(hex) = text.get(index + 1..index + 3)
                && let Ok(byte) = u8::from_str_radix(hex, 16)
            {
                out.push(byte);
                index += 3;
                continue;
            }

            out.push(bytes[index]);
            index += 1;
        }

        String::from_utf8_lossy(&out).into_owned()
    };

    String::from_utf8_lossy(body)
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| (decode(key), decode(value)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_code_signs_in_once_and_the_session_holds_its_token() {
        let sessions = Sessions::new();
        let code = sessions.issue_code(true);
        assert_eq!(code.len(), 64);

        let (id, session) = sessions.redeem(&code).expect("a fresh code");
        assert!(sessions.redeem(&code).is_none(), "codes are single-use");
        assert!(sessions.redeem("made-up").is_none());
        assert_eq!(sessions.session(&id).unwrap().csrf, session.csrf);
        assert!(session.actions);
        assert!(sessions.session("made-up").is_none());
        assert!(same_token(&session.csrf, &session.csrf.clone()));
        assert!(!same_token(&session.csrf, &code));
    }

    #[test]
    fn a_session_may_only_look_unless_asked_for_with_actions() {
        let sessions = Sessions::new();
        let code = sessions.issue_code(false);
        let (_, session) = sessions.redeem(&code).expect("a fresh code");
        assert!(!session.actions);

        let token = sessions.issue_token(false);
        assert!(!sessions.session(&token).expect("a live session").actions);
        assert!(
            sessions
                .session(&sessions.issue_token(true))
                .unwrap()
                .actions
        );
    }

    #[test]
    fn forms_name_their_actions() {
        let fields = |text: &[u8]| form_fields(text);
        assert_eq!(
            Action::from_form("promote", &fields(b"app=shop&release=r2")),
            Ok(Action::Promote {
                app: "shop".to_owned(),
                release: "r2".to_owned()
            })
        );
        assert_eq!(
            Action::from_form(
                "scale",
                &fields(b"app=shop&release=r1&group=default&processes=3")
            ),
            Ok(Action::Scale {
                app: "shop".to_owned(),
                release: "r1".to_owned(),
                group: "default".to_owned(),
                processes: Some(3),
            })
        );
        assert_eq!(
            Action::from_form(
                "scale",
                &fields(b"app=shop&release=r1&group=default&unpin=on")
            ),
            Ok(Action::Scale {
                app: "shop".to_owned(),
                release: "r1".to_owned(),
                group: "default".to_owned(),
                processes: None,
            })
        );
        assert_eq!(
            Action::from_form("restart", &fields(b"app=shop&release=r1&worker=1&force=on")),
            Ok(Action::Restart {
                app: "shop".to_owned(),
                release: "r1".to_owned(),
                worker: 1,
                force: true,
            })
        );
        assert_eq!(
            Action::from_form("restart", &fields(b"app=shop&release=r1&worker=x")),
            Err("worker must be an index".to_owned())
        );
        assert_eq!(
            Action::from_form("drain", &fields(b"app=shop")),
            Err("the form needs a release".to_owned())
        );
        assert_eq!(
            Action::from_form("deploy", &fields(b"app=shop")),
            Err("there is no action deploy".to_owned())
        );
    }

    #[test]
    fn form_bodies_decode() {
        let fields = form_fields(b"app=shop&state=on&note=a+b%2Fc");
        assert_eq!(fields["app"], "shop");
        assert_eq!(fields["state"], "on");
        assert_eq!(fields["note"], "a b/c");
    }
}
