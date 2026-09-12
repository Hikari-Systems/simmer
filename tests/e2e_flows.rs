//! T2 end-to-end flows through the container (docs/TESTING.md; the test
//! programme's step 3): the content cases `docs/STATE.md` §6 records as never
//! having crossed a real relay, §7.3 with a real mail server on the other end,
//! and the control plane over its real socket while mail flows.
//!
//! It runs on the matrix stack. `jane@flows.matrix.test` selects
//! `test/config/simmer.matrix.yaml`'s `warming-flows` route, which delivers
//! straight into the matrix trap — so what a test reads is Simmer's output with
//! nothing in between — and falls through to `mailpit-direct`.
//!
//! ```sh
//! docker compose -f docker-compose.yml -f test/compose/acceptance.yml \
//!   -f test/compose/matrix.yml --profile acceptance --profile matrix up -d --build
//! cargo test --test e2e_flows -- --ignored --test-threads=1
//! ```

mod compose;

use std::collections::BTreeMap;

use serde_json::json;

use compose::admin;
use compose::loadgen::{self, Reply};
use compose::mail::{header, received};
use compose::stack::MATRIX;
use compose::traps;

const FLOWS_FROM: &str = "jane@flows.matrix.test";
const WARMING: &str = "warming-flows";
/// The link after `warming-flows`, and the default chain's only route.
const NEXT_LINK: &str = "mailpit-direct";

// ---------------------------------------------------------------------------
// content through the relay
// ---------------------------------------------------------------------------

#[test]
#[ignore = "needs the matrix compose profile"]
fn a_20_mb_message_is_rewritten_across_the_spill_threshold() {
    // §8.1 spills a message above 1 MiB to tmpfs, and when any part matches, §6.4
    // copies the whole body — the path STATE.md §6 records as never driven. How
    // much memory many of these take is stress S4's question; this is whether one
    // comes out right.
    let _logs = MATRIX.logs_on_failure();
    fresh();

    assert_all_250(&send("spill", &["--size", "20m"]));
    assert_eq!(traps::MATRIX.wait_for_count(1), 1);
    let raw = traps::MATRIX.raw_messages().remove(0);
    assert_eq!(header(&raw, "X-Simmer-Route").as_deref(), Some(WARMING));
    assert!(
        raw.len() > 20_000_000,
        "the message arrived short: {} bytes",
        raw.len()
    );
    assert!(
        raw.contains("https://newbrand.com/track"),
        "the link was not rewritten"
    );
    assert!(
        !raw.contains("https://oldbrand.com/"),
        "the original link survived"
    );
}

#[test]
#[ignore = "needs the matrix compose profile"]
fn single_byte_charsets_are_rewritten_in_their_own_bytes() {
    // D-044 and D-045: ISO-8859-1 and Windows-1252 are decoded, rewritten and
    // written back in their own charset and transfer encoding — never converted
    // to UTF-8 — so the accented bytes the server receives are the bytes sent.
    let _logs = MATRIX.logs_on_failure();
    fresh();

    for (flag, charset, marker) in [
        ("latin1", "iso-8859-1", &b"Caf\xe9 cr\xe8me, na\xefve"[..]),
        (
            "cp1252",
            "windows-1252",
            &b"Caf\xe9 cr\xe8me \x97 na\xefve, \x805"[..],
        ),
    ] {
        traps::MATRIX.reset();
        assert_all_250(&send(flag, &["--charset", flag]));
        assert_eq!(traps::MATRIX.wait_for_count(1), 1);
        let raw = traps::MATRIX.raw_bytes().remove(0);
        let text = String::from_utf8_lossy(&raw);

        assert!(
            contains(&raw, marker),
            "{charset}: the 8-bit bytes did not survive:\n{text}"
        );
        assert!(
            text.contains("https://newbrand.com/track"),
            "{charset}: not rewritten"
        );
        assert!(
            !text.contains("https://oldbrand.com/"),
            "{charset}: the original link survived"
        );
        assert_eq!(
            header(&text, "Content-Type"),
            Some(format!("text/plain; charset={charset}")),
            "{charset}: the charset changed"
        );
        assert_eq!(
            header(&text, "Content-Transfer-Encoding").as_deref(),
            Some("8bit"),
            "{charset}: the transfer encoding changed"
        );
    }
}

#[test]
#[ignore = "needs the matrix compose profile"]
fn a_part_the_engine_cannot_read_is_relayed_untouched_and_counted() {
    // §6.4 through the relay: a text part in a charset this build does not
    // implement (D-044) is never decoded, so it keeps its bytes — the matching
    // link included — and `simmer_body_rewrite_skipped_total` counts it. Until
    // now that counter was only ever driven by calling it directly.
    let _logs = MATRIX.logs_on_failure();
    fresh();
    let series = format!(
        r#"simmer_body_rewrite_skipped_total{{route="{WARMING}",reason="unsupported_charset"}}"#
    );
    let before = admin::metric(&series);

    // "Привет" in KOI8-R, then the link a UTF-8 part would have had rewritten.
    let line: &[u8] = b"\xf0\xd2\xc9\xd7\xc5\xd4 https://oldbrand.com/track";
    let mut message = b"From: Jane <jane@flows.matrix.test>\n\
        To: koi8-0@example.net\n\
        Subject: KOI8-R\n\
        Message-ID: <koi8@flows.matrix.test>\n\
        MIME-Version: 1.0\n\
        Content-Type: text/plain; charset=koi8-r\n\
        Content-Transfer-Encoding: 8bit\n\
        \n"
    .to_vec();
    message.extend_from_slice(line);
    message.push(b'\n');

    assert_all_250(&send_raw("koi8", &message));
    assert_eq!(traps::MATRIX.wait_for_count(1), 1);
    let raw = traps::MATRIX.raw_bytes().remove(0);
    assert!(
        contains(&raw, line),
        "the unreadable part was altered:\n{}",
        String::from_utf8_lossy(&raw)
    );
    assert_eq!(admin::metric(&series) - before, 1.0);
}

#[test]
#[ignore = "needs the matrix compose profile"]
fn a_received_line_long_enough_to_fold_is_folded_losslessly() {
    // RFC 5322's 998-octet limit on Simmer's own trace header (§6.1 step 8). An
    // EHLO name long enough to push the line past it — well inside the 4096-byte
    // command line — must fold at a space, and unfolding must give back the
    // header Simmer meant to write.
    let _logs = MATRIX.logs_on_failure();
    fresh();
    // Fifteen 60-octet labels: about 1,040 octets of Received: once Simmer adds
    // the peer, its own name, the id and the date. Fourteen came to ~985 and
    // never needed folding, which is what the guard below is for.
    let helo = format!("{}.fold.test", vec!["a".repeat(60); 15].join("."));

    assert_all_250(&send("fold", &["--helo", &helo]));
    assert_eq!(traps::MATRIX.wait_for_count(1), 1);
    let raw = traps::MATRIX.raw_messages().remove(0);

    let ours = received(&raw)
        .into_iter()
        .find(|l| l.contains("by simmer.acceptance"))
        .unwrap_or_else(|| panic!("no Received: from Simmer:\n{raw}"));
    // Without this the test passes vacuously on a line that fits.
    assert!(
        ours.len() > 998,
        "the EHLO name is too short to force a fold: Simmer's Received: is {} octets \
         unfolded",
        ours.len()
    );
    assert!(
        ours.starts_with(&format!("Received: from {helo} (")),
        "{ours}"
    );
    assert!(
        ours.contains(") by simmer.acceptance with ESMTPA id "),
        "{ours}"
    );

    let head: Vec<&str> = raw
        .split("\r\n\r\n")
        .next()
        .expect("a header block")
        .split("\r\n")
        .collect();

    assert!(
        head.iter().all(|l| l.len() <= 998),
        "a header line exceeds 998 octets"
    );
    let at = head
        .iter()
        .position(|l| l.starts_with(&format!("Received: from {helo} ")))
        .unwrap_or_else(|| panic!("no Received: naming the EHLO:\n{raw}"));
    assert!(
        head[at + 1].starts_with(' '),
        "Simmer's Received: was not folded:\n{}",
        head[at]
    );
}

// ---------------------------------------------------------------------------
// §7.3 recipient frequency
// ---------------------------------------------------------------------------

#[test]
#[ignore = "needs the matrix compose profile"]
fn recipient_frequency_steers_the_third_message_and_counts_per_address() {
    // warming-flows allows two messages per address per rolling day. The third
    // to the same address steers to the next link — steering, not refusal — and
    // a different address still goes to warming. STATE.md §6: §7.3 had never met
    // a real mail server.
    let _logs = MATRIX.logs_on_failure();
    fresh();

    for _ in 0..3 {
        assert_all_250(&send("freq", &[]));
    }
    assert_all_250(&send("other", &[]));
    assert_eq!(traps::MATRIX.wait_for_count(4), 4);

    let mut routes: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (return_path, raw) in traps::MATRIX.envelopes() {
        let to = header(&raw, "To").expect("To:");
        routes.entry(to).or_default().push(return_path);
    }
    for list in routes.values_mut() {
        list.sort();
    }
    let warming = format!("bounce@{WARMING}.out.test");
    let next = format!("bounce@{NEXT_LINK}.out.test");
    assert_eq!(
        routes["freq-0@example.net"],
        [next.clone(), warming.clone(), warming.clone()],
        "two to warming, the third steered"
    );
    assert_eq!(routes["other-0@example.net"], [warming]);
}

// ---------------------------------------------------------------------------
// the control plane under traffic (§9.3, §9.4)
// ---------------------------------------------------------------------------

#[test]
#[ignore = "needs the matrix compose profile"]
fn the_control_plane_steers_live_traffic_and_says_what_it_empties() {
    let _logs = MATRIX.logs_on_failure();
    fresh();
    // Route state and overrides are rows; a failed assertion must not leave
    // mailpit-direct paused under every later test.
    let _restore = RestoreRouteState;
    let mut delivered = 0;

    // The dry run and the next real message agree.
    assert_eq!(dryrun_selects(FLOWS_FROM, "ctl-a-0@example.net"), WARMING);
    assert_eq!(deliver(FLOWS_FROM, "ctl-a", &mut delivered), WARMING);

    // An override of 0 steers the next message; nothing is emptied, because
    // mailpit-direct still takes the chain.
    let (status, response) = admin::post(
        &MATRIX,
        &format!("/routes/{WARMING}/allowance"),
        &json!({ "domain_group": "catchall", "allowance": 0 }),
    );
    assert_eq!(status, 200, "{response}");
    assert_eq!(response["warnings"], json!([]), "{response}");
    assert_eq!(dryrun_selects(FLOWS_FROM, "ctl-b-0@example.net"), NEXT_LINK);
    assert_eq!(deliver(FLOWS_FROM, "ctl-b", &mut delivered), NEXT_LINK);

    // Clearing it (an explicit null) gives warming back.
    let (status, response) = admin::post(
        &MATRIX,
        &format!("/routes/{WARMING}/allowance"),
        &json!({ "domain_group": "catchall", "allowance": null }),
    );
    assert_eq!(status, 200, "{response}");
    assert_eq!(dryrun_selects(FLOWS_FROM, "ctl-c-0@example.net"), WARMING);
    assert_eq!(deliver(FLOWS_FROM, "ctl-c", &mut delivered), WARMING);

    // Pausing the default chain's only route empties it, and the response says so
    // (D-057)...
    let (status, response) =
        admin::post(&MATRIX, &format!("/routes/{NEXT_LINK}/pause"), &json!({}));
    assert_eq!(status, 200, "{response}");
    let warnings: Vec<String> = serde_json::from_value(response["warnings"].clone()).unwrap();
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("default_chain") && w.contains("451")),
        "pausing the default chain's only route should say it emptied it: {warnings:?}"
    );
    // ...a message on that chain is §10.3's 451 — never a 5xx, never delivered...
    let refused = send_from("jane@mailpit.matrix.test", "ctl-d", &[]);
    assert!(
        refused.iter().all(|r| r.code == 451),
        "a paused chain should answer 451: {refused:?}"
    );
    // ...and the flows chain, whose first link is untouched, carries on.
    assert_eq!(deliver(FLOWS_FROM, "ctl-e", &mut delivered), WARMING);

    // Resuming clears the warning, and the default chain delivers again.
    let (status, response) =
        admin::post(&MATRIX, &format!("/routes/{NEXT_LINK}/resume"), &json!({}));
    assert_eq!(status, 200, "{response}");
    assert_eq!(response["warnings"], json!([]), "{response}");
    assert_eq!(
        deliver("jane@mailpit.matrix.test", "ctl-f", &mut delivered),
        NEXT_LINK
    );

    // Graduation: accepted for a warming route, and undone the same way; refused
    // for an overflow route, which has no schedule to graduate to.
    let (status, response) =
        admin::post(&MATRIX, &format!("/routes/{WARMING}/graduate"), &json!({}));
    assert_eq!(status, 200, "{response}");
    assert_eq!(response["graduated"], json!(true), "{response}");
    let (status, response) = admin::post(
        &MATRIX,
        &format!("/routes/{NEXT_LINK}/graduate"),
        &json!({}),
    );
    assert_eq!(status, 400, "{response}");
    let (status, response) = admin::post(
        &MATRIX,
        &format!("/routes/{WARMING}/graduate"),
        &json!({ "graduated": false }),
    );
    assert_eq!(status, 200, "{response}");

    // Every applied mutation is an audit line in app's own log, and the emptied
    // chain is a WARN of its own.
    let log =
        String::from_utf8_lossy(&MATRIX.run(&["logs", "--no-color", "app"]).stdout).to_string();
    for (action, route) in [
        ("allowance", WARMING),
        ("pause", NEXT_LINK),
        ("resume", NEXT_LINK),
        ("graduate", WARMING),
    ] {
        assert!(
            log.lines().any(|l| l.contains("admin mutation applied")
                && l.contains(&format!("\"{action}\""))
                && l.contains(route)),
            "no audit line for {action} on {route}"
        );
    }
    assert!(
        log.lines()
            .any(|l| l.contains("WARN") && l.contains("has no eligible route")),
        "the emptied chain was not logged as a warning"
    );
}

// ---------------------------------------------------------------------------
// driving the flows
// ---------------------------------------------------------------------------

/// A clean slate: no quota rows, no route state, no frequency events, an empty
/// trap. Frequency events outlive a test run by design, so without this a rerun
/// sees the previous run's messages to `freq-0` and steers the first one.
fn fresh() {
    MATRIX.reset_quota();
    MATRIX.psql("truncate recipient_event;");
    traps::MATRIX.reset();
}

/// Puts route state back when the test that changed it ends — including when it
/// fails. Never panics: this runs during unwinding.
struct RestoreRouteState;

impl Drop for RestoreRouteState {
    fn drop(&mut self) {
        let _ = MATRIX
            .compose()
            .args([
                "exec",
                "-T",
                "simmer-db",
                "psql",
                "-U",
                "simmer",
                "-d",
                "simmer",
            ])
            .args([
                "-q",
                "-c",
                "truncate quota_usage, quota_reservation, route_state;",
            ])
            .output();
    }
}

fn send(tag: &str, extra: &[&str]) -> Vec<Reply> {
    send_from(FLOWS_FROM, tag, extra)
}

fn send_from(from: &str, tag: &str, extra: &[&str]) -> Vec<Reply> {
    let from_header = format!("Jane <{from}>");
    let mut args = vec![
        "--count",
        "1",
        "--tag",
        tag,
        "--from",
        from,
        "--from-header",
        &from_header,
    ];
    args.extend_from_slice(extra);
    loadgen::run(&MATRIX, &args)
}

/// Send `message` verbatim, to `<tag>-0@example.net`.
fn send_raw(tag: &str, message: &[u8]) -> Vec<Reply> {
    loadgen::run_with_stdin(
        &MATRIX,
        &[
            "--count",
            "1",
            "--tag",
            tag,
            "--from",
            FLOWS_FROM,
            "--raw-stdin",
        ],
        Some(message),
    )
}

/// Send one message and return the route that delivered it. `delivered` is the
/// trap's running total, so the wait knows what to wait for.
fn deliver(from: &str, tag: &str, delivered: &mut usize) -> String {
    assert_all_250(&send_from(from, tag, &[]));
    *delivered += 1;
    assert_eq!(traps::MATRIX.wait_for_count(*delivered), *delivered);
    let to = format!("{tag}-0@example.net");
    traps::MATRIX
        .raw_messages()
        .iter()
        .find(|raw| header(raw, "To").as_deref() == Some(to.as_str()))
        .and_then(|raw| header(raw, "X-Simmer-Route"))
        .unwrap_or_else(|| panic!("{to} did not arrive"))
}

/// The route §9.4's dry run would select for one message from `from`.
fn dryrun_selects(from: &str, recipient: &str) -> String {
    let (status, response) = admin::post(
        &MATRIX,
        "/dryrun",
        &json!({
            "envelope_from": from,
            "from_header": format!("Jane <{from}>"),
            "recipients": [recipient],
        }),
    );
    assert_eq!(status, 200, "{response}");
    response["recipients"][0]["selected"]
        .as_str()
        .unwrap_or_else(|| panic!("the dry run selected nothing: {response}"))
        .to_string()
}

fn assert_all_250(replies: &[Reply]) {
    assert!(
        !replies.is_empty() && replies.iter().all(|r| r.code == 250),
        "{replies:?}"
    );
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}
