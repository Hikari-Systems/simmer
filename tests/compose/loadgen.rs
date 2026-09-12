//! Running the loadgen (`src/bin/loadgen.rs`) inside a stack's network.

use std::io::Write;
use std::process::{Output, Stdio};

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
    run_with_stdin(stack, extra, None)
}

/// [`run`], with `stdin` piped to the loadgen — for `--raw-stdin`.
pub fn run_with_stdin(stack: &Stack, extra: &[&str], stdin: Option<&[u8]>) -> Vec<Reply> {
    let mut cmd = stack.compose();
    // `--no-deps`: the loadgen declares `depends_on: app`, and resolving that
    // dependency is enough to make compose reconcile `app` against a freshly
    // rendered config. The stack is already up and healthy; nothing here needs
    // compose to check again.
    cmd.args(["run", "--rm", "--no-deps", "--no-TTY", "loadgen"])
        .args(["--host", "app", "--port", "25"])
        .args(extra);

    let out: Output = match stdin {
        None => cmd.output().expect("docker compose run loadgen"),
        Some(bytes) => {
            let mut child = cmd
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("docker compose run loadgen");
            child
                .stdin
                .take()
                .expect("stdin")
                .write_all(bytes)
                .expect("write to the loadgen");
            child
                .wait_with_output()
                .expect("docker compose run loadgen")
        }
    };
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
