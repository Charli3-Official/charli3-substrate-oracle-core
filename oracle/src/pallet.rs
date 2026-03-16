pub use pallet::*;

use frame_support::pallet_prelude::{BoundedVec, ConstU32};
use frame_system::{
    offchain::{SignMessage, Signer, SigningTypes},
    pallet_prelude::BlockNumberFor,
};
use hex::ToHex;
use pallet_timestamp::{self as timestamp};
use parity_scale_codec::{DecodeWithMemTracking, Encode};
use scale_info::prelude::{vec, vec::Vec};
use sp_core::crypto::KeyTypeId;
use sp_runtime::SaturatedConversion;
use sp_std::boxed::Box;
use sp_std::collections::btree_map::BTreeMap;

pub const KEY_TYPE: KeyTypeId = KeyTypeId(*b"orac");

use crate::aggregation::{calculate_median, filter_outliers};
use crate::price_providers::{GenericApiProvider, PriceProvider};
use crate::types::{ChannelId, ConsensusConfiguration, MessagesConfiguration, TradePair};

pub mod crypto {
    use super::KEY_TYPE;
    use sp_core::ed25519::Signature as Ed25519Signature;
    use sp_runtime::{
        MultiSignature, MultiSigner,
        app_crypto::{app_crypto, ed25519},
        traits::Verify,
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
    use alloc::vec::Vec as AllocVec;
    use frame_support::{pallet_prelude::*, traits::BuildGenesisConfig};
    use frame_system::{
        offchain::{AppCrypto, CreateSignedTransaction, SendSignedTransaction, Signer},
        pallet_prelude::*,
    };
    use minicbor::encode::Encoder;
    use parity_scale_codec::{Decode, Encode, MaxEncodedLen};
    use scale_info::{TypeInfo, prelude::fmt};
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

    #[pallet::storage]
    pub type ChannelsToTradePairs<T> = StorageValue<_, MessagesConfiguration>;

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
        Key1 = (ChannelId, u64),
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
        AddedOracleNode {
            which: T::AccountId,
            block: BlockNumberFor<T>,
        },
        RemovedOracleNode {
            which: T::AccountId,
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

            // --- channel_id ---
            encoder.bytes(&self.channel_id).unwrap(); // encode bytestring

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
                    SignatureStorage::<T>::insert(
                        (message.channel_id, message.timestamp),
                        &who,
                        signature_encoded,
                    );
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
    }
}
