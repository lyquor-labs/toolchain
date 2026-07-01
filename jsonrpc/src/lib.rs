//! JSON-RPC client primitives used by Lyquor Ethereum-facing code.
//!
//! The crate provides typed request/response traits, a transport connector, and a client handle for
//! one-shot requests and subscriptions. Ethereum sequencing and tooling code define their message
//! types against these traits so transport details stay separate from chain-specific call logic.
//! That keeps WebSocket request plumbing below the Ethereum modules while preserving typed
//! decoding at the call site.

/// JSON-RPC client connector and handle types.
pub mod client;
/// Shared JSON-RPC request and response types.
pub mod types;

// Re-export commonly used items
pub use jsonrpsee::core::traits::ToRpcParams;
