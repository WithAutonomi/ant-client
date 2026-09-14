# Architecture Decision Records

This directory contains Architecture Decision Records (ADRs) for this repository.

## Rules

1. Use `ADR-NNNN-short-title.md` names with four-digit numbers.
2. New ADRs start as `Proposed`.
3. `Accepted` ADRs are immutable. If the decision changes, create a new ADR and mark the old ADR as superseded by reference, not by editing its accepted content.
4. Architectural PRs must add or update an ADR before merge.
5. Reviews must check ADR correctness, evidence, trade-offs, and compliance — not just presence.

## Template

Use [`TEMPLATE.md`](./TEMPLATE.md).

## Tooling

See [`TOOLING.md`](./TOOLING.md) for `adrs`, `adr-kit`, and AI harness setup.

## Index

- [ADR-0001: Adopt Architecture Decision Records](./ADR-0001-adopt-architecture-decision-records.md)
- [ADR-0002: Client fallback for full-node shunning](./ADR-0002-client-fallback-for-full-node-shunning.md)
- [ADR-0003: Multi-batch external Merkle signing](./ADR-0003-multi-batch-external-merkle-signing.md)
- [ADR-0004: Direct browser read client](./ADR-0004-direct-browser-read-client.md)
