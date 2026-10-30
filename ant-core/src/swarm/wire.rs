//! What two clients say to each other over their WebRTC data channel.
//!
//! Every message is one binary data channel message: a type byte, then
//! big-endian fields. A record goes as [`Message::Piece`] frames of at most
//! [`FRAME_BYTES`], in order (the channel is reliable and ordered), and the
//! receiver checks the whole record against its address before using it.
//! Everything a peer sends is bounded before anything is allocated for it.

use ant_protocol::pqc::api::MlDsaVariant;
use bytes::Bytes;

use crate::data::XorName;

/// The largest record (a 4 MiB chunk) a piece may carry in all.
pub(crate) const MAX_RECORD_BYTES: usize = ant_protocol::MAX_CHUNK_SIZE;
/// The data of one piece frame. Browsers deliver data channel messages of
/// up to 256 KiB everywhere; 16 KiB frames keep any one message small.
pub(crate) const FRAME_BYTES: usize = 16 * 1024;
/// Addresses in one [`Message::Have`].
pub(crate) const MAX_HAVE_ADDRESSES: usize = 512;
/// The largest message a peer may send.
pub(crate) const MAX_MESSAGE_BYTES: usize = 1 + 4 + 4 + 4 + FRAME_BYTES;
/// A member's key for one room, as [`Message::Hello`] carries it.
pub(crate) const MEMBER_KEY_BYTES: usize = MlDsaVariant::MlDsa65.public_key_size();
/// A member's signature over one link, as [`Message::Proof`] carries it.
pub(crate) const MEMBER_SIGNATURE_BYTES: usize = MlDsaVariant::MlDsa65.signature_size();
const _: () = assert!(1 + 32 + MEMBER_SIGNATURE_BYTES <= MAX_MESSAGE_BYTES);
const _: () = assert!(MEMBER_KEY_BYTES < MAX_MESSAGE_BYTES);

const HELLO: u8 = 0x01;
const HAVE: u8 = 0x02;
const GET: u8 = 0x03;
const PIECE: u8 = 0x04;
const MISSING: u8 = 0x05;
const CANCEL: u8 = 0x06;
const BUSY: u8 = 0x07;
const PROOF: u8 = 0x08;

/// One message between two clients.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Message {
    /// The first message on a channel: the sender's key for this membership
    /// of the room, which names it there, so two links to the same client
    /// (met through two nodes) can be told apart, and which its
    /// [`Message::Proof`] is signed with.
    Hello { key: Vec<u8> },
    /// Records the sender holds and will serve.
    Have { addresses: Vec<XorName> },
    /// Asks for a record; answered by pieces or [`Message::Missing`].
    Get { id: u32, address: XorName },
    /// Part of the record asked for by `id`: `total` bytes in all, this frame
    /// starting at `offset`.
    Piece {
        id: u32,
        total: u32,
        offset: u32,
        data: Bytes,
    },
    /// The sender does not have the record asked for by `id` (any more).
    Missing { id: u32 },
    /// The asker no longer wants the record asked for by `id`.
    Cancel { id: u32 },
    /// The sender has the record asked for by `id` but is serving as many
    /// as it will; ask elsewhere.
    Busy { id: u32 },
    /// Sent with `Hello`: proof that the sender knows the room's secret, and
    /// holds the key it named, for this connection alone (see
    /// [`super::link::proof`] and [`super::link::signed`]). Nothing of the
    /// room is said or served on a link before the peer's proof checks out.
    Proof { mac: [u8; 32], signature: Vec<u8> },
}

/// Why a message was not understood.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum WireError {
    #[error("empty message")]
    Empty,
    #[error("message too large")]
    TooLarge,
    #[error("unknown message type {0}")]
    UnknownType(u8),
    #[error("truncated message")]
    Truncated,
    #[error("piece out of bounds")]
    PieceOutOfBounds,
    #[error("too many addresses")]
    TooManyAddresses,
}

impl Message {
    pub(crate) fn encode(&self) -> Vec<u8> {
        match self {
            Self::Hello { key } => [&[HELLO][..], key].concat(),
            Self::Have { addresses } => {
                let mut out = Vec::with_capacity(1 + 2 + addresses.len() * 32);
                out.push(HAVE);
                let count = u16::try_from(addresses.len().min(MAX_HAVE_ADDRESSES)).unwrap_or(0);
                out.extend_from_slice(&count.to_be_bytes());
                for address in addresses.iter().take(usize::from(count)) {
                    out.extend_from_slice(address);
                }
                out
            }
            Self::Get { id, address } => {
                let mut out = Vec::with_capacity(37);
                out.push(GET);
                out.extend_from_slice(&id.to_be_bytes());
                out.extend_from_slice(address);
                out
            }
            Self::Piece {
                id,
                total,
                offset,
                data,
            } => {
                let mut out = Vec::with_capacity(13 + data.len());
                out.push(PIECE);
                out.extend_from_slice(&id.to_be_bytes());
                out.extend_from_slice(&total.to_be_bytes());
                out.extend_from_slice(&offset.to_be_bytes());
                out.extend_from_slice(data);
                out
            }
            Self::Missing { id } => [&[MISSING][..], &id.to_be_bytes()].concat(),
            Self::Cancel { id } => [&[CANCEL][..], &id.to_be_bytes()].concat(),
            Self::Busy { id } => [&[BUSY][..], &id.to_be_bytes()].concat(),
            Self::Proof { mac, signature } => [&[PROOF][..], &mac[..], signature].concat(),
        }
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        if bytes.len() > MAX_MESSAGE_BYTES {
            return Err(WireError::TooLarge);
        }
        let (&kind, rest) = bytes.split_first().ok_or(WireError::Empty)?;
        let mut reader = Reader(rest);
        let message = match kind {
            HELLO => Self::Hello {
                key: reader.bytes(MEMBER_KEY_BYTES)?.to_vec(),
            },
            HAVE => {
                let count = usize::from(u16::from_be_bytes(reader.array()?));
                if count > MAX_HAVE_ADDRESSES {
                    return Err(WireError::TooManyAddresses);
                }
                let mut addresses = Vec::with_capacity(count);
                for _ in 0..count {
                    addresses.push(reader.array()?);
                }
                Self::Have { addresses }
            }
            GET => Self::Get {
                id: u32::from_be_bytes(reader.array()?),
                address: reader.array()?,
            },
            PIECE => {
                let id = u32::from_be_bytes(reader.array()?);
                let total = u32::from_be_bytes(reader.array()?);
                let offset = u32::from_be_bytes(reader.array()?);
                let data = Bytes::copy_from_slice(reader.rest());
                let end = u64::from(offset) + data.len() as u64;
                if data.is_empty()
                    || data.len() > FRAME_BYTES
                    || usize::try_from(total).map_or(true, |total| total > MAX_RECORD_BYTES)
                    || end > u64::from(total)
                {
                    return Err(WireError::PieceOutOfBounds);
                }
                return Ok(Self::Piece {
                    id,
                    total,
                    offset,
                    data,
                });
            }
            MISSING => Self::Missing {
                id: u32::from_be_bytes(reader.array()?),
            },
            CANCEL => Self::Cancel {
                id: u32::from_be_bytes(reader.array()?),
            },
            BUSY => Self::Busy {
                id: u32::from_be_bytes(reader.array()?),
            },
            PROOF => Self::Proof {
                mac: reader.array()?,
                signature: reader.bytes(MEMBER_SIGNATURE_BYTES)?.to_vec(),
            },
            other => return Err(WireError::UnknownType(other)),
        };
        if !reader.rest().is_empty() {
            return Err(WireError::TooLarge);
        }
        Ok(message)
    }
}

/// Splits a record into the pieces that carry it.
pub(crate) fn pieces(id: u32, record: &Bytes) -> impl Iterator<Item = Message> + '_ {
    let total = u32::try_from(record.len()).unwrap_or(u32::MAX);
    (0..record.len()).step_by(FRAME_BYTES).map(move |start| {
        let end = (start + FRAME_BYTES).min(record.len());
        Message::Piece {
            id,
            total,
            offset: u32::try_from(start).unwrap_or(u32::MAX),
            data: record.slice(start..end),
        }
    })
}

/// Room an assembly starts with; it grows, doubling, as pieces arrive.
const ASSEMBLY_START: usize = 64 * 1024;

/// Puts a record back together from its pieces, in order. It holds no more
/// than the pieces that came, so a peer that names a large record and stops
/// costs only what it sent.
#[derive(Debug)]
pub(crate) struct Assembly {
    buffer: Vec<u8>,
    total: usize,
}

impl Assembly {
    /// Starts a record of `total` bytes, or `None` if that is out of bounds.
    pub(crate) fn new(total: u32) -> Option<Self> {
        let total = usize::try_from(total).ok()?;
        if total == 0 || total > MAX_RECORD_BYTES {
            return None;
        }
        Some(Self {
            buffer: Vec::new(),
            total,
        })
    }

    /// Adds the next piece. Returns the record once it is whole, or an error
    /// if the piece is not the next one, disagrees about the size, or is
    /// shorter than a frame without ending the record (so a record arrives
    /// in at most `total / FRAME_BYTES + 1` pieces).
    pub(crate) fn add(
        &mut self,
        total: u32,
        offset: u32,
        data: &[u8],
    ) -> Result<Option<Bytes>, WireError> {
        let expected = usize::try_from(total).ok();
        let at = usize::try_from(offset).ok();
        if expected != Some(self.total) || at != Some(self.buffer.len()) {
            return Err(WireError::PieceOutOfBounds);
        }
        let end = self.buffer.len() + data.len();
        if end > self.total || (data.len() != FRAME_BYTES && end != self.total) {
            return Err(WireError::PieceOutOfBounds);
        }
        if end > self.buffer.capacity() {
            // Doubling, but never past the record's size, so a whole record
            // holds exactly its bytes.
            let room = self
                .buffer
                .capacity()
                .saturating_mul(2)
                .max(ASSEMBLY_START)
                .max(end)
                .min(self.total);
            self.buffer
                .try_reserve_exact(room - self.buffer.len())
                .map_err(|_| WireError::TooLarge)?;
        }
        self.buffer.extend_from_slice(data);
        if self.buffer.len() == self.total {
            return Ok(Some(Bytes::from(std::mem::take(&mut self.buffer))));
        }
        Ok(None)
    }
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn array<const N: usize>(&mut self) -> Result<[u8; N], WireError> {
        let (head, tail) = self.0.split_at_checked(N).ok_or(WireError::Truncated)?;
        self.0 = tail;
        head.try_into().map_err(|_| WireError::Truncated)
    }

    fn bytes(&mut self, count: usize) -> Result<&'a [u8], WireError> {
        let (head, tail) = self.0.split_at_checked(count).ok_or(WireError::Truncated)?;
        self.0 = tail;
        Ok(head)
    }

    fn rest(&self) -> &'a [u8] {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_message_round_trips() {
        let messages = [
            Message::Hello {
                key: vec![7; MEMBER_KEY_BYTES],
            },
            Message::Have {
                addresses: vec![[1; 32], [2; 32]],
            },
            Message::Get {
                id: 9,
                address: [3; 32],
            },
            Message::Piece {
                id: 9,
                total: 5,
                offset: 2,
                data: Bytes::from_static(b"abc"),
            },
            Message::Missing { id: 9 },
            Message::Cancel { id: 9 },
            Message::Busy { id: 9 },
            Message::Proof {
                mac: [4; 32],
                signature: vec![5; MEMBER_SIGNATURE_BYTES],
            },
        ];
        for message in messages {
            assert_eq!(Message::decode(&message.encode()), Ok(message));
        }
    }

    #[test]
    fn malformed_messages_are_refused() {
        assert_eq!(Message::decode(&[]), Err(WireError::Empty));
        assert_eq!(Message::decode(&[0x7f]), Err(WireError::UnknownType(0x7f)));
        assert_eq!(Message::decode(&[GET, 0, 0]), Err(WireError::Truncated));
        // A key or a signature of any other length is not one.
        let short_key = [&[HELLO][..], &[1; MEMBER_KEY_BYTES - 1]].concat();
        assert_eq!(Message::decode(&short_key), Err(WireError::Truncated));
        let long_key = [&[HELLO][..], &[1; MEMBER_KEY_BYTES + 1]].concat();
        assert_eq!(Message::decode(&long_key), Err(WireError::TooLarge));
        let short_signature = [&[PROOF][..], &[1; 32], &[2; MEMBER_SIGNATURE_BYTES - 1]].concat();
        assert_eq!(Message::decode(&short_signature), Err(WireError::Truncated));
        let mut too_many = vec![HAVE];
        too_many.extend_from_slice(&u16::MAX.to_be_bytes());
        assert_eq!(Message::decode(&too_many), Err(WireError::TooManyAddresses));
        let past_end = Message::Piece {
            id: 1,
            total: 2,
            offset: 1,
            data: Bytes::from_static(b"ab"),
        };
        assert_eq!(
            Message::decode(&past_end.encode()),
            Err(WireError::PieceOutOfBounds)
        );
        let huge = Message::Piece {
            id: 1,
            total: u32::MAX,
            offset: 0,
            data: Bytes::from_static(b"a"),
        };
        assert_eq!(
            Message::decode(&huge.encode()),
            Err(WireError::PieceOutOfBounds)
        );
        assert_eq!(
            Message::decode(&vec![PIECE; MAX_MESSAGE_BYTES + 1]),
            Err(WireError::TooLarge)
        );
        let mut trailing = Message::Missing { id: 1 }.encode();
        trailing.push(0);
        assert_eq!(Message::decode(&trailing), Err(WireError::TooLarge));
    }

    #[test]
    fn an_assembly_holds_only_the_pieces_that_came() {
        let total = u32::try_from(MAX_RECORD_BYTES).unwrap_or(u32::MAX);
        let mut assembly = Assembly::new(total).unwrap_or_else(|| panic!("size"));
        let piece = vec![7u8; FRAME_BYTES];
        assert!(matches!(assembly.add(total, 0, &piece), Ok(None)));
        assert!(assembly.buffer.capacity() <= ASSEMBLY_START);
        // Grown piece by piece, a whole record holds exactly its bytes.
        let mut offset = FRAME_BYTES;
        let mut whole = None;
        while whole.is_none() {
            let at = u32::try_from(offset).unwrap_or(u32::MAX);
            whole = assembly
                .add(total, at, &piece)
                .unwrap_or_else(|e| panic!("{e}"));
            offset += FRAME_BYTES;
            assert!(assembly.buffer.capacity() <= MAX_RECORD_BYTES);
        }
        assert_eq!(whole.map(|record| record.len()), Some(MAX_RECORD_BYTES));
    }

    #[test]
    fn a_record_survives_being_split_into_pieces() {
        let record = Bytes::from(
            (0..(3 * FRAME_BYTES + 100))
                .map(|n| (n % 251) as u8)
                .collect::<Vec<_>>(),
        );
        let frames: Vec<Message> = pieces(4, &record).collect();
        assert_eq!(frames.len(), 4);
        let mut assembly = Assembly::new(u32::try_from(record.len()).unwrap_or(0))
            .unwrap_or_else(|| panic!("size"));
        let mut whole = None;
        for frame in frames {
            let decoded = Message::decode(&frame.encode());
            let Ok(Message::Piece {
                total,
                offset,
                data,
                ..
            }) = decoded
            else {
                panic!("not a piece: {decoded:?}");
            };
            whole = assembly
                .add(total, offset, &data)
                .unwrap_or_else(|e| panic!("{e}"));
        }
        assert_eq!(whole, Some(record));
    }

    #[test]
    fn out_of_order_or_resized_pieces_are_refused() {
        let total = u32::try_from(FRAME_BYTES + 10).unwrap_or(0);
        let frame = vec![1u8; FRAME_BYTES];
        let mut assembly = Assembly::new(total).unwrap_or_else(|| panic!("size"));
        assert!(assembly.add(total, 5, &frame).is_err());
        assert!(assembly.add(total + 1, 0, &frame).is_err());
        assert_eq!(assembly.add(total, 0, &frame), Ok(None));
        assert!(assembly
            .add(total, u32::try_from(FRAME_BYTES).unwrap_or(0), &[1; 11])
            .is_err());
        assert!(Assembly::new(0).is_none());
        assert!(Assembly::new(u32::MAX).is_none());
    }

    #[test]
    fn a_piece_short_of_a_frame_must_end_the_record() {
        let mut assembly = Assembly::new(10).unwrap_or_else(|| panic!("size"));
        assert!(assembly.add(10, 0, b"abcde").is_err());
        let mut whole = Assembly::new(10).unwrap_or_else(|| panic!("size"));
        assert_eq!(
            whole.add(10, 0, b"abcdefghij"),
            Ok(Some(Bytes::from_static(b"abcdefghij")))
        );
    }
}
