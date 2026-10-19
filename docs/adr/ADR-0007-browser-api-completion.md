# ADR-0007: Browser manifest creation and pointer payment recovery

- **Status:** Proposed
- **Date:** 2026-10-15
- **Decision owners:** Mick
- **Reviewers:** Pending human engineering review
- **Supersedes:** none
- **Superseded by:** none
- **Related:** ADR-0004; ADR-0006

## Context

Browser applications need shared-core manifest exports and pointer operations with recoverable payments. Losing a page after broadcast must not authorize another payment.

## Decision Drivers

- Keep protocol validation and payment proofs in Rust.
- Retain durable evidence across failures and cancellation.
- Preserve compatibility with existing readers and wallet adapters.

## Considered Options

1. Expose raw bindings and leave each application to reconstruct the protocol.
2. Persist only paid proofs, leaving broadcast-to-confirmation failures unresolved.
3. Typed wrappers and pre-payment checkpoints with observation-only recovery (chosen).

## Decision

Export manifest construction through the existing shared format encoder, which refuses anything its decoder would refuse. Browser input is held to native writer policy: a root DataMap is embedded only up to the native size limit, and a size hint must match a root DataMap's exact size. The entry count is checked before any entry is parsed. Refusals are a typed `ManifestInputError`. A link carries each embedded map's shrunk form, so an embedded root map must be one an upload shrank and stored the wrapper records for, as every upload does.

Pointer writes take an optional trailing options object, `{ onPrepared, onSubmission, onPaid }`; unknown options are refused before anything is paid. The bare `onPaid` function earlier releases took is no longer accepted.

- `onPrepared` is awaited, for at most 30 seconds, with a versioned checkpoint string containing the signed record and verified plan (never the owner seed) before the wallet is invoked. Its byte fields are base64 and the plan MessagePack, so it is a fraction of the size of JSON number arrays. A failed or unsettled `onPrepared` prevents payment.
- `onSubmission` is handed each transaction the wallet reports broadcasting, through the `onSubmission` callback the wallet is given, and the wallet waits on it. The page therefore keeps the transaction before the wallet goes on, without every wallet having to.
- `onPaid` is handed `{ record, proof }`; its failure must not prevent storing an already paid state.

The settlement-refusal check is repeated immediately before the wallet is invoked. Nothing is paid unless the wallet is invoked, and every failure before that ends "payment was not submitted"; any later failure may have paid and is recovered from what the callbacks were handed.

Export a recovery function that builds the paid proof from the original plan and the wallet's receipt or reported submission: `transactionHashes` keyed by quote hash, which must name the checkpoint's quote, or a single `transactionHash` paying its one quote, exactly as the write accepts. It does not sign, quote or submit another transaction, and the transaction must be confirmed on chain first. Storing a paid state whose quotes are older than storers accept reports that the proof expired, rather than a generic store failure.

## Consequences

### Positive

- Applications can recover interrupted writes without an automatic second payment.
- Native and browser formats and pointer policy remain shared.

### Negative / Trade-offs

- Applications must supply durable persistence and an observation-only recovery provider for unresolved payments. A crash before submission may require explicit verification that nothing was broadcast.
- Checkpoints contain signed records and transaction evidence and must be treated as sensitive application data.

### Neutral / Operational

- Additive APIs except the pointer write options, which no longer accept a bare `onPaid` function; no network protocol change. New checkpoint schemas are versioned.
- SDK releases require the companion Rust changes on ant-client main.

## Validation

Test malformed manifests and their JavaScript input, canonical round trips, pending and paid recovery (from a receipt and from a reported submission), callback failures and timeouts, network mismatches, cancellation and real-browser paid pointer writes and streaming saves. Record exact revisions and limitations in a dated audit. Human engineering review is required.

## Notes for AI-assisted work

This ADR remains Proposed until human engineering review.
