//! The dashboard's mutating actions (design specification section 10.7),
//! off unless the daemon's configuration enables them. A person signs in
//! with a single-use link that `muructl dashboard login` asks the daemon
//! for over the admin socket, so whoever may use the socket may act; the
//! link gives the browser a session cookie, and every action is a POST
//! carrying the session's own token, from a page of the same origin.

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
}

/// Carries out an action for a session, named by a short label for the
/// audit trail, and says what happened or why not.
pub type ActionSink =
    Arc<dyn Fn(Action, String) -> crate::BoxFuture<Result<String, String>> + Send + Sync>;

/// A signed-in browser.
#[derive(Debug, Clone)]
pub struct Session {
    /// The token its forms carry.
    pub csrf: String,
    /// A short label for the audit trail: the start of the session's id.
    pub label: String,
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
    codes: HashMap<String, Instant>,
    sessions: HashMap<String, Session>,
}

impl Sessions {
    /// No codes and no sessions.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// A single-use login code, good for five minutes.
    pub fn issue_code(&self) -> String {
        let code = random_token();
        let mut inner = self.inner.lock().expect("sessions poisoned");
        let now = Instant::now();
        inner.codes.retain(|_, expires| *expires > now);
        inner.codes.insert(code.clone(), now + CODE_LIFETIME);
        code
    }

    /// Redeem a login code for a new session's id, once.
    pub fn redeem(&self, code: &str) -> Option<(String, Session)> {
        let mut inner = self.inner.lock().expect("sessions poisoned");
        let now = Instant::now();

        if inner
            .codes
            .remove(code)
            .is_none_or(|expires| expires <= now)
        {
            return None;
        }

        inner.sessions.retain(|_, session| session.expires > now);
        let id = random_token();
        let session = Session {
            csrf: random_token(),
            label: id[..8].to_owned(),
            expires: now + SESSION_LIFETIME,
        };
        inner.sessions.insert(id.clone(), session.clone());
        Some((id, session))
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
        let code = sessions.issue_code();
        assert_eq!(code.len(), 64);

        let (id, session) = sessions.redeem(&code).expect("a fresh code");
        assert!(sessions.redeem(&code).is_none(), "codes are single-use");
        assert!(sessions.redeem("made-up").is_none());
        assert_eq!(sessions.session(&id).unwrap().csrf, session.csrf);
        assert!(sessions.session("made-up").is_none());
        assert!(same_token(&session.csrf, &session.csrf.clone()));
        assert!(!same_token(&session.csrf, &code));
    }

    #[test]
    fn form_bodies_decode() {
        let fields = form_fields(b"app=shop&state=on&note=a+b%2Fc");
        assert_eq!(fields["app"], "shop");
        assert_eq!(fields["state"], "on");
        assert_eq!(fields["note"], "a b/c");
    }
}
