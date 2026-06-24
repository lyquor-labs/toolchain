# Lyquor Toolchain

Build, packaging, publishing, and deployment tooling for [Lyquor](https://lyquor.xyz/)
Lyquids — most notably **`shaker`**, the CLI that compiles a Lyquid to WASM, packs it
as an OCI artifact, and deploys it to a Lyquor node.

This repository is a generated, browsable mirror of the `toolchain/` ring of the
Lyquor monorepo. It is published alongside the [LDK](https://github.com/lyquor-labs/ldk)
so the tools that compile and deploy your Lyquids are open for inspection and building
from source rather than only as prebuilt binaries.

## Crates

| Crate | Description |
| --- | --- |
| `shaker` | Lyquid build, packaging, publishing, and deployment CLI |
| `lyquor-wasm` | WASM metadata extraction and binary manipulation |
| `lyquor-oci` | OCI packaging and registry helpers for Lyquid artifacts |
| `lyquor-proto` | gRPC/JSON service definitions for the Lyquor node API |
| `lyquor-jsonrpc` | JSON-RPC client and server |
| `lyquor-eth` | Shared Ethereum helpers |
| `lyquor-cli` | CLI tracing/console and build-version helpers |

## Build

```sh
cargo build --release
```

`shaker` invokes `cargo` to cross-compile Lyquids to `wasm32-unknown-unknown`, so the
WASM target (pinned in `rust-toolchain.toml`) is required to *use* it, and `solc` is
downloaded on demand for Solidity sequencer generation.

## Install shaker

```sh
cargo install shaker
```

Or use the prebuilt binaries via the installer documented in the
[LDK](https://github.com/lyquor-labs/ldk).
