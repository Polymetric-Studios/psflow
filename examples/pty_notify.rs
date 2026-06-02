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
    // Optional first arg: how many seconds to wait for a human to answer via the
    // remote-control URL (default 20). Use a large value for a live human test.
    let wait_secs: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);

    let mut opts = SessionOptions::default()
        .with_arg("--permission-mode")
        .with_arg("default");
    opts.turn_timeout_ms = (wait_secs as u128) * 1000;

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

    println!("{SCOPE} running write-tool prompt (forces a dialog)… waiting up to {wait_secs}s for a human");
    let body = "Use the Write tool to create the file /tmp/psflow_notify_probe.txt with contents: ok. Do it now.";
    match session.run_turn(body) {
        Ok(turn) => {
            let created = std::path::Path::new("/tmp/psflow_notify_probe.txt").exists();
            println!("{SCOPE} COMPLETED — dialog was answered remotely. result={:?}", turn.result);
            println!("{SCOPE} file_created={created} (true => the human's 'Yes' really took effect)");
        }
        Err(e) => println!("{SCOPE} not answered in time: {e}"),
    }
    let _ = std::fs::remove_file("/tmp/psflow_notify_probe.txt");
    Ok(())
}
