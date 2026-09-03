# Generated WASM smoke tests

These tests exercise the JavaScript bindings emitted from `ant-core` without
providing a browser application or SDK. Build the package and run the tests
from the `ant-core` directory:

```bash
wasm-pack build --target web --out-dir wasm-tests/pkg --release . \
  --no-default-features --features browser-wasm
node --import ./wasm-tests/setup-wasm.mjs \
  --test ./wasm-tests/*.test.mjs
```

The generated `pkg/` directory is ignored. Browser-native integration,
examples, and end-to-end browser tests belong to `ant-client-browser-sdk`.
