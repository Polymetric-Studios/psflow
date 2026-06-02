//! Verify dialog *routing*: force an approval dialog, route it through the
//! notifier (carrying the prompt + the session's remote-control URL), and
//! `Defer` the decision. With no one answering, the turn should wait and then
//! time out — proving the dialog was surfaced and NOT auto-answered.
//!
//! (A human opening the printed remote-control URL would answer it, the dialog
//! would clear, and the turn would complete — that last step needs a human.)
//!
//! Run: cargo run --example pty_notify --features terminal

use psflow::adapter::{ApprovalChoice, ApprovalPolicy, ClaudeTerminalSession, SessionOptions};
use std::sync::Arc;

const SCOPE: &str = "[PTY-NOTIFY][pty_notify]";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut opts = SessionOptions::default()
        .with_arg("--permission-mode")
        .with_arg("default");
    opts.turn_timeout_ms = 20_000; // bound the wait — no human will answer here

    println!("{SCOPE} spawning (permission-mode=default)…");
    let mut session = ClaudeTerminalSession::spawn_ready(opts)?;
    session.set_approval_policy(ApprovalPolicy::custom(|_| ApprovalChoice::Defer));
    session.set_approval_notifier(Arc::new(|prompt, url| {
        println!(
            "{SCOPE} ROUTED dialog: question={:?} options={:?}",
            prompt.question, prompt.options
        );
        println!("{SCOPE} -> answer at: {}", url.unwrap_or("(none on screen)"));
    }));

    println!("{SCOPE} running write-tool prompt (forces a dialog)…");
    let body = "Use the Write tool to create the file /tmp/psflow_notify_probe.txt with contents: ok. Do it now.";
    match session.run_turn(body) {
        Ok(turn) => println!("{SCOPE} UNEXPECTED completion: {:?}", turn.result),
        Err(e) => println!("{SCOPE} waited then: {e} (expected: timed out, dialog left for a human)"),
    }
    let _ = std::fs::remove_file("/tmp/psflow_notify_probe.txt");
    Ok(())
}
