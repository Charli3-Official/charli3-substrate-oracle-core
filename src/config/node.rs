use codec::alloc::string::{String, ToString};
use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use frame_support::pallet_prelude::{BoundedVec, ConstU32};
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

#[derive(Serialize, Deserialize, Debug, Clone, Encode, Decode, TypeInfo, Default)]
pub struct NodeConfig {
    #[serde(default = "default_sources")]
    pub sources: BTreeMap<TradePair, Vec<DataSource>>,
    #[serde(default = "default_http_request_timeout_millis")]
    pub http_request_timeout_millis: u64,
    #[serde(default = "default_http_response_wait_millis")]
    pub http_response_wait_millis: u64,
}

/// Trade Pair measures price of base (from) currency in terms of quote (to) currency.
/// E.g. ADA-USD (BASE-QUOTE) price tells a price of 1 ADA in USD.
#[derive(
    Clone,
    Encode,
    DecodeWithMemTracking,
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

    /// Convert the TradePair to a ticker string (e.g., "ADA-USD").
    /// Uses '-' as the delimiter.
    /// Panics if the ticker exceeds 128 bytes or if the data is not valid UTF-8.
    /// Assumes base_currency and quote_currency are valid UTF-8.
    pub fn to_ticker(&self) -> String {
        // Convert BoundedVec to Vec<u8> for base and quote
        let base: Vec<u8> = self.base_currency.clone().into();
        let quote: Vec<u8> = self.quote_currency.clone().into();

        // Create the ticker by concatenating base, delimiter, and quote
        let mut ticker = base;
        ticker.push(b'-'); // Add delimiter
        ticker.extend(quote);

        // Ensure the result fits within the 128-byte bound
        let bounded_ticker =
            BoundedVec::<u8, ConstU32<128>>::try_from(ticker).expect("Ticker exceeds 128 bytes");

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
    pub headers: Vec<(String, String)>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Encode, Decode, TypeInfo)]
#[serde(untagged)]
pub enum JsonPathElement {
    Key(String),
    Index(u32),
}

impl NodeConfig {
    #[inline]
    pub fn from_json_str(json_str: &str) -> Result<Self, &'static str> {
        serde_json::from_str(json_str).map_err(|_| "Failed to parse JSON")
    }
}

#[inline]
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
