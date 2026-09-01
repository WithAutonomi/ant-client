# Autonomi direct browser client

This web application is the browser-facing client for ADR-0009. It loads a
local testnet bootstrap manifest, connects directly to storage nodes over
WebRTC Direct, and performs closest-node lookup through Saorsa's shared Rust
lookup engine. `ant-core` is compiled to WASM and owns the WebRTC peer
connections and data channels, wire framing, authenticated HELLO, connection
pool, Kademlia walk, native self-encryption, quote and commitment verification,
payment planning, record upload/download, public DataMap serialization,
whole-file reconstruction, random-access range decryption, and BLAKE3 content
verification. JavaScript drives the page, browser file/save and service-worker
APIs, and Ethers transaction submission.

The node-side WebRTC Direct listener and testnet manifest API live in the
`ant-node-web-support` sibling repository. No HTTP gateway performs lookup or
proxies file bytes.

## Requirements

- Rust 1.88 or newer for the node's Saorsa WebRTC transport.
- The `wasm32-unknown-unknown` Rust target.
- `wasm-pack` 0.15.
- Node.js 20.19+ or 22.12+.
- A current browser implementing `RTCPeerConnection`. The WebRTC Direct dialer
  extracts the stable certificate fingerprint from each node multiaddress.

Nodes serialize these addresses from the native `saorsa_core::MultiAddr`
representation; the shared Rust client parser consumes that canonical string
form in WASM.

Native QUIC and browser WebRTC uploads use the same `ant-core` scheduling
policy: the adaptive store limiter, a 64 MiB in-flight source-record budget,
four-of-seven close-group quorum with one-for-one fallback targets, and three
whole-record retries with 500 ms, 1 s, and 2 s backoff. The browser transport
adapts WebRTC requests to that shared engine rather than maintaining a separate
upload algorithm in JavaScript.

Install the WASM build tools once if needed:

```bash
rustup target add wasm32-unknown-unknown
cargo install wasm-pack --version 0.15.0 --locked
```

## Run the browser-enabled testnet

From `ant-node-web-support`:

```bash
cargo run --bin ant-devnet -- \
  --preset minimal \
  --base-port 23000 \
  --webrtc-direct \
  --webrtc-direct-base-port 24000 \
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
custom file may be up to 1 GB (1,000,000,000 bytes) in this local launcher.
Uploads are self-encrypted incrementally in a dedicated worker and stage each
encrypted record in IndexedDB, so the page does not hold the plaintext or the
complete encrypted file in memory. The browser needs enough temporary origin
storage for approximately the file size; staged records are removed when the
upload finishes or fails. Complete-file downloads remain memory-bound; use the
random-access reader for large media.

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
2. **Connect and use as bootstrap** parses the first node multiaddress,
   performs a pinned WebRTC Direct `HELLO`, and verifies its ML-DSA identity
   signature.
3. **Find closest** runs Saorsa's iterative lookup engine and WebRTC Direct
   query batches entirely in Rust/WASM.
4. Under **Paid public file upload**, choose a file, paste the funded private
   key printed by ant-devnet, then select **Pay and upload file**. A dedicated
   worker runs Rust/WASM streaming self-encryption and stages one encrypted
   record at a time in IndexedDB. The Rust network client then performs
   closest-node selection, quote/commitment verification, payment-total
   calculation, and storage while loading only active records. A narrow
   JavaScript callback uses Ethers for token approval and the wallet
   transaction; Rust verifies the callback's reported total before continuing.
   The key remains in the form for repeat demo uploads, and the resulting
   public DataMap address is placed in the download field.
5. **Download and save file** accepts any public DataMap address, fetches the
   DataMap and encrypted chunks from direct closest storage nodes, reconstructs
   the whole file, verifies BLAKE3, opens the browser save flow, and retains a
   **Save again** link. Manifest metadata supplies the original filename when
   available; an address-only download uses an address-derived `.bin` name.
6. For a browser-supported video, select **Prepare video stream** and then use
   the native video controls. A Rust `BrowserFileReader` resolves the root
   DataMap and fetches only records overlapping each requested byte range. A
   thin service-worker adapter presents those decrypted ranges to the native
   `<video>` element as `206 Partial Content`, including suffix ranges used to
   locate MP4 metadata and disjoint ranges used for seeking.

The endpoint field also supports manifest-free bootstrap. Paste one complete
WebRTC Direct multiaddress and click **Connect and use as bootstrap**. After
the authenticated HELLO succeeds, that address replaces the Rust network
client's seed list and the authenticated HELLO response supplies the public
payment configuration. The same address can be supplied when opening the page:

```text
http://127.0.0.1:5173/?endpoint=<URL-encoded-WebRTC-Direct-multiaddr>
```

When `endpoint` is present, startup skips the default local-manifest fetch.
Public file downloads and byte-range streams need only the DataMap address;
ant-core fetches the authenticated DataMap and derives its file size and chunk
descriptors. A manifest remains useful for optional filename, MIME type, and
whole-file hash metadata.
Traversal to independently deployed nodes bootstraps from this one address:
nodes propagate their WebRTC Direct multiaddresses in Saorsa's authenticated
DHT address sets, and the browser verifies each discovered peer during HELLO.

The browser receives the DataMap and all encrypted file bytes from UDP
24000-24004 over ICE, DTLS, SCTP, and data channels, not from the manifest
server on TCP 25000. The manifest server is bootstrap metadata only.
Video playback keeps a bounded 32 MiB encrypted-record cache and does not
reconstruct the complete file. Playback still depends on the browser supporting
the file's container and codecs. The page must remain open because it owns the
authenticated WebRTC associations used by the service worker's range response.

For a LAN test, start the node devnet with `--host <LAN_IPV4>` and serve Vite with
`npm run dev -- --host 0.0.0.0`. Change the manifest URL in the
page to `http://<LAN_IPV4>:25000/api/browser-manifest.json`.

## Verify the client

```bash
npm test
npm run build
```

Run the real-browser WebRTC Direct smoke test with:

```bash
npm run test:browser
```

The first run downloads Playwright's pinned headless Chromium build. The test
then starts the sibling `ant-node-web-support` minimal devnet and local Anvil on
dedicated test ports, serves this application, loads its real manifest, and
requires an authenticated HELLO over a browser `RTCDataChannel`. Set
`ANT_NODE_DIR` when the patched node checkout is not at the default sibling
path. Set `ANT_WEBRTC_SMOKE_LOG=warn` (or another tracing level) to include
native devnet diagnostics while troubleshooting a failure.

Both commands build the WASM package automatically. To validate the Rust
boundary directly:

```bash
cargo check -p ant-core --target wasm32-unknown-unknown \
  --no-default-features --features browser-wasm
cargo test -p ant-core --lib browser::
```

The tests cover Saorsa's shared lookup engine and generic query driver, fixed-width IDs,
bidirectional binary framing, browser manifest/payment validation, signed
quote verification including the native Keccak-256 EVM quote hash, and a Rust
`self_encryption 0.36` wire vector with native/WASM round-trip and tamper
verification.
Cross-repository live verification additionally starts the node testnet,
downloads all public-file records through WebRTC Direct, reconstructs the
original bytes, pays real quotes on local Anvil, uploads an incompressible file
large enough to require a nested DataMap through the ordinary node payment
validator, and downloads and verifies it again.
The same live-browser test opens that nested file through `BrowserFileReader`
and verifies exact disjoint and suffix HTTP byte ranges through the service
worker, exercising seek and end-of-file metadata access without a gateway.

## Library boundary

The reusable Autonomi behavior belongs to `ant-core`, the library crate in this
repository. Its cross-platform `browser` modules own bootstrap manifest and
public-file types, WebRTC Direct addresses and framing, HELLO identity
authentication, BLAKE3/self-encryption, Saorsa lookup, storage quote and native
commitment verification, pricing and payment planning, and complete public
upload/download workflows. It also exposes a bounded random-access
`BrowserFileReader` for media and other seekable consumers. The `browser-wasm`
host adapter owns
`RTCPeerConnection` and `RTCDataChannel` through `web-sys`, allowing any web
application to use `BrowserNetworkClient` without copying the Autonomi protocol
into JavaScript.

The application JavaScript owns only capabilities tied to the page or the
selected wallet stack: DOM events, browser `File`/save-picker/worker/IndexedDB/
service-worker APIs, and Ethers contract calls. The service worker does not
connect to nodes;
it translates native media byte-range requests into calls on the page-owned
Rust reader. This is also the deliberate extension seam: another web app can
provide its own UI and wallet callback while sharing all network and Autonomi
logic from the Rust library.

The patched `ant-protocol 2.3.1` exposes a `portable` feature that omits Tokio,
EVM, Saorsa transport, and `saorsa-pqc`. Both native and WASM clients now import
the same storage commitment type, signing encodings, quote hash, price curve,
and ML-DSA-65 verifier from `ant-protocol`; only that crate selects the native
or FIPS-204 verification backend.

The local testnet manifest is intentionally unsigned bootstrap material. A
production deployment still needs ML-DSA-signed endpoint records, exceptional
certificate-rotation recovery, network dissemination, relayed WebRTC, and
production traffic quotas as specified by ADR-0009.
