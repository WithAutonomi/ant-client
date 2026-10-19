// Copyright 2026 MaidSafe.net limited.
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Browser inputs for the shared manifest encoder. No network or filesystem I/O.

use super::manifest::MAX_SAFE_JS_INTEGER;
use super::BrowserManifestView;
use crate::data::manifest::{
    self, embedded_len, is_embeddable, ContentRef, Link, Manifest, ManifestEntry, ManifestError,
    TorrentReference, MANIFEST_LINK_RECOMMENDED_MAX_BYTES, MAX_EMBEDDED_ROOT_MAP_BYTES,
    MAX_MANIFEST_BYTES, MAX_MANIFEST_ENTRIES, MAX_PUBLISHED_MAP_CHUNKS,
};
use self_encryption::DataMap;
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

#[cfg(test)]
use self_encryption::ChunkInfo;
#[cfg(test)]
use xor_name::XorName;

/// The [`ManifestInput`] field holding a [`TorrentInput`], as JavaScript
/// names it.
pub const TORRENT_FIELD: &str = "torrent";

/// Why [`ManifestInput::create`] refused its input.
#[derive(Debug, Error)]
pub enum ManifestInputError {
    /// The shared encoder refused the manifest.
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    /// A public entry named something other than a file.
    #[error("public entry requires a file address")]
    NotAFileAddress,
    /// The embedded DataMaps alone exceed what one manifest can hold.
    #[error("embedded DataMaps exceed the {max}-byte manifest size limit")]
    EmbeddedTooLarge {
        /// The limit.
        max: usize,
    },
    /// An embedded DataMap did not decode.
    #[error("invalid DataMap: {0}")]
    InvalidDataMap(String),
    /// An embedded DataMap decoded, but not from its canonical encoding.
    #[error("DataMap must use canonical MessagePack encoding")]
    NonCanonicalDataMap,
    /// A root DataMap too large to embed, as a native writer would refuse.
    #[error(
        "a root DataMap of {len} bytes exceeds the {max}-byte embedding limit; \
         embed the shrunk DataMap its upload returned"
    )]
    RootDataMapTooLarge {
        /// Embedded length of the map.
        len: usize,
        /// The limit.
        max: usize,
    },
    /// A shrunk DataMap that is not the published form an upload returns,
    /// such as an intermediate shrink level.
    #[error(
        "a shrunk DataMap of {chunks} chunks is not the published form an upload returns, \
         which lists at most {max}"
    )]
    UnpublishedDataMap {
        /// Chunks the map lists.
        chunks: usize,
        /// The most a published map lists.
        max: usize,
    },
    /// A size hint JavaScript cannot represent exactly.
    #[error("size {0} exceeds JavaScript's safe integer range")]
    UnsafeSize(u64),
    /// A size hint that contradicts the size a root DataMap fixes.
    #[error("size {hint} does not match the embedded DataMap's {known} bytes")]
    SizeMismatch {
        /// The size given.
        hint: u64,
        /// The size the DataMap lists.
        known: u64,
    },
    /// A torrent hash given under the other version's field.
    #[error("{field} has the length of the other torrent version")]
    TorrentHashVersion {
        /// The input field.
        field: &'static str,
    },
}

/// The field names a struct deserializes from, exactly as its serde derive
/// declares them, after any `rename_all`. A JavaScript caller checks keys
/// against these: `serde_wasm_bindgen` reads only the declared fields, so
/// `deny_unknown_fields` cannot refuse the others there.
pub fn struct_fields<'de, T: Deserialize<'de>>() -> &'static [&'static str] {
    let mut recorder = FieldRecorder(&[]);
    // Fails by design: the recorder only notes the names it is handed.
    let _ = T::deserialize(&mut recorder);
    recorder.0
}

/// A deserializer that records the field names a struct asks for.
struct FieldRecorder(&'static [&'static str]);

impl<'de> Deserializer<'de> for &mut FieldRecorder {
    type Error = de::value::Error;

    fn deserialize_any<V: Visitor<'de>>(self, _: V) -> Result<V::Value, Self::Error> {
        Err(de::Error::custom("not a struct"))
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _: &'static str,
        fields: &'static [&'static str],
        _: V,
    ) -> Result<V::Value, Self::Error> {
        self.0 = fields;
        Err(de::Error::custom("field names recorded"))
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes
        byte_buf option unit unit_struct newtype_struct seq tuple tuple_struct map enum
        identifier ignored_any
    }
}

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
    ///
    /// The `.ant` bytes carry the map as given; the link carries its shrunk
    /// form, which a reader resolves through the wrapper records an upload
    /// stores. So a root map must be one an upload shrank, as every upload
    /// does, never one encrypted without being uploaded.
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
    pub fn create(self) -> Result<CreatedManifest, ManifestInputError> {
        // Counted before any entry is parsed or any DataMap decoded.
        if self.entries.len() > MAX_MANIFEST_ENTRIES {
            return Err(ManifestError::TooManyEntries {
                count: self.entries.len(),
                max: MAX_MANIFEST_ENTRIES,
            }
            .into());
        }
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
        // Validated and sorted once: the view lists entries in the order the
        // bytes do, and the link form keeps every entry's name, so it stays
        // canonical too.
        manifest.canonicalize()?;
        let bytes = manifest.encode_canonical()?;
        let link = manifest.link_form()?;
        let link_bytes = link.encode_canonical()?;
        Ok(CreatedManifest {
            manifest: BrowserManifestView::from_manifest_and_link(&manifest, &link)?,
            bytes,
            link: manifest::manifest_link_from_bytes(&link_bytes),
            link_recommended: link_bytes.len() <= MANIFEST_LINK_RECOMMENDED_MAX_BYTES,
        })
    }
}

impl TorrentInput {
    fn into_reference(self) -> Result<TorrentReference, ManifestInputError> {
        Ok(TorrentReference {
            info_hash_v1: torrent_hash(self.info_hash_v1, "infoHashV1", |t| t.info_hash_v1)?,
            info_hash_v2: torrent_hash(self.info_hash_v2, "infoHashV2", |t| t.info_hash_v2)?,
        })
    }
}

impl EntryInput {
    /// The manifest entry. `embedded_bytes` totals the DataMap input so far,
    /// so a page cannot make this decode more than a manifest can hold.
    fn into_entry(self, embedded_bytes: &mut usize) -> Result<ManifestEntry, ManifestInputError> {
        let entry = match self {
            Self::Public {
                path,
                size,
                address,
            } => {
                let Link::File(address) = manifest::parse_link(&address)? else {
                    return Err(ManifestInputError::NotAFileAddress);
                };
                ManifestEntry {
                    path,
                    size,
                    sha256: None,
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
                    .ok_or(ManifestInputError::EmbeddedTooLarge {
                        max: MAX_MANIFEST_BYTES,
                    })?;
                ManifestEntry {
                    path,
                    size,
                    sha256: None,
                    source: ContentRef::Embedded {
                        data_map: embeddable_data_map(&data_map)?,
                    },
                }
            }
        };
        if let Some(size) = entry.size.filter(|size| *size > MAX_SAFE_JS_INTEGER) {
            return Err(ManifestInputError::UnsafeSize(size));
        }
        // A root DataMap lists every chunk, so its size is exact.
        if let Some((known, hint)) = entry.known_size().zip(entry.size) {
            if known != hint {
                return Err(ManifestInputError::SizeMismatch { hint, known });
            }
        }
        Ok(entry)
    }
}

/// Parse `hex` with the shared torrent hash parser, keeping the hash `pick`
/// selects: a hash of the other version's length is refused.
fn torrent_hash<T>(
    hex: Option<String>,
    field: &'static str,
    pick: impl FnOnce(TorrentReference) -> Option<T>,
) -> Result<Option<T>, ManifestInputError> {
    hex.map(|hex| {
        let parsed = TorrentReference::parse_hex(&hex)?;
        pick(parsed).ok_or(ManifestInputError::TorrentHashVersion { field })
    })
    .transpose()
}

/// Decode a canonical DataMap and hold it to what a native writer embeds: a
/// root map only while it is small, otherwise the published map an upload
/// returns.
fn embeddable_data_map(bytes: &[u8]) -> Result<DataMap, ManifestInputError> {
    let data_map: DataMap = rmp_serde::from_slice(bytes)
        .map_err(|e| ManifestInputError::InvalidDataMap(e.to_string()))?;
    let canonical = rmp_serde::to_vec(&data_map)
        .map_err(|e| ManifestInputError::InvalidDataMap(e.to_string()))?;
    if canonical != bytes {
        return Err(ManifestInputError::NonCanonicalDataMap);
    }
    if is_embeddable(&data_map)? {
        return Ok(data_map);
    }
    Err(if data_map.is_child() {
        ManifestInputError::UnpublishedDataMap {
            chunks: data_map.len(),
            max: MAX_PUBLISHED_MAP_CHUNKS,
        }
    } else {
        ManifestInputError::RootDataMapTooLarge {
            len: embedded_len(&data_map)?,
            max: MAX_EMBEDDED_ROOT_MAP_BYTES,
        }
    })
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
    fn struct_fields_are_the_names_javascript_uses() {
        assert_eq!(
            struct_fields::<ManifestInput>(),
            ["name", TORRENT_FIELD, "entries"]
        );
        assert_eq!(
            struct_fields::<TorrentInput>(),
            ["infoHashV1", "infoHashV2"]
        );
    }

    #[test]
    fn an_embedded_entry_is_addressed_as_its_link_form_is() {
        // Enough chunks that the link form shrinks the map.
        let input = ManifestInput {
            name: None,
            torrent: None,
            entries: vec![EntryInput::Embedded {
                path: None,
                size: None,
                data_map: data_map_bytes(MAX_PUBLISHED_MAP_CHUNKS * 4, None),
            }],
        };
        let created = input.create().expect("manifest");
        let decoded = Manifest::decode(&created.bytes).expect("native decode");
        let address = hex::encode(decoded.entries[0].source.content_address().unwrap());
        let entry = &created.manifest.entries[0];
        assert_eq!(entry.address, address);
        assert_eq!(entry.name, address, "a path-less entry is named by it");
    }

    #[test]
    fn the_entry_count_is_checked_before_any_entry_is_parsed() {
        // Unparseable addresses: only the count can refuse these first.
        let entries = (0..=MAX_MANIFEST_ENTRIES)
            .map(|_| EntryInput::Public {
                path: None,
                size: None,
                address: String::new(),
            })
            .collect();
        let error = ManifestInput {
            name: None,
            torrent: None,
            entries,
        }
        .create()
        .unwrap_err();
        assert!(
            matches!(
                error,
                ManifestInputError::Manifest(ManifestError::TooManyEntries { .. })
            ),
            "{error}"
        );
    }

    #[test]
    fn an_empty_data_map_is_refused_rather_than_written_unreadable() {
        let error = embedded(None, data_map_bytes(0, None))
            .create()
            .unwrap_err()
            .to_string();
        assert!(error.contains("at least one chunk"), "{error}");
    }

    #[test]
    fn a_root_data_map_fixes_the_size_and_decodes_natively() {
        let chunks = 3;
        let exact = u64::try_from(chunks * CHUNK_SIZE).unwrap();
        let error = embedded(Some(exact - 1), data_map_bytes(chunks, None))
            .create()
            .unwrap_err();
        assert!(
            matches!(error, ManifestInputError::SizeMismatch { hint, known } if hint == exact - 1 && known == exact),
            "{error}"
        );

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
    fn only_a_small_root_or_published_data_map_is_embedded() {
        let error = embedded(None, data_map_bytes(OVERSIZED_CHUNKS, None))
            .create()
            .unwrap_err();
        assert!(
            matches!(error, ManifestInputError::RootDataMapTooLarge { .. }),
            "{error}"
        );

        // A published map carries no exact size, so any hint is kept as given.
        let created = embedded(Some(1), data_map_bytes(MAX_PUBLISHED_MAP_CHUNKS, Some(1)))
            .create()
            .expect("the published map an upload returns is embedded");
        assert_eq!(created.manifest.entries[0].known_size, None);
        Manifest::decode(&created.bytes).expect("native decode");

        // A larger shrunk map is an intermediate level no writer embeds.
        let error = embedded(None, data_map_bytes(MAX_PUBLISHED_MAP_CHUNKS + 1, Some(1)))
            .create()
            .unwrap_err();
        assert!(
            matches!(error, ManifestInputError::UnpublishedDataMap { .. }),
            "{error}"
        );
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
