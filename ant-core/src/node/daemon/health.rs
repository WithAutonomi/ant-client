//! Fleet health: an extensible, always-available indicator of node-fleet health.
//!
//! The model is deliberately generic — [`FleetHealth`] is a list of [`HealthCheck`]s plus an
//! `overall` level — so future signals (connectivity, sync lag, reward health, …) can be added as
//! new [`HealthCheckKind`]s without changing the API surface the CLI and GUI consume.
//!
//! The first and only check today is **disk space**. It answers, per partition: are we comfortable
//! (green), is an eviction likely soon (warning, with the candidate named), or is the partition at
//! the eviction threshold (critical)? The candidate it names is computed by [`super::disk`] — the
//! exact same selection the eviction monitor uses, so the warning never points at a different node
//! than the one that actually gets evicted.

use serde::{Deserialize, Serialize};

use super::disk::PartitionState;

/// One mebibyte, in bytes.
pub const MIB: u64 = 1024 * 1024;

/// Fixed free-space floor at which the daemon evicts a node, mirroring the node's own refuse-to-store
/// reserve in `ant-node`'s storage layer. Internal constant — deliberately not user-configurable.
const EVICTION_THRESHOLD_MB: u64 = 500;

/// Fixed free-space level at which the fleet health turns to `Warning` and names the node that would
/// be evicted next. Internal constant — deliberately not user-configurable.
const WARNING_THRESHOLD_MB: u64 = 1024;

/// Severity level for a single check or the fleet as a whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum HealthLevel {
    /// Everything is comfortable.
    Green,
    /// Action may be needed soon (e.g. an eviction is approaching).
    Warning,
    /// At or past a hard limit right now (e.g. eviction is imminent, or space is exhausted and the
    /// daemon cannot help automatically).
    Critical,
}

impl HealthLevel {
    fn rank(self) -> u8 {
        match self {
            HealthLevel::Green => 0,
            HealthLevel::Warning => 1,
            HealthLevel::Critical => 2,
        }
    }

    /// The more severe of two levels.
    pub fn worst(self, other: HealthLevel) -> HealthLevel {
        if other.rank() > self.rank() {
            other
        } else {
            self
        }
    }
}

/// Which signal produced a [`HealthCheck`]. Extensible: new variants slot in here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum HealthCheckKind {
    /// Free disk space at node data directories.
    DiskSpace,
}

/// The node a check has identified as the next eviction candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct EvictionCandidate {
    pub node_id: u32,
    #[schema(value_type = String)]
    pub data_dir: String,
    /// Bytes the candidate's data directory currently occupies (≈ space its eviction would free).
    pub size_bytes: u64,
}

/// A single health finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct HealthCheck {
    pub kind: HealthCheckKind,
    pub level: HealthLevel,
    /// Human-readable, user-facing one-liner.
    pub summary: String,
    /// Partition this finding concerns (disk checks only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub partition: Option<String>,
    /// Free bytes on the partition (disk checks only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available_bytes: Option<u64>,
    /// Free-space floor at which an eviction triggers (disk checks only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eviction_threshold_bytes: Option<u64>,
    /// The node that would be evicted next, when one applies.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate: Option<EvictionCandidate>,
}

/// Fleet-wide health snapshot: the worst level across all checks, plus the individual findings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct FleetHealth {
    pub overall: HealthLevel,
    pub checks: Vec<HealthCheck>,
}

impl FleetHealth {
    /// A green snapshot with no findings (e.g. no nodes registered).
    pub fn healthy() -> Self {
        FleetHealth {
            overall: HealthLevel::Green,
            checks: Vec::new(),
        }
    }

    /// Build the fleet health from measured partition states and the configured disk thresholds.
    pub fn from_partitions(partitions: &[PartitionState], thresholds: &DiskThresholds) -> Self {
        let checks: Vec<HealthCheck> = partitions
            .iter()
            .map(|p| disk_space_check(p, thresholds))
            .collect();
        let overall = checks
            .iter()
            .fold(HealthLevel::Green, |acc, c| acc.worst(c.level));
        FleetHealth { overall, checks }
    }
}

/// Configured free-space thresholds for the disk-space check.
#[derive(Debug, Clone, Copy)]
pub struct DiskThresholds {
    /// Evict a node once free space falls to/below this many bytes.
    pub eviction_bytes: u64,
    /// Turn the health to `Warning` once free space falls to/below this many bytes.
    pub warning_bytes: u64,
}

impl Default for DiskThresholds {
    fn default() -> Self {
        DiskThresholds {
            eviction_bytes: EVICTION_THRESHOLD_MB * MIB,
            warning_bytes: WARNING_THRESHOLD_MB * MIB,
        }
    }
}

/// Evaluate the disk-space health of a single partition.
fn disk_space_check(p: &PartitionState, thresholds: &DiskThresholds) -> HealthCheck {
    let available = p.available_bytes;
    let candidate = p.eviction_candidate();
    // Eviction only helps the *other* nodes on the partition, so it is only possible when at least
    // two nodes share it (one is evicted, at least one remains to benefit). A partition with a
    // single node therefore cannot be auto-helped — this subsumes the "only one node running" case.
    let can_evict = p.nodes.len() >= 2;

    let (level, summary) = if available <= thresholds.eviction_bytes {
        if can_evict {
            let who = candidate
                .map(|c| format!("node {}", c.node_id))
                .unwrap_or_else(|| "a node".to_string());
            (
                HealthLevel::Critical,
                format!(
                    "Disk space critical on {}: {} free (≤ {}). Evicting {} to reclaim space.",
                    p.partition,
                    fmt_bytes(available),
                    fmt_bytes(thresholds.eviction_bytes),
                    who,
                ),
            )
        } else {
            (
                HealthLevel::Critical,
                format!(
                    "Disk space critical on {}: {} free (≤ {}), but only one node is running here \
                     so it cannot be auto-evicted. Free disk space or reduce node count manually.",
                    p.partition,
                    fmt_bytes(available),
                    fmt_bytes(thresholds.eviction_bytes),
                ),
            )
        }
    } else if available <= thresholds.warning_bytes {
        let who = candidate
            .filter(|_| can_evict)
            .map(|c| format!("; node {} would be evicted next", c.node_id))
            .unwrap_or_default();
        (
            HealthLevel::Warning,
            format!(
                "Disk space low on {}: {} free. An eviction may occur once it reaches {}{}.",
                p.partition,
                fmt_bytes(available),
                fmt_bytes(thresholds.eviction_bytes),
                who,
            ),
        )
    } else {
        (
            HealthLevel::Green,
            format!(
                "Disk space healthy on {}: {} free.",
                p.partition,
                fmt_bytes(available)
            ),
        )
    };

    // Only surface a candidate when an eviction could actually happen.
    let candidate_out = if can_evict && level != HealthLevel::Green {
        candidate.map(|c| EvictionCandidate {
            node_id: c.node_id,
            data_dir: c.data_dir.to_string_lossy().into_owned(),
            size_bytes: c.size_bytes,
        })
    } else {
        None
    };

    HealthCheck {
        kind: HealthCheckKind::DiskSpace,
        level,
        summary,
        partition: Some(p.partition.to_string()),
        available_bytes: Some(available),
        eviction_threshold_bytes: Some(thresholds.eviction_bytes),
        candidate: candidate_out,
    }
}

/// Format a byte count as a human-friendly string (GiB/MiB).
fn fmt_bytes(bytes: u64) -> String {
    const GIB: u64 = 1024 * MIB;
    if bytes >= GIB {
        format!("{:.2} GiB", bytes as f64 / GIB as f64)
    } else {
        format!("{:.0} MiB", bytes as f64 / MIB as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::daemon::disk::{NodeDiskUsage, PartitionKey, PartitionState};
    use std::path::PathBuf;

    fn node(id: u32, size: u64) -> NodeDiskUsage {
        NodeDiskUsage {
            node_id: id,
            data_dir: PathBuf::from(format!("/data/node-{id}")),
            size_bytes: size,
        }
    }

    fn partition(available_bytes: u64, nodes: Vec<NodeDiskUsage>) -> PartitionState {
        PartitionState {
            partition: PartitionKey::for_test("p0"),
            available_bytes,
            nodes,
        }
    }

    fn thresholds() -> DiskThresholds {
        DiskThresholds {
            eviction_bytes: 500 * MIB,
            warning_bytes: 1024 * MIB,
        }
    }

    #[test]
    fn green_when_space_comfortable() {
        let p = partition(4 * 1024 * MIB, vec![node(1, 100), node(2, 200)]);
        let health = FleetHealth::from_partitions(&[p], &thresholds());
        assert_eq!(health.overall, HealthLevel::Green);
        assert_eq!(health.checks[0].candidate, None);
    }

    #[test]
    fn warning_names_candidate_between_thresholds() {
        // 800 MiB free: below warning (1024) but above eviction (500).
        let p = partition(800 * MIB, vec![node(1, 900), node(2, 100)]);
        let health = FleetHealth::from_partitions(&[p], &thresholds());
        assert_eq!(health.overall, HealthLevel::Warning);
        // Smallest node (2) is the candidate.
        assert_eq!(health.checks[0].candidate.as_ref().unwrap().node_id, 2);
    }

    #[test]
    fn critical_when_at_eviction_threshold_multi_node() {
        let p = partition(400 * MIB, vec![node(1, 900), node(2, 100)]);
        let health = FleetHealth::from_partitions(&[p], &thresholds());
        assert_eq!(health.overall, HealthLevel::Critical);
        assert_eq!(health.checks[0].candidate.as_ref().unwrap().node_id, 2);
        assert!(health.checks[0].summary.contains("Evicting node 2"));
    }

    #[test]
    fn critical_sole_node_warns_no_candidate() {
        // Single node on a full partition: cannot auto-evict.
        let p = partition(400 * MIB, vec![node(7, 100)]);
        let health = FleetHealth::from_partitions(&[p], &thresholds());
        assert_eq!(health.overall, HealthLevel::Critical);
        assert!(health.checks[0].candidate.is_none());
        assert!(health.checks[0].summary.contains("only one node"));
    }

    #[test]
    fn overall_is_worst_across_partitions() {
        let healthy = partition(4 * 1024 * MIB, vec![node(1, 100), node(2, 100)]);
        let warning = partition(800 * MIB, vec![node(3, 100), node(4, 100)]);
        let health = FleetHealth::from_partitions(&[healthy, warning], &thresholds());
        assert_eq!(health.overall, HealthLevel::Warning);
        assert_eq!(health.checks.len(), 2);
    }

    #[test]
    fn empty_fleet_is_green() {
        let health = FleetHealth::from_partitions(&[], &thresholds());
        assert_eq!(health.overall, HealthLevel::Green);
        assert!(health.checks.is_empty());
    }
}
