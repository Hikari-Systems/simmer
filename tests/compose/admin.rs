//! The admin API (§9) from outside, on the port `docker-compose.yml` publishes.

use std::process::Command;
use std::sync::OnceLock;

use super::stack::Stack;

/// Where `app`'s admin API answers: the port `docker-compose.yml` publishes on the
/// host's loopback, or `SIMMER_TEST_ADMIN` from somewhere that cannot reach it —
/// a container or a jail on the stack's network, as `http://simmer-app-1:8080`.
pub fn base() -> String {
    std::env::var("SIMMER_TEST_ADMIN").unwrap_or_else(|_| "http://127.0.0.1:8080".to_string())
}

/// The token `app` is running with, read from the container rather than
/// repeated here.
pub fn token(stack: &Stack) -> String {
    static TOKEN: OnceLock<String> = OnceLock::new();
    TOKEN
        .get_or_init(|| {
            let out = stack.run(&["exec", "-T", "app", "printenv", "SIMMER_ADMIN_TOKEN"]);
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        })
        .clone()
}

/// `GET path`, failing on anything but a 2xx.
pub fn get(stack: &Stack, path: &str) -> serde_json::Value {
    let (status, body) = request(stack, "GET", path, None);
    assert!((200..300).contains(&status), "GET {path}: {status} {body}");
    body
}

/// `POST path` with a JSON body: the status and the parsed response. An error
/// response is an answer worth asserting on, so it is returned, not panicked on.
pub fn post(stack: &Stack, path: &str, body: &serde_json::Value) -> (u16, serde_json::Value) {
    request(stack, "POST", path, Some(body))
}

/// One series' value from `/metrics`, or 0 when it has not been written yet.
/// `series` is the name with its labels exactly as exported.
pub fn metric(series: &str) -> f64 {
    let out = Command::new("curl")
        .args(["-sf", &format!("{}/metrics", base())])
        .output()
        .expect("curl");
    assert!(out.status.success(), "GET /metrics failed");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix(series)?.strip_prefix(' '))
        .map(|v| v.trim().parse().expect("a metric value"))
        .unwrap_or(0.0)
}

fn request(
    stack: &Stack,
    method: &str,
    path: &str,
    body: Option<&serde_json::Value>,
) -> (u16, serde_json::Value) {
    let mut cmd = Command::new("curl");
    cmd.args(["-s", "-X", method, "-w", "\n%{http_code}"])
        .args(["-H", &format!("Authorization: Bearer {}", token(stack))]);
    if let Some(body) = body {
        cmd.args(["-H", "Content-Type: application/json"])
            .args(["--data-binary", &body.to_string()]);
    }
    let out = cmd.arg(format!("{}{path}", base())).output().expect("curl");
    assert!(out.status.success(), "{method} {path}: curl failed");

    let text = String::from_utf8_lossy(&out.stdout);
    let (body, status) = text
        .trim_end()
        .rsplit_once('\n')
        .unwrap_or(("", text.trim()));
    let status: u16 = status.parse().expect("an HTTP status");
    let body = if body.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_str(body).unwrap_or_else(|e| panic!("{method} {path}: {e}\n{body}"))
    };
    (status, body)
}
