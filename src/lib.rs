#![cfg_attr(not(feature = "std"), no_std)]

pub use pallet::*;

use codec::alloc::string::{String, ToString};
use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use frame_support::pallet_prelude::{BoundedVec, ConstU32};
use frame_system::{
    offchain::{SignMessage, Signer, SigningTypes},
    pallet_prelude::BlockNumberFor,
};
use hex::ToHex;
use num_rational::Ratio;
use num_traits::ops::checked::{CheckedAdd, CheckedMul};
use pallet_timestamp::{self as timestamp};
use scale_info::prelude::{vec, vec::Vec};
use sp_core::crypto::KeyTypeId;
use sp_runtime::{traits::CheckedSub, SaturatedConversion};
use sp_std::boxed::Box;
use sp_std::collections::btree_map::BTreeMap;

pub const KEY_TYPE: KeyTypeId = KeyTypeId(*b"orac");

mod price_providers;
use price_providers::{CryptoCompareProvider, PriceProvider};

pub const SCALING_FACTOR: u128 = 1000;
pub const PERCENT: u128 = 100;
pub const IQR_APPLICABILITY_THRESHOLD: usize = 4;

pub mod crypto {
    use super::KEY_TYPE;
    use sp_core::ed25519::Signature as Ed25519Signature;
    use sp_runtime::{
        app_crypto::{app_crypto, ed25519},
        traits::Verify,
        MultiSignature, MultiSigner,
    };
    app_crypto!(ed25519, KEY_TYPE);

    pub struct OracleAuthId;

    impl frame_system::offchain::AppCrypto<MultiSigner, MultiSignature> for OracleAuthId {
        type RuntimeAppPublic = Public;
        type GenericSignature = sp_core::ed25519::Signature;
        type GenericPublic = sp_core::ed25519::Public;
    }

    impl frame_system::offchain::AppCrypto<<Ed25519Signature as Verify>::Signer, Ed25519Signature>
        for OracleAuthId
    {
        type RuntimeAppPublic = Public;
        type GenericSignature = sp_core::ed25519::Signature;
        type GenericPublic = sp_core::ed25519::Public;
    }
}

#[frame_support::pallet]
pub mod pallet {
    use super::*;
    use codec::alloc::vec::Vec as AllocVec;
    use frame_support::{pallet_prelude::*, traits::BuildGenesisConfig};
    use frame_system::{
        offchain::{AppCrypto, CreateSignedTransaction, SendSignedTransaction, Signer},
        pallet_prelude::*,
    };
    use minicbor::encode::Encoder;
    use scale_info::{prelude::fmt, TypeInfo};
    use sp_core::hashing::blake2_256;
    use sp_runtime::{offchain::http, sp_std::str};

    #[pallet::pallet]
    pub struct Pallet<T>(_);

    #[pallet::config]
    pub trait Config:
        frame_system::Config
        + SigningTypes
        + CreateSignedTransaction<Call<Self>>
        + pallet_timestamp::Config
        + fmt::Debug
    {
        /// The overarching event type.
        type RuntimeEvent: From<Event<Self>> + IsType<<Self as frame_system::Config>::RuntimeEvent>;
        /// AuthorityId for offchain signing. Uses the associated `Public`/`Signature` from SigningTypes.
        type AuthorityId: AppCrypto<Self::Public, Self::Signature>;
    }

    pub type Rational = Ratio<u128>;

    /// Oracle configuration
    #[pallet::storage]
    pub type MinNodesForTrustedAggregation<T> = StorageValue<_, u32>;

    #[pallet::storage]
    pub type FeedAge<T> = StorageValue<_, u16>;

    #[pallet::storage]
    pub type OutliersRange<T> = StorageValue<_, u32>;

    #[pallet::storage]
    pub type Divergency<T> = StorageValue<_, u32>;

    #[pallet::storage]
    pub type TradePairs<T> = StorageValue<_, BoundedVec<TradePair, ConstU32<64>>>;

    /// Trade Pair measures price of base (from) currency in terms of quote (to) currency.
    /// E.g. ADA-USD (BASE-QUOTE) price tells a price of 1 ADA in USD.
    #[derive(
        Clone,
        Encode,
        DecodeWithMemTracking,
        Decode,
        Eq,
        PartialEq,
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
            let base_currency = BoundedVec::try_from(base.as_bytes().to_vec())
                .expect("Base currency exceeds 64 bytes");
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
            let bounded_ticker = BoundedVec::<u8, ConstU32<128>>::try_from(ticker)
                .expect("Ticker exceeds 128 bytes");

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

    #[derive(Clone, Encode, Decode, Eq, PartialEq, Debug, MaxEncodedLen, TypeInfo)]
    pub struct OracleConfiguration {
        pub min_nodes_for_trusted_aggregation: u32,
        pub feed_age: u16,
        pub outliers_range: u32,
        pub divergency: u32,
        pub trade_pairs: BoundedVec<TradePair, ConstU32<64>>,
    }

    /// NodesPrices store latest price for each node indexed by trade pair prefix
    /// about hashers https://docs.substrate.io/build/runtime-storage/#common-substrate-hashers
    #[pallet::storage]
    pub type NodesPrices<T: Config> = StorageDoubleMap<
        Hasher1 = Twox64Concat,
        Key1 = TradePair,
        Hasher2 = Identity,
        Key2 = T::AccountId,
        Value = (u32, BlockNumberFor<T>),
        QueryKind = OptionQuery,
    >;

    /// Prices after nodes "consensus"
    /// This is the aggregated Oracle Message
    #[pallet::storage]
    pub type Aggregation<T> = StorageValue<_, OracleMessage>;

    /// Signatures are indexed by oracle message timestamp.
    /// Second key is the signatory pub key, value is the signature bytes.
    #[pallet::storage]
    pub type SignatureStorage<T: Config> = StorageDoubleMap<
        Hasher1 = Twox64Concat,
        Key1 = u64,
        Hasher2 = Identity,
        Key2 = T::AccountId,
        Value = [u8; 64],
        QueryKind = OptionQuery,
    >;

    /// oracle genesis config definition and associated macros
    // see https://docs.substrate.io/reference/how-to-guides/basics/configure-genesis-state/
    #[pallet::genesis_config]
    pub struct GenesisConfig<T: Config> {
        pub min_nodes_for_trusted_aggregation: u32,
        pub feed_age: u16,
        pub outliers_range: u32,
        pub divergency: u32,
        pub trade_pairs: BoundedVec<TradePair, ConstU32<64>>,
        // Ties `T` to `GenesisConfig` because is needed for `impl<T: Config> BuildGenesisConfig ...`
        pub _marker: PhantomData<T>,
    }

    impl<T: Config> Default for GenesisConfig<T> {
        fn default() -> Self {
            Self {
                min_nodes_for_trusted_aggregation: Default::default(),
                feed_age: Default::default(),
                outliers_range: Default::default(),
                divergency: Default::default(),
                trade_pairs: BoundedVec::truncate_from(vec![TradePair::from_ticker("ADA-USD")]),
                _marker: Default::default(),
            }
        }
    }

    #[pallet::genesis_build]
    impl<T: Config> BuildGenesisConfig for GenesisConfig<T> {
        fn build(&self) {
            <MinNodesForTrustedAggregation<T>>::put(&self.min_nodes_for_trusted_aggregation);
            <FeedAge<T>>::put(&self.feed_age);
            <OutliersRange<T>>::put(&self.outliers_range);
            <Divergency<T>>::put(&self.divergency);
            <TradePairs<T>>::put(&self.trade_pairs);
        }
    }

    /// pallet events
    #[pallet::event]
    #[pallet::generate_deposit(pub(super) fn deposit_event)]
    pub enum Event<T: Config> {
        StoredPrices {
            count: u16,
            who: T::AccountId,
            when: BlockNumberFor<T>,
        },
        StoredSignature {
            message: OracleMessage,
            who: T::AccountId,
            when: BlockNumberFor<T>,
            signature: T::Signature,
        },
        Status {
            message: OracleMessage,
            block: BlockNumberFor<T>,
        },
    }

    #[derive(
        Clone, Encode, Decode, DecodeWithMemTracking, Eq, PartialEq, Debug, MaxEncodedLen, TypeInfo,
    )]
    pub struct OracleMessage {
        /// Vec of prices and their respective age (in blocks ago)
        pub prices_and_age: BoundedVec<Option<(u32, u16)>, ConstU32<64>>,
        /// Aggregation timestamp
        pub timestamp: u64,
        /// Vec of byte arrays for ed25519 public keys with reward multiplier
        pub rewards: BoundedVec<([u8; 32], u16), ConstU32<64>>,
    }

    impl OracleMessage {
        pub fn to_cardano_cbor(&self) -> AllocVec<u8> {
            let mut buf = AllocVec::new();
            let mut encoder = Encoder::new(&mut buf);

            // Write tag 121
            encoder.tag(minicbor::data::Tag::new(121)).unwrap();

            // Start indefinite-length array
            encoder.begin_array().unwrap();

            // Add median_price
            // TODO
            // encoder.u32(self.median_price).unwrap();

            // Add timestamp (as u32 if it fits, otherwise as u64)
            if self.timestamp <= u32::MAX as u64 {
                encoder.u32(self.timestamp as u32).unwrap();
            } else {
                encoder.u64(self.timestamp).unwrap();
            }

            // Add rewards as array of byte strings
            encoder.begin_array().unwrap();
            // TODO
            for (reward_account, _) in &self.rewards {
                encoder.bytes(reward_account).unwrap();
            }
            encoder.end().unwrap(); // End rewards array

            // End main array
            encoder.end().unwrap();

            buf
        }

        pub fn cardano_cbor_hash(&self) -> [u8; 32] {
            let cbor_data = self.to_cardano_cbor();
            blake2_256(&cbor_data)
        }
    }

    /// pallet calls
    #[pallet::call]
    impl<T: Config> Pallet<T> {
        #[pallet::call_index(0)]
        #[pallet::weight((0, Pays::No))]
        pub fn store_prices(origin: OriginFor<T>, prices: Vec<(TradePair, u32)>) -> DispatchResult {
            let who = ensure_signed(origin)?;
            let when = <frame_system::Pallet<T>>::block_number();
            prices.iter().for_each(|(tp, price)| {
                NodesPrices::<T>::insert(tp, &who, (price, when));
            });
            Self::deposit_event(Event::StoredPrices {
                count: TryInto::<u16>::try_into(prices.len())
                    .map_err(|_| sp_runtime::DispatchError::Other("CountOverflow"))?,
                who: who.clone(),
                when,
            });
            Ok(())
        }

        #[pallet::call_index(1)]
        #[pallet::weight((0, Pays::No))]
        pub fn store_signature(
            origin: OriginFor<T>,
            message: OracleMessage,
            signature: T::Signature,
        ) -> DispatchResult {
            let who: T::AccountId = ensure_signed(origin)?;
            let when = <frame_system::Pallet<T>>::block_number();

            let mut signature_bytes: AllocVec<u8> = signature.encode();
            signature_bytes.remove(0);
            let signature_encoded: [u8; 64] = signature_bytes
                .try_into()
                .expect("signature buffer should be exactly 64 bytes");
            SignatureStorage::<T>::insert(message.timestamp, &who, signature_encoded);

            Self::deposit_event(Event::StoredSignature {
                message,
                who,
                when,
                signature,
            });

            Ok(())
        }
    }

    /// pallet auxiliary methods
    impl<T: Config> Pallet<T> {
        pub fn fetch_prices(tickers: Vec<String>) -> Result<Vec<u32>, http::Error> {
            CryptoCompareProvider::fetch_prices(tickers)
        }
    }

    /// pallet hooks
    #[pallet::hooks]
    impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
        // Offchain worker that triggers the extrinsic submitting a price to the
        // NodePrices storage
        fn offchain_worker(_n: BlockNumberFor<T>) {
            log::info!("Starting offchain worker to query price");
            let mut acc_list = Signer::<T, T::AuthorityId>::keystore_accounts();
            match acc_list.next() {
                Some(signer_account) if acc_list.next().is_none() => {
                    let signer = Signer::<T, T::AuthorityId>::all_accounts()
                        .with_filter(vec![signer_account.clone().public]);

                    if let Some(trade_pairs) = TradePairs::<T>::get() {
                        if let Ok(prices) = Self::fetch_prices(
                            trade_pairs
                                .iter()
                                .map(|tp| tp.to_ticker().to_uppercase())
                                .collect(),
                        )
                        .map_err(|e| {
                            log::error!(
                                "[{:?}]: failed to fetch price: {:?}",
                                signer_account.id,
                                e
                            );
                        }) {
                            let result = signer.send_single_signed_transaction(
                                &signer_account,
                                Call::store_prices {
                                    prices: trade_pairs.into_iter().zip(prices).collect(),
                                },
                            );
                            if result.is_some_and(|res| res.is_ok()) {
                                log::info!(
                                    "[{:?}]: submit store price transaction success.",
                                    signer_account.id
                                )
                            } else {
                                log::error!(
                                    "[{:?}]: submit store price transaction failure.",
                                    signer_account.id
                                )
                            }
                        }
                        if let Some((message, signature)) = Self::sign_oracle_message(&signer) {
                            let result = signer.send_single_signed_transaction(
                                &signer_account,
                                Call::store_signature { message, signature },
                            );
                            if result.is_some_and(|res| res.is_ok()) {
                                log::info!(
                                    "[{:?}]: submit store signature transaction success.",
                                    signer_account.id
                                )
                            } else {
                                log::error!(
                                    "[{:?}]: submit store signature transaction failure.",
                                    signer_account.id
                                )
                            }
                        }
                    } else {
                        log::error!("Error fetching trade pairs configuration.")
                    }
                }
                Some(_accounts) => log::error!("More than one account. Expected only one"),
                _none => log::error!("No account available for oracle"),
            }
        }

        fn on_finalize(n: BlockNumberFor<T>) {
            log::info!("Aggregating median price for block {:?}", n);
            if let Some(OracleConfiguration {
                min_nodes_for_trusted_aggregation,
                feed_age,
                outliers_range,
                divergency,
                trade_pairs,
            }) = Self::get_oracle_config()
            {
                // Get timestamp in milliseconds
                let timestamp = timestamp::Pallet::<T>::get().saturated_into::<u64>();

                let mut all_rewards: BTreeMap<[u8; 32], u16> = BTreeMap::new();
                let prices_and_age = trade_pairs
                    .into_iter()
                    .enumerate()
                    .map(|(index, trade_pair)| {
                        log::info!("Aggregation for trade pair: {}", &trade_pair.to_ticker());
                        let mut participating_nodes: u32 = 0;
                        let prices = NodesPrices::<T>::iter_prefix(&trade_pair)
                            .by_ref()
                            .filter_map(|(k, (p, a))| {
                                if (n - a) <= feed_age.into() {
                                    participating_nodes += 1;
                                    Some((k, p))
                                } else {
                                    None
                                }
                            })
                            .collect();
                        let (price, age, rewards) = if min_nodes_for_trusted_aggregation <= participating_nodes {
                            log::info!(
                                "{:?} nodes submitted a price. Aggregating median price ...",
                                participating_nodes
                            );
                            Self::aggregate(prices, outliers_range, divergency)
                            .map(|(price, rewards)| (price, 0, rewards))
                            .or_else(|| {
                                log::error!(
                                    "Oracle consensus error: check for underflow/overflow and list size limitations"
                                    );
                                None})
                        } else {
                            log::error!(
                                "Not enough nodes for trusted aggregation. Reusing previous price ..."
                            );
                            None
                        }.or_else(|| {
                            let (price, age) = Self::get_previous_median(index)?;
                            Some((price, age, BoundedVec::new()))
                        })?;

                        rewards.into_iter().for_each(|acc| {
                            if let Some(existing) = all_rewards.get_mut(&acc) {
                                *existing += 1;
                            } else {
                                all_rewards.insert(acc, 1);
                            }
                        });

                        Some((price, age))
                    })
                    .collect();

                let oracle_message = OracleMessage {
                    prices_and_age: BoundedVec::truncate_from(prices_and_age),
                    timestamp,
                    rewards: BoundedVec::truncate_from(all_rewards.into_iter().collect()),
                };
                Aggregation::<T>::put(&oracle_message);
                log::info!(
                    "Aggregate message for block {:?} is {:?}",
                    n,
                    &oracle_message,
                );
                Self::deposit_event(Event::Status {
                    message: oracle_message,
                    block: n,
                })
            } else {
                log::error!("Couldn't fetch Oracle Config");
            }
        }
    }
}

impl<T: Config> Pallet<T> {
    fn aggregate(
        acc_and_prices: Vec<(T::AccountId, u32)>,
        outliers_range: u32,
        divergency: u32,
    ) -> Option<(u32, BoundedVec<[u8; 32], ConstU32<64>>)> {
        let mut acc_and_prices =
            BoundedVec::<(T::AccountId, u32), ConstU32<32>>::truncate_from(acc_and_prices);
        acc_and_prices.sort_by_key(|k| k.1);
        let sorted_acc_and_prices = acc_and_prices.to_vec();
        let (_addresses, sorted_prices): (Vec<T::AccountId>, Vec<u32>) =
            sorted_acc_and_prices.clone().into_iter().unzip();
        let median = Self::calculate_median(sorted_prices.clone());
        let consensus = median.and_then(|midpoint| {
            Self::filter_outliers(sorted_prices, midpoint, outliers_range, divergency)
        });

        let (median, (non_outlier_prices, outlier_prices)) = median.zip(consensus)?;
        let rewards: Vec<T::AccountId> = sorted_acc_and_prices
            .into_iter()
            .filter_map(|(account, price)| {
                if non_outlier_prices.contains(&price) {
                    Some(account)
                } else {
                    None
                }
            })
            .collect();
        let rewards = BoundedVec::truncate_from(
            rewards
                .iter()
                .map(|id| {
                    let account_bytes = id.encode();
                    let account_encoded: [u8; 32] = account_bytes
                        .try_into()
                        .expect("Account buffer should be exactly 32 bytes");
                    account_encoded
                })
                .collect(),
        );

        log::info!(
            "Median price is {:?} with outlier prices: {:?}",
            median,
            outlier_prices,
        );
        Some((median, rewards))
    }

    fn get_previous_median(trade_pair_index: usize) -> Option<(u32, u16)> {
        let message = Aggregation::<T>::get()?;
        let (price, age) = message.prices_and_age[trade_pair_index]?;
        Some((price, age + 1))
    }

    /// It will check for underflow/overflow and list size limitations (>0) and return None in that cases.
    fn calculate_median(sorted_integers: Vec<u32>) -> Option<u32> {
        match sorted_integers.as_slice() {
            [] => None,
            [x] => Some(*x),
            _ => Self::quantile(sorted_integers, Rational::new(1, 2))
                .and_then(|v| v.round().to_integer().try_into().ok()),
        }
    }

    /// Returns weighted (by proximity) average of the two elements closest to the quantile index q * (n - 1)
    /// It will also check for underflow/overflow and list size limitations (>1) and return None in that cases.
    fn quantile(
        // Input list sorted
        sorted_integers: Vec<u32>,
        // Desired quantile (between 0 and 100%)
        q: Rational,
    ) -> Option<Rational> {
        let length: u128 = sorted_integers.len().try_into().unwrap();

        let n_sub_one: Rational = Rational::from_integer(length.checked_sub(1)?);
        let quantile_index: Rational = q.checked_mul(&n_sub_one)?;

        // Integral part of q * (length - 1)
        let j: Rational = quantile_index.floor();
        // Fractional part of q * (length - 1)
        let g: Rational = quantile_index.checked_sub(&j)?;

        let mid_index: usize = j.to_integer().try_into().unwrap();
        // Get the j-th element of the list (0-indexed)
        let mid_left: u32 = *sorted_integers.get(mid_index)?;
        let x_j: u128 = mid_left.into();
        // Get the (j+1)-th element of the list
        let mid_right: u32 = *sorted_integers.get(mid_index + 1)?;
        let x_j_1: u128 = mid_right.into();

        // Linearly interpolate between x_j
        // and x_j_1, using g as the mixing factor.
        let one_g: Rational = Rational::from_integer(1).checked_sub(&g)?;
        let fst: Rational = one_g.checked_mul(&Rational::from_integer(x_j))?;
        let snd: Rational = g.checked_mul(&Rational::from_integer(x_j_1))?;
        fst.checked_add(&snd)
    }

    fn get_oracle_config() -> Option<OracleConfiguration> {
        let min_nodes_for_trusted_aggregation =
            MinNodesForTrustedAggregation::<T>::get().or_else(|| {
                log::error!("Error fetching MinNodesForTrustedAggregation");
                None
            })?;
        let feed_age = FeedAge::<T>::get().or_else(|| {
            log::error!("Error fetching FeedAge");
            None
        })?;
        let outliers_range = OutliersRange::<T>::get().or_else(|| {
            log::error!("Error fetching OutliersRange");
            None
        })?;
        let divergency = Divergency::<T>::get().or_else(|| {
            log::error!("Error fetching Divergency");
            None
        })?;
        let trade_pairs = TradePairs::<T>::get().or_else(|| {
            log::error!("Error fetching TradePairs");
            None
        })?;

        Some(OracleConfiguration {
            min_nodes_for_trusted_aggregation,
            feed_age,
            outliers_range,
            divergency,
            trade_pairs,
        })
    }

    fn filter_outliers(
        prices: Vec<u32>,
        median: u32,
        outliers_range: u32,
        divergency: u32,
    ) -> Option<(Vec<u32>, Vec<u32>)> {
        let length: usize = prices.len();

        if length == 1 {
            return Some((prices, Vec::new()));
        }

        if length < IQR_APPLICABILITY_THRESHOLD {
            return Some(
                prices
                    .into_iter()
                    .partition(|x| Self::within_divergency(*x, median, divergency)),
            );
        }

        let first_quartile: Rational = Self::quantile(prices.clone(), Rational::new(25, PERCENT))?;
        let third_quartile: Rational = Self::quantile(prices.clone(), Rational::new(75, PERCENT))?;
        let interquartile_range: Rational = third_quartile - first_quartile;

        let iqr_fence_multiplier: Rational = Rational::new(outliers_range.into(), PERCENT);
        let fence: Rational = iqr_fence_multiplier.checked_mul(&interquartile_range)?;
        let lower_bound: u32 = (first_quartile - fence)
            .round()
            .to_integer()
            .try_into()
            .unwrap();
        let upper_bound: u32 = (third_quartile + fence)
            .round()
            .to_integer()
            .try_into()
            .unwrap();

        Some(prices.into_iter().partition(|x| {
            if interquartile_range.round().to_integer() == 0 {
                log::warn!("IQR equals zero");
                Self::within_divergency(*x, median, divergency)
            } else {
                lower_bound <= *x && *x <= upper_bound
            }
        }))
    }

    fn within_divergency(x: u32, median: u32, divergency: u32) -> bool {
        let dif: Rational = Rational::from_integer(Self::unsigned_sub(x, median).into());
        let fraction =
            (dif * Rational::from_integer(SCALING_FACTOR)) / Rational::from_integer(median.into());
        fraction <= Rational::from_integer(divergency.into())
    }

    // substraction between two u32 can cause overflow
    fn unsigned_sub(x: u32, y: u32) -> u32 {
        if x <= y {
            y - x
        } else {
            x - y
        }
    }

    fn sign_oracle_message(
        signer: &Signer<T, <T as Config>::AuthorityId, frame_system::offchain::ForAll>,
    ) -> Option<(OracleMessage, T::Signature)> {
        let message = Aggregation::<T>::get()?;

        log::info!("Prepared Message: {:?}", message);
        let cbor_hex: Box<str> = message.to_cardano_cbor().encode_hex();
        log::debug!("Message cbor: {}", cbor_hex);

        let msg_hash_digest = message.cardano_cbor_hash();
        let msg_hash_hex: Box<str> = msg_hash_digest.encode_hex();
        log::debug!("Message hash: {}", msg_hash_hex);

        let signed_message = match signer.sign_message(&msg_hash_digest).pop() {
            Some(signed) => signed,
            _none => {
                log::error!("Couldn't retrieve signature");
                return None;
            }
        };

        log::info!("Account signed: {:?}", signed_message.0.id);
        let hex_signature: Box<str> = signed_message.1.encode().encode_hex();
        log::info!("Signed message: {}", hex_signature);

        Some((message, signed_message.1))
    }
}
