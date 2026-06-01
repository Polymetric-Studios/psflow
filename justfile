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
    
