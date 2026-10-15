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

Export manifest construction through the existing shared format encoder, which refuses anything its decoder would refuse. Browser input is held to native writer policy: a root DataMap is embedded only up to the native size limit, and a size hint must match a root DataMap's exact size.

Pointer writes take a trailing options object, `{ onPrepared, onPaid }`; a bare function is still accepted as `onPaid`, and unknown options are refused. `onPrepared` is awaited with a versioned checkpoint containing the signed record and verified plan before the wallet is invoked. The settlement-refusal check is repeated after it, immediately before the wallet is invoked. The application durably keeps that checkpoint, and its wallet keeps each broadcast's transaction hash: only the wallet observes the broadcast, and the core's `onSubmission` records nothing for a pointer write. Export a recovery function that builds the paid proof from the original plan and a receipt naming the transaction by quote hash (`transactionHashes`). It checks that the receipt pays the checkpoint's quote, without signing, quoting or submitting another transaction. A failed pre-payment checkpoint prevents payment; a failed paid checkpoint must not prevent storing an already paid state.

## Consequences

### Positive

- Applications can recover interrupted writes without an automatic second payment.
- Native and browser formats and pointer policy remain shared.

### Negative / Trade-offs

- Applications must supply durable persistence and an observation-only recovery provider for unresolved payments. A crash before submission may require explicit verification that nothing was broadcast.
- Checkpoints contain signed records and transaction evidence and must be treated as sensitive application data.

### Neutral / Operational

- Additive APIs; no network protocol change. New checkpoint schemas are versioned.
- SDK releases require the companion Rust changes on ant-client main.

## Validation

Test malformed manifests, canonical round trips, pending and paid recovery, callback failures, network mismatches, cancellation and real-browser paid pointer writes and streaming saves. Record exact revisions and limitations in a dated audit. Human engineering review is required.

## Notes for AI-assisted work

This ADR remains Proposed until human engineering review.
