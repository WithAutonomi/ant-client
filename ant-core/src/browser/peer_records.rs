// Copyright 2026 MaidSafe.net limited.
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Lossless browser wire adaptation for the shared client routing policy.

use super::protocol::{BrowserEndpoint, BrowserNode};
use crate::data::error::{Error as DataError, Result as DataResult};
use ant_protocol::transport::{DHTNode, MultiAddr, PeerId};

pub(crate) fn peer_record(node: &BrowserNode) -> DataResult<DHTNode> {
    use ant_protocol::transport::AddressType;
    let peer_id = PeerId::from_hex(&node.peer_id).map_err(|e| DataError::Network(e.to_string()))?;
    if let Some(encoded) = &node.address_record {
        use ant_protocol::transport::signed_address::{
            SignedAddressRecord, MAX_SIGNED_ADDRESS_BYTES,
        };
        if encoded.len() > MAX_SIGNED_ADDRESS_BYTES * 2 {
            return Err(DataError::Protocol(
                "signed address record exceeds limit".into(),
            ));
        }
        let bytes = hex::decode(encoded).map_err(|e| DataError::Protocol(e.to_string()))?;
        let proof = SignedAddressRecord::decode(&bytes)
            .and_then(|record| record.verify())
            .map_err(DataError::Protocol)?;
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
        wire.address_record = None;
        let hint = peer_record(&wire).unwrap();
        assert!(hint.address_authority.is_none());
        assert!(!client_routing::may_replace_owner_view(&original, &hint));
        wire.address_record = Some(hex::encode(signed.encode().unwrap()));
        wire.peer_id = PeerId::from_bytes([9; 32]).to_hex();
        assert!(peer_record(&wire).is_err());
    }
}
