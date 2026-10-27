//! Section 7.4: worker groups scaled to zero. A request for a group that
//! has no workers, when the snapshot says it can be woken, waits for a
//! pool with workers instead of being refused, and the daemon is told so
//! it starts one.

use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{broadcast, watch};

use crate::link::LinkPool;

/// A group of a release: application, release and group.
pub type Key = (String, String, String);

/// How often a group's waiting requests tell the daemon, at most.
const NOTICE_INTERVAL: Duration = Duration::from_millis(500);

/// The pools waiting requests are handed, and the notices for the daemon.
pub struct Wakes {
    pools: Mutex<HashMap<Key, watch::Sender<Option<Arc<LinkPool>>>>>,
    notices: broadcast::Sender<Key>,
    told: Mutex<HashMap<Key, Instant>>,
}

static WAKES: LazyLock<Wakes> = LazyLock::new(|| Wakes {
    pools: Mutex::new(HashMap::new()),
    notices: broadcast::channel(256).0,
    told: Mutex::new(HashMap::new()),
});

/// The edge's one registry.
pub fn wakes() -> &'static Wakes {
    &WAKES
}

impl Wakes {
    /// The pools of the tables just applied: one with workers is handed
    /// to the requests waiting for its group, and one without takes back
    /// whatever was handed before. Groups no longer in the tables are
    /// forgotten.
    pub fn publish(&self, pools: &[(Key, Arc<LinkPool>)]) {
        let mut senders = self.pools.lock().expect("wakes poisoned");
        let present: HashSet<&Key> = pools.iter().map(|(key, _)| key).collect();
        senders.retain(|key, _| present.contains(key));
        self.told
            .lock()
            .expect("wakes poisoned")
            .retain(|key, _| present.contains(key));

        for (key, pool) in pools {
            let value = (!pool.is_empty()).then(|| Arc::clone(pool));
            senders
                .entry(key.clone())
                .or_insert_with(|| watch::channel(None).0)
                .send_replace(value);
        }
    }

    /// Wait for a pool with workers for the group, telling the daemon.
    pub(crate) async fn wait(&self, key: &Key) -> io::Result<Arc<LinkPool>> {
        let mut receiver = self
            .pools
            .lock()
            .expect("wakes poisoned")
            .entry(key.clone())
            .or_insert_with(|| watch::channel(None).0)
            .subscribe();
        self.tell(key);

        let pool = receiver
            .wait_for(Option::is_some)
            .await
            .map_err(|_| io::Error::other("the group was removed while waking"))?;
        Ok(Arc::clone(pool.as_ref().expect("waited for a pool")))
    }

    /// Tell the daemon a group is wanted, at most once a notice interval.
    fn tell(&self, key: &Key) {
        let now = Instant::now();
        let mut told = self.told.lock().expect("wakes poisoned");

        if told
            .get(key)
            .is_some_and(|at| now.saturating_duration_since(*at) < NOTICE_INTERVAL)
        {
            return;
        }

        told.insert(key.clone(), now);
        let _ = self.notices.send(key.clone());
    }

    /// The groups requests are waiting for, as they wait.
    pub fn subscribe(&self) -> broadcast::Receiver<Key> {
        self.notices.subscribe()
    }
}
