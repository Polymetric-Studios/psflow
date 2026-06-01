# Project task runner
# https://github.com/casey/just

# Build the project
build:
    cargo build

# Run the project
run *ARGS:
    cargo run -- {{ARGS}}

# Run a named graph through the personal runner (composio on PATH).
# Example: just graph sheets-search --input query=INV
graph name *ARGS:
    PATH="$HOME/.composio:$PATH" cargo run --quiet --bin psflow-run --features runtime -- {{name}} {{ARGS}}

# Install psflow-run to ~/.cargo/bin (stable path for scheduled jobs).
install:
    cargo install --path . --bin psflow-run --features runtime --locked

# Composio API key, read from the macOS keychain (falls back to the env var).
# Store it once:  security add-generic-password -U -a "$USER" -s composio-api-key -w 'sk_...'
_composio_key := '${COMPOSIO_API_KEY:-$(security find-generic-password -a "$USER" -s composio-api-key -w 2>/dev/null)}'

# Create a Composio trigger via the SDK (prints the ti_… id). Needs the key in
# the keychain (above) and `npm i @composio/core`.
# Example: just trigger-create --user-id default --slug GOOGLESHEETS_CELL_RANGE_VALUES_CHANGED --sheet <id> --range "Sheet1!A1:C20"
trigger-create *ARGS:
    COMPOSIO_API_KEY="{{_composio_key}}" node scripts/trigger_create.mjs {{ARGS}}

# Listen to Composio triggers (SDK websocket) and run a handler graph per event.
# Needs the key in the keychain (above), `npm i @composio/core`, and a trigger id.
# Example: just triggers sheet-summary ti_xxx
triggers handler trigger_id:
    COMPOSIO_API_KEY="{{_composio_key}}" \
    PATH="$HOME/.composio:$PATH" \
    PSFLOW_LISTEN_CMD="node scripts/triggers_listen.mjs --trigger-id {{trigger_id}}" \
      cargo run --quiet --bin psflow-run --features runtime -- {{handler}} --listen

# Run all tests
test:
    cargo test

# Run tests with output
test-verbose:
    cargo test -- --nocapture

# Type-check without building
check:
    cargo check

# Format code
fmt:
    cargo fmt

# Run clippy lints
lint:
    cargo clippy -- -D warnings

# Clean build artifacts
clean:
    cargo clean

# Build WASM package for debugger
wasm:
    cd crates/psflow-wasm && wasm-pack build --target web --out-dir ../../debugger/pkg

# Run debugger dev server
debugger: wasm
    cd debugger && npx vite

# Build debugger for production
debugger-build: wasm
    cd debugger && npx vite build

release ref:
    ergon run release --input ref={{ref}}
post-release ref:
    @echo "[post-release] no project-specific actions configured for ref={{ref}}"
    
