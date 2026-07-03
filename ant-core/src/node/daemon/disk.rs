//! Disk-space measurement and eviction-candidate selection.
//!
//! This module is the single source of truth for two questions:
//!   1. How much free space is left on each partition that hosts node data?
//!   2. Which node should be evicted next to reclaim space?
//!
//! Both the low-disk eviction monitor (which actually evicts) and the fleet health layer (which
//! warns the user *before* an eviction and names the candidate) call into here, so they can never
//! disagree about who is next.
//!
//! Nodes that live on the same filesystem partition share its free space, so eviction is always
//! decided per partition: evicting one node frees space for every other node on that partition.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Measured disk usage of a single node's data directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeDiskUsage {
    pub node_id: u32,
    pub data_dir: PathBuf,
    /// Total bytes occupied by the node's data directory on disk (≈ space reclaimable by eviction).
    pub size_bytes: u64,
}

/// Opaque identifier for the filesystem partition a path lives on.
///
/// On Unix this is the device id of the path (or its nearest existing ancestor); elsewhere it falls
/// back to the canonicalized path prefix. Two paths with the same key share free space.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PartitionKey(String);

impl std::fmt::Display for PartitionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl PartitionKey {
    /// Construct a key with an arbitrary label. Test-only — real keys come from [`partition_key`].
    #[cfg(test)]
    pub(crate) fn for_test(label: &str) -> Self {
        PartitionKey(label.to_string())
    }
}

/// State of one partition that hosts node data: how much space is free and which running nodes live
/// on it (with their measured sizes).
#[derive(Debug, Clone)]
pub struct PartitionState {
    pub partition: PartitionKey,
    /// Free bytes available on the partition.
    pub available_bytes: u64,
    /// Running nodes whose data directory lives on this partition.
    pub nodes: Vec<NodeDiskUsage>,
}

impl PartitionState {
    /// The node that should be evicted first to reclaim space on this partition.
    ///
    /// Selection rule: the **smallest** data directory wins, because evicting a node forces the
    /// network to re-replicate everything it held — so we minimise that cascade by dropping the
    /// node that holds the least. Ties are broken by preferring the **newest** node (highest id,
    /// least established in the network). Returns `None` if the partition hosts no nodes.
    pub fn eviction_candidate(&self) -> Option<&NodeDiskUsage> {
        self.nodes.iter().min_by(|a, b| {
            a.size_bytes
                .cmp(&b.size_bytes)
                .then_with(|| b.node_id.cmp(&a.node_id))
        })
    }
}

/// Recursively sum the **allocated** disk usage of every regular file under `path`.
///
/// Because eviction is automatic and destructive, this must reflect the space that deleting the tree
/// would actually reclaim — not the logical file length. LMDB's `data.mdb` is a sparse file whose
/// logical length is the (often multi-GiB) map size while its allocated blocks are only the bytes it
/// really uses; ranking "smallest data dir" or reporting "reclaimed bytes" off `len()` would be
/// badly wrong. On Unix we therefore use allocated blocks (`blocks() * 512`); elsewhere we fall back
/// to logical length.
///
/// Symlinks are never followed: entries are examined with `symlink_metadata`, and a symlink is
/// skipped entirely so the walk cannot escape the node's data directory and count unrelated data.
/// Returns 0 if `path` does not exist. Best-effort: entries that cannot be read are skipped rather
/// than aborting the walk, so a transient permission error does not corrupt the eviction decision.
pub fn dir_size(path: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            // `symlink_metadata` reports the entry itself, never the symlink target.
            let meta = match std::fs::symlink_metadata(entry.path()) {
                Ok(meta) => meta,
                Err(_) => continue,
            };
            let file_type = meta.file_type();
            if file_type.is_symlink() {
                // Do not follow — recursing or counting through a link could escape the tree.
                continue;
            } else if file_type.is_dir() {
                stack.push(entry.path());
            } else if file_type.is_file() {
                total = total.saturating_add(allocated_size(&meta));
            }
        }
    }
    total
}

/// Disk space a regular file actually occupies.
///
/// On Unix this is the allocated block count (`blocks() * 512`, per POSIX these are always 512-byte
/// units regardless of filesystem block size), which correctly reflects sparse files. On other
/// platforms std exposes no allocated size, so we fall back to the logical length.
fn allocated_size(meta: &std::fs::Metadata) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.blocks().saturating_mul(512)
    }
    #[cfg(not(unix))]
    {
        meta.len()
    }
}

/// Free bytes on the partition hosting `path` (or its nearest existing ancestor).
///
/// Returns `None` if the available space cannot be determined.
pub fn available_space(path: &Path) -> Option<u64> {
    let existing = nearest_existing_ancestor(path);
    fs2::available_space(&existing).ok()
}

/// Group the given running nodes by partition, measuring each node's data-directory size and each
/// partition's free space.
///
/// `nodes` should be the set of currently-running nodes as `(id, data_dir)` pairs; evicted or
/// stopped nodes are irrelevant to a reclaim decision and should be filtered out by the caller.
/// Partitions whose free space cannot be read are skipped.
pub fn partition_states<I>(nodes: I) -> Vec<PartitionState>
where
    I: IntoIterator<Item = (u32, PathBuf)>,
{
    let mut grouped: BTreeMap<PartitionKey, Vec<NodeDiskUsage>> = BTreeMap::new();
    for (node_id, data_dir) in nodes {
        let key = partition_key(&data_dir);
        let size_bytes = dir_size(&data_dir);
        grouped.entry(key).or_default().push(NodeDiskUsage {
            node_id,
            data_dir,
            size_bytes,
        });
    }

    grouped
        .into_iter()
        .filter_map(|(partition, nodes)| {
            // Every node in the group shares the partition, so any node's data_dir answers the
            // free-space query.
            let probe = nodes.first()?.data_dir.clone();
            let available_bytes = available_space(&probe)?;
            Some(PartitionState {
                partition,
                available_bytes,
                nodes,
            })
        })
        .collect()
}

/// Identify the partition a path lives on.
fn partition_key(path: &Path) -> PartitionKey {
    let existing = nearest_existing_ancestor(path);

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let Ok(meta) = std::fs::metadata(&existing) {
            return PartitionKey(format!("dev:{}", meta.dev()));
        }
    }

    // Fallback (and the non-Unix path): the canonicalized prefix is a reasonable partition proxy.
    let canon = std::fs::canonicalize(&existing).unwrap_or(existing);
    let key = canon
        .components()
        .next()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .unwrap_or_else(|| canon.to_string_lossy().into_owned());
    PartitionKey(key)
}

/// Walk up `path` until an existing directory/file is found, since a node's `data_dir` may not yet
/// exist (or may have just been deleted). Falls back to the path itself if nothing exists.
fn nearest_existing_ancestor(path: &Path) -> PathBuf {
    let mut current = Some(path);
    while let Some(p) = current {
        if p.exists() {
            return p.to_path_buf();
        }
        current = p.parent();
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(node_id: u32, size_bytes: u64) -> NodeDiskUsage {
        NodeDiskUsage {
            node_id,
            data_dir: PathBuf::from(format!("/data/node-{node_id}")),
            size_bytes,
        }
    }

    fn partition_with(nodes: Vec<NodeDiskUsage>) -> PartitionState {
        PartitionState {
            partition: PartitionKey("test".to_string()),
            available_bytes: 0,
            nodes,
        }
    }

    #[test]
    fn candidate_picks_smallest_data_dir() {
        let p = partition_with(vec![usage(1, 900), usage(2, 100), usage(3, 500)]);
        assert_eq!(p.eviction_candidate().unwrap().node_id, 2);
    }

    #[test]
    fn candidate_tie_break_prefers_newest_id() {
        // Nodes 1 and 4 are tied on size; the newer (id 4) should be chosen.
        let p = partition_with(vec![usage(1, 100), usage(4, 100), usage(2, 800)]);
        assert_eq!(p.eviction_candidate().unwrap().node_id, 4);
    }

    #[test]
    fn candidate_none_when_empty() {
        let p = partition_with(vec![]);
        assert!(p.eviction_candidate().is_none());
    }

    #[test]
    fn dir_size_sums_nested_files() {
        let tmp = tempfile::tempdir().unwrap();
        // Non-zero bytes so no filesystem can silently hole-punch them away.
        std::fs::write(tmp.path().join("a.bin"), vec![1u8; 1000]).unwrap();
        let sub = tmp.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("b.bin"), vec![1u8; 2500]).unwrap();
        // dir_size reports allocated blocks, which for fully-written files is at least the logical
        // 3500 bytes (block-rounded upward).
        assert!(dir_size(tmp.path()) >= 3500);
    }

    #[test]
    fn dir_size_zero_for_missing_path() {
        assert_eq!(dir_size(Path::new("/nonexistent/path/xyz")), 0);
    }

    #[cfg(unix)]
    #[test]
    fn dir_size_does_not_follow_symlinks() {
        // A node data dir containing a symlink to a large external file must not have that file's
        // size counted — following it could escape the tree and count unrelated data.
        let tmp = tempfile::tempdir().unwrap();
        let node_dir = tmp.path().join("node");
        std::fs::create_dir(&node_dir).unwrap();
        std::fs::write(node_dir.join("real.bin"), vec![1u8; 4000]).unwrap();

        let external = tmp.path().join("external_big.bin");
        std::fs::write(&external, vec![1u8; 5_000_000]).unwrap();
        std::os::unix::fs::symlink(&external, node_dir.join("link")).unwrap();

        // Only real.bin is counted; the 5 MB symlink target is not.
        let size = dir_size(&node_dir);
        assert!(size > 0);
        assert!(size < 1_000_000, "symlink target was followed: {size}");
    }

    #[cfg(unix)]
    #[test]
    fn dir_size_uses_allocated_not_logical_size() {
        use std::io::Write;
        // A sparse file (LMDB's data.mdb behaves this way): 20 MiB logical, ~0 allocated.
        let tmp = tempfile::tempdir().unwrap();
        let f = std::fs::File::create(tmp.path().join("sparse.mdb")).unwrap();
        f.set_len(20 * 1024 * 1024).unwrap();
        drop(f);
        // Also a small fully-written file so the dir isn't empty.
        let mut real = std::fs::File::create(tmp.path().join("real.bin")).unwrap();
        real.write_all(&[1u8; 2000]).unwrap();
        drop(real);

        // Allocated usage is far below the 20 MiB logical size of the sparse file.
        let size = dir_size(tmp.path());
        assert!(
            size < 1024 * 1024,
            "expected allocated size well under 1 MiB, got {size}"
        );
    }

    #[test]
    fn partition_states_groups_colocated_nodes() {
        // Two nodes under the same temp dir share a partition.
        let tmp = tempfile::tempdir().unwrap();
        let d1 = tmp.path().join("node-1");
        let d2 = tmp.path().join("node-2");
        std::fs::create_dir_all(&d1).unwrap();
        std::fs::create_dir_all(&d2).unwrap();
        // Clearly different allocated sizes (non-zero bytes) so selection is by size, not tie-break.
        std::fs::write(d1.join("data"), vec![1u8; 200_000]).unwrap();
        std::fs::write(d2.join("data"), vec![1u8; 1000]).unwrap();

        let states = partition_states(vec![(1, d1), (2, d2)]);
        assert_eq!(states.len(), 1, "co-located nodes share one partition");
        let state = &states[0];
        assert_eq!(state.nodes.len(), 2);
        assert!(state.available_bytes > 0);
        // Node 2 is smaller, so it is the candidate.
        assert_eq!(state.eviction_candidate().unwrap().node_id, 2);
    }
}
