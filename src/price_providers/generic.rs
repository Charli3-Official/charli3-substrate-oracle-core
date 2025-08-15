use super::PriceProvider;
use crate::aggregation::{calculate_median, filter_outliers, SCALING_FACTOR};
use crate::config::{DataSource, JsonPathElement, NodeConfig};
use serde_json::Value;
use sp_runtime::offchain::http;
use sp_runtime::sp_std::{str, vec::Vec};

pub struct GenericApiProvider;

impl GenericApiProvider {
    #[inline]
    fn load_config() -> Result<NodeConfig, &'static str> {
        match sp_io::offchain::local_storage_get(
            sp_core::offchain::StorageKind::PERSISTENT,
            b"node_config",
        ) {
            Some(bytes) => match str::from_utf8(&bytes) {
                Ok(config_str) => NodeConfig::from_json_str(config_str),
                Err(_) => Err("Invalid UTF-8 in config"),
            },
            None => {
                log::warn!("No node config found in storage, using default configuration");
                Ok(NodeConfig::default())
            }
        }
    }

    #[inline]
    fn start_request(source: &DataSource) -> Option<(&DataSource, http::PendingRequest)> {
        let name = source.name_str().unwrap_or("unknown");
        let mut req = http::Request::get(source.url_str().ok()?);
        for (k, v) in &source.headers {
            if let (Ok(name), Ok(val)) = (str::from_utf8(k), str::from_utf8(v)) {
                req = req.add_header(name, val);
            }
        }
        req.send()
            .map_err(|_| {
                log::error!("Failed to send request to {}", name);
            })
            .ok()
            .map(|p| (source, p))
    }

    #[inline]
    fn finish_request(
        source: &DataSource,
        pending: http::PendingRequest,
    ) -> Result<f64, http::Error> {
        let name = source.name_str().unwrap_or("unknown");
        let resp = pending.wait().map_err(|_| {
            log::error!("Request failed for source {}", name);
            http::Error::Unknown
        })?;

        if resp.code != 200 {
            log::warn!("HTTP error {} from source {}", resp.code, name);
            return Err(http::Error::Unknown);
        }

        str::from_utf8(&resp.body().collect::<Vec<u8>>())
            .ok()
            .and_then(|s| Self::extract_price(s, &source.json_path))
            .ok_or_else(|| {
                log::warn!("Failed to extract price from source {}", name);
                http::Error::Unknown
            })
    }

    fn extract_price(json: &str, path: &[JsonPathElement]) -> Option<f64> {
        let mut curr = &serde_json::from_str::<Value>(json).ok()?;
        for elem in path {
            curr = match elem {
                JsonPathElement::String(_) => curr.get(elem.key_str()?.ok()?)?,
                JsonPathElement::Number(_) => curr.get(elem.index()? as usize)?,
            };
        }
        match curr {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => s.parse().ok(),
            Value::Array(a) if !a.is_empty() => match &a[0] {
                Value::Number(n) => n.as_f64(),
                Value::String(s) => s.parse().ok(),
                _ => None,
            },
            _ => None,
        }
    }

    fn aggregate_prices(mut prices: Vec<f64>) -> Option<f64> {
        if prices.is_empty() {
            return None;
        }
        prices.sort_by(|a, b| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal));
        let scaled: Vec<u32> = prices
            .iter()
            .map(|&p| (p * SCALING_FACTOR as f64) as u32)
            .collect();
        calculate_median(scaled.clone())
            .and_then(|m| filter_outliers(scaled, m, 150, 50))
            .and_then(|(valid, outliers)| {
                log::info!(
                    "Source aggregation: {} valid prices, {} outliers filtered",
                    valid.len(),
                    outliers.len()
                );
                calculate_median(valid)
            })
            .map(|m| m as f64 / SCALING_FACTOR as f64)
    }

    #[inline]
    const fn validate_price(price: f64) -> bool {
        price > 0.0
    }
}

impl PriceProvider for GenericApiProvider {
    fn fetch_price() -> Result<u32, http::Error> {
        let config = Self::load_config().map_err(|e| {
            log::error!("Failed to load config: {}", e);
            http::Error::Unknown
        })?;

        if config.sources.is_empty() {
            log::error!("No sources configured in oracle config");
            return Err(http::Error::Unknown);
        }

        let (prices, errors): (Vec<_>, usize) = config
            .sources
            .iter()
            .filter_map(Self::start_request)
            .map(|(src, pending)| {
                let name = src.name_str().unwrap_or("unknown");
                match Self::finish_request(src, pending) {
                    Ok(price) if Self::validate_price(price) => {
                        log::info!("Successfully fetched price {} from {}", price, name);
                        Ok(price)
                    }
                    Ok(price) => {
                        log::warn!("Invalid price {} from {}", price, name);
                        Err(())
                    }
                    Err(_) => {
                        log::warn!("Failed to fetch from {}", name);
                        Err(())
                    }
                }
            })
            .fold((Vec::new(), 0), |(mut prices, errs), result| match result {
                Ok(p) => {
                    prices.push(p);
                    (prices, errs)
                }
                Err(_) => (prices, errs + 1),
            });

        if prices.is_empty() {
            log::error!(
                "All {} sources failed or returned invalid prices",
                config.sources.len()
            );
            return Err(http::Error::Unknown);
        }

        log::info!(
            "Successfully fetched from {}/{} sources (errors: {})",
            prices.len(),
            config.sources.len(),
            errors
        );

        Self::aggregate_prices(prices)
            .map(|p| (p * SCALING_FACTOR as f64) as u32)
            .ok_or_else(|| {
                log::error!("Failed to aggregate prices using IQR method");
                http::Error::Unknown
            })
    }
}
