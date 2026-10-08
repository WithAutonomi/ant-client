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
//! field has a default and is omitted when absent, unknown fields are
//! ignored, and only optional fields or new [`ContentRef`] variants may be
//! added within a format version. An unknown variant or an unknown header
//! version is a hard error so a reader never silently skips a file it cannot
//! resolve. The normative schema is in ADR-0006.

pub mod link;
pub mod path;
mod wire;

#[cfg(feature = "native")]
pub mod build;
#[cfg(feature = "native")]
pub mod compact;
#[cfg(feature = "native")]
pub mod embed;
#[cfg(feature = "native")]
pub mod extract;
#[cfg(feature = "native")]
pub mod file;
#[cfg(feature = "native")]
pub mod history;

#[cfg(feature = "native")]
pub use self::embed::embeddable_data_map;

use ant_protocol::compute_address;
use self_encryption::{shrink_data_map, DataMap};
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

pub use self::link::{
    file_link, is_link, manifest_link, manifest_link_bytes, manifest_link_from_bytes, parse_link,
    Link,
};
pub use self::wire::CHUNK_RECORD_LEN;

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
/// Largest root DataMap, as written in a manifest, that a `.ant` file embeds
/// in place of the shrunk map a large upload produces. A root map lets a
/// reader start fetching data chunks with no wrapper-record fetches; at
/// [`CHUNK_RECORD_LEN`] bytes per chunk of up to about 4 MiB this covers
/// files up to about 4 GB. Links carry the published (shrunk) form instead
/// (see [`Manifest::link_form`]).
pub const MAX_EMBEDDED_ROOT_MAP_BYTES: usize = 64 * 1024;
/// Deepest msgpack nesting the decoder tolerates. A manifest nests five
/// levels; this leaves headroom for future optional fields.
const MAX_DECODE_DEPTH: usize = 16;
/// Length of a BitTorrent v1 info hash (SHA-1, BEP 3).
pub const TORRENT_INFO_HASH_V1_LEN: usize = 20;
/// Length of a BitTorrent v2 info hash (SHA-256, BEP 52).
pub const TORRENT_INFO_HASH_V2_LEN: usize = 32;

/// A set of files and how to fetch each one.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Suggested root directory name. One path component, subject to the
    /// portable path rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The BitTorrent identity of the same set of files, when the creator
    /// has one. Carried so a manifest can be matched to a torrent; what a
    /// client does with it is a later decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub torrent: Option<TorrentReference>,
    /// Sorted by effective name once encoded. Effective names are unique.
    #[serde(default)]
    pub entries: Vec<ManifestEntry>,
}

/// A BitTorrent info hash identifying the same files (BEP 3 and BEP 52).
/// At least one of the two hashes must be present.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TorrentReference {
    /// SHA-1 info hash of a v1 torrent.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "serde_bytes")]
    pub info_hash_v1: Option<[u8; TORRENT_INFO_HASH_V1_LEN]>,
    /// SHA-256 info hash of a v2 or hybrid torrent.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "serde_bytes")]
    pub info_hash_v2: Option<[u8; TORRENT_INFO_HASH_V2_LEN]>,
}

impl TorrentReference {
    /// Whether at least one hash is present.
    pub fn is_empty(&self) -> bool {
        self.info_hash_v1.is_none() && self.info_hash_v2.is_none()
    }

    /// Parse a hex info hash: 40 characters select v1, 64 select v2.
    pub fn parse_hex(hex_str: &str) -> Result<Self, ManifestError> {
        let bytes = hex::decode(hex_str.trim())
            .map_err(|e| ManifestError::InvalidTorrentHash(format!("not hex: {e}")))?;
        let mut reference = Self::default();
        match bytes.len() {
            TORRENT_INFO_HASH_V1_LEN => {
                let mut hash = [0u8; TORRENT_INFO_HASH_V1_LEN];
                hash.copy_from_slice(&bytes);
                reference.info_hash_v1 = Some(hash);
            }
            TORRENT_INFO_HASH_V2_LEN => {
                let mut hash = [0u8; TORRENT_INFO_HASH_V2_LEN];
                hash.copy_from_slice(&bytes);
                reference.info_hash_v2 = Some(hash);
            }
            other => {
                return Err(ManifestError::InvalidTorrentHash(format!(
                    "expected {TORRENT_INFO_HASH_V1_LEN} (v1) or {TORRENT_INFO_HASH_V2_LEN} (v2) bytes, got {other}"
                )))
            }
        }
        Ok(reference)
    }

    /// Combine two references, each contributing the hashes it has. A hash
    /// present in both must agree.
    pub fn merge(self, other: Self) -> Result<Self, ManifestError> {
        fn pick<const N: usize>(
            a: Option<[u8; N]>,
            b: Option<[u8; N]>,
        ) -> Result<Option<[u8; N]>, ManifestError> {
            match (a, b) {
                (Some(x), Some(y)) if x != y => Err(ManifestError::InvalidTorrentHash(
                    "two different hashes given for the same torrent version".to_string(),
                )),
                (Some(x), _) => Ok(Some(x)),
                (None, y) => Ok(y),
            }
        }
        Ok(Self {
            info_hash_v1: pick(self.info_hash_v1, other.info_hash_v1)?,
            info_hash_v2: pick(self.info_hash_v2, other.info_hash_v2)?,
        })
    }
}

/// One file in a manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    /// Relative path, subject to the portable path rules. When absent the
    /// entry is extracted under its content address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Plaintext length in bytes as recorded by the creator. Unverified: a
    /// hint for display only. Use [`ManifestEntry::known_size`] for a
    /// figure that can be relied on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// Where the bytes come from.
    pub source: ContentRef,
}

/// How an entry's bytes are fetched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContentRef {
    /// The DataMap itself. Saves the DataMap fetch a `Public` entry needs.
    Embedded {
        /// The file's DataMap: its root map when the writer could embed it,
        /// otherwise the shrunk map a large upload publishes. Either form
        /// has the same content address.
        #[serde(with = "wire")]
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
    /// A file failed after preflight. Completed uploads remain recoverable
    /// from `partial`, including a file whose reference resolution failed.
    #[cfg(feature = "native")]
    #[error("manifest build failed at {path:?}: {cause}")]
    BuildFailed {
        /// Manifest path being processed when the failure occurred.
        path: String,
        /// The underlying failure.
        #[source]
        cause: Box<ManifestError>,
        /// Completed uploads and pre-existing entries.
        partial: Box<build::BuildResult>,
    },
    /// The caller cancelled the operation.
    #[error("operation cancelled")]
    Cancelled,
    /// A torrent reference is malformed or empty.
    #[error("invalid torrent info hash: {0}")]
    InvalidTorrentHash(String),
    /// A filesystem error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// A network or data error from the client.
    #[error(transparent)]
    Data(#[from] crate::data::error::Error),
}

/// The form of `data_map` a public upload stores as its DataMap chunk: the
/// map shrunk until it lists at most three chunks, exactly as an upload
/// shrinks it. A map that is already that small is returned unchanged.
/// Computed locally; self-encryption is deterministic, so a root map and the
/// shrunk map it came from give the same result.
pub fn published_data_map(data_map: &DataMap) -> Result<DataMap, ManifestError> {
    shrink_data_map(data_map.clone(), |_, _| Ok(()))
        .map(|(shrunk, _)| shrunk)
        .map_err(|e| ManifestError::Encode(format!("DataMap could not be shrunk: {e}")))
}

/// The address `data_map` has, or would have, as a public file: the hash of
/// its published form's DataMap chunk bytes (self_encryption's versioned
/// msgpack, as the network stores them).
pub fn data_map_address(data_map: &DataMap) -> Result<[u8; ADDRESS_LEN], ManifestError> {
    let bytes = rmp_serde::to_vec(&published_data_map(data_map)?)
        .map_err(|e| ManifestError::Encode(format!("DataMap did not serialize: {e}")))?;
    Ok(compute_address(&bytes))
}

/// Bytes `data_map` occupies in an encoded manifest, as written.
pub fn embedded_len(data_map: &DataMap) -> Result<usize, ManifestError> {
    let mut bytes = Vec::new();
    wire::serialize(
        data_map,
        &mut rmp_serde::Serializer::new(&mut bytes).with_struct_map(),
    )
    .map_err(|e| ManifestError::Encode(e.to_string()))?;
    Ok(bytes.len())
}

impl ContentRef {
    /// The address that identifies this entry's content.
    ///
    /// For `Public` it is the address itself. For `Embedded` it is derived
    /// from the embedded DataMap by [`data_map_address`], so it is the
    /// file's public address whether the root or the shrunk map was
    /// embedded, and whether or not the file was ever made public. Nothing
    /// in the bytes can make it disagree with the DataMap.
    pub fn content_address(&self) -> Result<[u8; ADDRESS_LEN], ManifestError> {
        match self {
            Self::Public { address } => Ok(*address),
            Self::Embedded { data_map } => data_map_address(data_map),
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
    /// The plaintext size the file is known to have: from an embedded root
    /// DataMap, which lists every chunk. `None` for a `Public` entry or a
    /// shrunk map, whose size is not known until it is resolved.
    pub fn known_size(&self) -> Option<u64> {
        match &self.source {
            ContentRef::Embedded { data_map } if !data_map.is_child() => {
                u64::try_from(data_map.original_file_size()).ok()
            }
            _ => None,
        }
    }

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
            torrent: None,
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
        if self
            .torrent
            .as_ref()
            .is_some_and(TorrentReference::is_empty)
        {
            return Err(ManifestError::InvalidTorrentHash(
                "a torrent reference needs a v1 or v2 info hash".to_string(),
            ));
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

    /// The manifest as a link carries it: every embedded DataMap replaced by
    /// its published (shrunk) form, at most three chunks. Lossless: each
    /// entry keeps its content address, and a reader resolves the root
    /// from the wrapper records the upload stored. Keeps links short; a
    /// `.ant` file keeps the root maps.
    pub fn link_form(&self) -> Result<Self, ManifestError> {
        let mut link = self.clone();
        for entry in &mut link.entries {
            if let ContentRef::Embedded { data_map } = &mut entry.source {
                *data_map = published_data_map(data_map)?;
            }
        }
        Ok(link)
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
    /// limits and the portable path rules. The structure, including the
    /// entry count, is checked before any entry is decoded.
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
        wire::check_structure(body)?;
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

    /// Header; `{"name": "r", "entries": [`; the unnamed embedded entry,
    /// sorted first by its hex effective name, whose DataMap is one 68-byte
    /// chunk record (dst hash, src hash, big-endian u32 size) in a `bin`;
    /// then the `Public` entry `a` with its address as a 32-byte `bin`.
    const GOLDEN_V1_HEX: &str = concat!(
        "c1414e5401",
        "82a46e616d65a172a7656e747269657392",
        "81a6736f7572636581a8456d62656464656481a8646174615f6d617081a66368756e6b73c444",
        "2222222222222222222222222222222222222222222222222222222222222222",
        "3333333333333333333333333333333333333333333333333333333333333333",
        "00000102",
        "83a470617468a161a473697a6505a6736f7572636581a65075626c696381a761646472657373c420",
        "1111111111111111111111111111111111111111111111111111111111111111",
    );

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
            torrent: None,
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
        let hex_name = hex::encode(data_map_address(&data_map(9)).unwrap());
        assert_eq!(
            names,
            vec!["a.txt".to_string(), hex_name, "photos/b.jpg".into()]
        );
        assert_eq!(decoded.name.as_deref(), Some("album"));
        assert_eq!(decoded.total_size(), None);
    }

    #[test]
    fn decode_rejects_manifest_names_with_separators() {
        for name in ["../outside", "/tmp/outside", "a/b", "a\\b"] {
            let mut manifest = sample();
            manifest.name = Some(name.into());
            assert!(matches!(
                manifest.encode(),
                Err(ManifestError::InvalidName(_))
            ));
            // Bypass the writer's validation to represent hostile input,
            // keeping every entry path valid to isolate the name defect.
            let mut bytes = MANIFEST_MAGIC.to_vec();
            bytes.push(MANIFEST_FORMAT_VERSION);
            bytes.extend(rmp_serde::to_vec_named(&manifest).unwrap());
            assert!(matches!(
                Manifest::decode(&bytes),
                Err(ManifestError::InvalidName(_))
            ));
        }
        let bytes = sample().encode().unwrap();
        assert_eq!(
            Manifest::decode(&bytes).unwrap().name.as_deref(),
            Some("album")
        );
    }

    /// A root map with more chunks than a published map may hold.
    fn large_root() -> DataMap {
        DataMap::new(
            (0..8u8)
                .map(|i| ChunkInfo {
                    index: usize::from(i),
                    dst_hash: XorName([i; 32]),
                    src_hash: XorName([i.wrapping_add(100); 32]),
                    src_size: 4_000_000,
                })
                .collect(),
        )
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
        deep.extend(std::iter::repeat_n(0x91, MAX_DECODE_DEPTH * 2));
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
        let mut bytes = MANIFEST_MAGIC.to_vec();
        bytes.push(MANIFEST_FORMAT_VERSION);
        bytes.extend(rmp_serde::to_vec_named(&too_many).unwrap());
        assert!(matches!(
            Manifest::decode(&bytes),
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
    fn torrent_reference_round_trips_and_is_absent_from_bytes_when_unset() {
        let without = sample().encode().unwrap();
        let mut with_torrent = sample();
        with_torrent.torrent = Some(TorrentReference {
            info_hash_v1: Some([0xaa; TORRENT_INFO_HASH_V1_LEN]),
            info_hash_v2: None,
        });
        let bytes = with_torrent.encode().unwrap();
        assert!(bytes.len() > without.len());
        assert_eq!(Manifest::decode(&bytes).unwrap(), {
            let mut expected = with_torrent.clone();
            expected.canonicalize().unwrap();
            expected
        });
        // An absent reference is omitted from the bytes entirely.
        assert_eq!(without, {
            let mut plain = with_torrent.clone();
            plain.torrent = None;
            plain.encode().unwrap()
        });
    }

    #[test]
    fn torrent_reference_parsing_merging_and_validation() {
        let v1 = TorrentReference::parse_hex(&"ab".repeat(TORRENT_INFO_HASH_V1_LEN)).unwrap();
        assert_eq!(v1.info_hash_v1, Some([0xab; TORRENT_INFO_HASH_V1_LEN]));
        assert_eq!(v1.info_hash_v2, None);
        let v2 = TorrentReference::parse_hex(&"CD".repeat(TORRENT_INFO_HASH_V2_LEN)).unwrap();
        assert_eq!(v2.info_hash_v2, Some([0xcd; TORRENT_INFO_HASH_V2_LEN]));
        let both = v1.clone().merge(v2.clone()).unwrap();
        assert!(both.info_hash_v1.is_some() && both.info_hash_v2.is_some());
        let other_v1 = TorrentReference::parse_hex(&"ef".repeat(TORRENT_INFO_HASH_V1_LEN)).unwrap();
        assert!(matches!(
            v1.merge(other_v1),
            Err(ManifestError::InvalidTorrentHash(_))
        ));
        assert!(matches!(
            TorrentReference::parse_hex("abc"),
            Err(ManifestError::InvalidTorrentHash(_))
        ));
        assert!(matches!(
            TorrentReference::parse_hex(&"zz".repeat(TORRENT_INFO_HASH_V1_LEN)),
            Err(ManifestError::InvalidTorrentHash(_))
        ));

        let mut empty = Manifest::new(None);
        empty.torrent = Some(TorrentReference::default());
        assert!(matches!(
            empty.validate(),
            Err(ManifestError::InvalidTorrentHash(_))
        ));
    }

    #[test]
    fn embedded_content_address_is_the_published_record_address() {
        // A small map is its own published form.
        let dm = data_map(4);
        let entry = ContentRef::Embedded {
            data_map: dm.clone(),
        };
        assert_eq!(
            entry.content_address().unwrap(),
            compute_address(&rmp_serde::to_vec(&dm).unwrap())
        );

        // A root map and its shrunk map share one identity: the address of
        // the shrunk map's chunk, which is what a public upload stores.
        let root = large_root();
        let shrunk = published_data_map(&root).unwrap();
        assert!(shrunk.is_child());
        assert!(shrunk.len() < root.len());
        let expected = compute_address(&rmp_serde::to_vec(&shrunk).unwrap());
        assert_eq!(data_map_address(&root).unwrap(), expected);
        assert_eq!(data_map_address(&shrunk).unwrap(), expected);
        assert_eq!(published_data_map(&shrunk).unwrap(), shrunk);
    }

    #[test]
    fn link_form_carries_published_maps_and_keeps_identity() {
        let mut manifest = sample();
        manifest.entries.push(ManifestEntry {
            path: Some("big.bin".into()),
            size: None,
            source: ContentRef::Embedded {
                data_map: large_root(),
            },
        });
        let link = manifest.link_form().unwrap();
        assert!(link.encode().unwrap().len() < manifest.encode().unwrap().len());
        for (full, linked) in manifest.entries.iter().zip(&link.entries) {
            assert_eq!(
                full.source.content_address().unwrap(),
                linked.source.content_address().unwrap()
            );
            if let ContentRef::Embedded { data_map } = &linked.source {
                assert!(data_map.len() <= 3);
            }
        }
    }

    #[test]
    fn known_size_comes_from_a_root_map_only() {
        let root = large_root();
        let entry = |source| ManifestEntry {
            path: None,
            size: Some(1),
            source,
        };
        assert_eq!(
            entry(ContentRef::Embedded {
                data_map: root.clone()
            })
            .known_size(),
            Some(root.original_file_size() as u64)
        );
        assert_eq!(
            entry(ContentRef::Embedded {
                data_map: published_data_map(&root).unwrap()
            })
            .known_size(),
            None
        );
        assert_eq!(
            entry(ContentRef::Public { address: [1; 32] }).known_size(),
            None
        );
    }

    /// Golden bytes: the v1 encoding of a fixed manifest. Any change to these
    /// bytes is a format change and needs a new version or an ADR amendment.
    #[test]
    fn encoding_matches_the_committed_fixture() {
        let manifest = Manifest {
            name: Some("r".into()),
            torrent: None,
            entries: vec![
                ManifestEntry {
                    path: Some("a".into()),
                    size: Some(5),
                    source: ContentRef::Public {
                        address: [0x11; 32],
                    },
                },
                ManifestEntry {
                    path: None,
                    size: None,
                    source: ContentRef::Embedded {
                        data_map: DataMap::new(vec![ChunkInfo {
                            index: 0,
                            dst_hash: XorName([0x22; 32]),
                            src_hash: XorName([0x33; 32]),
                            src_size: 0x0102,
                        }]),
                    },
                },
            ],
        };
        assert_eq!(hex::encode(manifest.encode().unwrap()), GOLDEN_V1_HEX);
        assert_eq!(
            Manifest::decode(&hex::decode(GOLDEN_V1_HEX).unwrap()).unwrap(),
            {
                let mut expected = manifest.clone();
                expected.canonicalize().unwrap();
                expected
            }
        );
    }

    #[test]
    fn decode_rejects_positional_encoding() {
        let mut bytes = MANIFEST_MAGIC.to_vec();
        bytes.push(MANIFEST_FORMAT_VERSION);
        bytes.extend(rmp_serde::to_vec(&sample()).unwrap());
        assert!(matches!(
            Manifest::decode(&bytes),
            Err(ManifestError::Decode(_))
        ));
    }

    #[test]
    fn absent_optionals_are_omitted_not_nil() {
        let manifest = Manifest {
            name: None,
            torrent: None,
            entries: vec![ManifestEntry {
                path: None,
                size: None,
                source: ContentRef::Public { address: [1; 32] },
            }],
        };
        let bytes = manifest.encode().unwrap();
        assert!(!bytes[MANIFEST_HEADER_LEN..].contains(&0xc0));
    }

    #[test]
    fn deserializer_depth_limit_setter_is_in_effect() {
        let mut de = Deserializer::from_read_ref(&[0x90u8][..]);
        de.set_max_depth(MAX_DECODE_DEPTH);
        let v: Vec<u8> = serde::Deserialize::deserialize(&mut de).unwrap();
        assert!(v.is_empty());
    }
}
