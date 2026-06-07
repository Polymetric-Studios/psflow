//! Spike: transcript-based turn-completion. Drives a tool-using turn and detects
//! completion by polling the session transcript for the final assistant entry
//! (`message.stop_reason == "end_turn"`) — instead of watching the screen
//! spinner. Verifies: the marker is timely, and we don't stop on the
//! intermediate `tool_use` assistant entry.
//!
//! Run: cargo run --example pty_transcript --features terminal

use psflow::adapter::{ClaudeTerminalSession, SessionOptions};
use std::time::{Duration, Instant};

const SCOPE: &str = "[PTY-TRANSCRIPT][pty_transcript]";
const END_TURN: &str = "end_turn";

/// Stop reasons of assistant entries beyond `skip` lines, in order.
fn assistant_stop_reasons(path: &std::path::Path, skip: usize) -> Vec<String> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    content
        .lines()
        .skip(skip)
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v.get("type").and_then(|t| t.as_str()) == Some("assistant"))
        .map(|v| {
            v.get("message")
                .and_then(|m| m.get("stop_reason"))
                .and_then(|s| s.as_str())
                .unwrap_or("(none)")
                .to_string()
        })
        .collect()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("{SCOPE} spawning…");
    let mut session = ClaudeTerminalSession::spawn_ready(SessionOptions::default())?;

    // The transcript file is created on first activity, so submit first; this is
    // a fresh session (one turn), so all assistant entries belong to this turn.
    let body = "Run the bash command `echo transcript-spike-OK` and report its output.";
    println!("{SCOPE} submitting tool-using prompt…");
    session.submit(body)?;
    let baseline = 0usize;

    let start = Instant::now();
    let mut transcript_done_at: Option<u128> = None;
    let mut screen_idle_at: Option<u128> = None;
    loop {
        std::thread::sleep(Duration::from_millis(250));
        let elapsed = start.elapsed().as_millis();
        let reasons = match session.transcript_path() {
            Some(p) => assistant_stop_reasons(&p, baseline),
            None => Vec::new(),
        };
        // Screen spinner signal, for timing comparison.
        let screen_busy = session.screen_text().contains('…');

        let transcript_done = reasons.last().map(|r| r == END_TURN).unwrap_or(false);
        if transcript_done && transcript_done_at.is_none() {
            transcript_done_at = Some(elapsed);
            println!("{SCOPE} t={elapsed}ms TRANSCRIPT done — stop_reasons={reasons:?}");
        }
        if !screen_busy && screen_idle_at.is_none() && elapsed > 1500 {
            // first stable non-busy after work started
            screen_idle_at = Some(elapsed);
        }
        println!("{SCOPE} t={elapsed}ms assistant_reasons={reasons:?} screen_busy={screen_busy}");

        if transcript_done {
            break;
        }
        if elapsed > 60_000 {
            println!("{SCOPE} TIMEOUT");
            break;
        }
    }

    println!(
        "{SCOPE} SUMMARY: transcript_done_at={:?}ms screen_idle_at={:?}ms",
        transcript_done_at, screen_idle_at
    );
    println!("{SCOPE} done.");
    Ok(())
}
