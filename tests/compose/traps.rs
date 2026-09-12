//! Reading a Mailpit trap over its HTTP API.
//!
//! Through `curl` rather than an HTTP client crate, because the test tier needs
//! nothing else from one and `curl` is on every machine that runs Docker.

use std::process::Command;
use std::time::{Duration, Instant};

/// One Mailpit instance, by the base URL of its API.
#[derive(Clone, Copy, Debug)]
pub struct Trap {
    pub base: &'static str,
}

/// The acceptance stack's two traps (`docker-compose.yml`).
pub const WARMING: Trap = Trap {
    base: "http://127.0.0.1:18025",
};
pub const OVERFLOW: Trap = Trap {
    base: "http://127.0.0.1:18026",
};

/// The T2 matrix's trap, which every Postfix variant relays into
/// (`test/compose/matrix.yml`).
pub const MATRIX: Trap = Trap {
    base: "http://127.0.0.1:18027",
};

/// Mailpit's page size here. The API caps a page, so every listing pages.
const PAGE: usize = 250;

impl Trap {
    /// Delete everything, and assert it is gone. `ACCEPTANCE.md` §6: reset
    /// between runs, or one run's assertions see the previous run's mail.
    pub fn reset(self) {
        let out = Command::new("curl")
            .args([
                "-sf",
                "-X",
                "DELETE",
                &format!("{}/api/v1/messages", self.base),
            ])
            .output()
            .expect("curl");
        assert!(out.status.success(), "failed to reset {}", self.base);
        assert_eq!(self.count(), 0, "{} did not reset", self.base);
    }

    pub fn count(self) -> usize {
        let v = json(&get(&format!("{}/api/v1/messages?limit=1", self.base)));
        v["total"].as_u64().expect("total") as usize
    }

    /// Poll until the count reaches `want` and then stops moving.
    ///
    /// **Never sleep a fixed interval** — `ACCEPTANCE.md` §6 names that as the most
    /// likely source of flakes. Waiting for the count to be *stable* rather than
    /// merely correct is what catches an off-by-one that arrives late: a run that
    /// should deliver 5 and delivers 6 would otherwise pass by being read at the
    /// right moment.
    pub fn wait_for_count(self, want: usize) -> usize {
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut last = self.count();
        let mut stable_since = Instant::now();

        loop {
            std::thread::sleep(Duration::from_millis(200));
            let now = self.count();
            if now != last {
                last = now;
                stable_since = Instant::now();
            } else if now == want && stable_since.elapsed() > Duration::from_secs(2) {
                return now;
            } else if stable_since.elapsed() > Duration::from_secs(10) {
                // Settled on the wrong number. Return it; the caller's assertion
                // says what was expected far better than a timeout message would.
                return now;
            }
            if Instant::now() > deadline {
                return now;
            }
        }
    }

    /// Every message id, paging through the whole trap — the acceptance suite's
    /// original `?limit=500` silently truncated anything larger.
    pub fn message_ids(self) -> Vec<String> {
        let mut ids = Vec::new();
        loop {
            let v = json(&get(&format!(
                "{}/api/v1/messages?start={}&limit={PAGE}",
                self.base,
                ids.len()
            )));
            let page = v["messages"].as_array().expect("messages");
            if page.is_empty() {
                return ids;
            }
            ids.extend(
                page.iter()
                    .map(|m| m["ID"].as_str().expect("ID").to_string()),
            );
            if ids.len() as u64 >= v["total"].as_u64().unwrap_or(0) {
                return ids;
            }
        }
    }

    /// Every message's raw source, exactly as the receiving server stored it.
    pub fn raw_messages(self) -> Vec<String> {
        self.message_ids()
            .iter()
            .map(|id| get(&format!("{}/api/v1/message/{id}/raw", self.base)))
            .collect()
    }

    /// The envelope sender each message arrived with, per the trap's own record —
    /// not per a header Simmer wrote.
    pub fn return_paths(self) -> Vec<String> {
        self.message_ids()
            .iter()
            .map(|id| {
                let v = json(&get(&format!("{}/api/v1/message/{id}", self.base)));
                v["ReturnPath"].as_str().unwrap_or_default().to_string()
            })
            .collect()
    }

    /// Every message as `(envelope sender, raw source)`, both read per id so the
    /// pairs cannot be mismatched by a message arriving between two listings.
    pub fn envelopes(self) -> Vec<(String, String)> {
        self.message_ids()
            .iter()
            .map(|id| {
                let v = json(&get(&format!("{}/api/v1/message/{id}", self.base)));
                let return_path = v["ReturnPath"].as_str().unwrap_or_default().to_string();
                let raw = get(&format!("{}/api/v1/message/{id}/raw", self.base));
                (return_path, raw)
            })
            .collect()
    }
}

pub fn get(url: &str) -> String {
    let out = Command::new("curl")
        .args(["-sf", url])
        .output()
        .expect("curl");
    assert!(out.status.success(), "GET {url} failed");
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn json(body: &str) -> serde_json::Value {
    serde_json::from_str(body).unwrap_or_else(|e| panic!("trap JSON: {e}\n{body}"))
}
