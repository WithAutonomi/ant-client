# Autonomi direct browser client

This web application is the browser-facing client for ADR-0009. It loads a
local testnet bootstrap manifest, connects directly to storage nodes over
WebTransport, performs the XOR closest-node lookup in JavaScript, retrieves a
public DataMap and every encrypted file chunk, reconstructs the complete file,
and verifies its whole-file BLAKE3 hash before saving it.

The node-side WebTransport listener and testnet manifest API live in the
`ant-node-web-support` sibling repository. No HTTP gateway performs lookup or
proxies file bytes.

## Requirements

- Rust 1.88 or newer for the node's optional `wtransport` dependency.
- Node.js 20.19+ or 22.12+.
- A current browser implementing WebTransport certificate hashes. The client
  extracts them from node multiaddresses; users do not enter hashes separately.

Nodes serialize these addresses from the native `saorsa_core::MultiAddr`
representation; the JavaScript parser consumes that canonical string form.

## Run the browser-enabled testnet

From `ant-node-web-support`:

```bash
cargo run --features webtransport-poc --bin ant-devnet -- \
  --preset minimal \
  --base-port 23000 \
  --webtransport \
  --webtransport-base-port 24000 \
  --serve-port 25000 \
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

Pass `--public-file /path/to/file` to publish another file instead. The built-in
file is generated as 5 MiB so the demo exercises whole-file reconstruction. A
custom file may be up to 64 MiB in this local launcher.

## Run the site

From this directory:

```bash
npm ci
npm run dev
```

Open `http://127.0.0.1:5173`. The page automatically loads the testnet
manifest from port 25000 and fills in the default file:

1. **Load testnet** refreshes the manifest and direct multiaddress catalog.
2. **Connect** parses the first node multiaddress and performs a pinned
   WebTransport `HELLO`.
3. **Find closest** runs the iterative lookup in the browser.
4. **Download and save file** opens the browser save flow, fetches the public
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

The tests cover fixed-width IDs, XOR ordering, response framing, browser
manifest validation, and a native `self_encryption 0.36` compatibility vector.
Cross-repository live verification additionally starts the node testnet,
downloads all public-file records through WebTransport, and reconstructs the
original bytes.

## Current boundary

The local testnet manifest is intentionally unsigned bootstrap material. A
production deployment still needs ML-DSA-signed endpoint records, certificate
overlap/rotation, network dissemination, relayed WebTransport, and production
traffic quotas as specified by ADR-0009.
