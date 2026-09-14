# Real browser integration

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
