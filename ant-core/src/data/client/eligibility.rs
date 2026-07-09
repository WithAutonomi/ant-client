//! ADR-0005 earned reward eligibility — the client-side predicate.
//!
//! Quote responses carry each responder's signed **audit report**: its raw
//! per-peer tally of audit outcomes (passes per day, the largest commitment
//! size that passed, and two row-local markers). This module aggregates a
//! quorum of those reports into the payee-selection decision: a quoter is
//! **eligible** when enough of its neighbours testify it has been audited
//! clean for about a week at the size it is monetizing.
//!
//! Everything here is policy over reported facts — no scores, no weights, no
//! decay. The parameters are client-side and env-tunable, so retuning the
//! gate never needs a fleet release. Reports are consumed at collection time
//! and dropped; nothing here is forwarded to storers.
//!
//! Default mode is **observe-only**: the gate logs what it would do without
//! changing selection. `ADR5_ENFORCE=1` turns enforcement on (used by the
//! local testnet; production flips only once telemetry shows honest nodes
//! reliably qualify).

use ant_protocol::payment::{AuditReport, AuditReportDay, AuditReportRow};
use ant_protocol::transport::PeerId;
use std::collections::HashMap;
use tracing::{debug, info, warn};

/// Client eligibility policy over reported audit facts (ADR-0005).
///
/// Day units are the node's tally-day buckets: a fixed 86400 s (one day) in
/// production, so the client's day-based thresholds line up with the ages the
/// node stamps into its reports.
#[derive(Debug, Clone, Copy)]
pub struct EligibilityPolicy {
    /// A qualifying observer must report at least this many distinct days
    /// that each carry a covering pass (D — the dues, ~a week).
    pub min_distinct_days: u16,
    /// The most recent qualifying day must be at most this old (the window's
    /// head must be covered: a node that stops answering audits drops out
    /// within ~a day).
    pub max_recency_days: u16,
    /// Size coverage slack: a day qualifies only if its largest passed
    /// commitment size × this multiplier ≥ the quoted count (grind small,
    /// cash in big fails day-coverage; honest growth clears in an audit
    /// cycle).
    pub size_slack: u32,
    /// Days older than this are ignored entirely — the client's own window
    /// bound, independent of what a reporter chooses to ship.
    pub window_days: u16,
    /// Floor of the eligibility bar (ADR-0005 v4): a subject always needs at
    /// least this many qualifying vouches, however few reporters know it.
    pub quorum_floor: usize,
    /// Enforce (change selection) vs observe-only (log would-be decisions).
    pub enforce: bool,
}

impl Default for EligibilityPolicy {
    fn default() -> Self {
        // NOTE: `window_days` can only NARROW the node's `TALLY_WINDOW_DAYS`, it
        // cannot widen it; and there is NO client knob for conviction
        // stickiness (node-side `CONVICTION_STICKY_DAYS`). "Retunable without a
        // node release" is true for D / recency / slack / floor / enforce ONLY.
        Self {
            min_distinct_days: env_u64_clamped("ADR5_D_DAYS", 7, 0, u64::from(u16::MAX)) as u16,
            max_recency_days: env_u64_clamped("ADR5_RECENCY_DAYS", 1, 0, u64::from(u16::MAX))
                as u16,
            size_slack: env_u64_clamped("ADR5_SIZE_SLACK", 2, 1, u64::from(u32::MAX)) as u32,
            window_days: env_u64_clamped("ADR5_WINDOW_DAYS", 14, 0, u64::from(u16::MAX)) as u16,
            quorum_floor: env_u64_clamped("ADR5_QUORUM", 3, 1, u64::from(u32::MAX)) as usize,
            enforce: std::env::var("ADR5_ENFORCE").is_ok_and(|v| v == "1" || v == "true"),
        }
    }
}

/// Read `name` as a u64, clamped to `[lo, hi]`, defaulting to `default`.
/// WARNs (not silently) when a set value is non-numeric (falls back to
/// default) or out of range (clamped) — otherwise an operator who typed
/// `ADR5_SIZE_SLACK=0` or a typo would get the opposite of what they meant
/// with no signal.
fn env_u64_clamped(name: &str, default: u64, lo: u64, hi: u64) -> u64 {
    let Some(raw) = std::env::var(name).ok() else {
        return default.clamp(lo, hi);
    };
    let Some(parsed) = raw.parse::<u64>().ok() else {
        warn!("ADR-0005: {name}={raw:?} is not a number; using default {default}");
        return default.clamp(lo, hi);
    };
    let clamped = parsed.clamp(lo, hi);
    if clamped != parsed {
        warn!("ADR-0005: {name}={parsed} out of range [{lo}, {hi}]; clamped to {clamped}");
    }
    clamped
}

/// Does one report day cover a quote of `quoted_key_count` under `policy`'s
/// size slack? (`max_passed_key_count * slack >= quoted`.) A baseline quote
/// (`quoted == 0`) is trivially covered by any pass.
#[must_use]
fn day_covers_size(
    day: &AuditReportDay,
    quoted_key_count: u32,
    policy: &EligibilityPolicy,
) -> bool {
    u64::from(day.max_passed_key_count) * u64::from(policy.size_slack)
        >= u64::from(quoted_key_count)
}

/// Shared history check for one observer's row: unfenced, unconvicted, and
/// enough DISTINCT recent days that satisfy `day_qualifies`.
///
/// This is the single place the ADR-0005 "≥ D distinct covering days, freshest
/// within recency, inside the window" rule lives. The per-day `day_qualifies`
/// predicate is what makes it size-aware or size-relaxed:
///   - size-eligible ([`row_qualifies`]): `day_covers_size` — the day must
///     also cover the quoted size, exactly as v4.
///   - dues-eligible ([`row_has_dues`], v5 tier 2): `|_| true` — any pass day.
///
/// Crucially the size check is applied PER DAY, INSIDE the distinct-day and
/// recency computation — it is NOT a separate whole-row predicate. Factoring
/// it out (`has_dues && covers_size_somewhere`) would be WRONG: a row with 7
/// small passes at ages 0-6 plus one covering pass at age 13 has dues and a
/// covering day, yet must NOT be size-eligible (only one covering day, and it
/// is stale). Keeping size a day predicate preserves v4 semantics exactly.
///
/// Duplicate `age_days` collapse to one distinct day, so a malformed report
/// cannot multiply its own testimony. Pass counts are not summed — a per-day
/// `passes != 0` guard is the only pass requirement (there is no total-passes
/// threshold).
fn row_meets_history(
    row: &AuditReportRow,
    policy: &EligibilityPolicy,
    day_qualifies: impl Fn(&AuditReportDay) -> bool,
) -> bool {
    if row.fenced || row.convicted {
        return false;
    }
    let mut qualifying_ages: std::collections::HashSet<u16> = std::collections::HashSet::new();
    for day in &row.days {
        if day.passes == 0 || day.age_days > policy.window_days {
            continue;
        }
        if day_qualifies(day) {
            qualifying_ages.insert(day.age_days);
        }
    }
    let freshest_age = qualifying_ages.iter().min().copied();
    qualifying_ages.len() >= usize::from(policy.min_distinct_days)
        && freshest_age.is_some_and(|age| age <= policy.max_recency_days)
}

/// Does `row` qualify the subject for a quote **at `quoted_key_count`** (the
/// SIZE-eligible tier)? Unfenced, unconvicted, and ≥ D distinct recent days
/// that each cover the quoted size. Identical semantics to v4.
#[must_use]
pub fn row_qualifies(
    row: &AuditReportRow,
    quoted_key_count: u32,
    policy: &EligibilityPolicy,
) -> bool {
    row_meets_history(row, policy, |day| {
        day_covers_size(day, quoted_key_count, policy)
    })
}

/// Does `row` show the subject has done its **dues** (the size-RELAXED tier 2,
/// ADR-0005 v5)? Same history rule as [`row_qualifies`] but with NO size
/// coverage: ≥ D distinct recent pass days at ANY size. A fresh identity (no
/// pass days) and a caught cheater (fenced/convicted) still fail — the
/// fallback drops only the size requirement, never the dues or the catch.
///
/// Every size-eligible row is dues-eligible: its covering days are a subset of
/// its any-size pass days, so it has ≥ D such days and its freshest any-size
/// day is no older than its freshest covering day.
#[must_use]
pub fn row_has_dues(row: &AuditReportRow, policy: &EligibilityPolicy) -> bool {
    row_meets_history(row, policy, |_day| true)
}

/// Which tier a subject cleared (ADR-0005 v5 two-tier fallback).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Majority of judges vouch AT THE QUOTED SIZE — full eligibility.
    Size,
    /// Majority of judges vouch for the DUES (a clean audited week at any
    /// size) but too few cover the quoted size — the v5 size-relaxed fallback.
    Dues,
}

/// One subject's eligibility, once it has cleared at least the dues bar. Every
/// entry is dues-eligible; `tier == Size` additionally means it cleared the
/// size bar. `size_vouches`/`dues_vouches` are the qualifying counts (dues is
/// a superset, so `dues_vouches >= size_vouches`).
#[derive(Debug, Clone, Copy)]
pub struct SubjectEligibility {
    /// Highest tier the subject cleared.
    pub tier: Tier,
    /// Judges vouching at the quoted size.
    pub size_vouches: usize,
    /// Judges vouching for the dues (any size).
    pub dues_vouches: usize,
}

/// Aggregate collected reports into the eligible subset of `subjects`, tagged
/// by the highest tier each cleared (ADR-0005 v5).
///
/// `reports` is keyed by the REPORTER (the quote responder that shipped it,
/// already signature-verified and nonce-bound). Judges of a subject are the
/// reporters — never the subject itself — that carry a row for it (an
/// "opinion"); reporters with no row ABSTAIN and are not counted. The bar is
/// the same majority-of-opinions bar for both tiers; only the qualifying
/// numerator differs (size-covering vs dues-only). The returned map contains
/// every subject that cleared at least the dues bar; `tier` distinguishes
/// which.
///
/// The map is keyed by `(PeerId, quoted_size)`, NOT peer alone: eligibility is
/// size-specific, and the same peer may appear at two quoted sizes in one
/// candidate set. Keying on the pair keeps each `(peer, size)` evaluated and
/// filtered against its own quote, so a size-eligible entry for a small quote
/// can never leak Size-tier admission onto a larger quote for that same peer.
#[must_use]
pub fn eligible_subjects(
    subjects: &[(PeerId, u32)],
    reports: &HashMap<PeerId, AuditReport>,
    policy: &EligibilityPolicy,
) -> HashMap<(PeerId, u32), SubjectEligibility> {
    let mut out: HashMap<(PeerId, u32), SubjectEligibility> = HashMap::new();
    for (subject, quoted_count) in subjects {
        let subject_bytes = *subject.as_bytes();
        // Majority-of-opinions bar (v4). An "opinion" is a reporter (never the
        // subject itself) whose report carries a row for the subject at all;
        // reporters with no row ABSTAIN. The SAME opinion denominator and bar
        // apply to both tiers — a row that vouches for dues but not size is
        // still a non-vouching SIZE opinion, so it counts in the size
        // denominator. Only the qualifying numerator differs. Excluding a peer
        // therefore requires non-vouching rows from over half its opinionated
        // reporters — genuine catches (sticky convictions hold the catcher's
        // row in the denominator for a dues period) or a collusion the size of
        // the neighbourhood-capture boundary. Abstention cannot suppress.
        let mut opinions = 0usize;
        let mut size_vouches = 0usize;
        let mut dues_vouches = 0usize;
        for (reporter, report) in reports {
            if reporter == subject {
                continue;
            }
            let Some(row) = report
                .rows
                .iter()
                .find(|row| row.subject_peer_id == subject_bytes)
            else {
                continue;
            };
            opinions += 1;
            // dues is a superset of size, so check size first and count both.
            if row_qualifies(row, *quoted_count, policy) {
                size_vouches += 1;
                dues_vouches += 1;
            } else if row_has_dues(row, policy) {
                dues_vouches += 1;
            }
        }
        let bar = policy.quorum_floor.max(opinions / 2 + 1);
        let tier = if size_vouches >= bar {
            Some(Tier::Size)
        } else if dues_vouches >= bar {
            Some(Tier::Dues)
        } else {
            None
        };
        // Structured per-subject decision line — the local-testnet runner
        // greps these to build the eligibility timeline.
        info!(
            target: "adr5::eligibility",
            subject = %subject,
            quoted_count,
            vouches = size_vouches,
            dues_vouches,
            opinions,
            bar,
            eligible = tier == Some(Tier::Size),
            dues_eligible = tier.is_some(),
            "eligibility decision"
        );
        if let Some(tier) = tier {
            out.insert(
                (*subject, *quoted_count),
                SubjectEligibility {
                    tier,
                    size_vouches,
                    dues_vouches,
                },
            );
        }
    }
    out
}

/// Which tier `gate_quoter_set` selected from (ADR-0005 v5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GateTier {
    /// Selected the size-eligible subset (full eligibility, `need` met).
    Size,
    /// Selected the dues-eligible subset (size relaxed, v5 fallback).
    Dues,
    /// Neither tier filled `need`: today's ungated rules apply. Degraded
    /// security — earned eligibility is bypassed.
    Ungated,
}

impl GateTier {
    fn label(self) -> &'static str {
        match self {
            GateTier::Size => "size-eligible",
            GateTier::Dues => "dues-eligible (size relaxed)",
            GateTier::Ungated => "ungated",
        }
    }
}

/// Apply the ADR-0005 gate to a quoter set at collection time (v5 two-tier
/// fallback).
///
/// Returns the peers to select from. When enforcing, three tiers are tried in
/// order until one fills `need`:
///   1. **size-eligible** — subjects a majority of judges vouch for AT THE
///      QUOTED SIZE. Whoever wins downstream selection is fully eligible.
///   2. **dues-eligible (size relaxed)** — if too few are size-eligible,
///      subjects a majority vouch for on DUES (a clean audited week at any
///      size). This is the v5 fallback for fast network-wide growth, where
///      nobody has a full week at the newly grown size. It STILL excludes
///      fresh identities (no dues) and caught cheaters (fenced/convicted), so
///      it drops only the size requirement; ADR-0004 independently forces a
///      current-size proof at payment, so a node can't be paid for a size it
///      doesn't hold. Every size-eligible node is in this tier too.
///   3. **ungated** — neither tier fills `need`: today's rules apply. This is
///      a degraded-SECURITY mode (earned eligibility bypassed), logged so
///      operators can see how often the network runs ungated.
///
/// Observe-only (`!policy.enforce`) never changes selection; it logs which
/// tier it WOULD have used so the rollout calibration can distinguish all
/// three outcomes.
///
/// NOTE (pre-existing v4 limitation, accepted): `size_count >= need` does not
/// guarantee downstream single-node selection succeeds — the witnessed path
/// searches for a close-group whose median issuer has enough witness support
/// and can still return `None`. This tiering filters the candidate set; it
/// does not drive that selector through the tiers. Merkle has no equivalent
/// issue once the selected tier holds ≥ `need` candidates.
#[must_use]
pub fn gate_quoter_set<T>(
    items: Vec<T>,
    subject_of: impl Fn(&T) -> (PeerId, u32),
    reports: &HashMap<PeerId, AuditReport>,
    policy: &EligibilityPolicy,
    need: usize,
    context: &str,
) -> Vec<T> {
    let subjects: Vec<(PeerId, u32)> = items.iter().map(&subject_of).collect();
    let eligible = eligible_subjects(&subjects, reports, policy);
    let total = items.len();
    // dues set is a superset of the size set; count both.
    let size_count = eligible.values().filter(|e| e.tier == Tier::Size).count();
    let dues_count = eligible.len();

    // The tier that WOULD be selected (also what observe-only reports).
    let tier = if size_count >= need {
        GateTier::Size
    } else if dues_count >= need {
        GateTier::Dues
    } else {
        GateTier::Ungated
    };

    if !policy.enforce {
        info!(
            "ADR-0005 observe-only [{context}]: would use {} — size-eligible {size_count}/{total}, \
             dues-eligible {dues_count}/{total} (need {need}, floor {}, reports {}); \
             selection unchanged",
            tier.label(),
            policy.quorum_floor,
            reports.len(),
        );
        return items;
    }

    match tier {
        GateTier::Size => {
            info!(
                "ADR-0005 gate [{context}]: size-eligible — selecting among {size_count} \
                 (dropped {} not size-eligible)",
                total - size_count,
            );
            items
                .into_iter()
                .filter(|item| {
                    eligible
                        .get(&subject_of(item))
                        .is_some_and(|e| e.tier == Tier::Size)
                })
                .collect()
        }
        GateTier::Dues => {
            // Too few size-eligible; fall back to the dues-eligible subset,
            // which INCLUDES the size-eligible ones. No further ranking:
            // single-node re-sorts by price/distance and merkle by closeness,
            // and preferring the dues tier over ungated IS the "most dues
            // done" preference the ADR calls for.
            info!(
                "ADR-0005 gate [{context}]: DUES-ELIGIBLE (size relaxed) — only \
                 {size_count} size-eligible < need {need}; selecting among {dues_count} \
                 with a clean audited week at any size (dropped {} without dues)",
                total - dues_count,
            );
            items
                .into_iter()
                .filter(|item| eligible.contains_key(&subject_of(item)))
                .collect()
        }
        GateTier::Ungated => {
            info!(
                "ADR-0005 gate [{context}]: UNGATED (degraded security) — only \
                 {size_count} size-eligible / {dues_count} dues-eligible < need {need}; \
                 paying under today's rules, earned eligibility bypassed"
            );
            debug!(
                "ADR-0005 ungated [{context}] eligible set: {:?}",
                eligible
                    .keys()
                    .map(|(peer, size)| format!("{peer}@{size}"))
                    .collect::<Vec<_>>()
            );
            items
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use ant_protocol::payment::AuditReportDay;

    fn policy() -> EligibilityPolicy {
        EligibilityPolicy {
            min_distinct_days: 7,
            max_recency_days: 1,
            size_slack: 2,
            window_days: 14,
            quorum_floor: 3,
            enforce: true,
        }
    }

    fn clean_row(subject: [u8; 32], days: u16, passes_per_day: u32, size: u32) -> AuditReportRow {
        AuditReportRow {
            subject_peer_id: subject,
            days: (0..days)
                .map(|age| AuditReportDay {
                    age_days: age,
                    passes: passes_per_day,
                    max_passed_key_count: size,
                })
                .collect(),
            fenced: false,
            convicted: false,
        }
    }

    #[test]
    fn a_week_of_covering_passes_qualifies() {
        let row = clean_row([1; 32], 7, 3, 1000);
        assert!(row_qualifies(&row, 1000, &policy()));
        assert!(
            row_qualifies(&row, 2000, &policy()),
            "2x slack covers a doubled quote"
        );
        assert!(
            !row_qualifies(&row, 2001, &policy()),
            "beyond slack the size is not covered"
        );
    }

    #[test]
    fn too_few_days_or_stale_fails() {
        assert!(!row_qualifies(
            &clean_row([1; 32], 6, 5, 1000),
            1000,
            &policy()
        ));
        let mut stale = clean_row([1; 32], 7, 5, 1000);
        for day in &mut stale.days {
            day.age_days += 2; // freshest is now 2 days old
        }
        assert!(!row_qualifies(&stale, 1000, &policy()));
    }

    #[test]
    fn fence_and_conviction_disqualify() {
        let mut row = clean_row([1; 32], 7, 5, 1000);
        row.fenced = true;
        assert!(!row_qualifies(&row, 1000, &policy()));
        let mut row = clean_row([1; 32], 7, 5, 1000);
        row.convicted = true;
        assert!(!row_qualifies(&row, 1000, &policy()));
    }

    #[test]
    fn grind_small_cash_big_fails_day_coverage() {
        // A week of passes at size 10, then one fresh pass at size 10_000:
        // quoting 10_000 must NOT qualify (only one covering day).
        let mut row = clean_row([1; 32], 7, 5, 10);
        if let Some(first) = row.days.first_mut() {
            first.max_passed_key_count = 10_000;
        }
        assert!(!row_qualifies(&row, 10_000, &policy()));
        // The small history still qualifies small quotes.
        assert!(row_qualifies(&row, 10, &policy()));
    }

    #[test]
    fn duplicate_ages_collapse_to_one_day() {
        // 7 entries all claiming age 0: one distinct day, not seven.
        let mut row = clean_row([1; 32], 1, 5, 1000);
        let day = row.days[0];
        row.days = vec![day; 7];
        assert!(!row_qualifies(&row, 1000, &policy()));
    }

    #[test]
    fn days_beyond_the_client_window_are_ignored() {
        // 7 covering days but 3 of them older than the window: only 4 count.
        let mut row = clean_row([1; 32], 7, 5, 1000);
        for day in row.days.iter_mut().skip(4) {
            day.age_days += 100;
        }
        assert!(!row_qualifies(&row, 1000, &policy()));
    }

    fn report_from(reporter_byte: u8, rows: Vec<AuditReportRow>) -> (PeerId, AuditReport) {
        let reporter = PeerId::from_bytes([reporter_byte; 32]);
        (
            reporter,
            AuditReport {
                reporter_peer_id: [reporter_byte; 32],
                nonce: [0; 32],
                rows,
                signature: Vec::new(),
            },
        )
    }

    #[test]
    fn relative_bar_aggregation_and_self_vouch_exclusion() {
        let subject = PeerId::from_bytes([9; 32]);
        let subjects = vec![(subject, 1000u32)];
        let good_row = clean_row([9; 32], 7, 5, 1000);

        // Two qualifying observers: below the floor of 3.
        let mut reports: HashMap<PeerId, AuditReport> = [
            report_from(1, vec![good_row.clone()]),
            report_from(2, vec![good_row.clone()]),
        ]
        .into_iter()
        .collect();
        assert!(eligible_subjects(&subjects, &reports, &policy()).is_empty());

        // A self-report must not count toward the bar.
        let (self_reporter, self_report) = report_from(9, vec![good_row.clone()]);
        reports.insert(self_reporter, self_report);
        assert!(eligible_subjects(&subjects, &reports, &policy()).is_empty());

        // A third independent observer clears the floor (3 reporters, bar =
        // max(3, 3-2) = 3, vouches 3) at the SIZE tier.
        let (r3, rep3) = report_from(3, vec![good_row.clone()]);
        reports.insert(r3, rep3);
        let eligible = eligible_subjects(&subjects, &reports, &policy());
        let e = eligible.get(&(subject, 1000u32)).expect("size-eligible");
        assert_eq!(e.tier, Tier::Size);
        assert_eq!(e.size_vouches, 3);

        // v4 MAJORITY-OF-OPINIONS bar: reporters with NO row for the subject
        // abstain — they must not raise the bar (no shortfall to calibrate).
        for r in 4..=10u8 {
            let (rr, rep_empty) = report_from(r, Vec::new());
            reports.insert(rr, rep_empty);
        }
        let eligible = eligible_subjects(&subjects, &reports, &policy());
        let e = eligible
            .get(&(subject, 1000u32))
            .expect("abstainers don't suppress");
        assert_eq!(
            e.tier,
            Tier::Size,
            "abstaining reporters must not suppress (3 opinions, 3 vouches)"
        );
        assert_eq!(e.size_vouches, 3);

        // But non-vouching ROWS count as opinions: catchers with sticky
        // convicted rows push the subject below the majority.
        let mut convicted_row = clean_row([9; 32], 7, 5, 1000);
        convicted_row.convicted = true;
        convicted_row.days.clear();
        for r in 4..=8u8 {
            let (rr, repv) = report_from(r, vec![convicted_row.clone()]);
            reports.insert(rr, repv);
        }
        // 8 opinions (3 clean + 5 convicted), bar = 8/2+1 = 5 > 3 vouches.
        assert!(
            eligible_subjects(&subjects, &reports, &policy()).is_empty(),
            "a majority of non-vouching rows must exclude the subject"
        );
    }

    #[test]
    fn gate_filters_only_when_enforcing_with_enough_eligible() {
        let subject_a = PeerId::from_bytes([9; 32]);
        let subject_b = PeerId::from_bytes([8; 32]);
        let items = vec![(subject_a, 1000u32), (subject_b, 1000u32)];
        let good_row_a = clean_row([9; 32], 7, 5, 1000);
        let reports: HashMap<PeerId, AuditReport> = [
            report_from(1, vec![good_row_a.clone()]),
            report_from(2, vec![good_row_a.clone()]),
            report_from(3, vec![good_row_a]),
        ]
        .into_iter()
        .collect();

        // Enforcing, need 1, subject_a eligible → only subject_a survives.
        let kept = gate_quoter_set(items.clone(), |item| *item, &reports, &policy(), 1, "test");
        assert_eq!(kept, vec![(subject_a, 1000u32)]);

        // Need 2 but only 1 eligible → degraded, unchanged.
        let kept = gate_quoter_set(items.clone(), |item| *item, &reports, &policy(), 2, "test");
        assert_eq!(kept.len(), 2);

        // Observe-only → unchanged even with an eligible subset.
        let mut observe = policy();
        observe.enforce = false;
        let kept = gate_quoter_set(items, |item| *item, &reports, &observe, 1, "test");
        assert_eq!(kept.len(), 2);
    }

    // ------------------------------------------------------------------
    // ADR-0005 v5: dues-eligibility (size-relaxed tier 2)
    // ------------------------------------------------------------------

    #[test]
    fn dues_ignores_size_but_keeps_the_week() {
        // A clean week at size 1 (a small node) has done its dues, but is NOT
        // size-eligible for a 100k-chunk quote.
        let row = clean_row([1; 32], 7, 5, 1);
        assert!(
            row_has_dues(&row, &policy()),
            "a clean week is dues, any size"
        );
        assert!(
            !row_qualifies(&row, 100_000, &policy()),
            "a small node is not size-eligible for a huge quote"
        );
        assert!(
            row_qualifies(&row, 1, &policy()),
            "it IS size-eligible at its own small size"
        );
    }

    #[test]
    fn dues_still_excludes_fresh_and_caught() {
        // Fresh identity: no pass days at all -> neither dues nor size.
        let fresh = clean_row([1; 32], 0, 0, 1000);
        assert!(!row_has_dues(&fresh, &policy()));
        assert!(!row_qualifies(&fresh, 1, &policy()));

        // Convicted: fails both, even with a full week of days underneath.
        let mut convicted = clean_row([1; 32], 7, 5, 1000);
        convicted.convicted = true;
        assert!(!row_has_dues(&convicted, &policy()));
        assert!(!row_qualifies(&convicted, 1000, &policy()));

        // Fenced: fails both.
        let mut fenced = clean_row([1; 32], 7, 5, 1000);
        fenced.fenced = true;
        assert!(!row_has_dues(&fenced, &policy()));
        assert!(!row_qualifies(&fenced, 1000, &policy()));
    }

    #[test]
    fn dues_is_a_superset_of_size_including_recency() {
        // codex-flagged counterexample class: 7 small passes at ages 0-6 plus
        // ONE covering pass at age 13. This has dues (7 distinct recent pass
        // days) but is NOT size-eligible for a big quote (only 1 covering day,
        // and it is stale) — the split must NOT collapse to
        // has_dues && covers_somewhere.
        let mut row = clean_row([1; 32], 7, 5, 1); // ages 0..6, size 1
        row.days.push(AuditReportDay {
            age_days: 13,
            passes: 5,
            max_passed_key_count: 100_000,
        });
        assert!(row_has_dues(&row, &policy()), "7 recent pass days = dues");
        assert!(
            !row_qualifies(&row, 100_000, &policy()),
            "one stale covering day is NOT size-eligibility"
        );
    }

    #[test]
    fn gate_falls_back_to_dues_when_size_short_but_never_admits_bad() {
        // 3 judges, all with a clean week at SMALL size 1; a big quote (size
        // 100k) makes nobody size-eligible, but all are dues-eligible.
        let subject = PeerId::from_bytes([9; 32]);
        let small_week = clean_row([9; 32], 7, 5, 1);
        let mut reports: HashMap<PeerId, AuditReport> = (1..=3u8)
            .map(|r| report_from(r, vec![small_week.clone()]))
            .collect();

        let big = vec![(subject, 100_000u32)];
        let elig = eligible_subjects(&big, &reports, &policy());
        assert_eq!(
            elig.get(&(subject, 100_000u32)).map(|e| e.tier),
            Some(Tier::Dues),
            "size short, dues met -> Dues tier"
        );
        // Enforcing gate at big size keeps the dues-eligible node.
        let kept = gate_quoter_set(big.clone(), |i| *i, &reports, &policy(), 1, "test");
        assert_eq!(kept, big, "dues fallback keeps the node");

        // A convicted judge-target is NOT dues-eligible even under fallback:
        // every judge carries a convicted row for it.
        let convicted_subject = PeerId::from_bytes([7; 32]);
        let mut conv = clean_row([7; 32], 7, 5, 1);
        conv.convicted = true;
        for r in 1..=3u8 {
            if let Some(rep) = reports.get_mut(&PeerId::from_bytes([r; 32])) {
                rep.rows.push(conv.clone());
            }
        }
        let elig = eligible_subjects(&[(convicted_subject, 100_000u32)], &reports, &policy());
        assert!(
            !elig.contains_key(&(convicted_subject, 100_000u32)),
            "a convicted node fails BOTH tiers"
        );
    }

    #[test]
    fn size_eligible_subjects_are_also_dues_tier_members() {
        // A size-eligible node reports tier Size but is counted in the dues
        // set too (dues_vouches >= size_vouches). gate_quoter_set's tier-2
        // filter must include it.
        let subject = PeerId::from_bytes([9; 32]);
        let full = clean_row([9; 32], 7, 5, 1000);
        let reports: HashMap<PeerId, AuditReport> = (1..=3u8)
            .map(|r| report_from(r, vec![full.clone()]))
            .collect();
        let elig = eligible_subjects(&[(subject, 1000u32)], &reports, &policy());
        let e = elig.get(&(subject, 1000u32)).expect("eligible");
        assert_eq!(e.tier, Tier::Size);
        assert_eq!(e.size_vouches, 3);
        assert_eq!(e.dues_vouches, 3, "size vouches are also dues vouches");
    }

    #[test]
    fn same_peer_at_two_sizes_never_leaks_size_tier_across_quotes() {
        // The eligibility map is keyed by (peer, size). If it were keyed by peer
        // alone, one peer appearing at a small (size-eligible) and a large
        // (dues-only) quote would collapse to one entry, and the small quote's
        // Size tier could admit the large quote through the size-only filter.
        // With a clean week at size 1 only:
        //   (S, 1)       -> Size-eligible
        //   (S, 100_000) -> Dues-eligible only (1 * slack < 100_000)
        let subject = PeerId::from_bytes([9; 32]);
        let small_week = clean_row([9; 32], 7, 5, 1);
        let reports: HashMap<PeerId, AuditReport> = (1..=3u8)
            .map(|r| report_from(r, vec![small_week.clone()]))
            .collect();

        // The large quote is evaluated on its own key: Dues, not Size.
        let elig = eligible_subjects(
            &[(subject, 1u32), (subject, 100_000u32)],
            &reports,
            &policy(),
        );
        assert_eq!(
            elig.get(&(subject, 1u32)).map(|e| e.tier),
            Some(Tier::Size),
            "the small quote is size-eligible"
        );
        assert_eq!(
            elig.get(&(subject, 100_000u32)).map(|e| e.tier),
            Some(Tier::Dues),
            "the large quote is dues-only, regardless of the small quote's tier"
        );

        // The Size-tier gate over BOTH quotes must keep only the small one — the
        // large quote has zero size vouches and must not ride the peer's small
        // quote into the size tier.
        let both = vec![(subject, 1u32), (subject, 100_000u32)];
        let kept = gate_quoter_set(both, |i| *i, &reports, &policy(), 1, "test");
        assert_eq!(
            kept,
            vec![(subject, 1u32)],
            "size gate admits only the size-eligible (peer,size) pair"
        );
    }
}
