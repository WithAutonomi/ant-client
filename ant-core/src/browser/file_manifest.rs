// Copyright 2026 MaidSafe.net limited.
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Browser inputs for the shared manifest encoder. No network or filesystem I/O.

use super::BrowserManifestView;
use crate::data::manifest::{self, ContentRef, Manifest, ManifestEntry, TorrentReference};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ManifestInput {
    pub name: Option<String>,
    pub torrent: Option<TorrentInput>,
    pub entries: Vec<EntryInput>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TorrentInput {
    pub info_hash_v1: Option<String>,
    pub info_hash_v2: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum EntryInput {
    Public {
        path: Option<String>,
        size: Option<u64>,
        address: String,
    },
    Embedded {
        path: Option<String>,
        size: Option<u64>,
        #[serde(rename = "dataMap", with = "serde_bytes")]
        data_map: Vec<u8>,
    },
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreatedManifest {
    pub manifest: BrowserManifestView,
    #[serde(with = "serde_bytes")]
    pub bytes: Vec<u8>,
    pub link: String,
    pub link_recommended: bool,
}

impl ManifestInput {
    pub fn create(self) -> Result<CreatedManifest, String> {
        if self.entries.len() > manifest::MAX_MANIFEST_ENTRIES {
            return Err("too many manifest entries".into());
        }
        let torrent = self
            .torrent
            .map(|input| -> Result<_, String> {
                fn hash<const N: usize>(value: Option<String>) -> Result<Option<[u8; N]>, String> {
                    value
                        .map(|s| {
                            hex::decode(s)
                                .map_err(|e| e.to_string())?
                                .try_into()
                                .map_err(|_| format!("torrent hash must be {N} bytes"))
                        })
                        .transpose()
                }
                Ok(TorrentReference {
                    info_hash_v1: hash(input.info_hash_v1)?,
                    info_hash_v2: hash(input.info_hash_v2)?,
                })
            })
            .transpose()?;
        let mut entries = Vec::with_capacity(self.entries.len());
        let mut data_map_bytes = 0usize;
        for entry in self.entries {
            let (path, size, source) = match entry {
                EntryInput::Public {
                    path,
                    size,
                    address,
                } => {
                    let manifest::Link::File(address) =
                        manifest::parse_link(&address).map_err(|e| e.to_string())?
                    else {
                        return Err("public entry requires a file address".into());
                    };
                    (path, size, ContentRef::Public { address })
                }
                EntryInput::Embedded {
                    path,
                    size,
                    data_map,
                } => {
                    data_map_bytes = data_map_bytes
                        .checked_add(data_map.len())
                        .ok_or("DataMap size overflow")?;
                    if data_map_bytes > manifest::MAX_MANIFEST_BYTES {
                        return Err("embedded DataMaps exceed the manifest size limit".into());
                    }
                    let decoded = rmp_serde::from_slice(&data_map)
                        .map_err(|e| format!("invalid DataMap: {e}"))?;
                    if rmp_serde::to_vec(&decoded).map_err(|e| e.to_string())? != data_map {
                        return Err("DataMap must use canonical MessagePack encoding".into());
                    }
                    (path, size, ContentRef::Embedded { data_map: decoded })
                }
            };
            if size.is_some_and(|n| n > super::manifest::MAX_SAFE_JS_INTEGER) {
                return Err("size exceeds JavaScript's safe integer range".into());
            }
            entries.push(ManifestEntry {
                path,
                size,
                sha256: None,
                source,
            });
        }
        let mut manifest = Manifest {
            name: self.name,
            torrent,
            entries,
        };
        manifest.canonicalize().map_err(|e| e.to_string())?;
        let bytes = manifest.encode().map_err(|e| e.to_string())?;
        let link_bytes = manifest::manifest_link_bytes(&manifest).map_err(|e| e.to_string())?;
        Ok(CreatedManifest {
            manifest: BrowserManifestView::from_manifest(&manifest).map_err(|e| e.to_string())?,
            bytes,
            link: manifest::manifest_link_from_bytes(&link_bytes),
            link_recommended: link_bytes.len() <= manifest::MANIFEST_LINK_RECOMMENDED_MAX_BYTES,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
