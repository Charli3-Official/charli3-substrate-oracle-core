#![cfg_attr(not(feature = "std"), no_std)]

pub use pallet::*;

use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use frame_support::pallet_prelude::{BoundedVec, ConstU32};
use frame_system::{
    offchain::{SignMessage, Signer, SigningTypes},
    pallet_prelude::BlockNumberFor,
};
use hex::ToHex;
use pallet_timestamp::{self as timestamp};
use scale_info::prelude::{vec, vec::Vec};
use sp_core::crypto::KeyTypeId;
use sp_runtime::SaturatedConversion;
use sp_std::boxed::Box;
use sp_std::collections::btree_map::BTreeMap;

pub const KEY_TYPE: KeyTypeId = KeyTypeId(*b"orac");

mod aggregation;
mod config;
mod price_providers;

use crate::aggregation::{calculate_median, filter_outliers};
use crate::config::TradePair;
use crate::price_providers::{GenericApiProvider, PriceProvider};

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
    use sp_runtime::sp_std::str;

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

    pub type ChannelId = BoundedVec<u8, ConstU32<64>>;

    pub type MessagesConfiguration =
        BoundedVec<(ChannelId, BoundedVec<u16, ConstU32<64>>), ConstU32<16>>;

    #[pallet::storage]
    pub type ChannelsToTradePairs<T> = StorageValue<_, MessagesConfiguration>;

    #[derive(
        Clone, Encode, Decode, Eq, PartialEq, Debug, MaxEncodedLen, DecodeWithMemTracking, TypeInfo,
    )]
    pub struct ConsensusConfiguration {
        pub min_nodes_for_trusted_aggregation: u32,
        pub feed_age: u16,
        pub outliers_range: u32,
        pub divergency: u32,
        pub trade_pairs: BoundedVec<TradePair, ConstU32<64>>,
    }

    #[pallet::storage]
    pub type AuthorizedOracleNodes<T: Config> = StorageMap<_, Identity, T::AccountId, ()>;

    /// NodesPrices store latest price for each node indexed by trade pair prefix
    /// about hashers https://docs.substrate.io/build/runtime-storage/#common-substrate-hashers
    #[pallet::storage]
    pub type NodesPrices<T: Config> = StorageDoubleMap<
        Hasher1 = Twox64Concat,
        Key1 = TradePair,
        Hasher2 = Identity,
        Key2 = T::AccountId,
        Value = (u64, BlockNumberFor<T>),
        QueryKind = OptionQuery,
    >;

    /// Prices after nodes "consensus"
    #[pallet::storage]
    pub type Aggregation<T> = StorageValue<_, AggregationState>;

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
        pub authorized_nodes: BoundedVec<T::AccountId, ConstU32<32>>,
        pub feed_age: u16,
        pub outliers_range: u32,
        pub divergency: u32,
        pub trade_pairs: BoundedVec<TradePair, ConstU32<64>>,
        pub channels_to_trade_pairs: MessagesConfiguration,
        // Ties `T` to `GenesisConfig` because is needed for `impl<T: Config> BuildGenesisConfig ...`
        pub _marker: PhantomData<T>,
    }

    impl<T: Config> Default for GenesisConfig<T> {
        fn default() -> Self {
            Self {
                min_nodes_for_trusted_aggregation: Default::default(),
                authorized_nodes: Default::default(),
                feed_age: Default::default(),
                outliers_range: Default::default(),
                divergency: Default::default(),
                trade_pairs: BoundedVec::truncate_from(vec![TradePair::from_ticker("ADA-USD")]),
                channels_to_trade_pairs: BoundedVec::new(),
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
            <ChannelsToTradePairs<T>>::put(&self.channels_to_trade_pairs);
            for oracle_account in &self.authorized_nodes {
                AuthorizedOracleNodes::<T>::insert(oracle_account, ());
            }
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
        StoredSignatures {
            who: T::AccountId,
            when: BlockNumberFor<T>,
            signatures: Vec<(OracleMessage, T::Signature)>,
        },
        Status {
            current_state: AggregationState,
            block: BlockNumberFor<T>,
        },
        UpdatedConfig {
            consensus_config: ConsensusConfiguration,
            channels_to_trade_pairs: MessagesConfiguration,
            block: BlockNumberFor<T>,
        },
    }

    #[pallet::error]
    pub enum Error<T> {
        /// Oracle node is not authorized to submit data
        UnauthorizedNode,
    }

    #[derive(
        Clone, Encode, Decode, DecodeWithMemTracking, Eq, PartialEq, Debug, MaxEncodedLen, TypeInfo,
    )]
    /// This is the aggregated data after oracle consensus
    pub struct AggregationState {
        /// Vec of prices and their respective age (in blocks ago),
        /// Third entry is a vec of byte arrays for rewarded nodes ed25519 public keys
        pub prices_age_and_rewards:
            BoundedVec<Option<(u64, u16, BoundedVec<[u8; 32], ConstU32<64>>)>, ConstU32<64>>,
        /// Aggregation timestamp
        pub timestamp: u64,
    }

    #[derive(
        Clone, Encode, Decode, DecodeWithMemTracking, Eq, PartialEq, Debug, MaxEncodedLen, TypeInfo,
    )]
    /// This is the aggregated Oracle Message
    pub struct OracleMessage {
        /// Channel aka Message Queue ID,
        /// any subscriber can then use this to identify the message he wanted to bridge.
        pub channel_id: ChannelId,
        /// Vec of prices and their respective age (in blocks ago).
        pub prices_and_age: BoundedVec<Option<(u64, u16)>, ConstU32<64>>,
        /// Aggregation timestamp.
        pub timestamp: u64,
        /// Vec of byte arrays for ed25519 public keys with reward multiplier.
        pub rewards: BoundedVec<([u8; 32], u16), ConstU32<64>>,
    }

    impl OracleMessage {
        pub fn to_cardano_cbor(&self) -> AllocVec<u8> {
            let mut buf = AllocVec::new();
            let mut encoder = Encoder::new(&mut buf);

            // Write tag 121 for the outer OracleMessage
            encoder.tag(minicbor::data::Tag::new(121)).unwrap();

            // Start main array
            encoder.begin_array().unwrap();

            // --- prices_and_age ---
            encoder.begin_array().unwrap();
            for maybe_entry in &self.prices_and_age {
                match maybe_entry {
                    Some((price, age)) => {
                        encoder.tag(minicbor::data::Tag::new(121)).unwrap(); // Some
                        encoder.begin_array().unwrap();
                        if *price <= u32::MAX as u64 {
                            encoder.u32(*price as u32).unwrap();
                        } else {
                            encoder.u64(*price).unwrap();
                        }
                        encoder.u16(*age).unwrap();
                        encoder.end().unwrap(); // end tuple
                    }
                    _none => {
                        encoder.tag(minicbor::data::Tag::new(122)).unwrap(); // None
                        encoder.begin_array().unwrap();
                        encoder.end().unwrap(); // empty array
                    }
                }
            }
            encoder.end().unwrap(); // end prices_and_age array

            // --- timestamp ---
            if self.timestamp <= u32::MAX as u64 {
                encoder.u32(self.timestamp as u32).unwrap();
            } else {
                encoder.u64(self.timestamp).unwrap();
            }

            // --- rewards ---
            encoder.begin_array().unwrap();
            for (reward_account, multiplier) in &self.rewards {
                encoder.begin_array().unwrap();
                encoder.bytes(reward_account).unwrap(); // pubkey
                encoder.u16(*multiplier).unwrap(); // reward multiplier
                encoder.end().unwrap(); // end [pubkey, multiplier]
            }
            encoder.end().unwrap(); // end rewards array

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
        pub fn store_prices(origin: OriginFor<T>, prices: Vec<(TradePair, u64)>) -> DispatchResult {
            let who = ensure_signed(origin)?;
            // Only authorized oracle nodes can submit prices
            ensure!(
                AuthorizedOracleNodes::<T>::contains_key(&who),
                Error::<T>::UnauthorizedNode
            );

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
        pub fn store_signatures(
            origin: OriginFor<T>,
            signatures: Vec<(OracleMessage, T::Signature)>,
        ) -> DispatchResult {
            let who: T::AccountId = ensure_signed(origin)?;
            ensure!(
                AuthorizedOracleNodes::<T>::contains_key(&who),
                Error::<T>::UnauthorizedNode
            );

            let when = <frame_system::Pallet<T>>::block_number();

            signatures
                .clone()
                .into_iter()
                .for_each(|(message, signature)| {
                    let mut signature_bytes: AllocVec<u8> = signature.encode();
                    signature_bytes.remove(0);
                    let signature_encoded: [u8; 64] = signature_bytes
                        .try_into()
                        .expect("signature buffer should be exactly 64 bytes");
                    SignatureStorage::<T>::insert(message.timestamp, &who, signature_encoded);
                });

            Self::deposit_event(Event::StoredSignatures {
                who,
                when,
                signatures,
            });

            Ok(())
        }

        #[pallet::call_index(2)]
        #[pallet::weight((0, Pays::No))]
        pub fn sudo_set_config(
            origin: OriginFor<T>,
            consensus_config: ConsensusConfiguration,
            channels_to_trade_pairs: MessagesConfiguration,
        ) -> DispatchResult {
            ensure_root(origin)?;
            let when = <frame_system::Pallet<T>>::block_number();

            <MinNodesForTrustedAggregation<T>>::put(
                &consensus_config.min_nodes_for_trusted_aggregation,
            );
            <FeedAge<T>>::put(&consensus_config.feed_age);
            <OutliersRange<T>>::put(&consensus_config.outliers_range);
            <Divergency<T>>::put(&consensus_config.divergency);
            <TradePairs<T>>::put(&consensus_config.trade_pairs);
            <ChannelsToTradePairs<T>>::put(&channels_to_trade_pairs);

            Self::deposit_event(Event::UpdatedConfig {
                consensus_config,
                channels_to_trade_pairs,
                block: when,
            });

            Ok(())
        }

        // TODO batch multiple register / deregister operations together
        // TODO deposit_event
        #[pallet::call_index(3)]
        #[pallet::weight((0, Pays::No))]
        pub fn sudo_register_oracle_node(
            origin: OriginFor<T>,
            oracle_account: T::AccountId,
        ) -> DispatchResult {
            ensure_root(origin)?;

            // Create the account in storage with zero balance
            frame_system::Pallet::<T>::inc_providers(&oracle_account);

            // Authorize it as an oracle node
            AuthorizedOracleNodes::<T>::insert(&oracle_account, ());

            Ok(())
        }

        #[pallet::call_index(4)]
        #[pallet::weight((0, Pays::No))]
        pub fn sudo_deregister_oracle_node(
            origin: OriginFor<T>,
            oracle_account: T::AccountId,
        ) -> DispatchResult {
            ensure_root(origin)?;

            // Remove from authorized nodes
            AuthorizedOracleNodes::<T>::remove(&oracle_account);

            // Attempt to remove (reap) the account from storage
            // If it fails (e.g., account still has references), log it but continue
            // The account is already deauthorized, so it can't submit oracle data
            if let Err(e) = frame_system::Pallet::<T>::dec_providers(&oracle_account) {
                log::error!(
                    "Could not fully remove account {:?} from storage: {:?}. Account is deauthorized but may still exist in state.",
                    oracle_account,
                    e
                );
            }

            Ok(())
        }
    }

    /// pallet auxiliary methods
    impl<T: Config> Pallet<T> {
        pub fn fetch_prices(tickers: Vec<TradePair>) -> Vec<(TradePair, u64)> {
            GenericApiProvider::fetch_prices(tickers)
        }
    }

    /// pallet hooks
    #[pallet::hooks]
    impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
        // Offchain worker that triggers the extrinsic submitting a price to the
        // NodePrices storage
        fn offchain_worker(_n: BlockNumberFor<T>) {
            log::info!("Starting offchain worker to query price from configured sources");
            let mut acc_list = Signer::<T, T::AuthorityId>::keystore_accounts();
            match acc_list.next() {
                Some(signer_account) if acc_list.next().is_none() => {
                    // TODO
                    // if !AuthorizedOracleNodes::<T>::contains_key(&signer_account) {
                    //     return Err("Oracle node not authorized".into());
                    // }
                    let signer = Signer::<T, T::AuthorityId>::all_accounts()
                        .with_filter(vec![signer_account.clone().public]);

                    if let Some(trade_pairs) = TradePairs::<T>::get() {
                        // Store prices tx
                        let prices = Self::fetch_prices(trade_pairs.into());
                        if !prices.is_empty() {
                            let result = signer.send_single_signed_transaction(
                                &signer_account,
                                Call::store_prices { prices },
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
                        } else {
                            log::error!("Failed to fetch prices.");
                        }
                        // Sign messages tx
                        match Self::sign_oracle_messages(&signer) {
                            Some(signatures) if !signatures.is_empty() => {
                                let result = signer.send_single_signed_transaction(
                                    &signer_account,
                                    Call::store_signatures { signatures },
                                );
                                if result.is_some_and(|res| res.is_ok()) {
                                    log::info!(
                                        "[{:?}]: submit store signatures transaction success.",
                                        signer_account.id
                                    )
                                } else {
                                    log::error!(
                                        "[{:?}]: submit store signatures transaction failure.",
                                        signer_account.id
                                    )
                                }
                            }
                            _none => log::error!("Couldn't sign oracle messages."),
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
            if let Some(ConsensusConfiguration {
                min_nodes_for_trusted_aggregation,
                feed_age,
                outliers_range,
                divergency,
                trade_pairs,
            }) = Self::get_oracle_config()
            {
                // Get timestamp in milliseconds
                let timestamp = timestamp::Pallet::<T>::get().saturated_into::<u64>();

                let prices_age_and_rewards = trade_pairs
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
                        if min_nodes_for_trusted_aggregation <= participating_nodes {
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
                        })
                    })
                    .collect();

                let new_state = AggregationState {
                    prices_age_and_rewards: BoundedVec::truncate_from(prices_age_and_rewards),
                    timestamp,
                };
                Aggregation::<T>::put(&new_state);
                log::info!("Aggregate state for block {:?} is {:?}", n, &new_state,);
                Self::deposit_event(Event::Status {
                    current_state: new_state,
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
        acc_and_prices: Vec<(T::AccountId, u64)>,
        outliers_range: u32,
        divergency: u32,
    ) -> Option<(u64, BoundedVec<[u8; 32], ConstU32<64>>)> {
        let mut acc_and_prices =
            BoundedVec::<(T::AccountId, u64), ConstU32<32>>::truncate_from(acc_and_prices);
        acc_and_prices.sort_by_key(|k| k.1);
        let sorted_acc_and_prices = acc_and_prices.to_vec();
        let (_addresses, sorted_prices): (Vec<T::AccountId>, Vec<u64>) =
            sorted_acc_and_prices.clone().into_iter().unzip();
        let median = calculate_median(sorted_prices.clone());
        let consensus = median.and_then(|midpoint| {
            filter_outliers(sorted_prices, midpoint, outliers_range, divergency)
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

    fn get_previous_median(trade_pair_index: usize) -> Option<(u64, u16)> {
        let aggregation_state = Aggregation::<T>::get()?;
        let (price, age, _) = aggregation_state.prices_age_and_rewards[trade_pair_index].clone()?;
        Some((price, age + 1))
    }

    fn get_oracle_config() -> Option<ConsensusConfiguration> {
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

        Some(ConsensusConfiguration {
            min_nodes_for_trusted_aggregation,
            feed_age,
            outliers_range,
            divergency,
            trade_pairs,
        })
    }

    fn sign_oracle_messages(
        signer: &Signer<T, <T as Config>::AuthorityId, frame_system::offchain::ForAll>,
    ) -> Option<Vec<(OracleMessage, T::Signature)>> {
        let all_trade_pairs = TradePairs::<T>::get().or_else(|| {
            log::error!("Error fetching TradePairs");
            None
        })?;
        let channels_to_trade_pairs = ChannelsToTradePairs::<T>::get().or_else(|| {
            log::error!("Error fetching ChannelsToTradePairs");
            None
        })?;
        let trade_pairs_dict: BTreeMap<usize, TradePair> =
            BTreeMap::from_iter(all_trade_pairs.clone().into_iter().enumerate());
        let channels_to_trade_pairs: Vec<(ChannelId, Vec<TradePair>)> = channels_to_trade_pairs
            .into_iter()
            .map(|(chan, pairs_index)| {
                let trade_pairs = pairs_index
                    .into_iter()
                    .filter_map(|i| Some(trade_pairs_dict.get(&(i as usize))?.clone()))
                    .collect();
                (chan, trade_pairs)
            })
            .collect();

        let current_state = Aggregation::<T>::get().or_else(|| {
            log::error!("Error fetching current state");
            None
        })?;
        log::debug!("Current state: {:?}", &current_state);

        Some(
            channels_to_trade_pairs
                .into_iter()
                .filter_map(|(chan, chan_trade_pairs)| {
                    log::debug!("Signing for channel: {:?}", &chan);
                    let message = Self::convert_aggregation_state_to_oracle_message(
                        &current_state,
                        &all_trade_pairs,
                        chan,
                        chan_trade_pairs,
                    );

                    log::debug!("Prepared Message: {:?}", message);
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

                    log::debug!("Account signed: {:?}", signed_message.0.id);
                    let hex_signature: Box<str> = signed_message.1.encode().encode_hex();
                    log::debug!("Signed message: {}", hex_signature);

                    Some((message, signed_message.1))
                })
                .collect(),
        )
    }

    fn convert_aggregation_state_to_oracle_message(
        aggregation_state: &AggregationState,
        all_trade_pairs: &Vec<TradePair>,
        channel_id: ChannelId,
        this_trade_pairs: Vec<TradePair>,
    ) -> OracleMessage {
        let state_mapping: BTreeMap<
            &TradePair,
            Option<(u64, u16, BoundedVec<[u8; 32], ConstU32<64>>)>,
        > = BTreeMap::from_iter(
            all_trade_pairs
                .into_iter()
                .zip(aggregation_state.prices_age_and_rewards.clone()),
        );

        let mut this_rewards: BTreeMap<[u8; 32], u16> = BTreeMap::new();
        let prices_and_age = this_trade_pairs
            .into_iter()
            .map(|trade_pair| {
                let (price, age, rewards) = state_mapping.get(&trade_pair)?.clone()?;
                rewards.into_iter().for_each(|acc| {
                    this_rewards
                        .entry(acc)
                        .and_modify(|count| *count += 1)
                        .or_insert(1);
                });

                Some((price, age))
            })
            .collect();

        OracleMessage {
            channel_id,
            prices_and_age: BoundedVec::truncate_from(prices_and_age),
            timestamp: aggregation_state.timestamp,
            rewards: BoundedVec::truncate_from(this_rewards.into_iter().collect()),
        }
    }
}
