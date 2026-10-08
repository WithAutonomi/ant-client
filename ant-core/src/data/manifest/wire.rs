//! The manifest's own msgpack layout (ADR-0006).
//!
//! Two pieces live here. The first is the encoding of an embedded DataMap,
//! which the format defines itself instead of inheriting self_encryption's
//! serde form, so manifest bytes do not change when that crate's
//! serialisation does and hashes are written as `bin`, not integer arrays.
//! The second is a structural check run on untrusted bytes before typed
//! decoding: it rejects positional (array) encodings of structs, which the
//! typed decoder would otherwise accept, and enforces the entry limit
//! before any entry is materialised.

use self_encryption::{ChunkInfo, DataMap};
use serde::de::Error as _;
use serde::ser::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use xor_name::XorName;

use super::{ManifestError, MAX_DECODE_DEPTH, MAX_MANIFEST_ENTRIES};

/// Length of a chunk hash in an embedded DataMap.
const HASH_LEN: usize = 32;
/// Length of a chunk's plaintext size field: a big-endian `u32`.
const CHUNK_SIZE_LEN: usize = std::mem::size_of::<u32>();
/// One chunk record of an embedded DataMap: post-encryption hash,
/// pre-encryption hash, plaintext size. A chunk's index is its position.
pub const CHUNK_RECORD_LEN: usize = HASH_LEN + HASH_LEN + CHUNK_SIZE_LEN;

// msgpack markers (https://github.com/msgpack/msgpack/blob/master/spec.md).
const POS_FIXINT_MAX: u8 = 0x7f;
const FIXMAP_MIN: u8 = 0x80;
const FIXMAP_MAX: u8 = 0x8f;
const FIXARRAY_MIN: u8 = 0x90;
const FIXARRAY_MAX: u8 = 0x9f;
const FIXSTR_MIN: u8 = 0xa0;
const FIXSTR_MAX: u8 = 0xbf;
const FIXMAP_LEN_MASK: u8 = 0x0f;
const FIXARRAY_LEN_MASK: u8 = 0x0f;
const FIXSTR_LEN_MASK: u8 = 0x1f;
const NIL: u8 = 0xc0;
const NEVER_USED: u8 = 0xc1;
const FALSE: u8 = 0xc2;
const TRUE: u8 = 0xc3;
const BIN8: u8 = 0xc4;
const BIN16: u8 = 0xc5;
const BIN32: u8 = 0xc6;
const EXT8: u8 = 0xc7;
const EXT16: u8 = 0xc8;
const EXT32: u8 = 0xc9;
const FLOAT32: u8 = 0xca;
const FLOAT64: u8 = 0xcb;
const UINT8: u8 = 0xcc;
const UINT16: u8 = 0xcd;
const UINT32: u8 = 0xce;
const UINT64: u8 = 0xcf;
const INT8: u8 = 0xd0;
const INT16: u8 = 0xd1;
const INT32: u8 = 0xd2;
const INT64: u8 = 0xd3;
const FIXEXT1: u8 = 0xd4;
const FIXEXT2: u8 = 0xd5;
const FIXEXT4: u8 = 0xd6;
const FIXEXT8: u8 = 0xd7;
const FIXEXT16: u8 = 0xd8;
const STR8: u8 = 0xd9;
const STR16: u8 = 0xda;
const STR32: u8 = 0xdb;
const ARRAY16: u8 = 0xdc;
const ARRAY32: u8 = 0xdd;
const MAP16: u8 = 0xde;
const MAP32: u8 = 0xdf;
/// Bytes of the type tag every ext value carries before its payload.
const EXT_TYPE_LEN: usize = 1;

/// An entry: its `source` is a `ContentRef`.
static ENTRY: Shape = Shape::Struct(&[(
    "source",
    Shape::Variant(&[
        (
            "Embedded",
            Shape::Struct(&[("data_map", Shape::Struct(&[]))]),
        ),
        ("Public", Shape::Struct(&[])),
    ]),
)]);
/// The manifest body.
static MANIFEST: Shape = Shape::Struct(&[
    ("torrent", Shape::Struct(&[])),
    ("entries", Shape::List(&ENTRY, MAX_MANIFEST_ENTRIES)),
]);

/// The embedded DataMap fields as written.
#[derive(Serialize)]
struct DataMapOut {
    #[serde(with = "serde_bytes")]
    chunks: Vec<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    child: Option<u64>,
}

/// The embedded DataMap fields as read; unknown fields are ignored.
#[derive(Deserialize)]
struct DataMapIn {
    #[serde(with = "serde_bytes")]
    chunks: Vec<u8>,
    #[serde(default)]
    child: Option<u64>,
}

/// Serialise a DataMap in the manifest layout. Used through
/// `#[serde(with = "wire")]`.
pub fn serialize<S: Serializer>(data_map: &DataMap, serializer: S) -> Result<S::Ok, S::Error> {
    let mut chunks = Vec::with_capacity(data_map.len() * CHUNK_RECORD_LEN);
    for (position, info) in data_map.infos().iter().enumerate() {
        if info.index != position {
            return Err(S::Error::custom(format!(
                "DataMap chunk {position} has index {}; indices must be contiguous from 0",
                info.index
            )));
        }
        let size = u32::try_from(info.src_size).map_err(|_| {
            S::Error::custom(format!("DataMap chunk {position} is too large to encode"))
        })?;
        chunks.extend_from_slice(&info.dst_hash.0);
        chunks.extend_from_slice(&info.src_hash.0);
        chunks.extend_from_slice(&size.to_be_bytes());
    }
    let child = data_map
        .child()
        .map(u64::try_from)
        .transpose()
        .map_err(|_| S::Error::custom("DataMap child level does not fit in u64"))?;
    DataMapOut { chunks, child }.serialize(serializer)
}

/// Deserialise a DataMap from the manifest layout.
pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<DataMap, D::Error> {
    let DataMapIn { chunks, child } = DataMapIn::deserialize(deserializer)?;
    let (records, remainder) = chunks.as_chunks::<CHUNK_RECORD_LEN>();
    if records.is_empty() || !remainder.is_empty() {
        return Err(D::Error::custom(format!(
            "embedded DataMap chunks must be a non-empty multiple of {CHUNK_RECORD_LEN} bytes, got {}",
            chunks.len()
        )));
    }
    let infos = records
        .iter()
        .enumerate()
        .map(|(index, record)| {
            let (dst, rest) = record.split_at(HASH_LEN);
            let (src, size) = rest.split_at(HASH_LEN);
            let mut dst_hash = [0u8; HASH_LEN];
            dst_hash.copy_from_slice(dst);
            let mut src_hash = [0u8; HASH_LEN];
            src_hash.copy_from_slice(src);
            let mut size_bytes = [0u8; CHUNK_SIZE_LEN];
            size_bytes.copy_from_slice(size);
            ChunkInfo {
                index,
                dst_hash: XorName(dst_hash),
                src_hash: XorName(src_hash),
                src_size: u32::from_be_bytes(size_bytes) as usize,
            }
        })
        .collect();
    match child {
        None => Ok(DataMap::new(infos)),
        Some(level) => usize::try_from(level)
            .map(|level| DataMap::with_child(infos, level))
            .map_err(|_| D::Error::custom("DataMap child level does not fit in usize")),
    }
}

/// What the structural check expects at one position of the document.
enum Shape {
    /// Any value; skipped.
    Any,
    /// A struct: a map keyed by field name, or nil for an absent optional.
    /// Listed fields are checked; any other field is skipped.
    Struct(&'static [(&'static str, Shape)]),
    /// An externally tagged enum: a one-entry map from variant name to
    /// body. An unknown variant is left for the typed decoder to reject.
    Variant(&'static [(&'static str, Shape)]),
    /// An array of the given shape holding at most `max` items.
    List(&'static Shape, usize),
}

/// Check the structure of a manifest body before typed decoding: every
/// struct is a map or nil, never an array, the entry list is within
/// [`MAX_MANIFEST_ENTRIES`], nesting is within the decode depth, and the
/// body is exactly one msgpack value. Type errors inside a field are left
/// to the typed decoder.
pub fn check_structure(body: &[u8]) -> Result<(), ManifestError> {
    let mut reader = Reader {
        bytes: body,
        pos: 0,
    };
    reader.check(&MANIFEST, 0)?;
    if reader.pos != body.len() {
        return Err(decode_error("trailing bytes after the manifest body"));
    }
    Ok(())
}

fn decode_error(message: impl Into<String>) -> ManifestError {
    ManifestError::Decode(message.into())
}

/// The kind and length of one msgpack value, its header consumed.
enum Header {
    /// A value whose bytes are fully consumed.
    Scalar,
    Nil,
    /// A string of this many bytes, not yet consumed.
    Str(usize),
    Array(usize),
    Map(usize),
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8], ManifestError> {
        let end = self
            .pos
            .checked_add(len)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(|| decode_error("manifest body ends early"))?;
        let slice = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn uint(&mut self, width: usize) -> Result<usize, ManifestError> {
        let value = self
            .take(width)?
            .iter()
            .fold(0u64, |acc, byte| (acc << u8::BITS) | u64::from(*byte));
        usize::try_from(value).map_err(|_| decode_error("length does not fit in memory"))
    }

    fn skip(&mut self, len: usize) -> Result<Header, ManifestError> {
        self.take(len)?;
        Ok(Header::Scalar)
    }

    fn header(&mut self) -> Result<Header, ManifestError> {
        let marker = self.take(1)?[0];
        match marker {
            0..=POS_FIXINT_MAX => Ok(Header::Scalar),
            FIXMAP_MIN..=FIXMAP_MAX => Ok(Header::Map(usize::from(marker & FIXMAP_LEN_MASK))),
            FIXARRAY_MIN..=FIXARRAY_MAX => {
                Ok(Header::Array(usize::from(marker & FIXARRAY_LEN_MASK)))
            }
            FIXSTR_MIN..=FIXSTR_MAX => Ok(Header::Str(usize::from(marker & FIXSTR_LEN_MASK))),
            NIL => Ok(Header::Nil),
            NEVER_USED => Err(decode_error("msgpack marker 0xc1 is never used")),
            FALSE | TRUE => Ok(Header::Scalar),
            BIN8 => {
                let len = self.uint(1)?;
                self.skip(len)
            }
            BIN16 => {
                let len = self.uint(2)?;
                self.skip(len)
            }
            BIN32 => {
                let len = self.uint(4)?;
                self.skip(len)
            }
            EXT8 => {
                let len = self.uint(1)?;
                self.skip(EXT_TYPE_LEN + len)
            }
            EXT16 => {
                let len = self.uint(2)?;
                self.skip(EXT_TYPE_LEN + len)
            }
            EXT32 => {
                let len = self.uint(4)?;
                self.skip(EXT_TYPE_LEN.saturating_add(len))
            }
            UINT8 | INT8 => self.skip(1),
            UINT16 | INT16 => self.skip(2),
            FLOAT32 | UINT32 | INT32 => self.skip(4),
            FLOAT64 | UINT64 | INT64 => self.skip(8),
            FIXEXT1 => self.skip(EXT_TYPE_LEN + 1),
            FIXEXT2 => self.skip(EXT_TYPE_LEN + 2),
            FIXEXT4 => self.skip(EXT_TYPE_LEN + 4),
            FIXEXT8 => self.skip(EXT_TYPE_LEN + 8),
            FIXEXT16 => self.skip(EXT_TYPE_LEN + 16),
            STR8 => Ok(Header::Str(self.uint(1)?)),
            STR16 => Ok(Header::Str(self.uint(2)?)),
            STR32 => Ok(Header::Str(self.uint(4)?)),
            ARRAY16 => Ok(Header::Array(self.uint(2)?)),
            ARRAY32 => Ok(Header::Array(self.uint(4)?)),
            MAP16 => Ok(Header::Map(self.uint(2)?)),
            MAP32 => Ok(Header::Map(self.uint(4)?)),
            // Negative fixint.
            _ => Ok(Header::Scalar),
        }
    }

    /// Consume the rest of a value whose header has been read.
    fn skip_body(&mut self, header: Header, depth: usize) -> Result<(), ManifestError> {
        match header {
            Header::Scalar | Header::Nil => Ok(()),
            Header::Str(len) => self.take(len).map(drop),
            Header::Array(len) => (0..len).try_for_each(|_| self.check(&Shape::Any, depth + 1)),
            Header::Map(len) => (0..len).try_for_each(|_| {
                self.check(&Shape::Any, depth + 1)?;
                self.check(&Shape::Any, depth + 1)
            }),
        }
    }

    /// Read a map key: its text when it is a string, `None` otherwise.
    fn key(&mut self, depth: usize) -> Result<Option<&'a [u8]>, ManifestError> {
        match self.header()? {
            Header::Str(len) => self.take(len).map(Some),
            other => self.skip_body(other, depth).map(|()| None),
        }
    }

    fn check(&mut self, shape: &Shape, depth: usize) -> Result<(), ManifestError> {
        if depth > MAX_DECODE_DEPTH {
            return Err(decode_error("manifest nests too deeply"));
        }
        let header = self.header()?;
        match (shape, header) {
            (Shape::Struct(_) | Shape::Variant(_), Header::Array(_)) => Err(decode_error(
                "positional (array) encoding is not allowed; structs are maps",
            )),
            (Shape::Struct(fields) | Shape::Variant(fields), Header::Map(len)) => {
                for _ in 0..len {
                    let key = self.key(depth + 1)?;
                    let field = key.and_then(|key| {
                        fields
                            .iter()
                            .find(|(name, _)| name.as_bytes() == key)
                            .map(|(_, shape)| shape)
                    });
                    self.check(field.unwrap_or(&Shape::Any), depth + 1)?;
                }
                Ok(())
            }
            (Shape::List(_, max), Header::Array(len)) if len > *max => {
                Err(ManifestError::TooManyEntries {
                    count: len,
                    max: *max,
                })
            }
            (Shape::List(item, _), Header::Array(len)) => {
                (0..len).try_for_each(|_| self.check(item, depth + 1))
            }
            (_, header) => self.skip_body(header, depth),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Holder {
        #[serde(with = "super")]
        data_map: DataMap,
    }

    fn chunk(index: usize, seed: u8) -> ChunkInfo {
        ChunkInfo {
            index,
            dst_hash: XorName([seed; HASH_LEN]),
            src_hash: XorName([seed.wrapping_add(1); HASH_LEN]),
            src_size: 1000 + index,
        }
    }

    #[test]
    fn data_map_round_trips_root_and_child() {
        for data_map in [
            DataMap::new(vec![chunk(0, 1), chunk(1, 2), chunk(2, 3), chunk(3, 4)]),
            DataMap::with_child(vec![chunk(0, 5), chunk(1, 6), chunk(2, 7)], 2),
        ] {
            let holder = Holder { data_map };
            let bytes = rmp_serde::to_vec_named(&holder).unwrap();
            assert_eq!(rmp_serde::from_slice::<Holder>(&bytes).unwrap(), holder);
        }
    }

    #[test]
    fn chunk_records_are_one_bin_value() {
        let holder = Holder {
            data_map: DataMap::new(vec![chunk(0, 1), chunk(1, 2), chunk(2, 3)]),
        };
        let bytes = rmp_serde::to_vec_named(&holder).unwrap();
        // {"data_map": {"chunks": bin8(3 records)}}: the hashes are raw bytes.
        let records = 3 * CHUNK_RECORD_LEN;
        assert!(bytes
            .windows(2)
            .any(|w| w == [BIN8, u8::try_from(records).unwrap()]));
        assert!(bytes.len() < records + 32);
    }

    #[test]
    fn data_map_rejects_bad_records_and_gapped_indices() {
        let truncated = rmp_serde::to_vec_named(&serde_json::json!({
            "data_map": {"chunks": serde_bytes::ByteBuf::from(vec![0u8; CHUNK_RECORD_LEN - 1])}
        }))
        .unwrap();
        assert!(rmp_serde::from_slice::<Holder>(&truncated).is_err());
        let empty = rmp_serde::to_vec_named(&serde_json::json!({
            "data_map": {"chunks": serde_bytes::ByteBuf::new()}
        }))
        .unwrap();
        assert!(rmp_serde::from_slice::<Holder>(&empty).is_err());

        let gapped = Holder {
            data_map: DataMap::new(vec![chunk(0, 1), chunk(2, 2)]),
        };
        assert!(rmp_serde::to_vec_named(&gapped).is_err());
    }

    #[test]
    fn structure_check_rejects_arrays_for_structs() {
        let positional = rmp_serde::to_vec(&serde_json::json!([null, [], []])).unwrap();
        assert!(matches!(
            check_structure(&positional),
            Err(ManifestError::Decode(_))
        ));
        let positional_entry = rmp_serde::to_vec_named(&serde_json::json!({
            "entries": [[null, null, {"Public": {"address": 1}}]]
        }))
        .unwrap();
        assert!(check_structure(&positional_entry).is_err());
        let positional_variant = rmp_serde::to_vec_named(&serde_json::json!({
            "entries": [{"source": {"Public": [1]}}]
        }))
        .unwrap();
        assert!(check_structure(&positional_variant).is_err());
    }

    #[test]
    fn structure_check_skips_unknown_fields_of_any_shape() {
        let body = rmp_serde::to_vec_named(&serde_json::json!({
            "future": [1, [2, {"x": -3}], "s", 1.5, null, true],
            "entries": [{"source": {"Future": [1, 2]}, "extra": {"a": [1]}}]
        }))
        .unwrap();
        check_structure(&body).unwrap();
    }

    #[test]
    fn structure_check_enforces_entry_count_and_trailing_bytes() {
        let mut body = vec![FIXMAP_MIN | 1, FIXSTR_MIN | 7];
        body.extend_from_slice(b"entries");
        body.push(ARRAY32);
        body.extend_from_slice(&u32::MAX.to_be_bytes());
        assert!(matches!(
            check_structure(&body),
            Err(ManifestError::TooManyEntries { .. })
        ));

        let mut trailing = rmp_serde::to_vec_named(&serde_json::json!({})).unwrap();
        trailing.push(NIL);
        assert!(check_structure(&trailing).is_err());
    }
}
