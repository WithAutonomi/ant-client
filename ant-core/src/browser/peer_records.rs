// Copyright 2026 MaidSafe.net limited.
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Lossless browser wire adaptation for the shared client routing policy.

use super::protocol::{BrowserEndpoint, BrowserNode};
use crate::data::error::{Error as DataError, Result as DataResult};
use ant_protocol::transport::signed_address::{
    SignedAddressRecord, VerifiedAddressRecord, MAX_SIGNED_ADDRESS_BYTES,
};
use ant_protocol::transport::{DHTNode, MultiAddr, PeerId};
use lru::LruCache;
use std::{cell::RefCell, num::NonZeroUsize};

// Browser wire adaptation repeatedly visits the same immutable publications.
// Cache successful verification, not peer views: reliability and owner-view
// replacement policy must still be evaluated by each caller. Exact encodings
// include the signature, key, owner, sequence and all transport addresses.
const VERIFIED_ADDRESS_CACHE_CAPACITY: usize = 256;

thread_local! {
    static VERIFIED_ADDRESSES: RefCell<LruCache<String, VerifiedAddressRecord>> =
        RefCell::new(LruCache::new(
            NonZeroUsize::new(VERIFIED_ADDRESS_CACHE_CAPACITY).unwrap_or(NonZeroUsize::MIN),
        ));
}

pub(super) fn verified_address_record(encoded: &str) -> Result<VerifiedAddressRecord, String> {
    VERIFIED_ADDRESSES.with(|cache| verify_with_cache(&mut cache.borrow_mut(), encoded))
}

fn verify_with_cache(
    cache: &mut LruCache<String, VerifiedAddressRecord>,
    encoded: &str,
) -> Result<VerifiedAddressRecord, String> {
    if encoded.len() > MAX_SIGNED_ADDRESS_BYTES * 2 {
        return Err("signed address record exceeds limit".into());
    }
    if let Some(proof) = cache.get(encoded) {
        return Ok(proof.clone());
    }
    let bytes = hex::decode(encoded).map_err(|error| error.to_string())?;
    let proof = SignedAddressRecord::decode(&bytes)?.verify()?;
    cache.put(encoded.to_owned(), proof.clone());
    Ok(proof)
}

pub(crate) fn peer_record(node: &BrowserNode) -> DataResult<DHTNode> {
    use ant_protocol::transport::AddressType;
    let peer_id = PeerId::from_hex(&node.peer_id).map_err(|e| DataError::Network(e.to_string()))?;
    if let Some(encoded) = &node.address_record {
        let proof = verified_address_record(encoded).map_err(DataError::Protocol)?;
        if proof.owner() != peer_id {
            return Err(DataError::Protocol(
                "signed address record owner mismatch".into(),
            ));
        }
        return Ok(proof.peer_record(node.reliability));
    }
    let mut record = if let Some(encoded) = &node.peer_record {
        let bytes = hex::decode(encoded).map_err(|e| DataError::Protocol(e.to_string()))?;
        let record: DHTNode =
            rmp_serde::from_slice(&bytes).map_err(|e| DataError::Protocol(e.to_string()))?;
        if record.peer_id != peer_id {
            return Err(DataError::Protocol("peer record ID mismatch".into()));
        }
        record
    } else {
        DHTNode {
            peer_id,
            addresses: node
                .native_addresses
                .iter()
                .map(|address| address.parse::<MultiAddr>())
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| DataError::Protocol(e.to_string()))?,
            address_types: Vec::new(),
            distance: None,
            reliability: node.reliability,
            address_authority: None,
        }
    };
    // Neither legacy metadata nor a forwarded JSON object proves ownership.
    record.address_authority = None;
    // Dialability is an adapter concern. Keep native-only records in witness views.
    if let Some(endpoint) = &node.webrtc_direct {
        let address = endpoint
            .multiaddr
            .parse::<MultiAddr>()
            .map_err(|e| DataError::Network(e.to_string()))?;
        if address.peer_id() != Some(&peer_id) {
            return Err(DataError::Network(
                "endpoint belongs to another peer".into(),
            ));
        }
        if !record.addresses.contains(&address) {
            record.address_types = record
                .typed_addresses()
                .into_iter()
                .map(|(_, kind)| kind)
                .collect();
            record.addresses.push(address);
            record.address_types.push(AddressType::Unverified);
        }
    }
    Ok(record)
}

pub(crate) fn browser_record(record: DHTNode) -> DataResult<BrowserNode> {
    let encoded =
        rmp_serde::to_vec_named(&record).map_err(|e| DataError::Protocol(e.to_string()))?;
    Ok(BrowserNode {
        address_record: match &record.address_authority {
            Some(ant_protocol::transport::signed_address::AddressAuthority::Signed(proof)) => Some(
                hex::encode(proof.signed().encode().map_err(DataError::Protocol)?),
            ),
            _ => None,
        },
        peer_record: Some(hex::encode(encoded)),
        peer_id: record.peer_id.to_hex(),
        native_addresses: record
            .addresses
            .iter()
            .filter(|a| !a.is_webrtc_direct())
            .map(ToString::to_string)
            .collect(),
        reliability: record.reliability,
        webrtc_direct: record
            .addresses_by_priority()
            .into_iter()
            .find(|a| a.is_webrtc_direct())
            .map(|a| BrowserEndpoint {
                multiaddr: a.to_string(),
            }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ant_protocol::transport::{client_routing, AddressType};

    fn publication(identity: &ant_protocol::transport::NodeIdentity, sequence: u64) -> String {
        use ant_protocol::transport::{KnownReachability, TransportAddressRecord};
        let address: MultiAddr = "/ip4/9.9.9.9/udp/9000/quic".parse().unwrap();
        hex::encode(
            SignedAddressRecord::sign(
                identity,
                sequence,
                vec![
                    TransportAddressRecord::from_multiaddr(&address, KnownReachability::Direct)
                        .unwrap()
                        .unwrap(),
                ],
            )
            .unwrap()
            .encode()
            .unwrap(),
        )
    }

    #[test]
    fn verification_cache_reuses_exact_proofs_and_evicts_least_recently_used() {
        let identity = ant_protocol::transport::NodeIdentity::generate().unwrap();
        let [first, second, third] = [1, 2, 3].map(|seq| publication(&identity, seq));
        let mut cache = LruCache::new(NonZeroUsize::new(2).unwrap());
        let original = verify_with_cache(&mut cache, &first).unwrap();
        verify_with_cache(&mut cache, &second).unwrap();
        let reused = verify_with_cache(&mut cache, &first).unwrap();
        assert!(std::ptr::eq(original.signed(), reused.signed()));
        verify_with_cache(&mut cache, &third).unwrap();
        assert_eq!(cache.len(), 2);
        assert!(cache.contains(&first));
        assert!(!cache.contains(&second));
        assert!(cache.contains(&third));
    }

    #[test]
    fn cached_owner_does_not_authorize_changed_or_malformed_proofs() {
        let identity = ant_protocol::transport::NodeIdentity::generate().unwrap();
        let original = publication(&identity, 1);
        let replacement = publication(&identity, 2);
        let mut cache = LruCache::new(NonZeroUsize::new(2).unwrap());
        let old = verify_with_cache(&mut cache, &original).unwrap();
        let new = verify_with_cache(&mut cache, &replacement).unwrap();
        assert_eq!(new.sequence(), 2);
        assert!(!std::ptr::eq(old.signed(), new.signed()));
        // Retain the valid owner, fields and encoding but alter the signature.
        let mut tampered = hex::decode(&replacement).unwrap();
        *tampered.last_mut().unwrap() ^= 1;
        let tampered = hex::encode(tampered);
        for bad in [
            tampered,
            "zz".into(),
            "0".repeat(MAX_SIGNED_ADDRESS_BYTES * 2 + 1),
        ] {
            assert!(verify_with_cache(&mut cache, &bad).is_err());
            assert!(!cache.contains(&bad));
            assert_eq!(cache.len(), 2);
        }
        // Caching an older valid signature must not bypass replacement policy.
        let old_again = verify_with_cache(&mut cache, &original).unwrap();
        assert!(!client_routing::may_replace_owner_view(
            &new.peer_record(1.0),
            &old_again.peer_record(1.0),
        ));
    }

    #[test]
    fn native_only_records_keep_their_vote_and_address_metadata() {
        let record = DHTNode {
            peer_id: PeerId::from_bytes([1; 32]),
            addresses: vec!["/ip4/198.51.100.1/udp/9000/quic".parse().unwrap()],
            address_types: vec![AddressType::Relay],
            distance: Some(vec![4, 5, 6]),
            reliability: 0.3,
            address_authority: None,
        };
        let wire = browser_record(record.clone()).unwrap();
        assert!(wire.webrtc_direct.is_none());
        let restored = peer_record(&wire).unwrap();
        assert_eq!(restored.address_types, record.address_types);
        assert_eq!(restored.distance, record.distance);
        let native = client_routing::build_witnessed_close_group(
            &[0; 32],
            1,
            1,
            vec![record.clone()],
            vec![(record.peer_id, vec![record])],
        );
        let browser = client_routing::build_witnessed_close_group(
            &[0; 32],
            1,
            1,
            vec![restored.clone()],
            vec![(restored.peer_id, vec![restored])],
        );
        assert_eq!(
            rmp_serde::to_vec(&native.responder_views[0].closest).unwrap(),
            rmp_serde::to_vec(&browser.responder_views[0].closest).unwrap()
        );
    }

    #[test]
    fn metadata_cannot_change_the_advertised_peer_identity() {
        let original = BrowserNode {
            address_record: None,
            peer_record: None,
            peer_id: hex::encode([1; 32]),
            native_addresses: Vec::new(),
            reliability: 0.5,
            webrtc_direct: None,
        };
        let mut wire = browser_record(peer_record(&original).unwrap()).unwrap();
        wire.peer_id = hex::encode([2; 32]);
        assert!(peer_record(&wire).is_err());
    }
    #[test]
    fn owner_proof_survives_browser_adaptation_and_rejects_substitution() {
        use ant_protocol::transport::signed_address::{AddressAuthority, SignedAddressRecord};
        use ant_protocol::transport::{KnownReachability, NodeIdentity, TransportAddressRecord};
        let identity = NodeIdentity::generate().unwrap();
        let address: MultiAddr = "/ip4/9.9.9.9/udp/9000/quic".parse().unwrap();
        let signed = SignedAddressRecord::sign(
            &identity,
            10,
            vec![
                TransportAddressRecord::from_multiaddr(&address, KnownReachability::Direct)
                    .unwrap()
                    .unwrap(),
            ],
        )
        .unwrap();
        let original = signed.verify().unwrap().peer_record(1.0);
        let mut wire = browser_record(original.clone()).unwrap();
        wire.native_addresses = vec!["/ip4/1.1.1.1/udp/9000/quic".into()];
        let restored = peer_record(&wire).unwrap();
        assert_eq!(restored.addresses, vec![address]);
        assert!(matches!(
            restored.address_authority,
            Some(AddressAuthority::Signed(_))
        ));
        assert_eq!(
            browser_record(restored).unwrap().address_record,
            wire.address_record
        );
        // A verification hit must derive a fresh view, including local metadata.
        wire.reliability = 0.25;
        assert_eq!(peer_record(&wire).unwrap().reliability, 0.25);
        wire.address_record = None;
        let hint = peer_record(&wire).unwrap();
        assert!(hint.address_authority.is_none());
        assert!(!client_routing::may_replace_owner_view(&original, &hint));
        wire.address_record = Some(hex::encode(signed.encode().unwrap()));
        wire.peer_id = PeerId::from_bytes([9; 32]).to_hex();
        assert!(peer_record(&wire).is_err());
    }
}
