//! Compacting a manifest: replacing embedded DataMaps with public addresses.
//!
//! An embedded DataMap can only become an address once its chunk is on the
//! network. Planning finds out which entries already are and which would
//! have to be published first. Publishing is a separate, paid step the
//! caller must opt into, because it makes those files public.

use std::collections::BTreeSet;

use crate::data::Client;

use super::{ContentRef, Manifest, ManifestError, ADDRESS_LEN};

#[cfg(test)]
use super::ManifestEntry;
#[cfg(test)]
use self_encryption::{ChunkInfo, DataMap};
#[cfg(test)]
use xor_name::XorName;

/// One embedded entry and where its DataMap chunk would live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddedEntry {
    /// Index into `manifest.entries`.
    pub index: usize,
    /// Effective name, for display.
    pub name: String,
    /// Address the DataMap chunk has, or would have.
    pub address: [u8; ADDRESS_LEN],
}

/// What compacting a manifest would involve.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompactPlan {
    /// Embedded entries whose DataMap chunk is already on the network.
    pub already_public: Vec<EmbeddedEntry>,
    /// Embedded entries whose DataMap chunk would have to be published.
    pub needs_publish: Vec<EmbeddedEntry>,
}

impl CompactPlan {
    /// Whether every embedded entry can be compacted without publishing.
    pub fn is_free(&self) -> bool {
        self.needs_publish.is_empty()
    }

    /// Indices of every embedded entry in the plan.
    pub fn all_indices(&self) -> BTreeSet<usize> {
        self.already_public
            .iter()
            .chain(&self.needs_publish)
            .map(|e| e.index)
            .collect()
    }
}

/// Check every embedded entry against the network.
pub async fn plan_compaction(
    client: &Client,
    manifest: &Manifest,
) -> Result<CompactPlan, ManifestError> {
    let mut plan = CompactPlan::default();
    for (index, entry) in manifest.entries.iter().enumerate() {
        if !matches!(entry.source, ContentRef::Embedded { .. }) {
            continue;
        }
        let embedded = EmbeddedEntry {
            index,
            name: entry.effective_name()?,
            address: entry.source.content_address()?,
        };
        if client.chunk_exists(&embedded.address).await? {
            plan.already_public.push(embedded);
        } else {
            plan.needs_publish.push(embedded);
        }
    }
    Ok(plan)
}

/// Store the DataMap chunk of each entry in `entries`. Paid; makes those
/// files public. Returns the addresses stored, in the same order.
pub async fn publish_data_maps(
    client: &Client,
    manifest: &Manifest,
    entries: &[EmbeddedEntry],
) -> Result<Vec<[u8; ADDRESS_LEN]>, ManifestError> {
    let mut stored = Vec::with_capacity(entries.len());
    for embedded in entries {
        let entry = manifest.entries.get(embedded.index).ok_or_else(|| {
            ManifestError::Build(format!("entry {} is out of range", embedded.index))
        })?;
        let ContentRef::Embedded { data_map } = &entry.source else {
            return Err(ManifestError::Build(format!(
                "entry {} is not embedded",
                embedded.name
            )));
        };
        let address = client.data_map_store(data_map).await?;
        if address != embedded.address {
            return Err(ManifestError::Build(format!(
                "entry {} stored at an unexpected address",
                embedded.name
            )));
        }
        stored.push(address);
    }
    Ok(stored)
}

/// Replace the embedded entries at `indices` with their public addresses.
/// Entries not listed are left as they are.
pub fn apply_compaction(
    manifest: &Manifest,
    indices: &BTreeSet<usize>,
) -> Result<Manifest, ManifestError> {
    let mut compacted = manifest.clone();
    for (index, entry) in compacted.entries.iter_mut().enumerate() {
        if !indices.contains(&index) {
            continue;
        }
        if let ContentRef::Embedded { .. } = entry.source {
            let address = entry.source.content_address()?;
            entry.source = ContentRef::Public { address };
        }
    }
    Ok(compacted)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn data_map(seed: u8) -> DataMap {
        DataMap::new(vec![ChunkInfo {
            index: 0,
            dst_hash: XorName([seed; 32]),
            src_hash: XorName([seed; 32]),
            src_size: 10,
        }])
    }

    #[test]
    fn apply_compaction_replaces_only_the_requested_embedded_entries() {
        let manifest = Manifest {
            name: None,
            entries: vec![
                ManifestEntry {
                    path: Some("a".into()),
                    size: None,
                    source: ContentRef::Embedded {
                        data_map: data_map(1),
                    },
                },
                ManifestEntry {
                    path: Some("b".into()),
                    size: None,
                    source: ContentRef::Embedded {
                        data_map: data_map(2),
                    },
                },
                ManifestEntry {
                    path: Some("c".into()),
                    size: None,
                    source: ContentRef::Public { address: [9; 32] },
                },
            ],
        };
        let expected_a = manifest.entries[0].source.content_address().unwrap();
        let compacted = apply_compaction(&manifest, &BTreeSet::from([0, 2])).unwrap();
        assert_eq!(
            compacted.entries[0].source,
            ContentRef::Public {
                address: expected_a
            }
        );
        assert!(matches!(
            compacted.entries[1].source,
            ContentRef::Embedded { .. }
        ));
        assert_eq!(
            compacted.entries[2].source,
            ContentRef::Public { address: [9; 32] }
        );
        assert_eq!(compacted.entries[0].path.as_deref(), Some("a"));
    }

    #[test]
    fn plan_helpers() {
        let plan = CompactPlan {
            already_public: vec![EmbeddedEntry {
                index: 0,
                name: "a".into(),
                address: [1; 32],
            }],
            needs_publish: vec![EmbeddedEntry {
                index: 2,
                name: "c".into(),
                address: [3; 32],
            }],
        };
        assert!(!plan.is_free());
        assert_eq!(plan.all_indices(), BTreeSet::from([0, 2]));
        assert!(CompactPlan::default().is_free());
    }
}
