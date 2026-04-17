use clap::Parser;
use psflow::{load_mermaid, Executor, NodeRegistry, NodeState, Outputs, TopologicalExecutor};
use std::path::PathBuf;
use std::process::ExitCode;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "psflow", version, about = "Process flow execution engine")]
struct Cli {
    /// Path to an annotated Mermaid (.mmd) file to execute.
    file: PathBuf,

    /// Validate the graph without executing it.
    #[arg(long)]
    validate: bool,

    /// Print the execution result (node states and outputs) as JSON.
    #[arg(long)]
    json: bool,

    /// Write the execution trace to a file (for use with the debugger).
    #[arg(long, value_name = "PATH")]
    trace: Option<PathBuf>,

    /// Start a WebSocket debug server on the given port.
    /// The engine starts paused and waits for a debugger to connect.
    #[arg(long, value_name = "PORT")]
    debug_ws: Option<u16>,

    /// Log verbosity. Repeat for more detail: -v (info), -vv (debug), -vvv (trace).
    /// Can also be set via RUST_LOG env var (e.g. RUST_LOG=psflow=debug).
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    // Initialize tracing subscriber.
    // RUST_LOG env var takes precedence; otherwise use -v flags.
    let filter = if std::env::var("RUST_LOG")
        .ok()
        .filter(|v| !v.is_empty())
        .is_some()
    {
        EnvFilter::from_default_env()
    } else {
        let level = match cli.verbose {
            0 => "warn",
            1 => "psflow=info",
            2 => "psflow=debug",
            _ => "psflow=trace",
        };
        EnvFilter::new(level)
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();

    // Read the .mmd file
    let content = match std::fs::read_to_string(&cli.file) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: cannot read '{}': {e}", cli.file.display());
            return ExitCode::FAILURE;
        }
    };

    // Parse the mermaid content into a graph
    let graph = match load_mermaid(&content) {
        Ok(g) => g,
        Err(errors) => {
            for e in &errors {
                eprintln!("parse error: {e}");
            }
            return ExitCode::FAILURE;
        }
    };

    let name = graph.metadata().name.as_deref().unwrap_or("(unnamed)");
    eprintln!(
        "loaded graph '{}': {} nodes, {} edges",
        name,
        graph.node_count(),
        graph.edge_count()
    );

    // Build the handler registry with all built-in handlers
    let engine = psflow::scripting::engine::default_script_engine();
    let registry = NodeRegistry::with_defaults(engine);

    // Check for unregistered handlers
    let missing = registry.validate_graph(&graph);
    if !missing.is_empty() {
        for (node_id, handler) in &missing {
            eprintln!("warning: node '{node_id}' references unregistered handler '{handler}'");
        }
    }

    // Validate-only mode
    if cli.validate {
        let errors = graph.validate_as_dag();
        if !errors.is_empty() {
            for e in &errors {
                eprintln!("validation error: {e}");
            }
            return ExitCode::FAILURE;
        }
        eprintln!("validation passed");
        return ExitCode::SUCCESS;
    }

    // Execute the graph
    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
    let handlers = registry.into_handler_registry();

    // Debug server mode: start WebSocket server instead of normal execution
    if let Some(port) = cli.debug_ws {
        return rt.block_on(async {
            match psflow::debug_server::run_debug_server(port, content, &graph, &handlers).await {
                Ok(()) => {
                    eprintln!("debug session ended");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("debug server error: {e}");
                    ExitCode::FAILURE
                }
            }
        });
    }

    let result = rt.block_on(async { TopologicalExecutor::new().execute(&graph, &handlers).await });

    match result {
        Ok(result) => {
            // Write trace file if requested
            if let Some(ref trace_path) = cli.trace {
                let trace = result.trace();
                match serde_json::to_string_pretty(&trace) {
                    Ok(json) => {
                        if let Err(e) = std::fs::write(trace_path, &json) {
                            eprintln!(
                                "error: cannot write trace to '{}': {e}",
                                trace_path.display()
                            );
                        } else {
                            eprintln!("trace written to {}", trace_path.display());
                        }
                    }
                    Err(e) => eprintln!("error: cannot serialize trace: {e}"),
                }
            }

            let failed: Vec<_> = result
                .node_states
                .iter()
                .filter(|(_, s)| **s == NodeState::Failed)
                .map(|(id, _)| id.as_str())
                .collect();

            if cli.json {
                print_json_result(&result.node_states, &result.node_outputs);
            } else {
                let mut states: Vec<_> = result.node_states.iter().collect();
                states.sort_by_key(|(id, _)| (*id).clone());
                for (id, state) in &states {
                    let symbol = match state {
                        NodeState::Completed => "+",
                        NodeState::Failed => "!",
                        NodeState::Cancelled => "~",
                        _ => "?",
                    };
                    eprintln!("  [{symbol}] {id}: {state}");
                }
                eprintln!(
                    "completed in {:.1}ms",
                    result.elapsed.as_secs_f64() * 1000.0
                );
            }

            if failed.is_empty() {
                ExitCode::SUCCESS
            } else {
                eprintln!("failed nodes: {}", failed.join(", "));
                ExitCode::FAILURE
            }
        }
        Err(e) => {
            eprintln!("execution error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn print_json_result(
    states: &std::collections::HashMap<String, NodeState>,
    outputs: &std::collections::HashMap<String, Outputs>,
) {
    let result = serde_json::json!({
        "nodes": states.iter().map(|(id, state)| {
            serde_json::json!({
                "id": id,
                "state": format!("{state}"),
                "outputs": outputs.get(id).cloned().unwrap_or_default(),
            })
        }).collect::<Vec<_>>(),
    });
    println!("{}", serde_json::to_string_pretty(&result).unwrap());
}
