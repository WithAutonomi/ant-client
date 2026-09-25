# Real browser integration

For read-only startup profiling against an existing network, build with
`--no-default-features --features browser-wasm,test-utils` and run:

```sh
node read-startup.mjs ../wasm-tests/pkg /path/to/seeds.json single 3 > startup.jsonl
```

The JSON file must contain an array of complete WebRTC Direct seed addresses.
`single` uses the first seed (matching File Vault's single-address API); `all`
uses the entire array. Each run starts a fresh Chromium context, downloads the
public file `134e4537ad1b2e29f0dc48f8e025a560989e91055ebf1c66bca2208ca8bba889`,
and stops after three verified content chunks or 180 seconds. JSONL includes
connection setup, authentication, lookup and GET timing. An optional final `sdk`
argument also performs and closes an authentication probe before constructing
the cold download client, matching the SDK's connect lifecycle. These are live
network samples; alternate baseline and candidate runs and retain failures.

The trace export is test-only and must not be included in production packages.

## Local paid integration

Run `npm ci`, `npx playwright install chromium`, then `npm test` in this directory.
First build the bindings from `ant-core`:

```sh
wasm-pack build --target web --out-dir wasm-tests/pkg --release . --no-default-features --features browser-wasm
```

The test starts seven real ant-nodes and an isolated Anvil chain, then uses
Chromium's real WebRTC implementation. `ANT_NODE_DIR` overrides the default
sibling `ant-node-web-support` checkout. CI checks out the node revision pinned
by `ant-core/Cargo.toml`. Anvil must be installed and available on PATH.

The harness authenticates through HELLO, makes one batched storage payment,
injects a post-payment byte-loader failure, resumes on a new client without
another payment, and downloads using only the public DataMap address. No RTC
or node RPC mocks are installed. Temporary node data is removed on shutdown.
The suite uses local funds only and verifies that the RPC belongs to the local
Anvil devnet before invoking the wallet.
