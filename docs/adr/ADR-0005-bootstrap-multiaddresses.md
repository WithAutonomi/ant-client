# ADR-0005: Bootstrap multiaddresses and portable network defaults

- **Status:** Proposed
- **Date:** 2026-09-15
- **Decision owners:** Engineering team
- **Reviewers:** Pending human engineering review
- **Supersedes:** none
- **Superseded by:** none
- **Related:** ADR-0004; browser-sdk ADR-0002

## Context

Native release bootstrap configuration contains socket addresses. Browsers need
WebRTC Direct multiaddresses with certificate and peer identity pins. Mainnet
WebRTC seeds are not yet available, but their distribution and selection should
be ready before deployment.

## Decision Drivers

- Preserve transport and identity information in bootstrap configuration.
- Share payment defaults with evmlib and expose them through WASM.
- Allow WebRTC seed publication without another code change.
- Never dial QUIC seeds through the browser or fall back from custom networks.

## Considered Options

1. Maintain unrelated frontend constants.
2. Share a bundled, transport-separated multiaddress resource and Rust resolver.
3. Fetch bootstrap configuration from a remote service at runtime.

## Decision

Use option 2. `ant-core/resources/bootstrap_peers.toml` contains `quic` and
`webrtc` arrays of multiaddresses. Native release packaging installs this file
under the existing filename. Legacy `peers` socket-address files remain readable.
The native CLI passes QUIC multiaddresses, including optional peer identities,
to the transport. Existing SocketAddr library entry points remain available.

Portable Rust validates each list with the shared transport parsers, rejects
wrong transports and duplicates, and provides mainnet payment identity and RPC
from evmlib. WASM exports only the WebRTC seed list and payment defaults.
No remote configuration service or node-provided RPC URL is introduced.

The WebRTC array starts empty. Reading defaults succeeds; attempting a default
browser connection fails clearly until seeds are supplied. Add verified complete
WebRTC multiaddresses to this resource, rebuild WASM, and publish the SDK to
activate defaults. Explicit browser profiles stay authoritative. Public seed
availability and certificate-rotation overlap are deployment responsibilities.

## Consequences

### Positive

- A single packaged resource covers both transports without losing pins.
- Applications can use Rust-owned mainnet payment defaults.
- Missing WebRTC seeds do not require placeholder or guessed addresses.

### Negative / Trade-offs

- TOML parsing is included in the portable core; it is an existing dependency.
- Older client versions cannot read the new resource format. Installers already
  preserve existing user configuration; new releases require the updated loader.
- Changing bundled seed certificates requires an updated artifact and overlap
  with previously published certificate pins.

### Neutral / Operational

- No wire or stored-data change. Bootstrap configuration gains a new format with
  a legacy reader. Native multiaddress APIs and WASM defaults are additive.
- SDK payment adapters still use explicit application/wallet providers; the
  exported default RPC can be passed when constructing those providers.
- Review and test browser seeds before publishing a mainnet-ready claim.

## Validation

Test legacy parsing, IPv4/IPv6 QUIC pins, transport isolation, malformed and
duplicate seeds, empty WebRTC defaults, and evmlib payment identity. Exercise the
generated WASM and SDK default/custom connection paths. Run native CLI/core
checks and a real-browser isolated devnet probe; record limitations in the SDK
audit. Acceptance requires human engineering review.
