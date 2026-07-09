# ADR-0003: Client-side earned-reward eligibility gate

- **Status:** Proposed
- **Date:** 2026-07-10
- **Decision owners:** Anselme (@grumbach)
- **Reviewers:** <pending>
- **Supersedes:** none
- **Superseded by:** none
- **Related:** ant-node ADR-0005 (earned reward eligibility — the canonical design and node-side tally/report plumbing); ADR-0002 (client-side fallback); ADR-0001 (adopt ADRs)

## Context

The full design and rationale for earned reward eligibility live in the
**ant-node ADR-0005** document. This ADR records only the **client's** half: the
eligibility predicate and where the gate sits in quote collection. The node
produces signed audit reports; the client is the only party that decides
eligibility and acts on it. Nothing here changes payee selection, payment
construction, or on-chain verification.

This is **earned future eligibility, not slashing**: the client may decline to
*pay* a node that has not yet earned its place, but no settled payment is ever
removed and no penalty is applied on-chain.

## Decision

- **Collect signed reports, verify before trusting.** For each quote request the
  client collects the responders' signed audit reports. A report is only trusted
  after its ML-DSA signature verifies against the responder's peer-bound quote
  key, its echoed nonce matches this request's fresh random nonce (replay
  protection), and it is within the byte/row/day caps (an over-cap report is
  dropped whole before parse).

- **The eligibility predicate.** A subject quoter is eligible when **more than
  half of its opinionated reporters vouch** for it (floor 3). An opinion is a
  reporter that carries a row for the subject; reporters with no row abstain. A
  row vouches when it is unfenced, unconvicted, and shows ≥ D distinct days
  (default 7) each carrying a pass at a proven size covering the quoted size
  within a slack factor (default 2×), inside a trailing window (default 14 days),
  most recent covering day within the recency bound (default 1 day). The subject
  never vouches for itself. `fenced` and `convicted` rows count as non-vouching
  opinions (denominator, not numerator); a convicted row stays sticky for the dues
  period, matching the node semantics — the client and node MUST agree on this.

- **Two-tier collection gate, then fallback.** The client fills its quote set /
  merkle candidate pool eligible-first at the quoted size. If too few clear the
  size bar (e.g. fast network-wide growth), it prefers nodes with a clean audited
  week at **any** size (the dues tier), then falls back to today's ungated rules.
  The fallback is a **degraded-security mode** — it is logged and surfaced as a
  metric so operators can see when the network is running ungated. The dues tier
  still excludes fresh identities and caught cheaters; only the size requirement is
  relaxed, and ADR-0004 still forces a current-size proof at payment.

- **Observe-only first.** Enforcement is off by default. The client runs
  observe-only (logging would-be exclusions) so shadow eligibility can be
  calibrated against churn / NAT / newcomer cases before any hard gate is enabled.

- **Policy is client-side.** D, slack, recency, floor, and the enforce switch are
  client policy (env-tunable), so they can be retuned without a node release. The
  trailing window and conviction stickiness are node-side constants.

## Consequences

- Payee selection and payment verification are untouched: the gate only narrows
  the candidate list before the existing median/closest selection runs.
- The client does per-quote signature/nonce/cap verification on collected reports;
  bounded work, all client-side.
- Honest newcomers are never marked malicious: they are simply not yet
  size-eligible and are still selectable via the dues tier or the ungated
  fallback. Suppressing a qualified node requires capturing more than half its
  per-address reporter set — the same neighbourhood-capture boundary the network
  already accepts.

## Validation

The client half of ADR-0005 is correct only if the following hold; these are
exercised by the `e2e_adr0005` suite and validated against the node's canonical
ADR-0005 criteria:

- **Selection unchanged when nobody is gated.** With enforcement off (observe-only)
  the client selects exactly as today; the gate only logs would-be exclusions.
- **Eligible-first without stalling.** With enough eligible quoters the client pays
  an eligible one; when too few are size-eligible it falls back to the dues tier
  and then to today's ungated rules, and uploads still succeed.
- **The fallback still excludes the two things the gate is for.** Under the dues
  tier a fresh no-history node and a convicted/fenced node are never selected.
- **Reports are trusted only after verification.** A report with a bad signature,
  wrong request nonce, or over-cap size contributes nothing to the decision.
- **Payee selection and payment verification are unaffected**, confirmed by the
  existing payment/merkle e2e suites passing unchanged.

See ant-node ADR-0005 for the full trade-off analysis, validation criteria, and
the node-side tally/report design.
