pub mod generic;
pub use generic::GenericApiProvider;
use sp_runtime::sp_std::vec::Vec;

use crate::types::TradePair;

pub trait PriceProvider {
    fn fetch_prices(trade_pairs: Vec<TradePair>) -> Vec<(TradePair, u64)>;
}
