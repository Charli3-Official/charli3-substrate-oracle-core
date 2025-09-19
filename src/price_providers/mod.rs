pub mod generic;
pub use generic::GenericApiProvider;
use sp_runtime::sp_std::vec::Vec;

use crate::config::TradePair;

pub trait PriceProvider {
    fn fetch_prices(
        trade_pairs: Vec<TradePair>,
        external_prices: Vec<(TradePair, f64)>,
    ) -> Option<Vec<(TradePair, u32)>>;
}
