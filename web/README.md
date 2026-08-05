# Autonomi direct browser client

This web application is the browser-facing client for ADR-0009. It loads a
local testnet bootstrap manifest, connects directly to storage nodes over
WebTransport, and performs closest-node lookup through Saorsa's shared Rust
lookup engine. The portable part of `ant-core` is compiled to WASM and drives
the Kademlia walk, native self-encryption, public DataMap serialization,
reconstruction, and BLAKE3 content verification. The thin JavaScript layer
executes WebTransport requests, retrieves and uploads records, uses the browser
wallet/payment APIs, and drives the page.

The node-side WebTransport listener and testnet manifest API live in the
`ant-node-web-support` sibling repository. No HTTP gateway performs lookup or
proxies file bytes.

## Requirements

- Rust 1.88 or newer for the node's optional `wtransport` dependency.
- The `wasm32-unknown-unknown` Rust target.
- `wasm-pack` 0.15.
- Node.js 20.19+ or 22.12+.
- A current browser implementing WebTransport certificate hashes. The client
  extracts them from node multiaddresses; users do not enter hashes separately.

Nodes serialize these addresses from the native `saorsa_core::MultiAddr`
representation; the JavaScript parser consumes that canonical string form.

Install the WASM build tools once if needed:

```bash
rustup target add wasm32-unknown-unknown
cargo install wasm-pack --version 0.15.0 --locked
```

## Run the browser-enabled testnet

From `ant-node-web-support`:

```bash
cargo run --features webtransport-poc --bin ant-devnet -- \
  --preset minimal \
  --base-port 23000 \
  --webtransport \
  --webtransport-base-port 24000 \
  --serve-port 25000 \
  --enable-evm \
  --enable-logging
```

This starts five native nodes on UDP 23000-23004 and five direct browser
listeners on UDP 24000-24004. It also:

- self-encrypts the built-in `autonomi-browser-testnet.txt` and publishes its
  encrypted chunks and public DataMap through the ordinary node PUT handler
  using devnet-prepaid cache entries;
- exposes all direct node multiaddresses, including their certificate pins and
  peer IDs, at
  `http://127.0.0.1:25000/api/browser-manifest.json`;
- includes the public DataMap address, plaintext BLAKE3 hash, resolved chunk
  metadata, filename, size, and replica count in that manifest.
- starts local Anvil payment contracts and prints a disposable funded wallet
  private key. The browser manifest contains only public RPC and contract
  addresses, never the private key.

Confirm that `HELLO.payment.rpc_url` is a loopback Anvil URL. An
`https://arb1.arbitrum.io/rpc` value means an older devnet was started without
`--enable-evm`; its disposable local key cannot fund uploads there.

Pass `--public-file /path/to/file` to publish another file instead. The built-in
file is generated as 5 MiB so the demo exercises whole-file reconstruction. A
custom file may be up to 64 MiB in this local launcher.

## Run the site

From this directory:

```bash
npm ci
npm run dev
```

The `predev` hook builds `ant-core` with `--no-default-features --features
browser-wasm` into the ignored `web/pkg/` directory before Vite starts. No
published or hand-maintained JavaScript copy of the Rust algorithms is used.

Open `http://127.0.0.1:5173`. The page automatically loads the testnet
manifest from port 25000 and fills in the default file:

1. **Load testnet** refreshes the manifest and direct multiaddress catalog.
2. **Connect** parses the first node multiaddress and performs a pinned
   WebTransport `HELLO`.
3. **Find closest** runs Saorsa's iterative lookup engine in WASM, with
   JavaScript executing its WebTransport query batches.
4. Under **Paid public file upload**, choose a file, paste the funded private
   key printed by ant-devnet, then select **Pay and upload file**. Rust/WASM
   performs encryption and DataMap generation; quote/commitment verification,
   approval, and payment happen locally in the browser. The key field is
   cleared immediately and the resulting public DataMap address is placed in
   the download field.
5. **Download and save file** opens the browser save flow, fetches the public
   DataMap and encrypted chunks from direct closest storage nodes, reconstructs
   the whole file, verifies BLAKE3, and retains a **Save again** link.

The browser receives the DataMap and all encrypted file bytes from UDP
24000-24004 over HTTP/3, not from the manifest server on TCP 25000. The
manifest server is bootstrap metadata only.

For a LAN test, start the node devnet with `--host <LAN_IPV4>`, serve Vite with
`npm run dev -- --host 0.0.0.0`, and add the exact site Origin with
`--webtransport-origin http://<LAN_IPV4>:5173`. Change the manifest URL in the
page to `http://<LAN_IPV4>:25000/api/browser-manifest.json`.

## Verify the client

```bash
npm test
npm run build
```

Both commands build the WASM package automatically. To validate the Rust
boundary directly:

```bash
cargo check -p ant-core --target wasm32-unknown-unknown \
  --no-default-features --features browser-wasm
cargo test -p ant-core --lib browser::tests
```

The tests cover Saorsa's shared lookup engine and generic query driver, fixed-width IDs,
bidirectional binary framing, browser manifest/payment validation, signed
quote verification including the native Keccak-256 EVM quote hash, and a Rust
`self_encryption 0.36` wire vector with native/WASM round-trip and tamper
verification.
Cross-repository live verification additionally starts the node testnet,
downloads all public-file records through WebTransport, reconstructs the
original bytes, pays a real quote on local Anvil, uploads a fresh record
through the ordinary node payment validator, and reads it back.

## Current boundary

JavaScript currently owns WebTransport stream handling, Ethers payment
submission, ML-DSA quote verification, and DOM/save APIs. Rust/WASM owns the
complete iterative lookup loop; JavaScript implements only its transport batch
callback.
`ant-protocol 2.3.1` still enables native Saorsa/Tokio networking and cannot be
linked into a browser WASM build; a future transport-free feature can move the
remaining quote and multiaddress verification into Rust.

The local testnet manifest is intentionally unsigned bootstrap material. A
production deployment still needs ML-DSA-signed endpoint records, certificate
overlap/rotation, network dissemination, relayed WebTransport, and production
traffic quotas as specified by ADR-0009.
