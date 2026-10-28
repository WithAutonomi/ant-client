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
argument authenticates through the network client's retained pool before the
download, matching the SDK's connect lifecycle. These are live
network samples; alternate baseline and candidate runs and retain failures.

The trace export is test-only and must not be included in production packages.

To time the complete SDK connection and download path, including its bundled
seed profile, serve a built SDK directly in an isolated Chromium context:

```sh
node sdk-startup.mjs ../../../ant-client-browser-sdk/dist 3 candidate > sdk-startup.jsonl
```

The probe stops after three content chunks by default. Append `full` to fetch the
entire file with `download()` and report its verified byte count and hash:

```sh
node sdk-startup.mjs ../../../ant-client-browser-sdk/dist 1 candidate full > full-download.jsonl
```

Full mode has a 30-minute limit and includes reconstruction and verification,
without saving another copy to disk. `seconds` includes SDK initialization
and connection, while `downloadSeconds` starts when the download is invoked.
Preserve a baseline SDK `dist` directory before rebuilding, then alternate the
two directories to compare the same bundled seed set. This probe uses an isolated
local HTTP server and does not require a running application or wallet.

## Local paid integration

Run `npm ci`, `npx playwright install chromium`, then `npm test` in this directory.
First build the bindings from `ant-core`:

```sh
wasm-pack build --target web --out-dir wasm-tests/pkg --release . --no-default-features --features browser-wasm
```

The test starts seven real ant-nodes and an isolated Anvil chain, then uses
Chromium's real WebRTC implementation. `ANT_NODE_DIR` overrides the default
sibling `ant-node-web-support` checkout. CI checks out the exact browser-enabled
node revision in [`node-revision`](node-revision); use that revision locally to
reproduce CI. Without `ANT_NODE_DIR`, the devnet refuses to start unless the
sibling checkout is that revision without tracked edits, and builds it with
`--locked`. The pin runs the same code as a node release already on the
network, so the suite checks this client against older nodes. It is currently
`26129a26`, the ant-node `main` commit that adds only the Rust 1.99 fixes to
the 0.21.0 release: its one runtime change renames `fetch_update` to
`try_update`, the same method. CI builds the pin without `-D warnings`, because
each new toolchain can add warnings to an old release. This integration pin is
separate from the native `ant-node` dependency in `ant-core/Cargo.toml`. Update
the pin only with a passing paid browser integration run against the
replacement. In CI, the run fails unless every test ran and passed:
`run-guard.js` fails a run with a skipped or unfinished test, and `.only` is
rejected. Anvil must be installed and available on PATH.

The harness authenticates through `BrowserNetworkClient.connect()` and retains
that pool for the upload, makes one batched storage payment,
injects a post-payment byte-loader failure, resumes on a new client without
another payment, and downloads using only the public DataMap address. No RTC
or node RPC mocks are installed. It also checks that a closed network client
rejects reconnection. Temporary node data is removed on shutdown.
The suite uses local funds only and verifies that the RPC belongs to the local
Anvil devnet before invoking the wallet.

The pointer recovery test pays once on the local chain, then loses the page
after the payment confirms and before the write stores the state. It rebuilds
the paid state from the `onPrepared` checkpoint and the transaction
`onSubmission` kept, stores it without paying again, and checks that a write
refused before the wallet says "payment was not submitted". It also builds a
manifest in the page and writes the `.ant` file and link to its test output
directory, for decoding with `ant manifest show`.

The swarm test (`swarm.spec.js`) uploads a 6 MiB file, reads it in one page,
and keeps that page's client open in the file's room. A second page, in its
own browser context, then reads the file. Against nodes that advertise
`swarm_rendezvous`, the two meet through the file's rendezvous nodes, which
also answer their STUN, and the second page must take every data record
from the first (the DataMap, whose root names the room, comes from the
nodes). Against the pinned node, which predates
swarm rooms, the second page must read from the nodes with no link and no
swarm message sent. Run it against a node with swarm rooms by pointing
`ANT_NODE_DIR` at one.

## Large-file disk streaming

`large-file.mjs` is an opt-in check, not run in CI, that a file larger than
4 GiB streams to disk in real Chromium. Build the production bindings as above
and a release CLI (`cargo build --release --bin ant` in the workspace root),
start `node start-devnet.mjs` in another terminal, then run:

```sh
node large-file.mjs [path to ant]
```

It writes the 4,303,347,835-byte plaintext of the
[`large-file.json`](../wasm-tests/fixtures/large-file.json) fixture to a
temporary file and uploads it with `ant`, under a throwaway home so the upload
stays out of your upload history, and checks it lands on the fixture's
address. Chromium then opens the file, reads across 4 GiB, and pipes all of it
into the origin private file system with `pipeTo`. The run checks the BLAKE3
hash and the stored size and bytes, and that a complete in-memory download over
a 64 MiB budget fails and points to `pipeTo`. It exits non-zero if any check
fails and needs about 5 GB of free disk. The file is one repeated byte, so its
records compress to about 67 KB: the run checks correctness and local
decryption and writing, not network throughput.
