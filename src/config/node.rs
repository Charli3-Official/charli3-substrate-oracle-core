use codec::{Decode, Encode};
use scale_info::{prelude::string::String, TypeInfo};
use serde::{Deserialize, Serialize};
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
    pub sources: Vec<DataSource>,
    #[serde(default = "default_http_request_timeout_millis")]
    pub http_request_timeout_millis: u64,
    #[serde(default = "default_http_response_wait_millis")]
    pub http_response_wait_millis: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, Encode, Decode, TypeInfo)]
pub struct DataSource {
    #[serde(deserialize_with = "str_to_bytes")]
    pub name: Vec<u8>,
    #[serde(deserialize_with = "str_to_bytes")]
    pub url: Vec<u8>,
    pub json_path: Vec<JsonPathElement>,
    #[serde(default, deserialize_with = "headers_to_bytes")]
    pub headers: Vec<(Vec<u8>, Vec<u8>)>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Encode, Decode, TypeInfo)]
#[serde(untagged)]
pub enum JsonPathElement {
    #[serde(deserialize_with = "str_to_bytes")]
    String(Vec<u8>),
    Number(u32),
}

impl NodeConfig {
    #[inline]
    pub fn from_json_str(json_str: &str) -> Result<Self, &'static str> {
        serde_json::from_str(json_str).map_err(|_| "Failed to parse JSON")
    }
}

impl DataSource {
    #[inline]
    pub fn name_str(&self) -> Result<&str, &'static str> {
        str::from_utf8(&self.name).map_err(|_| "Invalid UTF-8 in name")
    }

    #[inline]
    pub fn url_str(&self) -> Result<&str, &'static str> {
        str::from_utf8(&self.url).map_err(|_| "Invalid UTF-8 in URL")
    }
}

impl JsonPathElement {
    #[inline]
    pub const fn index(&self) -> Option<u32> {
        match self {
            Self::Number(n) => Some(*n),
            Self::String(_) => None,
        }
    }

    #[inline]
    pub fn key_str(&self) -> Option<Result<&str, &'static str>> {
        match self {
            Self::String(key) => Some(str::from_utf8(key).map_err(|_| "Invalid UTF-8 in key")),
            Self::Number(_) => None,
        }
    }
}

#[inline]
fn default_sources() -> Vec<DataSource> {
    vec![DataSource {
        name: b"bitget".to_vec(),
        url: b"https://api.bitget.com/api/spot/v1/market/ticker?symbol=ADAUSDC_SPBL".to_vec(),
        json_path: vec![
            JsonPathElement::String(b"data".to_vec()),
            JsonPathElement::String(b"close".to_vec()),
        ],
        headers: Vec::new(),
    }]
}

#[inline]
fn str_to_bytes<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(<&str>::deserialize(deserializer)?.as_bytes().to_vec())
}

#[inline]
fn headers_to_bytes<'de, D>(deserializer: D) -> Result<Vec<(Vec<u8>, Vec<u8>)>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Vec::<(String, String)>::deserialize(deserializer)?
        .into_iter()
        .map(|(k, v)| (k.into_bytes(), v.into_bytes()))
        .collect())
}
