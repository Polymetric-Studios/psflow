use clap::Parser;
use psflow::{
    load_mermaid, NodeRegistry, NodeState, TopologicalExecutor,
    Executor, Outputs,
};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "psflow", version, about = "Process flow execution engine")]
struct Cli {
    /// Path to an annotated Mermaid (.mmd) file to execute.
    file: PathBuf,

    /// Validate the graph without executing it.
    #[arg(long)]
    validate: bool,

    /// Print the execution trace (node states and outputs) as JSON.
    #[arg(long)]
    json: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

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

    let name = graph
        .metadata()
        .name
        .as_deref()
        .unwrap_or("(unnamed)");
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
    let result = rt.block_on(async {
        TopologicalExecutor::new()
            .execute(&graph, &handlers)
            .await
    });

    match result {
        Ok(result) => {
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
                eprintln!("completed in {:.1}ms", result.elapsed.as_secs_f64() * 1000.0);
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
