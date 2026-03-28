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
