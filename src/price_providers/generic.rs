use super::PriceProvider;
use crate::aggregation::statistics::Rational;
use crate::aggregation::{calculate_median, SCALING_FACTOR};
use crate::types::config::{
    DataSource, JsonPathElement, PriceProviderConfig, DEFAULT_PRICE_CACHE_TTL_MS,
};
use crate::types::TradePair;
use parity_scale_codec::Decode;
use sp_io::offchain;
use sp_runtime::offchain::http;
use sp_runtime::offchain::Duration;
use sp_runtime::sp_std::{vec, vec::Vec};
use sp_std::collections::btree_map::BTreeMap;

pub struct GenericApiProvider;

impl GenericApiProvider {
    #[inline]
    fn load_config() -> Option<PriceProviderConfig> {
        match sp_io::offchain::local_storage_get(
            sp_core::offchain::StorageKind::PERSISTENT,
            b"node_config",
        ) {
            Some(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| log::error!("Failed to load config json: {}", e))
                .ok(),
            _none => {
                log::warn!("No node config found in storage, using default configuration");
                Some(PriceProviderConfig::default())
            }
        }
    }

    /// Parse response body with JSON path into a price.
    fn extract_price(body: &[u8], path: &[JsonPathElement]) -> Option<f64> {
        let json: serde_json::Value = serde_json::from_slice(body).ok()?;

        let mut cursor = &json;
        for p in path {
            match p {
                JsonPathElement::Key(k) => {
                    cursor = cursor.get(k)?;
                }
                JsonPathElement::Index(i) => {
                    cursor = cursor.get(*i as usize)?;
                }
            }
        }

        match cursor.as_str() {
            Some(s) => s.parse::<f64>().ok(),
            _none => cursor.as_f64(),
        }
    }

    fn aggregate_prices(mut prices: Vec<f64>) -> Option<u64> {
        if prices.is_empty() {
            return None;
        }
        prices.sort_by(|a, b| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal));
        let scaled: Vec<u64> = prices
            .into_iter()
            .filter_map(|price| match price_to_u64(price) {
                Ok(converted) => Some(converted),
                Err(e) => {
                    log::error!("Failed to convert price: {}", e);
                    None
                }
            })
            .collect();
        calculate_median(scaled)
    }

    #[inline]
    fn validate_price(price: f64) -> bool {
        price > 0.0
    }

    fn fetch_cached_prices(
        trade_pairs: Vec<TradePair>,
        optional_config: Option<PriceProviderConfig>,
    ) -> Vec<(TradePair, f64)> {
        let price_cache_ttl_ms = optional_config.map_or(DEFAULT_PRICE_CACHE_TTL_MS, |conf| {
            conf.price_cache_ttl_millis
        });
        let mut prices_from_cache = Vec::new();
        for ticker in trade_pairs {
            let now = sp_io::offchain::timestamp().unix_millis();
            match sp_io::offchain::local_storage_get(
                sp_core::offchain::StorageKind::PERSISTENT,
                &ticker.to_ticker_bytes()[..],
            ) {
                Some(entry_bytes) => match <(f64, u64)>::decode(&mut &entry_bytes[..]) {
                    Ok((price, timestamp)) => {
                        if now - timestamp < price_cache_ttl_ms {
                            log::info!(
                                    "Decoded valid cached price for {} from offchain storage: {} (age: {}ms)",
                                    ticker.to_ticker(),
                                    price,
                                    now - timestamp
                                );
                            prices_from_cache.push((ticker, price));
                        } else {
                            log::warn!(
                                "Cached price for {} expired (age: {}ms)",
                                ticker.to_ticker(),
                                now - timestamp
                            );
                        }
                    }
                    Err(e) => {
                        log::error!("Failed to decode price for {}: {:?}", ticker.to_ticker(), e);
                    }
                },
                _none => {
                    log::warn!(
                        "Couldn't fetch price from offchain storage for {}",
                        ticker.to_ticker()
                    );
                }
            }
        }
        prices_from_cache
    }
}

impl PriceProvider for GenericApiProvider {
    fn fetch_prices(trade_pairs: Vec<TradePair>) -> Vec<(TradePair, u64)> {
        // Load Node Api Provider Config
        let optional_config = Self::load_config();

        // Load external prices from cache
        let external_prices =
            Self::fetch_cached_prices(trade_pairs.clone(), optional_config.clone());

        // Load Node Api Provider Config
        let config = match optional_config {
            Some(cfg) => cfg,
            _none => {
                log::warn!("Node Api Provider Config not found — using only external prices");
                return external_prices
                    .into_iter()
                    .filter_map(|(pair, price)| match price_to_u64(price) {
                        Ok(converted) => Some((pair, converted)),
                        Err(e) => {
                            log::error!(
                                "Failed to convert external price for pair {:?}: {}",
                                pair,
                                e
                            );
                            None
                        }
                    })
                    .collect();
            }
        };
        if config.sources.is_empty() {
            log::warn!("No sources configured in oracle config");
        }

        // Start all requests
        let mut requests_sources: Vec<(TradePair, DataSource)> = Vec::new();
        let mut pending_requests: Vec<http::PendingRequest> = Vec::new();
        for pair in trade_pairs.iter() {
            if let Some(sources) = config.sources.get(pair) {
                for source in sources {
                    let deadline = sp_io::offchain::timestamp()
                        .add(Duration::from_millis(config.http_request_timeout_millis));
                    let mut req = http::Request::get(&source.url).deadline(deadline);
                    for (name, val) in &source.headers {
                        req = req.add_header(name, val);
                    }

                    if let Ok(request) = req.send() {
                        requests_sources.push((pair.clone(), source.clone()));
                        pending_requests.push(request);
                    }
                }
            }
        }

        // Collect all responses
        let deadline =
            offchain::timestamp().add(Duration::from_millis(config.http_response_wait_millis));
        let finished: Vec<Result<Result<http::Response, http::Error>, http::PendingRequest>> =
            http::PendingRequest::try_wait_all(pending_requests, deadline);
        let results: Vec<(
            (TradePair, DataSource),
            Result<Result<http::Response, http::Error>, http::PendingRequest>,
        )> = requests_sources.into_iter().zip(finished).collect();
        let mut prices: BTreeMap<TradePair, Vec<f64>> = BTreeMap::new();
        for ((pair, source), result) in results {
            match result {
                // deadline reached
                Err(_still_pending) => {
                    log::error!("Deadline reached for pair {:?} source {:?}", &pair, &source)
                }
                // request completed but errored
                Ok(Err(_)) => {
                    log::error!("Request failed for pair {:?} source {:?}", &pair, &source)
                }
                Ok(Ok(response)) if response.code != 200 => {
                    log::error!("Request failed for pair {:?} source {:?}", &pair, &source)
                }
                // request completed successfully
                Ok(Ok(response)) => {
                    if let Some(price) = Self::extract_price(
                        &response.body().collect::<Vec<u8>>(),
                        &source.json_path,
                    )
                    .or_else(|| {
                        log::error!(
                            "Failed to extract price for pair {:?} source {:?}",
                            &pair,
                            &source
                        );
                        None
                    })
                    .and_then(|price| {
                        if Self::validate_price(price) {
                            log::debug!(
                                "Successfully fetched price {} for pair {:?} source {:?}",
                                price,
                                &pair,
                                &source
                            );
                            Some(price)
                        } else {
                            log::error!(
                                "Invalid price {} for pair {:?} source {:?}",
                                price,
                                &pair,
                                &source
                            );
                            None
                        }
                    }) {
                        prices
                            .entry(pair)
                            .and_modify(|xs: &mut Vec<f64>| xs.push(price))
                            .or_insert(vec![price]);
                    }
                }
            }
        }

        // Add external prices
        for (pair, price) in external_prices {
            prices
                .entry(pair)
                .and_modify(|xs: &mut Vec<f64>| xs.push(price))
                .or_insert(vec![price]);
        }

        // Aggregate prices
        let mut aggregated = Vec::new();
        prices
            .into_iter()
            .for_each(|(pair, ps)| match Self::aggregate_prices(ps) {
                Some(median) => {
                    log::debug!("Succeeded aggregation for trade pair {:?}", pair);
                    aggregated.push((pair, median))
                }
                _none => log::error!("Failed to aggregate prices for trade pair {:?}", pair),
            });

        aggregated
    }
}

pub fn price_to_u64(price: f64) -> Result<u64, &'static str> {
    let rational =
        Rational::approximate_float_unsigned(price).ok_or("Failed to convert f64 to rational")?;

    let scaled = (rational * SCALING_FACTOR).round().to_integer();

    u64::try_from(scaled).map_err(|_| "Overflow: too large for u64")
}
