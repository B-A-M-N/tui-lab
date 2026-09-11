//! Finding 60 compatibility note: the former giant MCP stdio E2E target has
//! been split along real product domains. The shared PTY/JSON-RPC harness now
//! lives in `tests/support/mcp.rs`; each `mcp_stdio_*` target is its own
//! Cargo integration test, so run one suite directly while iterating.
//!
//! The old `cargo test --test mcp_stdio_e2e -- --test-threads=1` selector no
//! longer names a binary; use `cargo test mcp_stdio_ -- --test-threads=1` to
//! exercise the complete stdio surface in the same sequential mode.
