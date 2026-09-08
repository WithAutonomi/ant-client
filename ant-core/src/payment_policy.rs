//! Deterministic payment policy shared by native and browser clients.
//!
//! Callers supply prices from their eligible, verified quote set. Transport
//! discovery, signature verification, wallet submission, and proof encoding
//! remain the responsibility of the adapters.

use saorsa_transport::webrtc::CLOSE_GROUP_SIZE;

pub(crate) const SINGLE_NODE_PAYMENT_MULTIPLIER: u64 = 3;

/// Arithmetic supplied by each adapter without narrowing native U256 amounts.
pub(crate) trait PaymentAmount: Copy + Ord {
    const ZERO: Self;
    fn checked_mul_multiplier(self, multiplier: u64) -> Option<Self>;
}

impl PaymentAmount for u128 {
    const ZERO: Self = 0;
    fn checked_mul_multiplier(self, multiplier: u64) -> Option<Self> {
        self.checked_mul(u128::from(multiplier))
    }
}

impl PaymentAmount for ant_protocol::evm::Amount {
    const ZERO: Self = Self::ZERO;
    fn checked_mul_multiplier(self, multiplier: u64) -> Option<Self> {
        self.checked_mul(Self::from(multiplier))
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(crate) enum PaymentPlanError {
    #[error("Single-node payment requires 1..={CLOSE_GROUP_SIZE} quotes, got {0}")]
    InvalidQuoteCount(usize),
    #[error("Price overflow when calculating 3x median")]
    PriceOverflow,
}

struct RankedQuotes {
    quote_indices: Vec<usize>,
    paid_position: usize,
}

fn rank_quotes<P: Ord>(prices: &[P]) -> Option<RankedQuotes> {
    if prices.is_empty() {
        return None;
    }
    let mut quote_indices = (0..prices.len()).collect::<Vec<_>>();
    // Stable sorting preserves input order for tied prices, so witness checks
    // and payment construction agree on the exact issuer, not just its price.
    quote_indices.sort_by_key(|&index| &prices[index]);
    Some(RankedQuotes {
        quote_indices,
        paid_position: prices.len() / 2,
    })
}

/// Original index of the upper-median quote, preserving input order for ties.
#[cfg(any(feature = "native", feature = "browser-wasm", test))]
pub(crate) fn median_quote_index<P: Ord>(prices: &[P]) -> Option<usize> {
    rank_quotes(prices).map(|ranked| ranked.quote_indices[ranked.paid_position])
}

pub(crate) fn enhanced_payment_amount<P: PaymentAmount>(price: P) -> Result<P, PaymentPlanError> {
    price
        .checked_mul_multiplier(SINGLE_NODE_PAYMENT_MULTIPLIER)
        .ok_or(PaymentPlanError::PriceOverflow)
}

/// One entry in the plan, referring to the adapter's original quote array.
#[derive(Debug)]
pub(crate) struct PlannedQuote<P> {
    pub(crate) quote_index: usize,
    pub(crate) amount: P,
}

/// One paid quote and its amount; all other quotes receive zero.
#[derive(Debug)]
pub(crate) struct SingleNodePaymentPlan<P> {
    pub(crate) quotes: Vec<PlannedQuote<P>>,
    paid_position: usize,
}

impl<P: PaymentAmount> SingleNodePaymentPlan<P> {
    pub(crate) fn from_prices(prices: &[P]) -> Result<Self, PaymentPlanError> {
        if !(1..=CLOSE_GROUP_SIZE).contains(&prices.len()) {
            return Err(PaymentPlanError::InvalidQuoteCount(prices.len()));
        }
        let ranked =
            rank_quotes(prices).ok_or(PaymentPlanError::InvalidQuoteCount(prices.len()))?;
        let paid_quote_index = ranked.quote_indices[ranked.paid_position];
        let amount = enhanced_payment_amount(prices[paid_quote_index])?;
        Ok(Self {
            quotes: ranked
                .quote_indices
                .into_iter()
                .map(|quote_index| PlannedQuote {
                    quote_index,
                    amount: if quote_index == paid_quote_index {
                        amount
                    } else {
                        P::ZERO
                    },
                })
                .collect(),
            paid_position: ranked.paid_position,
        })
    }

    pub(crate) fn paid_quote(&self) -> &PlannedQuote<P> {
        &self.quotes[self.paid_position]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_and_payment_vectors() {
        // Shared expectations for selection and amount calculation, including
        // outliers, an upper median for even sets, ties, and the one-quote case.
        for (prices, index, amount) in [
            (vec![100_u128, 1, 1, 1, 1, 1, 1], 4, 3),
            (vec![1, 10, 10, 10, 10, 10, 10], 3, 30),
            (vec![30, 10, 40, 20], 0, 90),
            (vec![7, 7, 7, 7], 2, 21),
            (vec![9], 0, 27),
        ] {
            let plan = SingleNodePaymentPlan::from_prices(&prices).expect("valid plan");
            assert_eq!(median_quote_index(&prices), Some(index));
            assert_eq!(plan.paid_quote().quote_index, index);
            assert_eq!(plan.paid_quote().amount, amount);
            {
                use ant_protocol::evm::Amount;
                let native_prices = prices.iter().copied().map(Amount::from).collect::<Vec<_>>();
                let native =
                    SingleNodePaymentPlan::from_prices(&native_prices).expect("native plan");
                for (native, portable) in native.quotes.iter().zip(&plan.quotes) {
                    assert_eq!(native.quote_index, portable.quote_index);
                    assert_eq!(native.amount, Amount::from(portable.amount));
                }
                assert_eq!(native.paid_quote().quote_index, index);
                assert_eq!(native.paid_quote().amount, Amount::from(amount));
            }
        }
    }

    #[test]
    fn rejects_invalid_counts_and_overflow() {
        for count in [0, CLOSE_GROUP_SIZE + 1] {
            assert_eq!(
                SingleNodePaymentPlan::from_prices(&vec![1_u128; count]).unwrap_err(),
                PaymentPlanError::InvalidQuoteCount(count),
            );
        }
        assert_eq!(median_quote_index::<u128>(&[]), None);
        assert_eq!(
            SingleNodePaymentPlan::from_prices(&[u128::MAX]).unwrap_err(),
            PaymentPlanError::PriceOverflow,
        );
        assert_eq!(
            SingleNodePaymentPlan::from_prices(&[u128::MAX / 3])
                .unwrap()
                .paid_quote()
                .amount,
            u128::MAX,
        );
        // An unpaid outlier must not trigger multiplication overflow.
        assert_eq!(
            SingleNodePaymentPlan::from_prices(&[1, 1, u128::MAX])
                .unwrap()
                .paid_quote()
                .amount,
            3,
        );
    }

    #[test]
    fn preserves_full_native_amount_range_and_protocol_constants() {
        use ant_protocol::evm::Amount;
        assert_eq!(CLOSE_GROUP_SIZE, ant_protocol::CLOSE_GROUP_SIZE);
        let price = Amount::from(u128::MAX) + Amount::from(1);
        assert_eq!(
            SingleNodePaymentPlan::from_prices(&[price])
                .unwrap()
                .paid_quote()
                .amount,
            price * Amount::from(3),
        );
        assert_eq!(
            SingleNodePaymentPlan::from_prices(&[Amount::MAX]).unwrap_err(),
            PaymentPlanError::PriceOverflow,
        );
    }
}
