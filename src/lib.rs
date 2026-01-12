#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

// Core modules (always available)
pub mod types;
pub mod aggregation;
pub mod encoding;
pub mod price_providers;

// Re-exports
pub use types::{TradePair, ConsensusConfiguration, ChannelId, MessagesConfiguration};
pub use aggregation::{calculate_median, filter_outliers};
pub use encoding::{CardanoCbor, CborHashable};
pub use price_providers::{PriceProvider, GenericApiProvider};

// Pallet module (only when pallet feature enabled)
#[cfg(feature = "pallet")]
pub mod pallet;

#[cfg(feature = "pallet")]
pub use pallet::*;

/// Scaling factor for price precision
pub const SCALING_FACTOR: u64 = 10_000;
