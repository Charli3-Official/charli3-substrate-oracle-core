use super::PriceProvider;
use crate::aggregation::{calculate_median, filter_outliers, SCALING_FACTOR};
use crate::config::{DataSource, JsonPathElement, NodeConfig, TradePair};
use sp_io::offchain;
use sp_runtime::offchain::http;
use sp_runtime::offchain::Duration;
use sp_runtime::sp_std::{vec, vec::Vec};
use sp_std::collections::btree_map::BTreeMap;

pub struct GenericApiProvider;

impl GenericApiProvider {
    #[inline]
    fn load_config() -> Option<NodeConfig> {
        match sp_io::offchain::local_storage_get(
            sp_core::offchain::StorageKind::PERSISTENT,
            b"node_config",
        ) {
            Some(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| log::error!("Failed to load config json: {}", e))
                .ok(),
            _none => {
                log::warn!("No node config found in storage, using default configuration");
                Some(NodeConfig::default())
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
    fn fetch_prices(trade_pairs: Vec<TradePair>) -> Option<Vec<(TradePair, u32)>> {
        let config = Self::load_config()?;

        if config.sources.is_empty() {
            log::error!("No sources configured in oracle config");
            return None;
        }

        // 1. Start all requests
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

        // 2. Collect all responses
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
                            log::info!(
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
                            .and_modify(|xs| xs.push(price))
                            .or_insert(vec![price]);
                    }
                }
            }
        }

        // 3. Aggregate prices
        let mut aggregated = Vec::new();
        prices
            .into_iter()
            .for_each(|(pair, ps)| match Self::aggregate_prices(ps) {
                Some(median) => aggregated.push((pair, (median * SCALING_FACTOR as f64) as u32)),
                _none => log::error!("Failed to aggregate prices for trade pair {:?}", pair),
            });

        Some(aggregated)
    }
}
