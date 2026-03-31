# Project task runner
# https://github.com/casey/just

# Build the project
build:
    cargo build

# Run the project
run *ARGS:
    cargo run -- {{ARGS}}

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
