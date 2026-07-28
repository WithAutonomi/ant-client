//! Merkle payment unit tests.
//!
//! These tests exercise the free functions — `should_use_merkle` and the
//! batch partitioning that payment and cost estimation share — so no Client or
//! network is needed. The rest live in `src/data/client/merkle.rs` (inline test
//! module).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use ant_core::data::client::merkle::{
    merkle_batch_partitions, merkle_batch_sizes, merkle_billable_leaves, should_use_merkle,
    PaymentMode, DEFAULT_MERKLE_THRESHOLD,
};
use ant_protocol::evm::MAX_LEAVES;
use std::collections::HashSet;

#[test]
fn test_threshold_constant() {
    assert_eq!(DEFAULT_MERKLE_THRESHOLD, 64);
}

#[test]
fn test_auto_mode() {
    assert!(!should_use_merkle(63, PaymentMode::Auto));
    assert!(should_use_merkle(64, PaymentMode::Auto));
}

#[test]
fn test_merkle_mode() {
    assert!(!should_use_merkle(1, PaymentMode::Merkle));
    assert!(should_use_merkle(2, PaymentMode::Merkle));
}

#[test]
fn test_single_mode() {
    assert!(!should_use_merkle(1000, PaymentMode::Single));
}

/// Counts covering the minimum tree, the merkle threshold, either side of a
/// full batch, and the `1 mod MAX_LEAVES` counts a naive `chunks(MAX_LEAVES)`
/// split turned into an unpayable one-address tail.
const PARTITION_CASES: [(usize, &[usize]); 10] = [
    (2, &[2]),
    (64, &[64]),
    (65, &[65]),
    (255, &[255]),
    (256, &[256]),
    (257, &[255, 2]),
    (300, &[256, 44]),
    (512, &[256, 256]),
    (513, &[256, 255, 2]),
    (769, &[256, 256, 255, 2]),
];

fn addresses(count: usize) -> Vec<[u8; 32]> {
    (0..count)
        .map(|i| {
            let mut addr = [0u8; 32];
            addr[..8].copy_from_slice(&(i as u64).to_be_bytes());
            addr
        })
        .collect()
}

#[test]
fn batch_sizes_rebalance_singleton_remainders() {
    for (total, expected) in PARTITION_CASES {
        assert_eq!(
            merkle_batch_sizes(total),
            expected,
            "{total} addresses must partition as {expected:?}, never with a one-address batch"
        );
    }
}

#[test]
fn batch_sizes_are_valid_tree_sizes_and_cover_the_input() {
    for total in 2..=(3 * MAX_LEAVES + 5) {
        let sizes = merkle_batch_sizes(total);
        assert_eq!(sizes.iter().sum::<usize>(), total);
        for size in sizes {
            assert!(
                (2..=MAX_LEAVES).contains(&size),
                "{total} addresses produced a batch of {size}, outside 2..={MAX_LEAVES}"
            );
        }
    }
}

#[test]
fn partitions_preserve_order_and_use_each_address_once() {
    for (total, expected) in PARTITION_CASES {
        let addrs = addresses(total);
        let partitions = merkle_batch_partitions(&addrs);

        let sizes: Vec<usize> = partitions.iter().map(|batch| batch.len()).collect();
        assert_eq!(sizes, expected);

        let flattened: Vec<[u8; 32]> = partitions.concat();
        assert_eq!(flattened, addrs, "{total}: order must be preserved");

        let unique: HashSet<[u8; 32]> = flattened.iter().copied().collect();
        assert_eq!(unique.len(), total, "{total}: no address may repeat");
    }
}

#[test]
fn billable_leaves_are_the_padded_partitions() {
    for (total, expected) in PARTITION_CASES {
        let padded: u64 = expected
            .iter()
            .map(|size| size.next_power_of_two() as u64)
            .sum();
        assert_eq!(
            merkle_billable_leaves(total as u64),
            padded,
            "{total} chunks must bill for the partition {expected:?}, padded"
        );
    }
}
