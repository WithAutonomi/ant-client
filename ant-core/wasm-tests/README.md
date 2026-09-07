# Generated WASM smoke tests

These tests exercise the JavaScript bindings emitted from `ant-core` without
providing a browser application or SDK. Build the package and run the tests
from the `ant-core` directory:

```bash
wasm-pack build --target web --out-dir wasm-tests/pkg --release . \
  --no-default-features --features browser-wasm,test-utils
node --import ./wasm-tests/setup-wasm.mjs \
  --test ./wasm-tests/*.test.mjs
```

The generated `pkg/` directory is ignored. Browser-native integration,
examples, and end-to-end browser tests belong to `ant-client-browser-sdk`.

The test build enables `test-utils` for a deterministic mock node. Transport
regressions use a mock `RTCPeerConnection` while running the real PQ handshake,
record encryption, lookup, quote verification, and upload implementation.
Production packages should enable only `browser-wasm`.

Upload fixtures follow native policy: seven initial peers, authenticated
witness views, a supported paid median, and four successful stores. Tests
cover inconsistent views, partial existing-holder votes, one payable quote,
even quote counts, wider PUT fallback, and reuse of the paid proof. Structured
remote errors are tested separately from an actual PUT-response deadline.

Download regressions cover known holders omitted by failed discovery, complete
discovery failure, bounded fallback, and unchanged BLAKE3 verification. The mock
retains successful PUT payloads across connection replacements so a full upload,
download and random-access read can run through the real WASM path, including
when the storage holders stop answering FIND_NODE.

The shared read-engine regressions in `src/client_engine/{read,files}.rs` run
with `cargo test -p ant-core --lib client_engine`. They cover native retry timing,
peer deduplication and fallback bounds, typed fetch errors, multi-level DataMaps
on a current-thread runtime, verified records, and range boundaries. Generated
WASM additionally downloads a native shrunk DataMap through the mock network and
checks media ranges across chunk boundaries and EOF.
