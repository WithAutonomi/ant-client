//! Release-channel semantics shared by node binary resolution and CLI self-update.
//!
//! The rule implemented here is deliberately a textual mirror of `ant-node`'s
//! `version_matches_channel` in `src/upgrade/monitor.rs`. A node that opts into the beta
//! channel picks its own upgrades with the node-side copy, while `ant node add` and
//! `ant update` pick binaries with this one; if the two ever diverge, a node would be
//! installed from a release it would then refuse to upgrade from (or vice versa).

use semver::Version;

use crate::node::types::UpgradeChannel;

/// Check whether a version is eligible for a release channel.
///
/// - Stable: only final releases, i.e. no pre-release component at all.
/// - Beta: final releases, plus pre-releases whose first identifier is exactly `beta`
///   (`0.17.0-beta.1`, `0.17.0-beta`).
///
/// Every other pre-release suffix is rejected on both channels. In particular `-rc.*`
/// is not a beta candidate: release candidates are published before the release gates
/// have given a verdict, and semver ranks `-rc` above `-beta`, so accepting them would
/// pull beta nodes off their soak build and onto un-gated code.
#[must_use]
pub fn version_matches_channel(version: &Version, channel: UpgradeChannel) -> bool {
    if version.pre.is_empty() {
        return true;
    }

    match channel {
        UpgradeChannel::Stable => false,
        UpgradeChannel::Beta => version.pre.as_str().split('.').next() == Some("beta"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).expect("test version parses")
    }

    #[test]
    fn stable_accepts_finals() {
        assert!(version_matches_channel(
            &v("0.16.0"),
            UpgradeChannel::Stable
        ));
        assert!(version_matches_channel(&v("1.0.0"), UpgradeChannel::Stable));
    }

    #[test]
    fn stable_rejects_every_pre_release() {
        assert!(!version_matches_channel(
            &v("0.17.0-beta.1"),
            UpgradeChannel::Stable
        ));
        assert!(!version_matches_channel(
            &v("0.17.0-rc.1"),
            UpgradeChannel::Stable
        ));
        assert!(!version_matches_channel(
            &v("0.17.0-alpha.1"),
            UpgradeChannel::Stable
        ));
    }

    #[test]
    fn beta_accepts_finals_and_beta_pre_releases() {
        assert!(version_matches_channel(&v("0.16.0"), UpgradeChannel::Beta));
        assert!(version_matches_channel(
            &v("0.17.0-beta.1"),
            UpgradeChannel::Beta
        ));
        assert!(version_matches_channel(
            &v("0.17.0-beta"),
            UpgradeChannel::Beta
        ));
    }

    #[test]
    fn beta_rejects_release_candidates() {
        assert!(!version_matches_channel(
            &v("0.17.0-rc.1"),
            UpgradeChannel::Beta
        ));
        assert!(!version_matches_channel(
            &v("0.17.0-rc"),
            UpgradeChannel::Beta
        ));
    }

    #[test]
    fn beta_rejects_other_pre_release_suffixes() {
        assert!(!version_matches_channel(
            &v("0.17.0-alpha.1"),
            UpgradeChannel::Beta
        ));
        // `betamax` must not be accepted by a prefix match on `beta`.
        assert!(!version_matches_channel(
            &v("0.17.0-betamax.1"),
            UpgradeChannel::Beta
        ));
    }

    /// The selection case from V2-1010: with a mixed release list, each channel picks the
    /// highest version it accepts, and beta never reaches for the rc.
    #[test]
    fn highest_eligible_of_a_mixed_release_list() {
        let releases = ["0.16.0", "0.17.0-beta.1", "0.17.0-rc.1"];

        let highest = |channel| {
            releases
                .iter()
                .map(|s| v(s))
                .filter(|version| version_matches_channel(version, channel))
                .max()
                .map(|version| version.to_string())
        };

        assert_eq!(highest(UpgradeChannel::Stable), Some("0.16.0".to_string()));
        assert_eq!(
            highest(UpgradeChannel::Beta),
            Some("0.17.0-beta.1".to_string())
        );
    }

    /// Ship+promote same-day: from `0.16.0-beta.1`, with both the promoted `0.16.0` and the
    /// next cut's `0.17.0-beta.1` published, beta hops straight to `0.17.0-beta.1` rather than
    /// pausing on stable.
    #[test]
    fn beta_hops_past_a_promoted_stable_to_the_next_beta() {
        let current = v("0.16.0-beta.1");
        let highest = ["0.16.0", "0.17.0-beta.1"]
            .iter()
            .map(|s| v(s))
            .filter(|version| *version > current)
            .filter(|version| version_matches_channel(version, UpgradeChannel::Beta))
            .max()
            .map(|version| version.to_string());

        assert_eq!(highest, Some("0.17.0-beta.1".to_string()));
    }
}
