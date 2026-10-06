//! File manifests (ADR-0006).
//!
//! A manifest lists a set of files, each with an optional relative path and
//! size, and says how to fetch each one: from a DataMap embedded in the
//! manifest or from the address of a public DataMap chunk. It is Autonomi's
//! equivalent of a `.torrent` file, minus trackers and peers.
//!
//! A manifest is shared off the network, as a `.ant` file or as an
//! `ant://manifest/<base64url>` link carrying the same bytes. It is never
//! stored on the network.
//!
//! # Byte form
//!
//! ```text
//! C1 41 4E 54   magic ("\xC1ANT")
//! 01            format version
//! ...           named msgpack encoding of `Manifest`
//! ```
//!
//! `0xC1` is unused by the msgpack specification, so no DataMap can begin
//! with it; the first byte alone tells a `.ant` file from a `.datamap` file,
//! but a decoder requires the whole five-byte header.
//!
//! # Forward compatibility
//!
//! Structs are encoded as msgpack maps with field names, every optional
//! field has a default, unknown fields are ignored, and only optional fields
//! or new [`ContentRef`] variants may be added within a format version. An
//! unknown variant or an unknown header version is a hard error so a reader
//! never silently skips a file it cannot resolve.

pub mod link;
pub mod path;

#[cfg(feature = "native")]
pub mod build;
#[cfg(feature = "native")]
pub mod compact;
#[cfg(feature = "native")]
pub mod extract;
#[cfg(feature = "native")]
pub mod file;
#[cfg(feature = "native")]
pub mod history;

use ant_protocol::compute_address;
use self_encryption::DataMap;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use self::path::PathError;

#[cfg(test)]
use rmp_serde::Deserializer;
#[cfg(test)]
use self_encryption::ChunkInfo;
#[cfg(test)]
use serde::ser::{SerializeMap, Serializer};
#[cfg(test)]
use xor_name::XorName;

pub use self::link::{file_link, is_link, manifest_link, parse_link, Link};

/// The four magic bytes that open every manifest.
pub const MANIFEST_MAGIC: [u8; 4] = [0xC1, b'A', b'N', b'T'];
/// The format version this code writes and the only one it reads.
pub const MANIFEST_FORMAT_VERSION: u8 = 1;
/// Magic plus version byte.
pub const MANIFEST_HEADER_LEN: usize = MANIFEST_MAGIC.len() + 1;
/// File extension for a manifest on disk.
pub const MANIFEST_EXTENSION: &str = "ant";
/// Largest manifest a decoder will accept.
pub const MAX_MANIFEST_BYTES: usize = 64 * 1024 * 1024;
/// Most entries a manifest may hold.
pub const MAX_MANIFEST_ENTRIES: usize = 100_000;
/// Encoded size above which tools warn that a manifest link is getting
/// long for a chat message and suggest a `.ant` file instead.
pub const MANIFEST_LINK_RECOMMENDED_MAX_BYTES: usize = 1_500;
/// Length of a content address in bytes.
pub const ADDRESS_LEN: usize = 32;
/// Deepest msgpack nesting the decoder tolerates. A manifest nests four
/// levels; this leaves headroom for future optional fields.
const MAX_DECODE_DEPTH: usize = 16;

/// A set of files and how to fetch each one.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Suggested root directory name. One path component, subject to the
    /// portable path rules.
    #[serde(default)]
    pub name: Option<String>,
    /// Sorted by effective name once encoded. Effective names are unique.
    #[serde(default)]
    pub entries: Vec<ManifestEntry>,
}

/// One file in a manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    /// Relative path, subject to the portable path rules. When absent the
    /// entry is extracted under its content address.
    #[serde(default)]
    pub path: Option<String>,
    /// Plaintext length in bytes as recorded by the creator. A hint for
    /// display only; nothing is decided by it.
    #[serde(default)]
    pub size: Option<u64>,
    /// Where the bytes come from.
    pub source: ContentRef,
}

/// How an entry's bytes are fetched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContentRef {
    /// The DataMap itself. Saves the DataMap fetch a `Public` entry needs.
    Embedded {
        /// The file's DataMap.
        data_map: DataMap,
    },
    /// Address of the file's public DataMap chunk.
    Public {
        /// The chunk address.
        #[serde(with = "serde_bytes")]
        address: [u8; ADDRESS_LEN],
    },
}

/// Errors from building, encoding, decoding or using a manifest.
#[derive(Debug, Error)]
pub enum ManifestError {
    /// The input exceeds [`MAX_MANIFEST_BYTES`].
    #[error("manifest too large: {len} bytes exceeds the {max} byte limit")]
    TooLarge {
        /// Observed length.
        len: usize,
        /// The limit.
        max: usize,
    },
    /// The manifest holds more than [`MAX_MANIFEST_ENTRIES`] entries.
    #[error("manifest has {count} entries, more than the {max} allowed")]
    TooManyEntries {
        /// Observed count.
        count: usize,
        /// The limit.
        max: usize,
    },
    /// The bytes do not start with the manifest header.
    #[error("not a manifest: missing or invalid header")]
    BadHeader,
    /// The header names a format version this code does not read.
    #[error("unsupported manifest format version {0}")]
    UnsupportedVersion(u8),
    /// Serialisation failed.
    #[error("manifest encoding failed: {0}")]
    Encode(String),
    /// Deserialisation failed.
    #[error("manifest decoding failed: {0}")]
    Decode(String),
    /// The manifest `name` breaks the portable path rules.
    #[error("invalid manifest name: {0}")]
    InvalidName(PathError),
    /// An entry path breaks the portable path rules.
    #[error("invalid entry path {path:?}: {reason}")]
    InvalidPath {
        /// The offending path.
        path: String,
        /// Which rule it broke.
        reason: PathError,
    },
    /// Two entries collide under the portable comparison.
    #[error("conflicting entries: {0}")]
    Conflict(PathError),
    /// A link could not be parsed.
    #[error("invalid link: {0}")]
    Link(String),
    /// Building or extracting hit a filesystem or layout problem.
    #[error("{0}")]
    Build(String),
    /// A filesystem error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// A network or data error from the client.
    #[error(transparent)]
    Data(#[from] crate::data::error::Error),
}

impl ContentRef {
    /// The address that identifies this entry's content.
    ///
    /// For `Public` it is the address itself. For `Embedded` it is the
    /// address the DataMap would have as a public chunk: the hash of its
    /// canonical positional msgpack bytes.
    pub fn content_address(&self) -> Result<[u8; ADDRESS_LEN], ManifestError> {
        match self {
            Self::Public { address } => Ok(*address),
            Self::Embedded { data_map } => {
                let bytes = rmp_serde::to_vec(data_map).map_err(|e| {
                    ManifestError::Encode(format!("embedded DataMap did not serialize: {e}"))
                })?;
                Ok(compute_address(&bytes))
            }
        }
    }

    /// Short label for display: `embedded` or `public`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Embedded { .. } => "embedded",
            Self::Public { .. } => "public",
        }
    }
}

impl ManifestEntry {
    /// The name this entry extracts under: its `path`, or the lowercase hex
    /// of its content address when it has none.
    pub fn effective_name(&self) -> Result<String, ManifestError> {
        match &self.path {
            Some(path) => Ok(path.clone()),
            None => Ok(hex::encode(self.source.content_address()?)),
        }
    }
}

impl Manifest {
    /// An empty manifest with an optional root name.
    pub fn new(name: Option<String>) -> Self {
        Self {
            name,
            entries: Vec::new(),
        }
    }

    /// Sum of the recorded sizes, or `None` when any entry lacks one.
    pub fn total_size(&self) -> Option<u64> {
        self.entries.iter().try_fold(0u64, |acc, entry| {
            entry.size.and_then(|s| acc.checked_add(s))
        })
    }

    /// Check the entry count, the name, every path and every collision rule.
    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.entries.len() > MAX_MANIFEST_ENTRIES {
            return Err(ManifestError::TooManyEntries {
                count: self.entries.len(),
                max: MAX_MANIFEST_ENTRIES,
            });
        }
        if let Some(name) = &self.name {
            path::validate_component(name).map_err(ManifestError::InvalidName)?;
        }
        let mut names = Vec::with_capacity(self.entries.len());
        for entry in &self.entries {
            if let Some(p) = &entry.path {
                path::validate_path(p).map_err(|reason| ManifestError::InvalidPath {
                    path: p.clone(),
                    reason,
                })?;
            }
            names.push(entry.effective_name()?);
        }
        path::check_collisions(names.iter().map(String::as_str)).map_err(ManifestError::Conflict)
    }

    /// Validate, then sort entries by effective name so encoding is
    /// deterministic.
    pub fn canonicalize(&mut self) -> Result<(), ManifestError> {
        self.validate()?;
        let mut keyed = self
            .entries
            .drain(..)
            .map(|entry| entry.effective_name().map(|name| (name, entry)))
            .collect::<Result<Vec<_>, _>>()?;
        keyed.sort_by(|a, b| a.0.cmp(&b.0));
        self.entries = keyed.into_iter().map(|(_, entry)| entry).collect();
        Ok(())
    }

    /// Encode to the byte form, validating and sorting first.
    pub fn encode(&self) -> Result<Vec<u8>, ManifestError> {
        let mut canonical = self.clone();
        canonical.canonicalize()?;
        let body = rmp_serde::to_vec_named(&canonical)
            .map_err(|e| ManifestError::Encode(e.to_string()))?;
        let mut bytes = Vec::with_capacity(MANIFEST_HEADER_LEN + body.len());
        bytes.extend_from_slice(&MANIFEST_MAGIC);
        bytes.push(MANIFEST_FORMAT_VERSION);
        bytes.extend_from_slice(&body);
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(ManifestError::TooLarge {
                len: bytes.len(),
                max: MAX_MANIFEST_BYTES,
            });
        }
        Ok(bytes)
    }

    /// Decode from the byte form, enforcing the size, depth and entry
    /// limits and the portable path rules.
    pub fn decode(bytes: &[u8]) -> Result<Self, ManifestError> {
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(ManifestError::TooLarge {
                len: bytes.len(),
                max: MAX_MANIFEST_BYTES,
            });
        }
        if bytes.len() < MANIFEST_HEADER_LEN || bytes[..MANIFEST_MAGIC.len()] != MANIFEST_MAGIC {
            return Err(ManifestError::BadHeader);
        }
        let version = bytes[MANIFEST_MAGIC.len()];
        if version != MANIFEST_FORMAT_VERSION {
            return Err(ManifestError::UnsupportedVersion(version));
        }
        let body = &bytes[MANIFEST_HEADER_LEN..];
        let mut deserializer = rmp_serde::Deserializer::from_read_ref(body);
        deserializer.set_max_depth(MAX_DECODE_DEPTH);
        let manifest = Manifest::deserialize(&mut deserializer)
            .map_err(|e| ManifestError::Decode(e.to_string()))?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Whether `bytes` start with the manifest magic. Cheap classification
    /// only; [`Manifest::decode`] still checks the full header.
    pub fn has_magic(bytes: &[u8]) -> bool {
        bytes.starts_with(&MANIFEST_MAGIC)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn data_map(seed: u8) -> DataMap {
        DataMap::new(vec![ChunkInfo {
            index: 0,
            dst_hash: XorName([seed; 32]),
            src_hash: XorName([seed.wrapping_add(1); 32]),
            src_size: 100,
        }])
    }

    fn sample() -> Manifest {
        Manifest {
            name: Some("album".into()),
            entries: vec![
                ManifestEntry {
                    path: Some("photos/b.jpg".into()),
                    size: Some(20),
                    source: ContentRef::Public { address: [7; 32] },
                },
                ManifestEntry {
                    path: Some("a.txt".into()),
                    size: Some(10),
                    source: ContentRef::Embedded {
                        data_map: data_map(1),
                    },
                },
                ManifestEntry {
                    path: None,
                    size: None,
                    source: ContentRef::Embedded {
                        data_map: data_map(9),
                    },
                },
            ],
        }
    }

    #[test]
    fn round_trip_sorts_and_preserves_entries() {
        let bytes = sample().encode().unwrap();
        assert!(Manifest::has_magic(&bytes));
        assert_eq!(bytes[MANIFEST_MAGIC.len()], MANIFEST_FORMAT_VERSION);
        let decoded = Manifest::decode(&bytes).unwrap();
        let names: Vec<_> = decoded
            .entries
            .iter()
            .map(|e| e.effective_name().unwrap())
            .collect();
        let hex_name = hex::encode(data_map_address(&data_map(9)));
        assert_eq!(
            names,
            vec!["a.txt".to_string(), hex_name, "photos/b.jpg".into()]
        );
        assert_eq!(decoded.name.as_deref(), Some("album"));
        assert_eq!(decoded.total_size(), None);
    }

    fn data_map_address(dm: &DataMap) -> [u8; 32] {
        compute_address(&rmp_serde::to_vec(dm).unwrap())
    }

    #[test]
    fn encoding_is_deterministic_regardless_of_input_order() {
        let a = sample().encode().unwrap();
        let mut reordered = sample();
        reordered.entries.reverse();
        let b = reordered.encode().unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn body_is_a_msgpack_map_not_an_array() {
        let bytes = sample().encode().unwrap();
        let first = bytes[MANIFEST_HEADER_LEN];
        // fixmap 0x80..=0x8f, map16 0xde, map32 0xdf
        assert!((0x80..=0x8f).contains(&first) || first == 0xde || first == 0xdf);
    }

    #[test]
    fn rejects_bad_header_and_unknown_version() {
        let bytes = sample().encode().unwrap();
        assert!(matches!(
            Manifest::decode(&bytes[1..]),
            Err(ManifestError::BadHeader)
        ));
        let mut wrong_version = bytes.clone();
        wrong_version[MANIFEST_MAGIC.len()] = MANIFEST_FORMAT_VERSION + 1;
        assert!(matches!(
            Manifest::decode(&wrong_version),
            Err(ManifestError::UnsupportedVersion(_))
        ));
        assert!(matches!(
            Manifest::decode(&MANIFEST_MAGIC),
            Err(ManifestError::BadHeader)
        ));
    }

    #[test]
    fn unknown_fields_are_ignored_and_missing_optionals_default() {
        // Hand-built msgpack body with an extra top-level field, an extra
        // entry field, an extra field inside the variant, and no optional
        // fields anywhere.
        let mut body = Vec::new();
        {
            let mut ser = rmp_serde::Serializer::new(&mut body).with_struct_map();
            let mut top = ser.serialize_map(Some(2)).unwrap();
            top.serialize_entry("future_field", &true).unwrap();
            top.serialize_entry(
                "entries",
                &vec![serde_json::json!({
                    "source": {"Public": {"address": serde_bytes::ByteBuf::from(vec![3u8; 32]), "future": 1}},
                    "another_future": "x"
                })],
            )
            .unwrap();
            top.end().unwrap();
        }
        let mut bytes = MANIFEST_MAGIC.to_vec();
        bytes.push(MANIFEST_FORMAT_VERSION);
        bytes.extend(body);
        let decoded = Manifest::decode(&bytes).unwrap();
        assert_eq!(decoded.name, None);
        assert_eq!(decoded.entries.len(), 1);
        assert_eq!(decoded.entries[0].path, None);
        assert_eq!(decoded.entries[0].size, None);
        assert_eq!(
            decoded.entries[0].source,
            ContentRef::Public { address: [3; 32] }
        );
    }

    #[test]
    fn unknown_variant_and_missing_payload_are_rejected() {
        for body_json in [
            serde_json::json!({"entries": [{"source": {"Inline": {"bytes": "x"}}}]}),
            serde_json::json!({"entries": [{"source": {"Public": {}}}]}),
        ] {
            let body = rmp_serde::to_vec_named(&body_json).unwrap();
            let mut bytes = MANIFEST_MAGIC.to_vec();
            bytes.push(MANIFEST_FORMAT_VERSION);
            bytes.extend(body);
            assert!(matches!(
                Manifest::decode(&bytes),
                Err(ManifestError::Decode(_))
            ));
        }
    }

    #[test]
    fn decode_enforces_limits() {
        let oversized = vec![0u8; MAX_MANIFEST_BYTES + 1];
        assert!(matches!(
            Manifest::decode(&oversized),
            Err(ManifestError::TooLarge { .. })
        ));

        let mut deep = MANIFEST_MAGIC.to_vec();
        deep.push(MANIFEST_FORMAT_VERSION);
        // fixmap with one key "name" (fixstr) whose value is nested arrays
        // far past the depth limit.
        deep.push(0x81);
        deep.push(0xa4);
        deep.extend_from_slice(b"name");
        for _ in 0..(MAX_DECODE_DEPTH * 2) {
            deep.push(0x91);
        }
        deep.push(0xc0);
        assert!(matches!(
            Manifest::decode(&deep),
            Err(ManifestError::Decode(_))
        ));

        let mut too_many = Manifest::new(None);
        too_many.entries = (0..=MAX_MANIFEST_ENTRIES)
            .map(|i| ManifestEntry {
                path: Some(format!("f{i}")),
                size: None,
                source: ContentRef::Public { address: [1; 32] },
            })
            .collect();
        assert!(matches!(
            too_many.validate(),
            Err(ManifestError::TooManyEntries { .. })
        ));
    }

    #[test]
    fn validate_rejects_bad_paths_and_collisions() {
        let mut m = Manifest::new(Some("bad:name".into()));
        assert!(matches!(m.validate(), Err(ManifestError::InvalidName(_))));
        m.name = None;
        m.entries.push(ManifestEntry {
            path: Some("../x".into()),
            size: None,
            source: ContentRef::Public { address: [1; 32] },
        });
        assert!(matches!(
            m.validate(),
            Err(ManifestError::InvalidPath { .. })
        ));
        m.entries[0].path = Some("A".into());
        m.entries.push(ManifestEntry {
            path: Some("a/b".into()),
            size: None,
            source: ContentRef::Public { address: [2; 32] },
        });
        assert!(matches!(m.validate(), Err(ManifestError::Conflict(_))));
    }

    #[test]
    fn embedded_content_address_matches_public_record_address() {
        let dm = data_map(4);
        let entry = ContentRef::Embedded {
            data_map: dm.clone(),
        };
        assert_eq!(entry.content_address().unwrap(), data_map_address(&dm));
    }

    #[test]
    fn deserializer_depth_limit_setter_is_in_effect() {
        let mut de = Deserializer::from_read_ref(&[0x90u8][..]);
        de.set_max_depth(MAX_DECODE_DEPTH);
        let v: Vec<u8> = serde::Deserialize::deserialize(&mut de).unwrap();
        assert!(v.is_empty());
    }
}
