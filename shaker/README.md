# Shaker CLI

`shaker` is the Lyquid build, packaging, publishing, inspection, and deployment
tool. It turns a Lyquid Rust crate into a WASM image, generates the Ethereum
sequencer contract, packages the result as a Lyquid pack, optionally publishes
the pack to an OCI registry, and deploys the generated contract through a
Lyquor node.

Most workflows use a local devnet endpoint:

```bash
export LYQUOR_ENDPOINT=ws://127.0.0.1:10087/ws
```

When `--endpoint` is omitted, commands that talk to a node read
`LYQUOR_ENDPOINT` and otherwise fall back to `ws://localhost:10087/ws`.

## Running Shaker

Build the binary once:

```bash
cargo build -p shaker
target/debug/shaker --help
```

Or run through Cargo during development:

```bash
cargo run -p shaker -- --help
```

The examples below use `shaker`. Replace it with `cargo run -p shaker --` if
you have not put a built binary on `PATH`.

## Common Concepts

### Lyquid Manifest

`<LYQUID_MANIFEST>` is the `Cargo.toml` of a Lyquid crate, for example:

```bash
shaker build ldk/lyquid-examples/hello/Cargo.toml
```

Shaker builds the crate with the Lyquid WASM target and writes generated output
under `lyquid_tools_target/` in the current working directory:

| Output | Path |
| --- | --- |
| Raw WASM | `lyquid_tools_target/wasm32-unknown-unknown/{release,debug}/<crate_name>.wasm` |
| Solidity source | `lyquid_tools_target/solidity/<package>/<package>.sol` |
| EVM bytecode | `lyquid_tools_target/solidity/<package>/<package>.bin` |
| Lyquid pack | `lyquid_tools_target/{release,debug}/<package>/lyquid.pack` |

`--debug` uses Cargo's dev profile. Without it, Shaker builds release artifacts.

### Node Endpoints

`--endpoint <URL>` identifies the Lyquor node API endpoint. `ws://` and
`wss://` endpoints are typical because the node exposes JSON-RPC over
websocket, but Shaker converts supported endpoint schemes to the HTTP/gRPC
forms needed by individual commands.

`shaker serve` is intentionally localnet-only in v1 and accepts only plaintext
`ws://` or `http://` endpoints.

### OCI References

Publishing commands use OCI references such as:

```text
ghcr.io/lyquor-labs/lyquids/hello:dev
http://127.0.0.1:10087/lyquids:hello
```

Bare references use HTTPS. Prefix a reference with `http://` for an insecure
local registry.

Commands that read or write OCI registries accept:

| Option | Meaning |
| --- | --- |
| `--token <TOKEN>` | OAuth2-style token. Shaker sends it as basic auth user `oauth2accesstoken`. |
| `--username <USERNAME>` | Basic-auth username. |
| `--password <PASSWORD>` | Basic-auth password. Defaults to an empty password when only `--username` is supplied. |

If explicit credentials are not provided, Shaker tries Docker credential helper
configuration for the target registry and falls back to anonymous registry
access.

## Command Summary

| Command | Purpose |
| --- | --- |
| `shaker solidity` | Generate a Solidity sequencer contract from a Lyquid WASM file. |
| `shaker build` | Build a Lyquid crate and write a local `lyquid.pack`. |
| `shaker push` | Build a Lyquid crate and publish the pack without deploying it. |
| `shaker deploy` | Build or pull a pack, publish it if needed, and deploy the EVM contract. |
| `shaker list` | List deployed or hosted Lyquids from a node. |
| `shaker console` | Stream stdout from a running Lyquid. |
| `shaker serve` | Expose one Lyquid virtual host through a local HTTP proxy. |
| `shaker to-hex` | Convert a Lyquid ID to the corresponding EVM address string. |
| `shaker inspect` | Inspect a pack, WASM file, or OCI reference. |

## `shaker solidity`

```bash
shaker solidity [--is-bartender] <WASM_FILE>
```

Generates Solidity source for the sequencer contract represented by a compiled
Lyquid WASM module and writes it to stdout.

Options:

| Option | Meaning |
| --- | --- |
| `--is-bartender` | Generate the special bartender sequencer contract, including bartender library hooks. |

Use this command when you need to inspect or compile the generated contract
outside the normal `build` or `deploy` flow:

```bash
shaker solidity lyquid_tools_target/wasm32-unknown-unknown/release/hello.wasm \
  > SequenceBackend.sol
```

## `shaker build`

```bash
shaker build [--debug] [--is-bartender] <LYQUID_MANIFEST>
```

Builds a Lyquid crate, generates the sequencer Solidity, compiles the EVM
deployment bytecode, collects any `assets/` directory next to the manifest, and
writes a local Lyquid pack.

Options:

| Option | Meaning |
| --- | --- |
| `--debug` | Build with Cargo's dev profile instead of release. |
| `--is-bartender` | Build as the bartender Lyquid. |

Example:

```bash
shaker build ldk/lyquid-examples/hello/Cargo.toml
shaker inspect lyquid_tools_target/release/hello/lyquid.pack
```

## `shaker push`

```bash
shaker push [OPTIONS] <LYQUID_MANIFEST>
```

Builds a Lyquid crate and pushes the resulting pack without deploying it.

Options:

| Option | Meaning |
| --- | --- |
| `-r, --reference <REFERENCES>` | Comma-separated OCI references to push to. |
| `-e, --endpoint <URL>` | Node API endpoint used when `--reference` is omitted. |
| `--debug` | Build with Cargo's dev profile instead of release. |
| `--is-bartender` | Build as the bartender Lyquid. |
| `--token <TOKEN>` | Registry token. |
| `--username <USERNAME>` | Registry username. |
| `--password <PASSWORD>` | Registry password. |

When `--reference` is omitted, Shaker derives a local node registry target from
`--endpoint` and pushes to `lyquids/local:latest` on that host and port.

Push to one registry:

```bash
shaker push \
  --reference ghcr.io/lyquor-labs/lyquids/hello:dev \
  ldk/lyquid-examples/hello/Cargo.toml
```

Push the same pack to multiple registries:

```bash
shaker push \
  --reference ghcr.io/lyquor-labs/lyquids/hello:dev,http://127.0.0.1:10087/lyquids:hello \
  ldk/lyquid-examples/hello/Cargo.toml
```

## `shaker deploy`

```bash
shaker deploy [OPTIONS] [LYQUID_MANIFEST]
```

Deploys a Lyquid contract through a Lyquor node. The command accepts three
source modes:

| Source mode | Behavior |
| --- | --- |
| Manifest only | Build locally, push to the node registry derived from `--endpoint`, then deploy the built pack. |
| Manifest plus `--reference <REFERENCE>` | Build locally, push to that tag reference, pin the pushed digest, then deploy from the pinned OCI image. |
| `--reference <REFERENCE>` only | Pull deployment layers from an existing OCI image and deploy them. |

At least one source is required: either `[LYQUID_MANIFEST]`, `--reference`, or
both. `--debug` is valid only when a manifest is provided. When a manifest and
`--reference` are provided together, the reference must be tag-based because
digest-pinned references cannot be push targets.

Options:

| Option | Meaning |
| --- | --- |
| `-e, --endpoint <URL>` | Lyquor node API endpoint. |
| `-r, --reference <REFERENCE>` | OCI reference to deploy from, or tag reference to push to before deploy when a manifest is also supplied. |
| `--update <LYQUID_ID>` | Update an existing Lyquid by superseding its current contract. Conflicts with `--is-bartender`. |
| `--bartender <ADDR>` | Use a specific bartender contract address instead of resolving it from the node. Conflicts with `--is-bartender`. |
| `--is-bartender` | Deploy as the bartender Lyquid. Conflicts with `--bartender` and `--update`. |
| `--debug` | Build with Cargo's dev profile instead of release. |
| `-i, --input <HEX>` | Ethereum ABI-encoded constructor arguments for the Lyquid constructor. |
| `--private-key <HEX>` | EVM private key used to submit the deployment transaction. Defaults to the standard Anvil/Hardhat devnet key. |
| `-o, --output <FORMAT>` | Output format: `text` or `json`. Defaults to `text`. |
| `--token <TOKEN>` | Registry token. |
| `--username <USERNAME>` | Registry username. |
| `--password <PASSWORD>` | Registry password. |

Deploy the hello example to a local devnet:

```bash
shaker deploy \
  --endpoint ws://127.0.0.1:10087/ws \
  ldk/lyquid-examples/hello/Cargo.toml
```

Deploy through a shared registry and print machine-readable output:

```bash
shaker deploy \
  --endpoint "$LYQUOR_ENDPOINT" \
  --reference ghcr.io/lyquor-labs/lyquids/hello:dev \
  --output json \
  ldk/lyquid-examples/hello/Cargo.toml
```

JSON output has this shape:

```json
{"contract":"0x...","lyquid_id":"Lyquid-...","os_version":"0.1"}
```

Update an existing Lyquid:

```bash
shaker deploy \
  --endpoint "$LYQUOR_ENDPOINT" \
  --reference ghcr.io/lyquor-labs/lyquids/hello:dev \
  --update Lyquid-... \
  ldk/lyquid-examples/hello/Cargo.toml
```

Pass constructor input:

```bash
INPUT=$(cast abi-encode "constructor(string)" "hello")
shaker deploy \
  --endpoint "$LYQUOR_ENDPOINT" \
  --input "$INPUT" \
  ldk/lyquid-examples/hello/Cargo.toml
```

Use `shaker inspect <PACK_OR_WASM> --abi-json` to check constructor argument
types before encoding input.

## `shaker list`

```bash
shaker list [--endpoint <URL>] [--hosted-only] [--image-digest <DIGEST>] [--output text|json]
```

Lists Lyquids visible through a node. By default the command lists deployed
Lyquid IDs. `--hosted-only` limits the result to Lyquids currently hosted by
that node. Text output prints one Lyquid ID per line.

Use JSON output for automation:

```bash
shaker list \
  --endpoint "$LYQUOR_ENDPOINT" \
  --hosted-only \
  --output json
```

JSON output has this shape:

```json
{"lyquids":[{"contract":"0x...","image_digest":"sha256:...","lyquid_id":"Lyquid-...","lyquid_number":{"image":1,"var":0},"sequence_backend":"0x..."}]}
```

Filter hosted Lyquids by image digest:

```bash
shaker list \
  --endpoint "$LYQUOR_ENDPOINT" \
  --hosted-only \
  --image-digest sha256:... \
  --output json
```

`--image-digest` accepts the same `sha256:...` format emitted by
`shaker inspect --output json`.

## `shaker console`

```bash
shaker console [--endpoint <URL>] <LYQUID_ID>
```

Streams stdout from a running Lyquid to the local terminal. The stream exits
when the server closes it or when the process receives `SIGINT` or `SIGTERM`.

Example:

```bash
shaker console \
  --endpoint "$LYQUOR_ENDPOINT" \
  Lyquid-...
```

## `shaker serve`

```bash
shaker serve [--endpoint <URL>] [--listen <ADDR>] <LYQUID_ID>
```

Starts a local HTTP proxy for a Lyquid virtual host. This is useful on local
devnets where wildcard DNS is not configured for `<lyquid>.<node>` virtual
hosts. Shaker resolves the node ID, rewrites the `Host` header to the Lyquid's
virtual host, and forwards HTTP requests and upgrade tunnels to the node.

`GET /lyquid/info` is proxied too. Shaker rewrites the returned `node_base_url`
to the local proxy URL so browser clients continue using the localhost route.

Options:

| Option | Meaning |
| --- | --- |
| `-e, --endpoint <URL>` | Plaintext local node endpoint. Defaults to `LYQUOR_ENDPOINT` or `ws://localhost:10087/ws`. |
| `--listen <ADDR>` | Local bind address. Defaults to `127.0.0.1:8080`. |

Example:

```bash
shaker serve \
  --endpoint ws://127.0.0.1:10087/ws \
  --listen 127.0.0.1:8080 \
  Lyquid-...
```

Open the printed local URL to access the Lyquid's HTTP exports and static
assets.

## `shaker to-hex`

```bash
shaker to-hex <LYQUID_ID>
```

Converts a Lyquid ID to the EVM address representation used by Ethereum-facing
tools:

```bash
shaker to-hex Lyquid-ahjg4huwvban3bvhxue7rcbaw4mnltwyolhqa
```

## `shaker inspect`

```bash
shaker inspect [--abi-json] [--output text|json] [AUTH_OPTIONS] <PACK_WASM_OR_OCI_REFERENCE>
```

Inspects one of:

- a local raw WASM file,
- a local `lyquid.pack`,
- an OCI reference containing a Lyquid pack.

For local WASM and pack files, normal output includes metadata, LDK version,
constructor input shape, ABI entry count, and extracted Lyquid methods. For
remote OCI references, normal output includes pack metadata, constructor input
shape, and ABI entry count.

Options:

| Option | Meaning |
| --- | --- |
| `-o, --output <FORMAT>` | Output format for inspect metadata: `text` or `json`. Defaults to `text`; ignored by `--abi-json`. |
| `--abi-json` | Print only the Ethereum JSON ABI as canonical JSON. |
| `--token <TOKEN>` | Registry token. |
| `--username <USERNAME>` | Registry username. |
| `--password <PASSWORD>` | Registry password. |

Examples:

```bash
shaker inspect lyquid_tools_target/release/hello/lyquid.pack
shaker inspect lyquid_tools_target/release/hello/lyquid.pack --output json
shaker inspect lyquid_tools_target/release/hello/lyquid.pack --abi-json
shaker inspect ghcr.io/lyquor-labs/lyquids/hello:dev --output json
shaker inspect ghcr.io/lyquor-labs/lyquids/hello:dev --abi-json
```

JSON output uses `image_digest` for the resolved Lyquid image identity and does
not duplicate the same value under an additional digest field. OCI JSON output
also includes the original `reference` when the input used one.

## Troubleshooting

Version warnings mean the Shaker binary and the target node were built from
different Lyquor versions. Align the binary and node before investigating
runtime behavior.

LDK warnings mean the Lyquid crate was built with an LDK version that may be
incompatible with the Shaker binary's WASM processing. Update Shaker or the
Lyquid crate's `lyquid` dependency so their versions are compatible.

Constructor input errors include the expected Solidity tuple shape, such as
`(string,address[])`. Encode exactly those constructor arguments with an
Ethereum ABI encoder and pass the resulting hex string with `--input`.

Registry authentication failures usually mean Docker credential lookup did not
find usable credentials for the registry. Pass explicit `--token` or
`--username`/`--password` credentials to confirm the registry path and
permissions.
