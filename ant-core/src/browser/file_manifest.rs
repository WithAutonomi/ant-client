// Copyright 2026 MaidSafe.net limited.
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Browser inputs for the shared manifest encoder. No network or filesystem I/O.

use super::manifest::MAX_SAFE_JS_INTEGER;
use super::BrowserManifestView;
use crate::data::manifest::{
    self, embedded_len, ContentRef, Link, Manifest, ManifestEntry, TorrentReference,
    MANIFEST_LINK_RECOMMENDED_MAX_BYTES, MAX_EMBEDDED_ROOT_MAP_BYTES, MAX_MANIFEST_BYTES,
};
use self_encryption::DataMap;
use serde::{Deserialize, Serialize};

#[cfg(test)]
use self_encryption::ChunkInfo;
#[cfg(test)]
use xor_name::XorName;

/// What `createManifest` takes: metadata the creator supplies. The address of
/// an embedded entry is derived from its DataMap, never taken on trust.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ManifestInput {
    /// Suggested root directory name.
    pub name: Option<String>,
    /// The BitTorrent identity of the same files.
    pub torrent: Option<TorrentInput>,
    /// The files, in any order; encoding sorts them.
    pub entries: Vec<EntryInput>,
}

/// Hex BitTorrent info hashes. At least one is required.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TorrentInput {
    /// SHA-1 info hash of a v1 torrent.
    pub info_hash_v1: Option<String>,
    /// SHA-256 info hash of a v2 or hybrid torrent.
    pub info_hash_v2: Option<String>,
}

/// One file of a manifest being created.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum EntryInput {
    /// A public file, by its hex DataMap address or `ant://` file link.
    Public {
        /// Relative path, subject to the portable path rules.
        path: Option<String>,
        /// Plaintext size hint.
        size: Option<u64>,
        /// The file's public address.
        address: String,
    },
    /// A file by its DataMap, in self_encryption's canonical MessagePack as
    /// the browser client hands it out. Grants access to a private file.
    Embedded {
        /// Relative path, subject to the portable path rules.
        path: Option<String>,
        /// Plaintext size; must match a root DataMap's exact size.
        size: Option<u64>,
        /// The serialized DataMap.
        #[serde(rename = "dataMap", with = "serde_bytes")]
        data_map: Vec<u8>,
    },
}

/// What `createManifest` returns.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreatedManifest {
    /// The manifest as encoded, entries in canonical order.
    pub manifest: BrowserManifestView,
    /// Canonical `.ant` file bytes.
    #[serde(with = "serde_bytes")]
    pub bytes: Vec<u8>,
    /// `ant://manifest/...` link carrying the manifest's link form.
    pub link: String,
    /// Whether the link is short enough to share in a chat message. A longer
    /// link still works; share the `.ant` file instead.
    pub link_recommended: bool,
}

impl ManifestInput {
    /// Validate the input and encode it with the shared encoder, so the bytes
    /// and link are exactly those a native writer produces.
    pub fn create(self) -> Result<CreatedManifest, String> {
        let torrent = self.torrent.map(TorrentInput::into_reference).transpose()?;
        let mut embedded_bytes = 0;
        let entries = self
            .entries
            .into_iter()
            .map(|entry| entry.into_entry(&mut embedded_bytes))
            .collect::<Result<_, _>>()?;
        let mut manifest = Manifest {
            name: self.name,
            torrent,
            entries,
        };
        // Sorted here too so the view lists entries in the order the bytes do.
        manifest.canonicalize().map_err(|e| e.to_string())?;
        let bytes = manifest.encode().map_err(|e| e.to_string())?;
        let link_bytes = manifest::manifest_link_bytes(&manifest).map_err(|e| e.to_string())?;
        Ok(CreatedManifest {
            manifest: BrowserManifestView::from_manifest(&manifest).map_err(|e| e.to_string())?,
            bytes,
            link: manifest::manifest_link_from_bytes(&link_bytes),
            link_recommended: link_bytes.len() <= MANIFEST_LINK_RECOMMENDED_MAX_BYTES,
        })
    }
}

impl TorrentInput {
    fn into_reference(self) -> Result<TorrentReference, String> {
        Ok(TorrentReference {
            info_hash_v1: torrent_hash(self.info_hash_v1, "infoHashV1", |t| t.info_hash_v1)?,
            info_hash_v2: torrent_hash(self.info_hash_v2, "infoHashV2", |t| t.info_hash_v2)?,
        })
    }
}

impl EntryInput {
    /// The manifest entry. `embedded_bytes` totals the DataMap input so far,
    /// so a page cannot make this decode more than a manifest can hold.
    fn into_entry(self, embedded_bytes: &mut usize) -> Result<ManifestEntry, String> {
        let entry = match self {
            Self::Public {
                path,
                size,
                address,
            } => {
                let Link::File(address) =
                    manifest::parse_link(&address).map_err(|e| e.to_string())?
                else {
                    return Err("public entry requires a file address".into());
                };
                ManifestEntry {
                    path,
                    size,
                    source: ContentRef::Public { address },
                }
            }
            Self::Embedded {
                path,
                size,
                data_map,
            } => {
                *embedded_bytes = embedded_bytes
                    .checked_add(data_map.len())
                    .filter(|total| *total <= MAX_MANIFEST_BYTES)
                    .ok_or("embedded DataMaps exceed the manifest size limit")?;
                ManifestEntry {
                    path,
                    size,
                    source: ContentRef::Embedded {
                        data_map: embeddable_data_map(&data_map)?,
                    },
                }
            }
        };
        if entry.size.is_some_and(|size| size > MAX_SAFE_JS_INTEGER) {
            return Err("size exceeds JavaScript's safe integer range".into());
        }
        // A root DataMap lists every chunk, so its size is exact.
        if entry
            .known_size()
            .zip(entry.size)
            .is_some_and(|(known, hint)| known != hint)
        {
            return Err("size does not match the embedded DataMap".into());
        }
        Ok(entry)
    }
}

/// Parse `hex` with the shared torrent hash parser, keeping the hash `pick`
/// selects: a hash of the other version's length is refused.
fn torrent_hash<T>(
    hex: Option<String>,
    field: &str,
    pick: impl FnOnce(TorrentReference) -> Option<T>,
) -> Result<Option<T>, String> {
    hex.map(|hex| {
        let parsed = TorrentReference::parse_hex(&hex).map_err(|e| e.to_string())?;
        pick(parsed).ok_or_else(|| format!("{field} has the length of the other torrent version"))
    })
    .transpose()
}

/// Decode a canonical DataMap and hold it to what a native writer embeds: a
/// root map only while it is small, otherwise the shrunk map an upload returns.
fn embeddable_data_map(bytes: &[u8]) -> Result<DataMap, String> {
    let data_map: DataMap =
        rmp_serde::from_slice(bytes).map_err(|e| format!("invalid DataMap: {e}"))?;
    if rmp_serde::to_vec(&data_map).map_err(|e| e.to_string())? != bytes {
        return Err("DataMap must use canonical MessagePack encoding".into());
    }
    let len = embedded_len(&data_map).map_err(|e| e.to_string())?;
    if !data_map.is_child() && len > MAX_EMBEDDED_ROOT_MAP_BYTES {
        return Err(format!(
            "a root DataMap of {len} bytes exceeds the {MAX_EMBEDDED_ROOT_MAP_BYTES}-byte \
             embedding limit; embed the shrunk DataMap its upload returned"
        ));
    }
    Ok(data_map)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Plaintext size of each chunk in a test DataMap.
    const CHUNK_SIZE: usize = 100;
    /// Enough chunk records to take a root map over the embedding limit.
    const OVERSIZED_CHUNKS: usize = MAX_EMBEDDED_ROOT_MAP_BYTES / manifest::CHUNK_RECORD_LEN + 1;

    fn data_map_bytes(chunks: usize, child: Option<usize>) -> Vec<u8> {
        let infos = (0..chunks)
            .map(|index| ChunkInfo {
                index,
                dst_hash: XorName::from_content(&index.to_be_bytes()),
                src_hash: XorName::from_content(&(index + chunks).to_be_bytes()),
                src_size: CHUNK_SIZE,
            })
            .collect();
        let data_map = match child {
            Some(level) => DataMap::with_child(infos, level),
            None => DataMap::new(infos),
        };
        rmp_serde::to_vec(&data_map).unwrap()
    }

    fn embedded(size: Option<u64>, data_map: Vec<u8>) -> ManifestInput {
        ManifestInput {
            name: None,
            torrent: None,
            entries: vec![EntryInput::Embedded {
                path: Some("file".into()),
                size,
                data_map,
            }],
        }
    }

    #[test]
    fn browser_inputs_round_trip_through_native_format() {
        let created = ManifestInput {
            name: Some("files".into()),
            torrent: None,
            entries: vec![
                EntryInput::Public {
                    path: Some("z.txt".into()),
                    size: Some(20),
                    address: "ab".repeat(32),
                },
                EntryInput::Public {
                    path: Some("a.txt".into()),
                    size: None,
                    address: "cd".repeat(32),
                },
            ],
        }
        .create()
        .expect("manifest");
        let decoded = Manifest::decode(&created.bytes).expect("native decode");
        assert_eq!(decoded.entries[0].path.as_deref(), Some("a.txt"));
        assert_eq!(
            manifest::parse_link(&created.link).expect("link"),
            manifest::Link::Manifest(decoded)
        );
        assert!(created.link_recommended);
    }

    #[test]
    fn browser_inputs_use_portable_paths_and_exact_sizes() {
        for (path, size) in [("../escape", 10), ("ok", u64::MAX)] {
            assert!(ManifestInput {
                name: None,
                torrent: None,
                entries: vec![EntryInput::Public {
                    path: Some(path.into()),
                    size: Some(size),
                    address: "ab".repeat(32),
                }]
            }
            .create()
            .is_err());
        }
    }

    #[test]
    fn an_empty_data_map_is_refused_rather_than_written_unreadable() {
        let error = embedded(None, data_map_bytes(0, None))
            .create()
            .unwrap_err();
        assert!(error.contains("at least one chunk"), "{error}");
    }

    #[test]
    fn a_root_data_map_fixes_the_size_and_decodes_natively() {
        let chunks = 3;
        let exact = u64::try_from(chunks * CHUNK_SIZE).unwrap();
        let error = embedded(Some(exact - 1), data_map_bytes(chunks, None))
            .create()
            .unwrap_err();
        assert!(error.contains("does not match"), "{error}");

        for size in [None, Some(exact)] {
            let created = embedded(size, data_map_bytes(chunks, None))
                .create()
                .expect("manifest");
            assert_eq!(created.manifest.entries[0].known_size, Some(exact));
            let decoded = Manifest::decode(&created.bytes).expect("native decode");
            assert_eq!(decoded.entries[0].size, size);
        }
    }

    #[test]
    fn only_a_small_root_data_map_is_embedded() {
        let error = embedded(None, data_map_bytes(OVERSIZED_CHUNKS, None))
            .create()
            .unwrap_err();
        assert!(error.contains("embedding limit"), "{error}");

        // A shrunk map carries no exact size, so any hint is kept as given.
        let created = embedded(Some(1), data_map_bytes(OVERSIZED_CHUNKS, Some(1)))
            .create()
            .expect("a shrunk map of any size is embedded");
        assert_eq!(created.manifest.entries[0].known_size, None);
        Manifest::decode(&created.bytes).expect("native decode");
    }

    #[test]
    fn torrent_hashes_are_parsed_by_version() {
        let v1 = "ab".repeat(manifest::TORRENT_INFO_HASH_V1_LEN);
        let v2 = "cd".repeat(manifest::TORRENT_INFO_HASH_V2_LEN);
        let with_torrent = |info_hash_v1: Option<&str>, info_hash_v2: Option<&str>| ManifestInput {
            name: None,
            torrent: Some(TorrentInput {
                info_hash_v1: info_hash_v1.map(Into::into),
                info_hash_v2: info_hash_v2.map(Into::into),
            }),
            entries: Vec::new(),
        };

        let created = with_torrent(Some(&v1), Some(&v2))
            .create()
            .expect("manifest");
        let torrent = created.manifest.torrent.expect("torrent");
        assert_eq!(torrent.info_hash_v1.as_deref(), Some(v1.as_str()));
        assert_eq!(torrent.info_hash_v2.as_deref(), Some(v2.as_str()));

        for (info_hash_v1, info_hash_v2) in [(Some(&v2), None), (None, Some(&v1)), (None, None)] {
            assert!(with_torrent(
                info_hash_v1.map(String::as_str),
                info_hash_v2.map(String::as_str)
            )
            .create()
            .is_err());
        }
    }
}
