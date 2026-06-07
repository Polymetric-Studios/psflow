//! Phase C live check — force an approval dialog (write-tool in `default`
//! permission mode) and confirm the `AllowAll` policy auto-answers it so the
//! turn completes and the action actually happens.
//!
//! Run: cargo run --example pty_approve --features terminal

use psflow::adapter::{ApprovalPolicy, ClaudeTerminalSession, SessionOptions};

const SCOPE: &str = "[PTY-APPROVE][pty_approve]";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let target = std::env::temp_dir().join("psflow_phase_c.txt");
    let _ = std::fs::remove_file(&target);

    // `default` mode makes the Write tool prompt instead of auto-approving.
    let opts = SessionOptions::default()
        .with_arg("--permission-mode")
        .with_arg("default");
    println!("{SCOPE} spawning (permission-mode=default)…");
    let mut session = ClaudeTerminalSession::spawn_ready(opts)?;
    session.set_approval_policy(ApprovalPolicy::AllowAll);
    println!("{SCOPE} policy=AllowAll; running write-tool prompt…");

    let body = format!(
        "Use the Write tool to create the file {} with the exact contents: ok. Do it now.",
        target.display()
    );
    let turn = session.run_turn(&body)?;

    let created = target.exists();
    let contents = std::fs::read_to_string(&target).unwrap_or_default();
    println!("{SCOPE} result={:?} source={:?}", turn.result, turn.source);
    println!(
        "{SCOPE} file_created={created} contents={:?}",
        contents.trim()
    );
    println!(
        "{SCOPE} VERDICT: {}",
        if created {
            "dialog auto-approved, action ran ✓"
        } else {
            "file NOT created — dialog was not approved ✗"
        }
    );
    let _ = std::fs::remove_file(&target);
    Ok(())
}
