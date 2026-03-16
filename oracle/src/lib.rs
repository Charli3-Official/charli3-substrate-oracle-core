#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

// Core modules (always available)
pub mod aggregation;
pub mod encoding;
pub mod price_providers;
pub mod types;

// Re-exports
pub use aggregation::{calculate_median, filter_outliers};
pub use encoding::{CardanoCbor, CborHashable};
pub use price_providers::{GenericApiProvider, PriceProvider};
pub use types::{ChannelId, ConsensusConfiguration, MessagesConfiguration, TradePair};

// Pallet module (only when pallet feature enabled)
#[cfg(feature = "pallet")]
pub mod pallet;

#[cfg(feature = "pallet")]
pub use pallet::*;
