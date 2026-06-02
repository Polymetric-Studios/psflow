//! PTY driver demo — drives the real interactive `claude` TUI headless via the
//! `ClaudeTerminalSession` engine: spawn, wait for the input box, run one prompt
//! collecting the result via the file-primary / screen-scrape-fallback path.
//!
//! Run: `cargo run --example pty_spike --features terminal`

use psflow::adapter::{ClaudeTerminalSession, SessionOptions};

const SCOPE: &str = "[PTY-SPIKE][pty_spike]";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("{SCOPE} spawning interactive claude on a PTY…");
    let mut session = ClaudeTerminalSession::spawn_ready(SessionOptions::default())?;
    println!("{SCOPE} input box ready.");

    println!("{SCOPE} session_id={}", session.session_id());
    let body = "What is 6 times 7? Reply with only the number.";
    println!("{SCOPE} running prompt: {body:?}");
    let turn = session.run_turn(body)?;

    println!("{SCOPE} result={:?} source={:?}", turn.result, turn.source);
    println!("{SCOPE} done.");
    Ok(())
}
