//! Merkle payment unit tests.
//!
//! These tests use the free function `should_use_merkle` — no Client or network needed.
//! The real tests are in `src/client/merkle.rs` (inline test module).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use ant_core::data::client::merkle::{should_use_merkle, PaymentMode, DEFAULT_MERKLE_THRESHOLD};

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
