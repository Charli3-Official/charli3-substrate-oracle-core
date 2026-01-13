extern crate alloc;
use alloc::string::{String, ToString};
use frame_support::pallet_prelude::{BoundedVec, ConstU32};
use parity_scale_codec::{Decode, Encode, MaxEncodedLen};
use scale_info::TypeInfo;
use serde::{Deserialize, Serialize};
use sp_std::collections::btree_map::BTreeMap;
use sp_std::{str, vec, vec::Vec};

const HTTP_REQUEST_TIMEOUT_MILLIS: u64 = 4000;

fn default_http_request_timeout_millis() -> u64 {
    HTTP_REQUEST_TIMEOUT_MILLIS
}

const HTTP_RESPONSE_WAIT_MILLIS: u64 = 3000;

fn default_http_response_wait_millis() -> u64 {
    HTTP_RESPONSE_WAIT_MILLIS
}

pub const DEFAULT_PRICE_CACHE_TTL_MS: u64 = 5 * 60 * 1000; // 5 minutes TTL

fn default_price_cache_ttl_millis() -> u64 {
    DEFAULT_PRICE_CACHE_TTL_MS
}

#[derive(Serialize, Deserialize, Debug, Clone, Encode, Decode, TypeInfo)]
pub struct PriceProviderConfig {
    #[serde(default = "default_sources")]
    pub sources: BTreeMap<TradePair, Vec<DataSource>>,
    #[serde(default = "default_http_request_timeout_millis")]
    pub http_request_timeout_millis: u64,
    #[serde(default = "default_http_response_wait_millis")]
    pub http_response_wait_millis: u64,
    #[serde(default = "default_price_cache_ttl_millis")]
    pub price_cache_ttl_millis: u64,
}

impl Default for PriceProviderConfig {
    fn default() -> Self {
        Self {
            sources: default_sources(),
            http_request_timeout_millis: default_http_request_timeout_millis(),
            http_response_wait_millis: default_http_response_wait_millis(),
            price_cache_ttl_millis: default_price_cache_ttl_millis(),
        }
    }
}

/// Trade Pair measures price of base (from) currency in terms of quote (to) currency.
/// E.g. ADA-USD (BASE-QUOTE) price tells a price of 1 ADA in USD.
#[derive(
    Clone,
    Encode,
    Decode,
    Eq,
    PartialEq,
    Ord,
    PartialOrd,
    Debug,
    MaxEncodedLen,
    TypeInfo,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(try_from = "String", into = "String")]
pub struct TradePair {
    /// Base aka from currency, e.g. ADA
    base_currency: BoundedVec<u8, ConstU32<64>>,
    /// Quote aka to currency, e.g. USD
    quote_currency: BoundedVec<u8, ConstU32<64>>,
}

impl TradePair {
    /// Create a TradePair from a ticker string (e.g., "ADA-USD").
    /// Accepts delimiters: '_', ' ', '/', '-', '.'.
    /// Returns a Result to handle parsing errors gracefully.
    pub fn from_ticker(ticker: &str) -> Self {
        let parts: Vec<&str> = ticker
            .split(|c| c == ' ' || c == '/' || c == '-' || c == '.' || c == '_')
            .collect();

        if parts.len() != 2 {
            panic!("Invalid ticker format: expected exactly two parts");
        }

        let base = parts[0];
        let quote = parts[1];

        // Convert base and quote to BoundedVec<u8, ConstU32<64>>
        let base_currency =
            BoundedVec::try_from(base.as_bytes().to_vec()).expect("Base currency exceeds 64 bytes");
        let quote_currency = BoundedVec::try_from(quote.as_bytes().to_vec())
            .expect("Quote currency exceeds 64 bytes");

        TradePair {
            base_currency,
            quote_currency,
        }
    }

    /// Convert the TradePair to a ticker bytes (e.g., "ADA-USD").
    /// Uses '-' as the delimiter.
    /// Panics if the ticker exceeds 128 bytes or if the data is not valid UTF-8.
    /// Assumes base_currency and quote_currency are valid UTF-8.
    pub fn to_ticker_bytes(&self) -> BoundedVec<u8, sp_core::ConstU32<128>> {
        // Convert BoundedVec to Vec<u8> for base and quote
        let base: Vec<u8> = self.base_currency.clone().into();
        let quote: Vec<u8> = self.quote_currency.clone().into();

        // Create the ticker by concatenating base, delimiter, and quote
        let mut ticker = base;
        ticker.push(b'-'); // Add delimiter
        ticker.extend(quote);

        // Ensure the result fits within the 128-byte bound
        BoundedVec::<u8, ConstU32<128>>::try_from(ticker).expect("Ticker exceeds 128 bytes")
    }

    /// Convert the TradePair to a ticker string (e.g., "ADA-USD").
    /// Uses '-' as the delimiter.
    /// Panics if the ticker exceeds 128 bytes or if the data is not valid UTF-8.
    /// Assumes base_currency and quote_currency are valid UTF-8.
    pub fn to_ticker(&self) -> String {
        let bounded_ticker = self.to_ticker_bytes();

        // Convert to String, assuming valid UTF-8
        // Safety: We assume base_currency and quote_currency are valid UTF-8
        // (enforced by from_ticker or extrinsic validation)
        sp_std::str::from_utf8(&bounded_ticker)
            .expect("Invalid utf-8")
            .to_string()
    }
}

impl From<TradePair> for String {
    fn from(tp: TradePair) -> Self {
        tp.to_ticker()
    }
}

impl TryFrom<String> for TradePair {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        // You can make from_ticker return Result to avoid panic
        Ok(Self::from_ticker(&value))
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Encode, Decode, TypeInfo)]
pub struct DataSource {
    pub name: String,
    pub url: String,
    pub json_path: Vec<JsonPathElement>,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Encode, Decode, TypeInfo)]
#[serde(untagged)]
pub enum JsonPathElement {
    Key(String),
    Index(u32),
}

fn default_sources() -> BTreeMap<TradePair, Vec<DataSource>> {
    BTreeMap::from([(
        TradePair::from_ticker("ADA-USD"),
        vec![DataSource {
            name: String::from("bitget"),
            url: String::from(
                "https://api.bitget.com/api/spot/v1/market/ticker?symbol=ADAUSDC_SPBL",
            ),
            json_path: vec![
                JsonPathElement::Key(String::from("data")),
                JsonPathElement::Key(String::from("close")),
            ],
            headers: Vec::new(),
        }],
    )])
}

// ChannelId === PolicyId on Cardano (PolicyId of Aggregation State NFT beacon)
pub type ChannelId = BoundedVec<u8, ConstU32<64>>;

pub type MessagesConfiguration =
    BoundedVec<(ChannelId, BoundedVec<u16, ConstU32<64>>), ConstU32<16>>;

#[derive(
    Clone, Encode, Decode, Eq, PartialEq, Debug, MaxEncodedLen, TypeInfo,
)]
pub struct ConsensusConfiguration {
    pub min_nodes_for_trusted_aggregation: u32,
    pub feed_age: u16,
    pub outliers_range: u32,
    pub divergency: u32,
    pub trade_pairs: BoundedVec<TradePair, ConstU32<64>>,
}
