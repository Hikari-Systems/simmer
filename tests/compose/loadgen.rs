//! Running the loadgen (`src/bin/loadgen.rs`) inside a stack's network.

use super::stack::Stack;

/// One transaction's final reply, as the loadgen recorded it. A transport failure
/// is code 0 with the error as text.
#[derive(Debug, serde::Deserialize)]
pub struct Reply {
    pub recipient: String,
    pub code: u16,
    pub text: String,
}

/// Run the loadgen against `app:25` — `extra` may name another port, since the
/// last `--port` wins — and parse the JSON replies it prints.
pub fn run(stack: &Stack, extra: &[&str]) -> Vec<Reply> {
    let mut cmd = stack.compose();
    // `--no-deps`: the loadgen declares `depends_on: app`, and resolving that
    // dependency is enough to make compose reconcile `app` against a freshly
    // rendered config. The stack is already up and healthy; nothing here needs
    // compose to check again.
    cmd.args(["run", "--rm", "--no-deps", "--no-TTY", "loadgen"])
        .args(["--host", "app", "--port", "25"])
        .args(extra);

    let out = cmd.output().expect("docker compose run loadgen");
    assert!(
        out.status.success(),
        "loadgen failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    let json = stdout
        .lines()
        .find(|l| l.starts_with('['))
        .unwrap_or_else(|| panic!("no JSON in loadgen output:\n{stdout}"));
    serde_json::from_str(json).unwrap_or_else(|e| panic!("loadgen JSON: {e}\n{json}"))
}
