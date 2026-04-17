//! Emit a JSON manifest of psflow's built-in handlers.
//!
//! Walks `NodeRegistry::with_defaults_full(engine, ctx)` and invokes
//! `NodeHandler::schema` on every registered handler, writing the combined
//! catalogue to stdout (or `--out FILE`).
//!
//! Consumed by downstream tooling: Ergon's MCP handler catalogue, VS Code
//! extensions, CI drift checks.
//!
//! Usage:
//!
//! ```text
//! cargo run --bin psflow-manifest -- [--out <path>] [--pretty]
//! ```

use std::path::PathBuf;
use std::sync::Arc;

use psflow::scripting::engine::ScriptEngine;
use psflow::{ExecutionContext, NodeRegistry};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut out: Option<PathBuf> = None;
    let mut pretty = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--out" => {
                i += 1;
                out = Some(PathBuf::from(
                    args.get(i).expect("--out requires a path argument"),
                ));
            }
            "--pretty" => pretty = true,
            "-h" | "--help" => {
                println!("psflow-manifest [--out <path>] [--pretty]");
                return;
            }
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(2);
            }
        }
        i += 1;
    }

    let engine = Arc::new(ScriptEngine::with_defaults());
    let ctx = Arc::new(ExecutionContext::new());
    let reg = NodeRegistry::with_defaults_full(engine, ctx);
    let manifest = reg.manifest();

    let rendered = if pretty {
        serde_json::to_string_pretty(&manifest).unwrap()
    } else {
        serde_json::to_string(&manifest).unwrap()
    };

    if let Some(path) = out {
        std::fs::write(&path, rendered).expect("failed to write manifest");
        eprintln!("manifest written to {}", path.display());
    } else {
        println!("{rendered}");
    }
}
