//! Capture real `claude` TUI screens across states (idle / working / approval
//! dialog) to drive recognizer hardening. Snapshots the screen on a timer while
//! a turn runs, prints a signature timeline, and writes each distinct screen to
//! /tmp/psflow_capture_NN.txt for inspection.
//!
//! Usage:
//!   cargo run --example pty_capture --features terminal -- [permission-mode] [prompt...]
//! Examples:
//!   # working state (auto-approve): a few seconds of output
//!   cargo run --example pty_capture --features terminal
//!   # approval dialog: force prompts, trigger a tool
//!   cargo run --example pty_capture --features terminal -- default "Run the shell command: echo hello"

use psflow::adapter::{ClaudeTerminalSession, SessionOptions};
use std::time::{Duration, Instant};

const SCOPE: &str = "[PTY-CAPTURE][pty_capture]";
const TICK_MS: u64 = 300;
const MAX_MS: u128 = 40_000;
const SETTLE_MS: u128 = 1500;

fn signature(screen: &str, ready: bool) -> (bool, bool, bool) {
    let busy = screen.contains("esc to interrupt") || screen.contains("to interrupt");
    let spinner = screen.contains('✻') || screen.contains("Working") || screen.contains("Brewed");
    (ready, busy, spinner)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let perm = args.next();
    let prompt = {
        let rest: Vec<String> = args.collect();
        if rest.is_empty() {
            "Count from 1 to 15, one number per line, with a one-word note on each.".to_string()
        } else {
            rest.join(" ")
        }
    };

    let mut opts = SessionOptions::default();
    if let Some(mode) = &perm {
        opts = opts.with_arg("--permission-mode").with_arg(mode.clone());
        println!("{SCOPE} permission-mode={mode}");
    }
    println!("{SCOPE} spawning…");
    let mut session = ClaudeTerminalSession::spawn_ready(opts)?;
    println!("{SCOPE} ready. submitting: {prompt:?}");
    session.submit(&prompt)?;

    let start = Instant::now();
    let mut distinct: Vec<String> = Vec::new();
    let mut last_screen = String::new();
    let mut last_change = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(TICK_MS));
        let screen = session.screen_text();
        let ready = session.input_ready();
        let (r, busy, spin) = signature(&screen, ready);
        let elapsed = start.elapsed().as_millis();
        if screen != last_screen {
            last_change = Instant::now();
            last_screen = screen.clone();
            if distinct.last().map(|s| s != &screen).unwrap_or(true) {
                distinct.push(screen.clone());
            }
        }
        let stable_ms = last_change.elapsed().as_millis();
        println!("{SCOPE} t={elapsed}ms ready={r} busy={busy} spinner={spin} stable={stable_ms}ms");

        // Stop conditions: settled back at the input box (turn done), OR a stable
        // non-ready screen that isn't actively working (likely an approval dialog).
        if ready && !busy && stable_ms >= SETTLE_MS && elapsed > 2000 {
            println!("{SCOPE} -> reached settled input box (turn complete)");
            break;
        }
        if !ready && !busy && stable_ms >= 2500 && elapsed > 2000 {
            println!("{SCOPE} -> stable non-ready screen (likely an approval dialog)");
            break;
        }
        if elapsed >= MAX_MS {
            println!("{SCOPE} -> timeout");
            break;
        }
    }

    println!("{SCOPE} writing {} distinct screens…", distinct.len());
    for (i, screen) in distinct.iter().enumerate() {
        let path = std::env::temp_dir().join(format!("psflow_capture_{i:02}.txt"));
        std::fs::write(&path, screen)?;
        println!("{SCOPE} wrote {}", path.display());
    }
    println!("{SCOPE} done.");
    Ok(())
}
