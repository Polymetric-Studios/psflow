//! WebSocket debug server for live step-through debugging.
//!
//! Wraps [`SteppedExecutor`] with a WebSocket interface that allows a connected
//! debugger to control execution: step one tick, resume continuous execution,
//! pause, or cancel. Execution events are streamed to the client in real-time.
//!
//! The server starts in **paused** mode and waits for a debugger to connect
//! before executing any nodes.

use crate::execute::context::ExecutionContext;
use crate::execute::event::ExecutionEvent;
use crate::execute::stepped::SteppedExecutor;
use crate::execute::HandlerRegistry;
use crate::graph::Graph;
use futures::stream::SplitSink;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;
use tracing::{debug, info, warn};

// --- Protocol types ---

/// Messages sent from server to client.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMsg {
    /// Initial graph source sent on connect.
    Graph { source: String },
    /// Execution events from the latest tick.
    Events { events: Vec<DebugEvent> },
    /// Server is paused, waiting for step/resume.
    Paused,
    /// Server is running (auto-stepping).
    Resumed,
    /// Execution is complete.
    Complete { trace_json: String },
    /// Error message.
    Error { message: String },
}

/// A simplified execution event for the wire protocol.
#[derive(Debug, Serialize)]
struct DebugEvent {
    node_id: String,
    from_state: String,
    to_state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    elapsed_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    outputs_json: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Commands sent from client to server.
#[derive(Debug, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
enum ClientMsg {
    /// Execute one tick then pause.
    Step,
    /// Continue executing until complete or paused.
    Resume,
    /// Pause continuous execution.
    Pause,
    /// Cancel execution.
    Cancel,
}

/// Internal commands from the WebSocket reader to the execution loop.
enum ControlCmd {
    Step,
    Resume,
    Pause,
    Cancel,
    Disconnected,
}

// --- Public API ---

/// Run the debug server. Blocks until execution completes or is cancelled.
///
/// 1. Binds to the given port
/// 2. Waits for a single debugger client to connect
/// 3. Sends the graph source
/// 4. Enters paused mode
/// 5. Processes step/resume/pause/cancel commands
/// 6. Streams events after each tick
pub async fn run_debug_server(
    port: u16,
    source: String,
    graph: &Graph,
    handlers: &HandlerRegistry,
) -> Result<(), Box<dyn std::error::Error>> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = TcpListener::bind(&addr).await?;
    info!("debug server listening on ws://{addr}");
    eprintln!("debug server listening on ws://{addr} — waiting for debugger to connect...");

    // Accept first connection
    let (stream, peer) = listener.accept().await?;
    info!(%peer, "debugger connected");
    eprintln!("debugger connected from {peer}");

    // Validate origin header to prevent cross-origin WebSocket hijacking
    let ws_stream = tokio_tungstenite::accept_hdr_async(stream, |req: &Request, resp: Response| {
        if let Some(origin) = req.headers().get("origin") {
            let origin_str = origin.to_str().unwrap_or("");
            let is_local = origin_str.starts_with("http://localhost")
                || origin_str.starts_with("http://127.0.0.1")
                || origin_str.starts_with("http://[::1]")
                || origin_str == "null";
            if !is_local {
                warn!(%peer, origin = origin_str, "rejected non-local origin");
                return Err(Response::builder()
                    .status(403)
                    .body(Some("Forbidden: non-local origin".into()))
                    .unwrap());
            }
        }
        Ok(resp)
    })
    .await?;

    run_session(ws_stream, source, graph, handlers).await
}

async fn run_session(
    ws: WebSocketStream<TcpStream>,
    source: String,
    graph: &Graph,
    handlers: &HandlerRegistry,
) -> Result<(), Box<dyn std::error::Error>> {
    let (ws_tx, ws_rx) = ws.split();

    // Channel for control commands from WS reader → execution loop
    let (cmd_tx, cmd_rx) = mpsc::channel::<ControlCmd>(16);

    // Spawn WS reader task
    let reader_handle = tokio::spawn(ws_reader(ws_rx, cmd_tx));

    // Run execution loop (owns the WS writer)
    let result = execution_loop(ws_tx, cmd_rx, source, graph, handlers).await;

    reader_handle.abort();
    result
}

/// Reads WebSocket messages and translates them to control commands.
async fn ws_reader(
    mut rx: futures::stream::SplitStream<WebSocketStream<TcpStream>>,
    cmd_tx: mpsc::Sender<ControlCmd>,
) {
    while let Some(msg_result) = rx.next().await {
        match msg_result {
            Ok(Message::Text(text)) => {
                match serde_json::from_str::<ClientMsg>(&text) {
                    Ok(ClientMsg::Step) => { let _ = cmd_tx.send(ControlCmd::Step).await; }
                    Ok(ClientMsg::Resume) => { let _ = cmd_tx.send(ControlCmd::Resume).await; }
                    Ok(ClientMsg::Pause) => { let _ = cmd_tx.send(ControlCmd::Pause).await; }
                    Ok(ClientMsg::Cancel) => { let _ = cmd_tx.send(ControlCmd::Cancel).await; }
                    Err(e) => warn!("invalid command: {e}"),
                }
            }
            Ok(Message::Close(_)) => {
                debug!("client sent close frame");
                break;
            }
            Err(e) => {
                warn!("websocket read error: {e}");
                break;
            }
            _ => {} // Ignore ping/pong/binary
        }
    }
    let _ = cmd_tx.send(ControlCmd::Disconnected).await;
}

/// Main execution loop: owns the executor and WS writer.
async fn execution_loop(
    mut ws_tx: SplitSink<WebSocketStream<TcpStream>, Message>,
    mut cmd_rx: mpsc::Receiver<ControlCmd>,
    source: String,
    graph: &Graph,
    handlers: &HandlerRegistry,
) -> Result<(), Box<dyn std::error::Error>> {
    // Send graph source
    send(&mut ws_tx, &ServerMsg::Graph { source }).await?;

    // Create executor and context
    let executor = SteppedExecutor::new();
    let ctx = executor.create_context();

    // Start paused
    send(&mut ws_tx, &ServerMsg::Paused).await?;

    let mut running = false;

    loop {
        if running {
            // Auto-stepping mode: check for commands without blocking, then tick
            match cmd_rx.try_recv() {
                Ok(ControlCmd::Pause) => {
                    running = false;
                    send(&mut ws_tx, &ServerMsg::Paused).await?;
                    continue;
                }
                Ok(ControlCmd::Cancel | ControlCmd::Disconnected) => {
                    executor.cancel_token().cancel();
                    if let Ok(events) = do_tick(&executor, graph, handlers, &ctx).await {
                        let _ = send_events(&mut ws_tx, events).await;
                    }
                    let _ = send_complete(&mut ws_tx, &ctx).await;
                    return Ok(());
                }
                _ => {}
            }

            // Execute one tick
            match do_tick(&executor, graph, handlers, &ctx).await {
                Ok(events) => {
                    let empty = events.is_empty();
                    let complete = is_all_terminal(graph, &ctx);
                    send_events(&mut ws_tx, events).await?;

                    if complete {
                        send_complete(&mut ws_tx, &ctx).await?;
                        return Ok(());
                    }

                    // Backoff when no nodes executed (waiting for predecessors)
                    if empty {
                        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    } else {
                        tokio::task::yield_now().await;
                    }
                }
                Err(e) => {
                    let _ = send(&mut ws_tx, &ServerMsg::Error { message: e.to_string() }).await;
                    return Err(e);
                }
            }
        } else {
            // Paused: block waiting for command
            let cmd = match cmd_rx.recv().await {
                Some(cmd) => cmd,
                None => return Ok(()), // Channel closed
            };

            match cmd {
                ControlCmd::Step => {
                    match do_tick(&executor, graph, handlers, &ctx).await {
                        Ok(events) => {
                            let complete = is_all_terminal(graph, &ctx);
                            send_events(&mut ws_tx, events).await?;

                            if complete {
                                send_complete(&mut ws_tx, &ctx).await?;
                                return Ok(());
                            }
                            send(&mut ws_tx, &ServerMsg::Paused).await?;
                        }
                        Err(e) => {
                            let _ = send(&mut ws_tx, &ServerMsg::Error { message: e.to_string() }).await;
                            return Err(e);
                        }
                    }
                }
                ControlCmd::Resume => {
                    running = true;
                    send(&mut ws_tx, &ServerMsg::Resumed).await?;
                }
                ControlCmd::Cancel | ControlCmd::Disconnected => {
                    executor.cancel_token().cancel();
                    if let Ok(events) = do_tick(&executor, graph, handlers, &ctx).await {
                        let _ = send_events(&mut ws_tx, events).await;
                    }
                    let _ = send_complete(&mut ws_tx, &ctx).await;
                    return Ok(());
                }
                ControlCmd::Pause => {} // Already paused
            }
        }
    }
}

/// Execute one tick and return the new events produced.
async fn do_tick(
    executor: &SteppedExecutor,
    graph: &Graph,
    handlers: &HandlerRegistry,
    ctx: &Arc<ExecutionContext>,
) -> Result<Vec<DebugEvent>, Box<dyn std::error::Error>> {
    let events_before = ctx.event_count();

    let tick_result = executor.tick(graph, handlers, ctx).await?;
    debug!(executed = ?tick_result.executed, complete = tick_result.is_complete, "tick");

    // Collect new events
    let new_events = ctx.events_since(events_before);
    let debug_events = new_events.into_iter().filter_map(convert_event).collect();

    Ok(debug_events)
}

/// Convert an ExecutionEvent to a wire-format DebugEvent.
fn convert_event(event: ExecutionEvent) -> Option<DebugEvent> {
    match event {
        ExecutionEvent::StateChanged {
            node_id,
            from,
            to,
            ..
        } => Some(DebugEvent {
            node_id,
            from_state: format!("{from}"),
            to_state: format!("{to}"),
            elapsed_ms: None,
            outputs_json: None,
            error: None,
        }),
        ExecutionEvent::NodeCompleted { node_id, outputs } => Some(DebugEvent {
            node_id,
            from_state: "running".into(),
            to_state: "completed".into(),
            elapsed_ms: None,
            outputs_json: serde_json::to_string(&outputs).ok(),
            error: None,
        }),
        ExecutionEvent::NodeFailed { node_id, error } => Some(DebugEvent {
            node_id,
            from_state: "running".into(),
            to_state: "failed".into(),
            elapsed_ms: None,
            outputs_json: None,
            error: Some(error.to_string()),
        }),
        ExecutionEvent::ExecutionStarted { .. }
        | ExecutionEvent::ExecutionCompleted { .. }
        | ExecutionEvent::NodeRetrying { .. } => None,
    }
}

fn is_all_terminal(graph: &Graph, ctx: &Arc<ExecutionContext>) -> bool {
    graph.nodes().all(|node| ctx.get_state(&node.id.0).is_terminal())
}

async fn send(
    ws_tx: &mut SplitSink<WebSocketStream<TcpStream>, Message>,
    msg: &ServerMsg,
) -> Result<(), Box<dyn std::error::Error>> {
    let json = serde_json::to_string(msg)?;
    ws_tx.send(Message::Text(json.into())).await?;
    Ok(())
}

async fn send_events(
    ws_tx: &mut SplitSink<WebSocketStream<TcpStream>, Message>,
    events: Vec<DebugEvent>,
) -> Result<(), Box<dyn std::error::Error>> {
    if events.is_empty() {
        return Ok(());
    }
    send(ws_tx, &ServerMsg::Events { events }).await
}

async fn send_complete(
    ws_tx: &mut SplitSink<WebSocketStream<TcpStream>, Message>,
    ctx: &Arc<ExecutionContext>,
) -> Result<(), Box<dyn std::error::Error>> {
    let trace = ctx.live_trace();
    let trace_json = serde_json::to_string(&trace)?;
    send(ws_tx, &ServerMsg::Complete { trace_json }).await
}
