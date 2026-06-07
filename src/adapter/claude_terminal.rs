//! Drive the real interactive `claude` TUI headless over a pseudo-terminal.
//!
//! Unlike [`ClaudeCliAdapter`](super::claude_cli::ClaudeCliAdapter) (one-shot
//! `claude -p`), this hosts the *actual interactive terminal UI* on a PTY and
//! drives it as if a human were typing: the full surface (slash commands,
//! workflows, approval dialogs, pause/resume) is available, and the session
//! bills as interactive rather than from the Agent SDK credit pool.
//!
//! ## Design
//!
//! - A [`portable_pty`] master/slave pair hosts `claude`; `claude` sees a real
//!   TTY (`isatty` true) so it launches the interactive UI, not print mode.
//! - A background thread pumps the master's output into a [`vt100`] virtual
//!   terminal, so we read the rendered screen exactly as a human would instead
//!   of regexing raw ANSI.
//! - A *recognizer* (here: cursor sitting on the `❯` input row) detects when the
//!   session is ready for input and when a turn has completed. Recognition is
//!   the fragile part across `claude` versions, so it is contained to one place.
//!
//! ## Result extraction
//!
//! [`ClaudeTerminalSession::run_collecting_result`] uses a *file-primary,
//! screen-scrape-fallback* strategy: the prompt instructs the session to write
//! its final result to a temp file (payload never depends on screen layout); if
//! the file is missing/empty we fall back to scraping the answer off the screen.
//!
//! ## Concurrency
//!
//! Methods are blocking (they poll the screen). Async callers should wrap a
//! session in [`tokio::task::spawn_blocking`]; one session owns one child
//! process and is not `Sync` across threads while driving.

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const DEFAULT_ROWS: u16 = 40;
const DEFAULT_COLS: u16 = 120;
/// Screen must be quiet this long before a state is considered stable.
const DEFAULT_SETTLE_MS: u128 = 1500;
const DEFAULT_READY_TIMEOUT_MS: u128 = 30_000;
const DEFAULT_TURN_TIMEOUT_MS: u128 = 300_000;
const POLL_MS: u64 = 150;
/// Minimum time after submit before a turn can be considered complete, so we
/// don't mistake the pre-response idle screen for a finished answer.
const MIN_TURN_MS: u128 = 1200;
/// Bytes of new output past the submit baseline that count as "responded".
const RESPONDED_BYTES: usize = 100;
/// Pause after typing before pressing Enter, letting the TUI echo the text.
const TYPE_ECHO_MS: u64 = 400;
/// Max time to wait for an answered dialog to clear before resuming the turn.
const DIALOG_CLEAR_TIMEOUT_MS: u128 = 8000;
/// `stop_reason` on the transcript's final assistant entry marking turn end.
const END_TURN: &str = "end_turn";
/// The input-box marker `claude` renders at the prompt row. This is present
/// *throughout* a turn (the input box never disappears), so it signals "the UI
/// is alive", not "the turn is done".
const INPUT_MARKER: char = '❯';
/// Bullet `claude` renders before a response line (screen-scrape anchor).
const RESPONSE_BULLET: char = '⏺';
/// Leading glyphs of the live "working" spinner status line (it cycles through
/// these frames). A working line looks like `✽ Crunching… `; the completed
/// summary looks like `✻ Brewed for 3s` — same glyph family, but only the live
/// line ends with `…`. Turn-completion keys off that `…` being gone.
const SPINNER_GLYPHS: &[char] = &[
    '✻', '✽', '✼', '✶', '✷', '✸', '✹', '✺', '✢', '✣', '✳', '✱', '❋', '✦', '∗',
];
/// Max length of a spinner status line (guards against matching long content
/// lines that happen to end with an ellipsis).
const SPINNER_LINE_MAX: usize = 60;

/// Errors from driving a terminal session.
#[derive(Debug, thiserror::Error)]
pub enum TerminalError {
    #[error("failed to spawn claude terminal: {0}")]
    Spawn(String),
    #[error("terminal io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("timed out after {0}ms waiting for {1}")]
    Timeout(u128, &'static str),
    #[error("terminal session closed unexpectedly")]
    Closed,
    #[error("terminal session cancelled")]
    Cancelled,
}

/// A keypress the driver can send to the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Enter,
    Escape,
    CtrlC,
    Up,
    Down,
    Left,
    Right,
    Tab,
}

impl Key {
    /// The byte sequence a real terminal sends for this key.
    fn bytes(self) -> &'static [u8] {
        match self {
            Key::Enter => b"\r",
            Key::Escape => b"\x1b",
            Key::CtrlC => b"\x03",
            Key::Up => b"\x1b[A",
            Key::Down => b"\x1b[B",
            Key::Right => b"\x1b[C",
            Key::Left => b"\x1b[D",
            Key::Tab => b"\t",
        }
    }
}

/// How to launch the session.
#[derive(Debug, Clone)]
pub struct SessionOptions {
    /// The `claude` binary (path or name on PATH).
    pub command: String,
    /// Extra CLI args (e.g. `--permission-mode`, `--mcp-config`).
    pub args: Vec<String>,
    /// Working directory for the session.
    pub cwd: Option<PathBuf>,
    /// Model override passed as `--model`.
    pub model: Option<String>,
    /// Explicit session UUID passed as `--session-id`. When `None`, a fresh v4
    /// UUID is generated at spawn so the transcript path is always known.
    pub session_id: Option<String>,
    /// Resume an existing conversation: emit `--resume <session_id>` instead of
    /// `--session-id <session_id>` (Claude continues the same transcript).
    /// Requires `session_id` to be set to the id being resumed.
    pub resume: bool,
    /// Extra environment variables to set on the child, applied after the
    /// inherited process env (so these win). Useful for e.g. depth markers.
    pub env: Vec<(String, String)>,
    pub rows: u16,
    pub cols: u16,
    pub settle_ms: u128,
    pub ready_timeout_ms: u128,
    pub turn_timeout_ms: u128,
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self {
            command: "claude".into(),
            args: Vec::new(),
            cwd: None,
            model: None,
            session_id: None,
            resume: false,
            env: Vec::new(),
            rows: DEFAULT_ROWS,
            cols: DEFAULT_COLS,
            settle_ms: DEFAULT_SETTLE_MS,
            ready_timeout_ms: DEFAULT_READY_TIMEOUT_MS,
            turn_timeout_ms: DEFAULT_TURN_TIMEOUT_MS,
        }
    }
}

impl SessionOptions {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with_command(mut self, command: impl Into<String>) -> Self {
        self.command = command.into();
        self
    }
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }
    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }
    pub fn with_arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }
    pub fn with_session_id(mut self, id: impl Into<String>) -> Self {
        self.session_id = Some(id.into());
        self
    }
    /// Resume the conversation named by `session_id` (emit `--resume`).
    pub fn with_resume(mut self, resume: bool) -> Self {
        self.resume = resume;
        self
    }
    /// Set an environment variable on the spawned child.
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }
}

/// Where a turn's result was read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultSource {
    /// Read from the session transcript JSONL Claude Code writes (deterministic).
    Transcript,
    /// Scraped off the rendered screen (fallback when the transcript is absent).
    Screen,
}

/// The outcome of one prompt → answer turn.
#[derive(Debug, Clone)]
pub struct TurnResult {
    /// The result payload — the final assistant message of the turn.
    pub result: String,
    /// How `result` was obtained.
    pub source: ResultSource,
    /// The full final screen, for debugging / fallback parsing.
    pub screen: String,
}

/// Hook fired when a dialog needs attention: `(prompt, remote_control_url)`.
pub type ApprovalNotifier = Arc<dyn Fn(&ApprovalPrompt, Option<&str>) + Send + Sync>;

/// A permission/approval dialog parsed off the screen — Claude Code is waiting
/// for a choice before it can continue the turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalPrompt {
    /// The question line (e.g. "Do you want to create foo.txt?").
    pub question: String,
    /// The numbered option labels, in order (e.g. ["Yes", "Yes, allow all…", "No"]).
    pub options: Vec<String>,
    /// Index into `options` of the currently-highlighted choice (the `❯` row).
    pub selected: usize,
}

/// How to answer an approval dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalChoice {
    /// Accept — select the first option (the default "Yes").
    Allow,
    /// Decline — cancel the dialog (Esc).
    Deny,
    /// Select a specific option by its 1-based number (e.g. `2` for "allow all").
    Select(usize),
    /// Don't answer — leave the dialog for an external actor (e.g. a human via
    /// the session's remote-control URL, or an MCP round-trip) to resolve. The
    /// drive loop keeps waiting until the dialog clears or the turn times out.
    Defer,
}

/// Decides how a dialog is answered. The `Custom` variant covers both an
/// allowlist (a pure predicate over the prompt) and the supervision TUI's
/// `Ask` flow (a callback that blocks on a human response).
#[derive(Clone)]
pub enum ApprovalPolicy {
    /// Approve every dialog (the first option).
    AllowAll,
    /// Decline every dialog. Safe default.
    DenyAll,
    /// Defer to a caller-supplied decision function.
    Custom(Arc<dyn Fn(&ApprovalPrompt) -> ApprovalChoice + Send + Sync>),
}

impl Default for ApprovalPolicy {
    fn default() -> Self {
        ApprovalPolicy::DenyAll
    }
}

impl std::fmt::Debug for ApprovalPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApprovalPolicy::AllowAll => f.write_str("AllowAll"),
            ApprovalPolicy::DenyAll => f.write_str("DenyAll"),
            ApprovalPolicy::Custom(_) => f.write_str("Custom(..)"),
        }
    }
}

impl ApprovalPolicy {
    /// A `Custom` policy from a closure.
    pub fn custom<F>(f: F) -> Self
    where
        F: Fn(&ApprovalPrompt) -> ApprovalChoice + Send + Sync + 'static,
    {
        ApprovalPolicy::Custom(Arc::new(f))
    }

    fn decide(&self, prompt: &ApprovalPrompt) -> ApprovalChoice {
        match self {
            ApprovalPolicy::AllowAll => ApprovalChoice::Allow,
            ApprovalPolicy::DenyAll => ApprovalChoice::Deny,
            ApprovalPolicy::Custom(f) => f(prompt),
        }
    }
}

/// Shared virtual-terminal state, updated by the reader thread.
struct VtState {
    parser: vt100::Parser,
    last_update: Instant,
    total_bytes: usize,
    closed: bool,
}

impl VtState {
    /// True when the cursor sits on a row containing the `❯` input marker —
    /// the robust "ready for input" signal (text heuristics are fragile).
    fn input_ready(&self) -> bool {
        let screen = self.parser.screen();
        let (cursor_row, _) = screen.cursor_position();
        let (_, cols) = screen.size();
        (0..cols)
            .filter_map(|c| screen.cell(cursor_row, c).map(|cell| cell.contents()))
            .collect::<String>()
            .contains(INPUT_MARKER)
    }
    fn idle_ms(&self) -> u128 {
        self.last_update.elapsed().as_millis()
    }
    fn screen_text(&self) -> String {
        self.parser.screen().contents()
    }
    /// True while a turn is actively running (the live spinner status line is on
    /// screen). This is the reliable "still working" signal — `input_ready`
    /// stays true throughout a turn and cannot detect completion on its own.
    fn busy(&self) -> bool {
        is_busy(&self.screen_text())
    }
    /// True when a permission/approval dialog is on screen.
    fn approval(&self) -> bool {
        detect_approval(&self.screen_text()).is_some()
    }
}

/// The remote-control URL Claude Code advertises on screen, if present
/// (`https://claude.ai/code/session_<id>`). A human can open it to answer a
/// dialog in this exact session.
fn remote_control_url(screen: &str) -> Option<String> {
    const PREFIX: &str = "https://claude.ai/code/session_";
    screen
        .split(|c: char| c.is_whitespace())
        .find(|w| w.starts_with(PREFIX))
        .map(|w| {
            w.trim_end_matches(|c: char| !c.is_alphanumeric())
                .to_string()
        })
}

/// Parse `"1. Yes"` (digits, a dot, then a label) into the label, or `None`.
fn parse_numbered_option(s: &str) -> Option<String> {
    let s = s.trim_start();
    let dot = s.find('.')?;
    let (num, rest) = s.split_at(dot);
    if num.is_empty() || !num.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let label = rest[1..].trim();
    if label.is_empty() {
        None
    } else {
        Some(label.to_string())
    }
}

/// Detect a permission/approval dialog. The structural anchor is a numbered
/// option list with one option highlighted by the `❯` selection marker — which
/// distinguishes a real dialog from an ordinary numbered list in response text
/// (no highlighted row) and from the empty input box (no number after `❯`).
pub fn detect_approval(screen: &str) -> Option<ApprovalPrompt> {
    let lines: Vec<&str> = screen.lines().collect();
    let mut options: Vec<String> = Vec::new();
    let mut selected: Option<usize> = None;
    let mut first_opt: Option<usize> = None;

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        let (is_selected, rest) = match trimmed.strip_prefix(INPUT_MARKER) {
            Some(r) => (true, r.trim_start()),
            None => (false, trimmed),
        };
        if let Some(label) = parse_numbered_option(rest) {
            if first_opt.is_none() {
                first_opt = Some(i);
            }
            if is_selected {
                selected = Some(options.len());
            }
            options.push(label);
        }
    }

    let selected = selected?; // a real dialog always has a highlighted option
    if options.len() < 2 {
        return None;
    }

    // The question is the nearest non-empty, non-option line above the options.
    let question = first_opt
        .map(|fo| {
            lines[..fo]
                .iter()
                .rev()
                .map(|l| l.trim())
                .find(|t| !t.is_empty() && parse_numbered_option(t).is_none())
                .unwrap_or("")
                .to_string()
        })
        .unwrap_or_default();

    Some(ApprovalPrompt {
        question,
        options,
        selected,
    })
}

/// Detect the live "working" spinner status line: a short line that starts with
/// a spinner glyph and ends with an ellipsis (e.g. `✽ Crunching… `). The
/// completed summary (`✻ Brewed for 3s`) shares the glyph but lacks the `…`.
fn is_busy(screen: &str) -> bool {
    screen.lines().any(|line| {
        let t = line.trim();
        t.ends_with('…')
            && t.chars().count() <= SPINNER_LINE_MAX
            && t.chars()
                .next()
                .is_some_and(|c| SPINNER_GLYPHS.contains(&c))
    })
}

/// A live interactive `claude` session hosted on a pseudo-terminal.
pub struct ClaudeTerminalSession {
    // `master` must outlive the reader thread; declared before nothing relevant
    // but dropped in field order after `writer`. We join the reader in `Drop`.
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    state: Arc<Mutex<VtState>>,
    reader: Option<JoinHandle<()>>,
    /// The resolved session UUID (passed as `--session-id`), used to locate the
    /// transcript Claude Code writes for this session.
    session_id: String,
    /// How approval dialogs are answered during a driven turn.
    approval_policy: ApprovalPolicy,
    /// Optional hook fired once when a dialog first appears, carrying the prompt
    /// and the session's remote-control URL (if any). Used to *route* a dialog
    /// to a human/another surface (remote-control, MCP, Slack) — independent of
    /// the policy that decides the keystroke.
    approval_notifier: Option<ApprovalNotifier>,
    /// Set by an external caller to abort the current wait (e.g. a psflow
    /// cancellation token); the drive/wait loops observe it and return
    /// `Cancelled`.
    cancel_flag: Arc<AtomicBool>,
    opts: SessionOptions,
}

impl ClaudeTerminalSession {
    /// Spawn `claude` on a fresh PTY and start pumping its screen. Does not wait
    /// for readiness — call [`wait_ready`](Self::wait_ready) next.
    pub fn spawn(opts: SessionOptions) -> Result<Self, TerminalError> {
        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows: opts.rows,
                cols: opts.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| TerminalError::Spawn(e.to_string()))?;

        // Pin the session id so the transcript path is deterministic.
        let session_id = opts
            .session_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        let mut cmd = CommandBuilder::new(&opts.command);
        for (k, v) in std::env::vars() {
            cmd.env(k, v);
        }
        cmd.env("TERM", "xterm-256color");
        // Caller-supplied env wins over the inherited process env.
        for (k, v) in &opts.env {
            cmd.env(k, v);
        }
        // Resume continues the same transcript (`--resume`); otherwise pin a new
        // session id (`--session-id`). The two flags are mutually exclusive.
        if opts.resume {
            cmd.arg("--resume");
        } else {
            cmd.arg("--session-id");
        }
        cmd.arg(&session_id);
        if let Some(model) = &opts.model {
            cmd.arg("--model");
            cmd.arg(model);
        }
        for arg in &opts.args {
            cmd.arg(arg);
        }
        // Default to the current working directory; without this the child
        // launches in $HOME and shows the first-run welcome screen.
        match &opts.cwd {
            Some(cwd) => cmd.cwd(cwd),
            None => {
                if let Ok(cwd) = std::env::current_dir() {
                    cmd.cwd(cwd);
                }
            }
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| TerminalError::Spawn(e.to_string()))?;
        drop(pair.slave);

        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| TerminalError::Spawn(e.to_string()))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| TerminalError::Spawn(e.to_string()))?;

        let state = Arc::new(Mutex::new(VtState {
            parser: vt100::Parser::new(opts.rows, opts.cols, 0),
            last_update: Instant::now(),
            total_bytes: 0,
            closed: false,
        }));

        let reader_state = state.clone();
        let reader = std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => {
                        reader_state.lock().unwrap().closed = true;
                        break;
                    }
                    Ok(n) => {
                        let mut s = reader_state.lock().unwrap();
                        s.parser.process(&buf[..n]);
                        s.total_bytes += n;
                        s.last_update = Instant::now();
                    }
                }
            }
        });

        Ok(Self {
            master: pair.master,
            writer,
            child,
            state,
            reader: Some(reader),
            session_id,
            approval_policy: ApprovalPolicy::default(),
            approval_notifier: None,
            cancel_flag: Arc::new(AtomicBool::new(false)),
            opts,
        })
    }

    /// Spawn and block until the input box is ready.
    pub fn spawn_ready(opts: SessionOptions) -> Result<Self, TerminalError> {
        let session = Self::spawn(opts)?;
        session.wait_ready()?;
        Ok(session)
    }

    /// Poll the screen until `pred` holds (and, when `require_settle`, the screen
    /// has been quiet for `settle_ms`), or `timeout_ms` elapses.
    fn poll_until<F>(
        &self,
        pred: F,
        require_settle: bool,
        timeout_ms: u128,
        what: &'static str,
    ) -> Result<(), TerminalError>
    where
        F: Fn(&VtState) -> bool,
    {
        let start = Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(POLL_MS));
            if self.cancel_flag.load(Ordering::Relaxed) {
                return Err(TerminalError::Cancelled);
            }
            {
                let s = self.state.lock().unwrap();
                if s.closed {
                    return Err(TerminalError::Closed);
                }
                let settled = !require_settle || s.idle_ms() >= self.opts.settle_ms;
                if s.total_bytes > 0 && settled && pred(&s) {
                    return Ok(());
                }
            }
            if start.elapsed().as_millis() >= timeout_ms {
                return Err(TerminalError::Timeout(timeout_ms, what));
            }
        }
    }

    /// Block until the session is settled and showing the input box.
    pub fn wait_ready(&self) -> Result<(), TerminalError> {
        self.poll_until(
            |s| s.input_ready(),
            true,
            self.opts.ready_timeout_ms,
            "input box",
        )
    }

    /// Type literal text into the input box (no submit).
    pub fn type_text(&mut self, text: &str) -> Result<(), TerminalError> {
        self.writer.write_all(text.as_bytes())?;
        self.writer.flush()?;
        Ok(())
    }

    /// Send a single keypress.
    pub fn send_key(&mut self, key: Key) -> Result<(), TerminalError> {
        self.writer.write_all(key.bytes())?;
        self.writer.flush()?;
        Ok(())
    }

    /// Interrupt the current turn (Esc).
    pub fn interrupt(&mut self) -> Result<(), TerminalError> {
        self.send_key(Key::Escape)
    }

    /// Type a prompt and submit it. The caller is responsible for having reached
    /// an input-ready state first (e.g. via [`wait_ready`](Self::wait_ready) or
    /// a prior [`wait_turn`](Self::wait_turn)).
    pub fn submit(&mut self, prompt: &str) -> Result<(), TerminalError> {
        self.type_text(prompt)?;
        std::thread::sleep(Duration::from_millis(TYPE_ECHO_MS));
        self.send_key(Key::Enter)
    }

    /// Block until the current turn finishes: output redrew past the submit
    /// baseline, the live spinner is gone (`!busy`), no approval dialog is
    /// waiting (`!approval`), the input box is present, and the screen settled.
    ///
    /// `!busy` and `!approval` are both load-bearing: the input box stays
    /// visible during a turn (so `input_ready` can't detect completion alone),
    /// and a dialog's selection cursor reuses the same `❯` marker (so without
    /// `!approval`, a waiting dialog would be misread as a completed turn). A
    /// dialog left unanswered makes this wait time out — `ApprovalPolicy`
    /// (Phase C) answers it so the turn proceeds.
    pub fn wait_turn(&self) -> Result<(), TerminalError> {
        let baseline = self.state.lock().unwrap().total_bytes;
        let submit_at = Instant::now();
        self.poll_until(
            move |s| {
                s.total_bytes > baseline + RESPONDED_BYTES
                    && !s.busy()
                    && !s.approval()
                    && s.input_ready()
                    && submit_at.elapsed().as_millis() >= MIN_TURN_MS
            },
            true,
            self.opts.turn_timeout_ms,
            "turn to complete",
        )
    }

    /// Drive the current turn to completion, answering approval dialogs as they
    /// appear (per the policy). Completion is detected **from the transcript**:
    /// the turn is done when a new `end_turn` assistant entry appears past
    /// `baseline_end_turns` (the count captured before submit) — deterministic,
    /// and it won't fire while a dialog is paused (no `end_turn` until resolved).
    /// The screen is consulted only to detect/answer dialogs.
    fn drive_to_completion(&mut self, baseline_end_turns: usize) -> Result<(), TerminalError> {
        let submit_at = Instant::now();
        // The dialog we've already routed to the notifier, so we fire it once.
        let mut notified: Option<ApprovalPrompt> = None;
        loop {
            std::thread::sleep(Duration::from_millis(POLL_MS));
            if self.cancel_flag.load(Ordering::Relaxed) {
                return Err(TerminalError::Cancelled);
            }
            let (closed, screen, idle) = {
                let s = self.state.lock().unwrap();
                (s.closed, s.screen_text(), s.idle_ms())
            };
            if closed {
                return Err(TerminalError::Closed);
            }
            if submit_at.elapsed().as_millis() >= self.opts.turn_timeout_ms {
                return Err(TerminalError::Timeout(
                    self.opts.turn_timeout_ms,
                    "turn to complete",
                ));
            }
            // Handle a settled approval dialog (don't key into a mid-render frame).
            if idle >= self.opts.settle_ms {
                if let Some(prompt) = detect_approval(&screen) {
                    // Route it once (remote-control / MCP / etc.) before deciding.
                    if notified.as_ref() != Some(&prompt) {
                        if let Some(notify) = &self.approval_notifier {
                            notify(&prompt, remote_control_url(&screen).as_deref());
                        }
                        notified = Some(prompt.clone());
                    }
                    match self.approval_policy.decide(&prompt) {
                        // Leave it for an external actor; keep waiting for it to clear.
                        ApprovalChoice::Defer => {}
                        choice => {
                            self.answer_approval(choice)?;
                            self.wait_dialog_cleared()?;
                            notified = None;
                        }
                    }
                    continue;
                }
                notified = None;
            }
            // Deterministic completion: a new finished turn in the transcript.
            if submit_at.elapsed().as_millis() >= MIN_TURN_MS
                && self.transcript_end_turns() > baseline_end_turns
            {
                return Ok(());
            }
        }
    }

    /// After answering a dialog, wait (briefly) for it to leave the screen so
    /// the next loop iteration doesn't re-answer the same prompt.
    fn wait_dialog_cleared(&self) -> Result<(), TerminalError> {
        let start = Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(POLL_MS));
            if self.cancel_flag.load(Ordering::Relaxed) {
                return Err(TerminalError::Cancelled);
            }
            let (closed, screen) = {
                let s = self.state.lock().unwrap();
                (s.closed, s.screen_text())
            };
            if closed {
                return Err(TerminalError::Closed);
            }
            if detect_approval(&screen).is_none() {
                return Ok(());
            }
            if start.elapsed().as_millis() >= DIALOG_CLEAR_TIMEOUT_MS {
                return Ok(()); // best-effort; the main loop will re-evaluate
            }
        }
    }

    /// The current rendered screen as plain text.
    pub fn screen_text(&self) -> String {
        self.state.lock().unwrap().screen_text()
    }

    /// True when the session is currently showing the input box (cursor on the
    /// `❯` row). Exposed for instrumentation and custom drive loops.
    pub fn input_ready(&self) -> bool {
        self.state.lock().unwrap().input_ready()
    }

    /// The approval dialog currently on screen, if any. The supervision TUI and
    /// `ApprovalPolicy` use this to decide and answer (Phase C).
    pub fn detect_approval(&self) -> Option<ApprovalPrompt> {
        detect_approval(&self.screen_text())
    }

    /// The resolved session UUID (used to locate the transcript).
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Path to the transcript JSONL Claude Code writes for this session, if it
    /// exists yet.
    pub fn transcript_path(&self) -> Option<PathBuf> {
        find_transcript(&self.session_id)
    }

    /// A handle a caller can set to `true` to abort an in-progress wait (maps a
    /// psflow cancellation token onto the blocking drive loop).
    pub fn cancel_handle(&self) -> Arc<AtomicBool> {
        self.cancel_flag.clone()
    }

    /// Replace the session's cancel flag with a caller-owned one — lets a caller
    /// that constructs the session inside a blocking task still signal it from
    /// outside (the session is created after the flag).
    pub fn set_cancel_flag(&mut self, flag: Arc<AtomicBool>) {
        self.cancel_flag = flag;
    }

    /// Count of completed turns in the transcript (`end_turn` assistant entries).
    fn transcript_end_turns(&self) -> usize {
        self.transcript_path()
            .map(|p| count_end_turns(&p))
            .unwrap_or(0)
    }

    /// Set how approval dialogs are answered during a driven turn (default
    /// `DenyAll`). Headless callers typically also launch in a non-prompting
    /// permission mode so dialogs never appear; this is the backstop / the hook
    /// the supervision TUI plugs an `Ask` callback into.
    pub fn set_approval_policy(&mut self, policy: ApprovalPolicy) {
        self.approval_policy = policy;
    }

    /// Set a hook fired once per dialog (prompt + remote-control URL) to route it
    /// to a human or another surface. Pair with `ApprovalPolicy` returning
    /// `Defer` to wait for that external actor to answer.
    pub fn set_approval_notifier(&mut self, notifier: ApprovalNotifier) {
        self.approval_notifier = Some(notifier);
    }

    /// The session's remote-control URL if Claude Code is advertising one on
    /// screen (a human can open it to drive/answer this exact session).
    pub fn remote_control_url(&self) -> Option<String> {
        remote_control_url(&self.screen_text())
    }

    /// Answer the on-screen approval dialog per `choice`.
    pub fn answer_approval(&mut self, choice: ApprovalChoice) -> Result<(), TerminalError> {
        match choice {
            ApprovalChoice::Deny => self.send_key(Key::Escape),
            ApprovalChoice::Allow => self.select_option(1),
            ApprovalChoice::Select(n) => self.select_option(n),
            ApprovalChoice::Defer => Ok(()), // external actor resolves it
        }
    }

    /// Pick a 1-based dialog option (type its number, then confirm).
    fn select_option(&mut self, n: usize) -> Result<(), TerminalError> {
        self.type_text(&n.to_string())?;
        std::thread::sleep(Duration::from_millis(TYPE_ECHO_MS));
        self.send_key(Key::Enter)
    }

    /// Run one prompt and return the final screen text. Liveness/debug helper;
    /// prefer [`run_turn`](Self::run_turn) for the result payload.
    pub fn run_prompt(&mut self, prompt: &str) -> Result<String, TerminalError> {
        self.submit(prompt)?;
        self.wait_turn()?;
        Ok(self.screen_text())
    }

    /// Submit a prompt, drive the turn to completion (answering any approval
    /// dialogs per the policy), and return its result.
    ///
    /// The payload is read **deterministically** from the session transcript
    /// Claude Code writes (the last assistant message of the turn) — not from
    /// the rendered screen and not from asking the model to write a file. Screen
    /// scraping is only the fallback when the transcript can't be located.
    pub fn run_turn(&mut self, prompt: &str) -> Result<TurnResult, TerminalError> {
        let baseline_end_turns = self.transcript_end_turns();
        self.submit(prompt)?;
        self.drive_to_completion(baseline_end_turns)?;
        let screen = self.screen_text();
        if let Some(path) = find_transcript(&self.session_id) {
            if let Some(text) = last_assistant_text(&path) {
                return Ok(TurnResult {
                    result: text,
                    source: ResultSource::Transcript,
                    screen,
                });
            }
        }
        Ok(TurnResult {
            result: scrape_answer(&screen),
            source: ResultSource::Screen,
            screen,
        })
    }
}

/// Locate the transcript JSONL Claude Code writes for `session_id`. The file is
/// named `<session_id>.jsonl` under a per-project subdir of
/// `~/.claude/projects/`; we search by the known filename rather than depend on
/// the cwd-encoding scheme.
fn find_transcript(session_id: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let projects = PathBuf::from(home).join(".claude").join("projects");
    let file = format!("{session_id}.jsonl");
    for entry in std::fs::read_dir(&projects).ok()?.flatten() {
        let candidate = entry.path().join(&file);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Count completed turns in a transcript: `assistant` entries whose
/// `message.stop_reason` is `end_turn`. A turn finishes when this increases.
fn count_end_turns(path: &std::path::Path) -> usize {
    let Ok(content) = std::fs::read_to_string(path) else {
        return 0;
    };
    content
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v.get("type").and_then(|t| t.as_str()) == Some("assistant"))
        .filter(|v| {
            v.get("message")
                .and_then(|m| m.get("stop_reason"))
                .and_then(|s| s.as_str())
                == Some(END_TURN)
        })
        .count()
}

/// Extract the last assistant message's text from a transcript JSONL: the
/// concatenated `text` content blocks of the final `type:"assistant"` entry.
fn last_assistant_text(path: &std::path::Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let mut last: Option<String> = None;
    for line in content.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue; // skip a partially-written trailing line
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("assistant") {
            continue;
        }
        let Some(blocks) = v
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
        else {
            continue;
        };
        let text = blocks
            .iter()
            .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n");
        if !text.trim().is_empty() {
            last = Some(text.trim().to_string());
        }
    }
    last
}

impl Drop for ClaudeTerminalSession {
    fn drop(&mut self) {
        // Kill the child so the slave fd closes; the reader then sees EOF.
        let _ = self.child.kill();
        if let Some(handle) = self.reader.take() {
            let _ = handle.join();
        }
        // `writer` and `master` drop in field order afterwards.
        let _ = &self.master;
    }
}

/// Pull response lines (those `claude` prefixes with the `⏺` bullet) off the
/// rendered screen. Fallback when the result file is absent; returns the full
/// screen text when no bullet lines are found.
fn scrape_answer(screen: &str) -> String {
    let lines: Vec<String> = screen
        .lines()
        .filter(|l| l.trim_start().starts_with(RESPONSE_BULLET))
        .map(|l| {
            l.trim_start()
                .trim_start_matches(RESPONSE_BULLET)
                .trim()
                .to_string()
        })
        .collect();
    if lines.is_empty() {
        screen.trim().to_string()
    } else {
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_bytes_are_terminal_sequences() {
        assert_eq!(Key::Enter.bytes(), b"\r");
        assert_eq!(Key::Escape.bytes(), b"\x1b");
        assert_eq!(Key::CtrlC.bytes(), b"\x03");
        assert_eq!(Key::Up.bytes(), b"\x1b[A");
        assert_eq!(Key::Down.bytes(), b"\x1b[B");
    }

    #[test]
    fn session_options_defaults() {
        let o = SessionOptions::default();
        assert_eq!(o.command, "claude");
        assert_eq!(o.rows, DEFAULT_ROWS);
        assert!(o.args.is_empty());
        assert!(o.model.is_none());
    }

    #[test]
    fn session_options_builder() {
        let o = SessionOptions::new()
            .with_command("/usr/local/bin/claude")
            .with_model("claude-opus-4-8")
            .with_arg("--permission-mode")
            .with_arg("acceptEdits");
        assert_eq!(o.command, "/usr/local/bin/claude");
        assert_eq!(o.model.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(o.args, vec!["--permission-mode", "acceptEdits"]);
    }

    #[test]
    fn session_options_resume_and_env() {
        let o = SessionOptions::new()
            .with_session_id("abc")
            .with_resume(true)
            .with_env("ERGON_CLAUDE_DEPTH", "1");
        assert!(o.resume);
        assert_eq!(o.session_id.as_deref(), Some("abc"));
        assert_eq!(o.env, vec![("ERGON_CLAUDE_DEPTH".into(), "1".into())]);
        // Defaults stay off.
        assert!(!SessionOptions::default().resume);
        assert!(SessionOptions::default().env.is_empty());
    }

    #[test]
    fn scrape_answer_extracts_bullet_lines() {
        let screen = "❯ what is 6 times 7?\n\n⏺ 42\n\n✻ Worked for 1s\n❯";
        assert_eq!(scrape_answer(screen), "42");
    }

    #[test]
    fn scrape_answer_joins_multiple_bullets() {
        let screen = "⏺ line one\nnoise\n⏺ line two";
        assert_eq!(scrape_answer(screen), "line one\nline two");
    }

    #[test]
    fn scrape_answer_falls_back_to_full_screen() {
        let screen = "  no bullets here  ";
        assert_eq!(scrape_answer(screen), "no bullets here");
    }

    // Real captured marker lines: working has a spinner glyph + trailing `…`;
    // the completed summary shares the glyph but ends in a duration.
    const WORKING_SCREEN: &str = "\
❯ Count from 1 to 15

✽ Crunching…
                                            ● high · /effort
❯ ";
    const DONE_SCREEN: &str = "\
❯ Count from 1 to 15

⏺ Here you go:
  1. one — start

✻ Brewed for 3s
                                            ● high · /effort
❯ ";

    #[test]
    fn is_busy_detects_live_spinner_line() {
        assert!(is_busy(WORKING_SCREEN));
    }

    #[test]
    fn is_busy_false_on_completed_summary() {
        // `✻ Brewed for 3s` shares the glyph but lacks the trailing ellipsis.
        assert!(!is_busy(DONE_SCREEN));
    }

    #[test]
    fn is_busy_handles_spinner_frame_variants() {
        for glyph in ['✻', '✽', '✶', '✷'] {
            let screen = format!("body\n{glyph} Working… \n❯ ");
            assert!(is_busy(&screen), "frame {glyph} should read as busy");
        }
    }

    #[test]
    fn is_busy_ignores_long_content_ending_in_ellipsis() {
        // A content line (no spinner glyph) ending in an ellipsis is not busy.
        let screen = "⏺ Let me think about this in more detail and continue…\n❯ ";
        assert!(!is_busy(screen));
    }

    #[test]
    fn is_busy_false_on_idle_input_box() {
        let screen = "❯ \n  [ psflow | main ]";
        assert!(!is_busy(screen));
    }

    #[test]
    fn last_assistant_text_reads_final_assistant_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        // user turn, an early assistant turn, then the final assistant turn.
        let jsonl = concat!(
            r#"{"type":"user","message":{"role":"user","content":"hi"}}"#,
            "\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"thinking"}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"42"}]}}"#,
            "\n",
        );
        std::fs::write(&path, jsonl).unwrap();
        assert_eq!(last_assistant_text(&path).as_deref(), Some("42"));
    }

    #[test]
    fn last_assistant_text_skips_partial_trailing_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let jsonl = concat!(
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"done"}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"role":"assist"#, // truncated mid-write
        );
        std::fs::write(&path, jsonl).unwrap();
        assert_eq!(last_assistant_text(&path).as_deref(), Some("done"));
    }

    // The real captured approval dialog (Phase A fixture).
    const DIALOG_FIXTURE: &str = include_str!("testdata/approval_dialog.txt");

    #[test]
    fn detect_approval_parses_real_dialog() {
        let prompt = detect_approval(DIALOG_FIXTURE).expect("dialog detected");
        assert_eq!(
            prompt.question,
            "Do you want to create psflow_dialog_probe.txt?"
        );
        assert_eq!(prompt.options.len(), 3);
        assert_eq!(prompt.options[0], "Yes");
        assert_eq!(prompt.options[2], "No");
        assert_eq!(prompt.selected, 0); // `❯ 1. Yes`
    }

    #[test]
    fn detect_approval_ignores_numbered_content_list() {
        // A numbered list in response text has no highlighted `❯` option.
        let screen = "⏺ Here you go:\n  1. one — start\n  2. two — pair\n  3. three\n❯ ";
        assert!(detect_approval(screen).is_none());
    }

    #[test]
    fn detect_approval_none_on_idle_and_working() {
        assert!(detect_approval(WORKING_SCREEN).is_none());
        assert!(detect_approval("❯ \n  [ psflow | main ]").is_none());
    }

    #[test]
    fn approval_policy_allow_all_and_deny_all() {
        let prompt = detect_approval(DIALOG_FIXTURE).unwrap();
        assert_eq!(
            ApprovalPolicy::AllowAll.decide(&prompt),
            ApprovalChoice::Allow
        );
        assert_eq!(
            ApprovalPolicy::DenyAll.decide(&prompt),
            ApprovalChoice::Deny
        );
    }

    #[test]
    fn approval_policy_default_is_deny() {
        assert_eq!(
            ApprovalPolicy::default().decide(&detect_approval(DIALOG_FIXTURE).unwrap()),
            ApprovalChoice::Deny
        );
    }

    #[test]
    fn approval_policy_custom_receives_prompt() {
        // An allowlist-style policy: allow only when the question mentions "create".
        let policy = ApprovalPolicy::custom(|p: &ApprovalPrompt| {
            if p.question.contains("create") {
                ApprovalChoice::Allow
            } else {
                ApprovalChoice::Deny
            }
        });
        let prompt = detect_approval(DIALOG_FIXTURE).unwrap();
        assert_eq!(policy.decide(&prompt), ApprovalChoice::Allow);
    }

    #[test]
    fn remote_control_url_extracted_from_fixture() {
        assert_eq!(
            remote_control_url(DIALOG_FIXTURE).as_deref(),
            Some("https://claude.ai/code/session_011nupjtwjRdbKGF1XQBLnAw")
        );
    }

    #[test]
    fn remote_control_url_none_when_absent() {
        assert!(remote_control_url("❯ \n  [ psflow | main ]").is_none());
    }

    #[test]
    fn approval_choice_defer_is_noop_label() {
        // Defer must be a distinct variant the drive loop treats as "wait".
        assert_ne!(ApprovalChoice::Defer, ApprovalChoice::Allow);
    }

    #[test]
    fn detect_approval_tracks_selected_row() {
        let screen = "Proceed?\n  1. Yes\n❯ 2. No\n Esc to cancel";
        let prompt = detect_approval(screen).expect("detected");
        assert_eq!(prompt.selected, 1);
        assert_eq!(prompt.options, vec!["Yes", "No"]);
    }

    #[test]
    fn parse_numbered_option_extracts_label() {
        assert_eq!(parse_numbered_option("1. Yes").as_deref(), Some("Yes"));
        assert_eq!(
            parse_numbered_option("  3. No way").as_deref(),
            Some("No way")
        );
        assert_eq!(parse_numbered_option("not numbered"), None);
        assert_eq!(parse_numbered_option("1."), None);
    }

    #[test]
    fn count_end_turns_counts_only_end_turn_assistants() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let jsonl = concat!(
            r#"{"type":"user","message":{"role":"user","content":"hi"}}"#,
            "\n",
            r#"{"type":"assistant","message":{"role":"assistant","stop_reason":"tool_use","content":[]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"a"}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"b"}]}}"#,
            "\n",
        );
        std::fs::write(&path, jsonl).unwrap();
        // Two end_turn entries; the tool_use one is not counted.
        assert_eq!(count_end_turns(&path), 2);
    }

    #[test]
    fn last_assistant_text_joins_text_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let jsonl = concat!(
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"line one"},{"type":"tool_use","name":"X"},{"type":"text","text":"line two"}]}}"#,
            "\n",
        );
        std::fs::write(&path, jsonl).unwrap();
        assert_eq!(
            last_assistant_text(&path).as_deref(),
            Some("line one\nline two")
        );
    }
}
