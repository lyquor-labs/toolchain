//! Generated protobuf bindings and JSON-over-gRPC helpers.
//!
//! This crate contains generated Lyquor service message types and the adapter utilities used to
//! expose those messages over JSON-compatible HTTP routes. Node HTTP handlers, tooling clients, and
//! tests share these bindings so the transport layer and protobuf schema stay synchronized.
//! The hand-written helpers sit next to the generated modules because they define the conversion
//! boundary between protobuf messages and JSON route payloads.

/// JSON-over-gRPC framing helpers.
pub mod json_grpc;

/// Core protobuf support shared by generated modules.
pub mod core;

pub mod lyquid {
    pub mod v1 {
        #![allow(
            clippy::use_self,
            clippy::useless_borrows_in_formatting,
            reason = "prost/pbjson-generated code uses patterns rejected by workspace clippy"
        )]
        crate::include_proto!("lyquor.lyquid.v1");
    }
}

pub mod node {
    pub mod v1 {
        #![allow(
            clippy::use_self,
            clippy::useless_borrows_in_formatting,
            reason = "prost/pbjson-generated code uses patterns rejected by workspace clippy"
        )]
        crate::include_proto!("lyquor.node.v1");
    }
}
