use sp_runtime::offchain::http;

pub mod generic;
pub use generic::GenericApiProvider;

pub trait PriceProvider {
    fn fetch_price() -> Result<u32, http::Error>;
}