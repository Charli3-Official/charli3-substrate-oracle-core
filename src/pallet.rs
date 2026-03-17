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
use crate::types::{ChannelId, ConsensusConfiguration, MessagesConfiguration, RewardConfiguration, TradePair};
use frame_support::pallet_prelude::DispatchResult;

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

    // ==================== STAKING TYPES ====================

    #[derive(Clone, Encode, Decode, DecodeWithMemTracking, Eq, PartialEq, Debug, MaxEncodedLen, TypeInfo)]
    pub enum OracleNodeStakingState {
        Inactive,
        StakingApproved,
        ActiveStake,
        RetireStake,
        SlashVoting,
        SlashApproved,
        WithdrawApproved,
    }

    #[derive(Clone, Encode, Decode, DecodeWithMemTracking, Eq, PartialEq, Debug, MaxEncodedLen, TypeInfo)]
    pub enum SlashVote {
        Approve,
        Deny,
    }

    #[derive(Clone, Encode, Decode, DecodeWithMemTracking, Eq, PartialEq, Debug, MaxEncodedLen, TypeInfo)]
    pub struct NodeStakingInfo<BlockNumber: MaxEncodedLen + TypeInfo + Encode + Decode + DecodeWithMemTracking + Clone + Eq + PartialEq + core::fmt::Debug> {
        pub stake_amount: u64,
        pub state: OracleNodeStakingState,
        pub stake_activated_at: BlockNumber,
        /// Cardano PKH for admin key — added to OracleSettings.nodes_admin on Cardano.
        /// Identifies the Stake UTxO on Cardano.
        pub cardano_pkh_admin: BoundedVec<u8, ConstU32<32>>,
        /// Cardano PKH for oracle/aggregation key — added to OracleSettings.nodes_aggregation on Cardano.
        pub cardano_pkh_aggregation: BoundedVec<u8, ConstU32<32>>,
    }

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
    pub type RewardPolicyId<T> = StorageValue<_, ChannelId>;

    #[pallet::storage]
    pub type RewardAssetName<T> = StorageValue<_, BoundedVec<u8, ConstU32<64>>>;

    #[pallet::storage]
    pub type TradePairs<T> = StorageValue<_, BoundedVec<TradePair, ConstU32<64>>>;

    #[pallet::storage]
    pub type ChannelsToTradePairs<T> = StorageValue<_, MessagesConfiguration>;

    #[pallet::storage]
    pub type AuthorizedOracleNodes<T: Config> =
        StorageMap<_, Identity, T::AccountId, NodeStakingInfo<BlockNumberFor<T>>>;

    /// Pending staking approval: node → (stake_amount, lock_until, cardano_pkh_admin, cardano_pkh_aggregation).
    /// Created by first admin signer, cleared when threshold is met.
    #[pallet::storage]
    pub type StakingApprovalInfo<T: Config> = StorageMap<
        _,
        Identity,
        T::AccountId,
        (u64, BlockNumberFor<T>, BoundedVec<u8, ConstU32<32>>, BoundedVec<u8, ConstU32<32>>),
        OptionQuery,
    >;

    /// Staking approval signatures: (node, signer) → (admin_ed25519_pubkey, admin_sig).
    /// Admins sign StakingMessage CBOR hash with their admin ed25519 key.
    /// Cleared when threshold is met and certificate is emitted.
    #[pallet::storage]
    pub type StakingApprovalSigs<T: Config> = StorageDoubleMap<
        _,
        Identity,
        T::AccountId,
        Identity,
        T::AccountId,
        ([u8; 32], [u8; 64]),
        OptionQuery,
    >;

    /// Pending withdrawal approval: node → approved_amount.
    #[pallet::storage]
    pub type WithdrawalApprovalInfo<T: Config> = StorageMap<
        _,
        Identity,
        T::AccountId,
        u64,
        OptionQuery,
    >;

    /// Withdrawal approval signatures: (node, signer) → (admin_ed25519_pubkey, admin_sig).
    /// Admins sign WithdrawalMessage CBOR hash with their admin ed25519 key.
    #[pallet::storage]
    pub type WithdrawalApprovalSigs<T: Config> = StorageDoubleMap<
        _,
        Identity,
        T::AccountId,
        Identity,
        T::AccountId,
        ([u8; 32], [u8; 64]),
        OptionQuery,
    >;

    /// Issued staking certificate — written when threshold is met, queryable by bridge-offchain.
    /// node → (stake_amount, lock_until, cardano_pkh_admin, cardano_pkh_aggregation, admin_sigs)
    /// Bridge-offchain uses cardano_pkh_admin + cardano_pkh_aggregation to update OracleSettings.
    /// Analogous to SignatureStorage for oracle messages.
    /// Cleared when node calls confirm_cardano_stake.
    #[pallet::storage]
    pub type IssuedStakingCerts<T: Config> = StorageMap<
        _,
        Identity,
        T::AccountId,
        (u64, BlockNumberFor<T>, BoundedVec<u8, ConstU32<32>>, BoundedVec<u8, ConstU32<32>>, BoundedVec<([u8; 32], [u8; 64]), ConstU32<32>>),
        OptionQuery,
    >;

    /// Issued withdrawal certificate — written when threshold is met, queryable by bridge-offchain.
    /// node → (approved_amount, admin_sigs)
    /// Cleared when node calls confirm_cardano_withdrawal.
    #[pallet::storage]
    pub type IssuedWithdrawalCerts<T: Config> = StorageMap<
        _,
        Identity,
        T::AccountId,
        (u64, BoundedVec<([u8; 32], [u8; 64]), ConstU32<32>>),
        OptionQuery,
    >;

    /// Slash votes: (target_node, voting_node) → SlashVote
    #[pallet::storage]
    pub type SlashVotes<T: Config> = StorageDoubleMap<
        _,
        Identity,
        T::AccountId,  // node being slashed
        Identity,
        T::AccountId,  // node voting
        SlashVote,
        OptionQuery,
    >;

    /// Slash proposals: node_account → (slash_amount, initiated_by, block_number)
    #[pallet::storage]
    pub type SlashProposals<T: Config> = StorageMap<
        _,
        Identity,
        T::AccountId,
        (u64, T::AccountId, BlockNumberFor<T>),
        OptionQuery,
    >;

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
        pub reward_policy_id: Option<ChannelId>,
        pub reward_asset_name: Option<BoundedVec<u8, ConstU32<64>>>,
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
                reward_policy_id: None,
                reward_asset_name: None,
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
            if let Some(reward_policy_id) = &self.reward_policy_id {
                <RewardPolicyId<T>>::put(reward_policy_id);
            }
            if let Some(reward_asset_name) = &self.reward_asset_name {
                <RewardAssetName<T>>::put(reward_asset_name);
            }
            for oracle_account in &self.authorized_nodes {
                // Genesis nodes are pre-authorized and start as ActiveStake.
                // They are the founding/trusted nodes that do not need to go
                // through the staking flow. New nodes joining later must stake.
                AuthorizedOracleNodes::<T>::insert(oracle_account, NodeStakingInfo {
                    stake_amount: 0u64,
                    state: OracleNodeStakingState::ActiveStake,
                    stake_activated_at: BlockNumberFor::<T>::from(0u32),
                    cardano_pkh_admin: BoundedVec::new(),
                    cardano_pkh_aggregation: BoundedVec::new(),
                });
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
            reward_config: Option<RewardConfiguration>,
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
        /// Staking certificate was issued and approved — carries threshold admin signatures inline.
        /// Bridge-offchain reads this single event to build the Cardano place-staking redeemer.
        StakingCertificateIssued {
            node: T::AccountId,
            amount: u64,
            lock_until: BlockNumberFor<T>,
            /// Admin key PKH — bridge-offchain adds to OracleSettings.nodes_admin
            cardano_pkh_admin: BoundedVec<u8, ConstU32<32>>,
            /// Aggregation key PKH — bridge-offchain adds to OracleSettings.nodes_aggregation
            cardano_pkh_aggregation: BoundedVec<u8, ConstU32<32>>,
            /// Admin ed25519 signatures: Vec<(pubkey_32, sig_64)>
            sigs: BoundedVec<([u8; 32], [u8; 64]), ConstU32<32>>,
            when: BlockNumberFor<T>,
        },
        /// Staking confirmed on Cardano
        StakingConfirmed {
            node: T::AccountId,
            tx_hash: [u8; 32],
            stake_amount: u64,
            when: BlockNumberFor<T>,
        },
        /// Node requested retire
        RetireRequested {
            node: T::AccountId,
            when: BlockNumberFor<T>,
        },
        /// Retire certificate issued
        RetireCertificateIssued {
            node: T::AccountId,
            lock_until: BlockNumberFor<T>,
            when: BlockNumberFor<T>,
        },
        /// Slash request initiated
        SlashRequested {
            node: T::AccountId,
            slash_amount: u64,
            initiated_by: T::AccountId,
            when: BlockNumberFor<T>,
        },
        /// Slash vote cast
        SlashVoteCast {
            node: T::AccountId,
            voter: T::AccountId,
            vote: SlashVote,
            when: BlockNumberFor<T>,
        },
        /// Slash approved by voting
        SlashApproved {
            node: T::AccountId,
            slash_amount: u64,
            approve_count: u32,
            deny_count: u32,
            when: BlockNumberFor<T>,
        },
        /// Slash rejected by voting
        SlashRejected {
            node: T::AccountId,
            approve_count: u32,
            deny_count: u32,
            when: BlockNumberFor<T>,
        },
        /// Withdrawal certificate issued — carries threshold admin signatures inline.
        /// Bridge-offchain reads this single event to build the Cardano withdraw redeemer.
        WithdrawalCertificateIssued {
            node: T::AccountId,
            approved_amount: u64,
            /// Admin ed25519 signatures: Vec<(pubkey_32, sig_64)>
            sigs: BoundedVec<([u8; 32], [u8; 64]), ConstU32<32>>,
            when: BlockNumberFor<T>,
        },
        /// An admin signed a staking approval — waiting for more signatures.
        StakingApprovalSigned {
            node: T::AccountId,
            signer: T::AccountId,
            when: BlockNumberFor<T>,
        },
        /// An admin signed a withdrawal approval — waiting for more signatures.
        WithdrawalApprovalSigned {
            node: T::AccountId,
            signer: T::AccountId,
            when: BlockNumberFor<T>,
        },
        /// Withdrawal confirmed on Cardano
        WithdrawalConfirmed {
            node: T::AccountId,
            tx_hash: [u8; 32],
            released_amount: u64,
            penalty_amount: u64,
            when: BlockNumberFor<T>,
        },
    }

    #[pallet::error]
    pub enum Error<T> {
        /// Oracle node is not authorized to submit data
        UnauthorizedNode,
        /// Invalid state for operation
        InvalidNodeState,
        /// Stake amount mismatch
        StakeMismatch,
        /// Slash amount invalid
        SlashAmountInvalid,
        /// Cannot vote for yourself
        CannotVoteForSelf,
        /// No active slash to vote on
        NoActiveSlash,
        /// Approved amount invalid
        ApprovedAmountInvalid,
        /// Amount mismatch on withdrawal
        AmountMismatch,
        /// Signing admin submitted different params than the existing proposal
        ProposalMismatch,
        /// This account has already signed this proposal
        AlreadySigned,
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
        /// Optional reward asset: (policy_id, asset_name).
        pub reward_asset: Option<(ChannelId, BoundedVec<u8, ConstU32<64>>)>,
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
                        encoder.array(0).unwrap(); // definite empty array (0x80), matches Lucid encoding
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

            // --- reward_asset: Option<Asset> ---
            match &self.reward_asset {
                Some((policy_id, asset_name)) => {
                    encoder.tag(minicbor::data::Tag::new(121)).unwrap(); // Some
                    encoder.begin_array().unwrap();
                    encoder.tag(minicbor::data::Tag::new(121)).unwrap(); // Asset constr(0)
                    encoder.begin_array().unwrap();
                    encoder.bytes(policy_id).unwrap();
                    encoder.bytes(asset_name).unwrap();
                    encoder.end().unwrap(); // end Asset array
                    encoder.end().unwrap(); // end Some array
                }
                None => {
                    encoder.tag(minicbor::data::Tag::new(122)).unwrap(); // None
                    encoder.array(0).unwrap(); // definite empty array (0x80), matches Lucid encoding
                }
            }

            // End main array
            encoder.end().unwrap();

            buf
        }

        pub fn cardano_cbor_hash(&self) -> [u8; 32] {
            let cbor_data = self.to_cardano_cbor();
            blake2_256(&cbor_data)
        }
    }

    #[derive(
        Clone, Encode, Decode, DecodeWithMemTracking, Eq, PartialEq, Debug, MaxEncodedLen, TypeInfo,
    )]
    /// Staking certificate message — CBOR-encoded and signed by admin nodes.
    /// Bridge-offchain reads the StakingCertificateIssued event (which carries sigs inline).
    /// Contains both Cardano PKHs so bridge-offchain can update OracleSettings.nodes_admin
    /// and OracleSettings.nodes_aggregation when placing the stake.
    pub struct StakingMessage {
        /// Cardano PKH for admin key — added to OracleSettings.nodes_admin.
        pub cardano_pkh_admin: BoundedVec<u8, ConstU32<32>>,
        /// Cardano PKH for oracle/aggregation key — added to OracleSettings.nodes_aggregation.
        pub cardano_pkh_aggregation: BoundedVec<u8, ConstU32<32>>,
        /// Stake amount in lovelace.
        pub stake_amount: u64,
        /// Partnerchain block until which stake is locked.
        pub lock_until: u32,
    }

    impl StakingMessage {
        /// Encode to Cardano-compatible CBOR.
        /// tag(121) + indefinite_array [ bytes(pkh_admin), bytes(pkh_aggregation), u64(stake_amount), u32(lock_until) ]
        pub fn to_cardano_cbor(&self) -> AllocVec<u8> {
            let mut buf = AllocVec::new();
            let mut encoder = Encoder::new(&mut buf);
            encoder.tag(minicbor::data::Tag::new(121)).unwrap();
            encoder.begin_array().unwrap();
            encoder.bytes(&self.cardano_pkh_admin).unwrap();
            encoder.bytes(&self.cardano_pkh_aggregation).unwrap();
            encoder.u64(self.stake_amount).unwrap();
            encoder.u32(self.lock_until).unwrap();
            encoder.end().unwrap();
            buf
        }

        /// Blake2b-256 hash of the CBOR — this is what each admin signs with their ed25519 key.
        pub fn cardano_cbor_hash(&self) -> [u8; 32] {
            blake2_256(&self.to_cardano_cbor())
        }
    }

    #[derive(
        Clone, Encode, Decode, DecodeWithMemTracking, Eq, PartialEq, Debug, MaxEncodedLen, TypeInfo,
    )]
    /// Withdrawal certificate message — CBOR-encoded and signed by admin nodes.
    /// The approved_amount encodes the penalty: if approved_amount < staked_amount,
    /// Cardano enforces that the difference goes to penalty_addr.
    /// Bridge-offchain reads the WithdrawalCertificateIssued event (which carries sigs inline).
    pub struct WithdrawalMessage {
        /// Cardano admin PKH of the node withdrawing — identifies the Stake UTxO on Cardano.
        pub cardano_pkh_admin: BoundedVec<u8, ConstU32<32>>,
        /// Approved withdrawal amount in lovelace.
        /// If less than staked, Cardano requires penalty output to penalty_addr.
        pub approved_amount: u64,
    }

    impl WithdrawalMessage {
        /// Encode to Cardano-compatible CBOR.
        /// tag(121) + indefinite_array [ bytes(cardano_pkh_admin), u64(approved_amount) ]
        pub fn to_cardano_cbor(&self) -> AllocVec<u8> {
            let mut buf = AllocVec::new();
            let mut encoder = Encoder::new(&mut buf);
            encoder.tag(minicbor::data::Tag::new(121)).unwrap();
            encoder.begin_array().unwrap();
            encoder.bytes(&self.cardano_pkh_admin).unwrap();
            encoder.u64(self.approved_amount).unwrap();
            encoder.end().unwrap();
            buf
        }

        pub fn cardano_cbor_hash(&self) -> [u8; 32] {
            blake2_256(&self.to_cardano_cbor())
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
            reward_config: Option<RewardConfiguration>,
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

            match &reward_config {
                Some(reward) => {
                    <RewardPolicyId<T>>::put(&reward.reward_policy_id);
                    <RewardAssetName<T>>::put(&reward.reward_asset_name);
                }
                None => {
                    <RewardPolicyId<T>>::kill();
                    <RewardAssetName<T>>::kill();
                }
            }

            Self::deposit_event(Event::UpdatedConfig {
                consensus_config,
                channels_to_trade_pairs,
                reward_config,
                block: when,
            });

            Ok(())
        }

        #[pallet::call_index(3)]
        #[pallet::weight((0, Pays::No))]
        pub fn sudo_register_oracle_node(
            origin: OriginFor<T>,
            oracle_account: T::AccountId,
        ) -> DispatchResult {
            ensure_root(origin)?;
            let when = <frame_system::Pallet<T>>::block_number();

            // Create the account in storage with zero balance
            frame_system::Pallet::<T>::inc_providers(&oracle_account);

            // Sudo-registered nodes start as Inactive.
            // They must go through the staking flow (request_stake →
            // generate_staking_certificate → confirm_cardano_stake)
            // before becoming ActiveStake.
            AuthorizedOracleNodes::<T>::insert(&oracle_account, NodeStakingInfo {
                stake_amount: 0u64,
                state: OracleNodeStakingState::Inactive,
                stake_activated_at: when,
                cardano_pkh_admin: BoundedVec::new(),
                cardano_pkh_aggregation: BoundedVec::new(),
            });

            Self::deposit_event(Event::AddedOracleNode {
                which: oracle_account,
                block: when,
            });

            Ok(())
        }

        #[pallet::call_index(4)]
        #[pallet::weight((0, Pays::No))]
        pub fn sudo_deregister_oracle_node(
            origin: OriginFor<T>,
            oracle_account: T::AccountId,
        ) -> DispatchResult {
            ensure_root(origin)?;
            let when = <frame_system::Pallet<T>>::block_number();

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

            Self::deposit_event(Event::RemovedOracleNode {
                which: oracle_account,
                block: when,
            });

            Ok(())
        }

        // ==================== STAKING EXTRINSICS ====================

        #[pallet::call_index(6)]
        #[pallet::weight((0, Pays::No))]
        pub fn generate_staking_certificate(
            origin: OriginFor<T>,
            node_account: T::AccountId,
            stake_amount: u64,
            lock_until_block: BlockNumberFor<T>,
            cardano_pkh_admin: BoundedVec<u8, ConstU32<32>>,
            cardano_pkh_aggregation: BoundedVec<u8, ConstU32<32>>,
            admin_pubkey: [u8; 32],
            admin_sig: [u8; 64],
        ) -> DispatchResult {
            let who = ensure_signed(origin)?;
            let when = <frame_system::Pallet<T>>::block_number();

            // Cannot approve your own staking
            ensure!(who != node_account, Error::<T>::CannotVoteForSelf);

            // Node must exist and be in Inactive state
            let mut info = AuthorizedOracleNodes::<T>::get(&node_account)
                .ok_or(Error::<T>::UnauthorizedNode)?;
            ensure!(
                info.state == OracleNodeStakingState::Inactive,
                Error::<T>::InvalidNodeState
            );

            // If a proposal already exists, verify params match
            if let Some((existing_amount, existing_lock, existing_pkh_admin, existing_pkh_agg)) =
                StakingApprovalInfo::<T>::get(&node_account)
            {
                ensure!(
                    existing_amount == stake_amount
                        && existing_lock == lock_until_block
                        && existing_pkh_admin == cardano_pkh_admin
                        && existing_pkh_agg == cardano_pkh_aggregation,
                    Error::<T>::ProposalMismatch
                );
            } else {
                // First signer — create the proposal
                StakingApprovalInfo::<T>::insert(
                    &node_account,
                    (stake_amount, lock_until_block, cardano_pkh_admin.clone(), cardano_pkh_aggregation.clone()),
                );
            }

            // Ensure this admin hasn't already signed
            ensure!(
                !StakingApprovalSigs::<T>::contains_key(&node_account, &who),
                Error::<T>::AlreadySigned
            );

            // Store this admin's signature
            StakingApprovalSigs::<T>::insert(&node_account, &who, (admin_pubkey, admin_sig));

            // Count collected signatures
            let sig_count = StakingApprovalSigs::<T>::iter_prefix(&node_account).count() as u32;
            let threshold = MinNodesForTrustedAggregation::<T>::get().unwrap_or(2);

            if sig_count >= threshold {
                // Collect all sigs into BoundedVec
                let mut collected_sigs: BoundedVec<([u8; 32], [u8; 64]), ConstU32<32>> =
                    BoundedVec::new();
                for (_signer, sig_pair) in StakingApprovalSigs::<T>::iter_prefix(&node_account) {
                    let _ = collected_sigs.try_push(sig_pair);
                }

                // Update node state
                info.stake_amount = stake_amount;
                info.state = OracleNodeStakingState::StakingApproved;
                info.cardano_pkh_admin = cardano_pkh_admin.clone();
                info.cardano_pkh_aggregation = cardano_pkh_aggregation.clone();
                AuthorizedOracleNodes::<T>::insert(&node_account, info);

                // Clean up approval storage
                StakingApprovalInfo::<T>::remove(&node_account);
                let _ = StakingApprovalSigs::<T>::clear_prefix(&node_account, u32::MAX, None);

                // Persist issued certificate so bridge-offchain can query it
                IssuedStakingCerts::<T>::insert(
                    &node_account,
                    (stake_amount, lock_until_block, cardano_pkh_admin.clone(), cardano_pkh_aggregation.clone(), collected_sigs.clone()),
                );

                Self::deposit_event(Event::StakingCertificateIssued {
                    node: node_account,
                    amount: stake_amount,
                    lock_until: lock_until_block,
                    cardano_pkh_admin,
                    cardano_pkh_aggregation,
                    sigs: collected_sigs,
                    when,
                });
            } else {
                Self::deposit_event(Event::StakingApprovalSigned {
                    node: node_account,
                    signer: who,
                    when,
                });
            }

            Ok(())
        }

        #[pallet::call_index(7)]
        #[pallet::weight((0, Pays::No))]
        pub fn confirm_cardano_stake(
            origin: OriginFor<T>,
            tx_hash: [u8; 32],
        ) -> DispatchResult {
            let who = ensure_signed(origin)?;
            let when = <frame_system::Pallet<T>>::block_number();

            let mut info = AuthorizedOracleNodes::<T>::get(&who)
                .ok_or(Error::<T>::UnauthorizedNode)?;
            ensure!(
                info.state == OracleNodeStakingState::StakingApproved,
                Error::<T>::InvalidNodeState
            );

            info.state = OracleNodeStakingState::ActiveStake;
            AuthorizedOracleNodes::<T>::insert(&who, info.clone());

            // Certificate has been used — clear it from queryable storage
            IssuedStakingCerts::<T>::remove(&who);

            Self::deposit_event(Event::StakingConfirmed {
                node: who,
                tx_hash,
                stake_amount: info.stake_amount,
                when,
            });

            Ok(())
        }

        #[pallet::call_index(8)]
        #[pallet::weight((0, Pays::No))]
        pub fn request_retire(origin: OriginFor<T>) -> DispatchResult {
            let who = ensure_signed(origin)?;
            let when = <frame_system::Pallet<T>>::block_number();

            let mut info = AuthorizedOracleNodes::<T>::get(&who)
                .ok_or(Error::<T>::UnauthorizedNode)?;
            ensure!(
                info.state == OracleNodeStakingState::ActiveStake,
                Error::<T>::InvalidNodeState
            );

            info.state = OracleNodeStakingState::RetireStake;
            AuthorizedOracleNodes::<T>::insert(&who, info);

            Self::deposit_event(Event::RetireRequested {
                node: who,
                when,
            });

            Ok(())
        }

        #[pallet::call_index(9)]
        #[pallet::weight((0, Pays::No))]
        pub fn generate_retire_certificate(
            origin: OriginFor<T>,
            node_account: T::AccountId,
            lock_until_block: BlockNumberFor<T>,
            _expires_at_block: BlockNumberFor<T>,
        ) -> DispatchResult {
            ensure_root(origin)?;
            let when = <frame_system::Pallet<T>>::block_number();

            let _info = AuthorizedOracleNodes::<T>::get(&node_account)
                .ok_or(Error::<T>::UnauthorizedNode)?;

            Self::deposit_event(Event::RetireCertificateIssued {
                node: node_account,
                lock_until: lock_until_block,
                when,
            });

            Ok(())
        }

        #[pallet::call_index(10)]
        #[pallet::weight((0, Pays::No))]
        pub fn request_slash(
            origin: OriginFor<T>,
            node_account: T::AccountId,
            slash_amount: u64,
        ) -> DispatchResult {
            let who = ensure_signed(origin)?;
            let when = <frame_system::Pallet<T>>::block_number();

            let mut info = AuthorizedOracleNodes::<T>::get(&node_account)
                .ok_or(Error::<T>::UnauthorizedNode)?;

            ensure!(
                matches!(info.state,
                    OracleNodeStakingState::ActiveStake | OracleNodeStakingState::RetireStake),
                Error::<T>::InvalidNodeState
            );
            ensure!(
                slash_amount < info.stake_amount,
                Error::<T>::SlashAmountInvalid
            );

            SlashProposals::<T>::insert(&node_account, (slash_amount, who.clone(), when));
            info.state = OracleNodeStakingState::SlashVoting;
            AuthorizedOracleNodes::<T>::insert(&node_account, info);

            Self::deposit_event(Event::SlashRequested {
                node: node_account,
                slash_amount,
                initiated_by: who,
                when,
            });

            Ok(())
        }

        #[pallet::call_index(11)]
        #[pallet::weight((0, Pays::No))]
        pub fn vote_slash(
            origin: OriginFor<T>,
            node_account: T::AccountId,
            vote: SlashVote,
        ) -> DispatchResult {
            let who = ensure_signed(origin)?;
            let when = <frame_system::Pallet<T>>::block_number();

            ensure!(
                AuthorizedOracleNodes::<T>::contains_key(&who),
                Error::<T>::UnauthorizedNode
            );

            ensure!(
                who != node_account,
                Error::<T>::CannotVoteForSelf
            );

            let info = AuthorizedOracleNodes::<T>::get(&node_account)
                .ok_or(Error::<T>::UnauthorizedNode)?;
            ensure!(
                info.state == OracleNodeStakingState::SlashVoting,
                Error::<T>::NoActiveSlash
            );

            SlashVotes::<T>::insert(&node_account, &who, vote.clone());

            Self::deposit_event(Event::SlashVoteCast {
                node: node_account.clone(),
                voter: who,
                vote,
                when,
            });

            // Auto-tally if threshold reached
            Self::tally_slash_vote(&node_account)?;

            Ok(())
        }

        #[pallet::call_index(12)]
        #[pallet::weight((0, Pays::No))]
        pub fn generate_withdrawal_certificate(
            origin: OriginFor<T>,
            node_account: T::AccountId,
            approved_amount: u64,
            admin_pubkey: [u8; 32],
            admin_sig: [u8; 64],
        ) -> DispatchResult {
            let who = ensure_signed(origin)?;
            let when = <frame_system::Pallet<T>>::block_number();

            // Cannot approve your own withdrawal
            ensure!(who != node_account, Error::<T>::CannotVoteForSelf);

            // Node must exist
            let info = AuthorizedOracleNodes::<T>::get(&node_account)
                .ok_or(Error::<T>::UnauthorizedNode)?;

            ensure!(
                approved_amount <= info.stake_amount,
                Error::<T>::ApprovedAmountInvalid
            );

            // If a proposal already exists, verify params match
            if let Some(existing_amount) = WithdrawalApprovalInfo::<T>::get(&node_account) {
                ensure!(
                    existing_amount == approved_amount,
                    Error::<T>::ProposalMismatch
                );
            } else {
                // First signer — create the proposal
                WithdrawalApprovalInfo::<T>::insert(&node_account, approved_amount);
            }

            // Ensure this admin hasn't already signed
            ensure!(
                !WithdrawalApprovalSigs::<T>::contains_key(&node_account, &who),
                Error::<T>::AlreadySigned
            );

            // Store this admin's signature
            WithdrawalApprovalSigs::<T>::insert(&node_account, &who, (admin_pubkey, admin_sig));

            // Count collected signatures
            let sig_count = WithdrawalApprovalSigs::<T>::iter_prefix(&node_account).count() as u32;
            let threshold = MinNodesForTrustedAggregation::<T>::get().unwrap_or(2);

            if sig_count >= threshold {
                // Collect all sigs into BoundedVec
                let mut collected_sigs: BoundedVec<([u8; 32], [u8; 64]), ConstU32<32>> =
                    BoundedVec::new();
                for (_signer, sig_pair) in WithdrawalApprovalSigs::<T>::iter_prefix(&node_account) {
                    let _ = collected_sigs.try_push(sig_pair);
                }

                // Clean up approval storage
                WithdrawalApprovalInfo::<T>::remove(&node_account);
                let _ = WithdrawalApprovalSigs::<T>::clear_prefix(&node_account, u32::MAX, None);

                // Persist issued certificate so bridge-offchain can query it
                IssuedWithdrawalCerts::<T>::insert(
                    &node_account,
                    (approved_amount, collected_sigs.clone()),
                );

                Self::deposit_event(Event::WithdrawalCertificateIssued {
                    node: node_account,
                    approved_amount,
                    sigs: collected_sigs,
                    when,
                });
            } else {
                Self::deposit_event(Event::WithdrawalApprovalSigned {
                    node: node_account,
                    signer: who,
                    when,
                });
            }

            Ok(())
        }

        #[pallet::call_index(13)]
        #[pallet::weight((0, Pays::No))]
        pub fn confirm_cardano_withdrawal(
            origin: OriginFor<T>,
            tx_hash: [u8; 32],
            released_amount: u64,
            penalty_amount: u64,
        ) -> DispatchResult {
            let who = ensure_signed(origin)?;
            let when = <frame_system::Pallet<T>>::block_number();

            let mut info = AuthorizedOracleNodes::<T>::get(&who)
                .ok_or(Error::<T>::UnauthorizedNode)?;

            ensure!(
                released_amount + penalty_amount == info.stake_amount,
                Error::<T>::AmountMismatch
            );

            info.state = OracleNodeStakingState::Inactive;
            AuthorizedOracleNodes::<T>::insert(&who, info);

            // Certificate has been used — clear it from queryable storage
            IssuedWithdrawalCerts::<T>::remove(&who);

            Self::deposit_event(Event::WithdrawalConfirmed {
                node: who,
                tx_hash,
                released_amount,
                penalty_amount,
                when,
            });

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
                    if !AuthorizedOracleNodes::<T>::contains_key(&signer_account.id) {
                        log::error!("Oracle node not authorized.");
                        return;
                    }
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
                let timestamp_ms = timestamp::Pallet::<T>::get().saturated_into::<u64>();
                let timestamp = (timestamp_ms / 1000) * 1000; // Round to nearest second

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
        let (price, age, _) = aggregation_state
            .prices_age_and_rewards
            .get(trade_pair_index)?
            .clone()?;
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

    pub fn get_reward_config() -> Option<RewardConfiguration> {
        let reward_policy_id = RewardPolicyId::<T>::get().or_else(|| {
            log::error!("Error fetching RewardPolicyId");
            None
        })?;
        let reward_asset_name = RewardAssetName::<T>::get().or_else(|| {
            log::error!("Error fetching RewardAssetName");
            None
        })?;

        Some(RewardConfiguration {
            reward_policy_id,
            reward_asset_name,
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

        let reward_asset = match (RewardPolicyId::<T>::get(), RewardAssetName::<T>::get()) {
            (Some(policy_id), Some(asset_name)) => Some((policy_id, asset_name)),
            _ => None,
        };

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
                        reward_asset.clone(),
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
        reward_asset: Option<(ChannelId, BoundedVec<u8, ConstU32<64>>)>,
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
            reward_asset,
        }
    }

    // ==================== STAKING HELPERS ====================

    fn tally_slash_vote(node_account: &T::AccountId) -> DispatchResult {
        let total_nodes = AuthorizedOracleNodes::<T>::iter().count() as u32;
        let votes: Vec<SlashVote> = SlashVotes::<T>::iter_prefix(node_account)
            .map(|(_, vote)| vote)
            .collect();

        let approve_count = votes.iter().filter(|v| **v == SlashVote::Approve).count() as u32;
        let deny_count = votes.iter().filter(|v| **v == SlashVote::Deny).count() as u32;
        let threshold = (total_nodes + 1) / 2;  // Simple majority

        // If threshold reached, finalize
        if approve_count >= threshold {
            Self::finalize_slash_approved(node_account, approve_count, deny_count)?;
        } else if deny_count >= threshold {
            Self::finalize_slash_rejected(node_account, approve_count, deny_count)?;
        }

        Ok(())
    }

    fn finalize_slash_approved(
        node_account: &T::AccountId,
        approve_count: u32,
        deny_count: u32,
    ) -> DispatchResult {
        let when = <frame_system::Pallet<T>>::block_number();
        let (slash_amount, _, _) = SlashProposals::<T>::get(node_account)
            .ok_or(Error::<T>::NoActiveSlash)?;

        let mut info = AuthorizedOracleNodes::<T>::get(node_account)
            .ok_or(Error::<T>::UnauthorizedNode)?;

        info.state = OracleNodeStakingState::SlashApproved;
        AuthorizedOracleNodes::<T>::insert(node_account, info);

        SlashProposals::<T>::remove(node_account);
        let _ = SlashVotes::<T>::clear_prefix(node_account, u32::MAX, None);

        Self::deposit_event(Event::SlashApproved {
            node: node_account.clone(),
            slash_amount,
            approve_count,
            deny_count,
            when,
        });

        Ok(())
    }

    fn finalize_slash_rejected(
        node_account: &T::AccountId,
        approve_count: u32,
        deny_count: u32,
    ) -> DispatchResult {
        let when = <frame_system::Pallet<T>>::block_number();

        let mut info = AuthorizedOracleNodes::<T>::get(node_account)
            .ok_or(Error::<T>::UnauthorizedNode)?;

        // Restore state to RetireStake — slash only happens after request-retire,
        // so the node was in RetireStake before SlashVoting.
        if info.state == OracleNodeStakingState::SlashVoting {
            info.state = OracleNodeStakingState::RetireStake;
        }
        AuthorizedOracleNodes::<T>::insert(node_account, info);

        SlashProposals::<T>::remove(node_account);
        let _ = SlashVotes::<T>::clear_prefix(node_account, u32::MAX, None);

        Self::deposit_event(Event::SlashRejected {
            node: node_account.clone(),
            approve_count,
            deny_count,
            when,
        });

        Ok(())
    }
}
