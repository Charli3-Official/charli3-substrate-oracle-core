#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;


/// Types module for the staking participation pallet.
pub mod types;

#[cfg(feature = "pallet")]
#[frame_support::pallet]
pub mod pallet {
    use frame_support::pallet_prelude::*;
    use frame_support::traits::BuildGenesisConfig;
    use frame_system::pallet_prelude::*;
    use parity_scale_codec::Encode;
    use sp_std::convert::TryInto;
    use crate::types;
    use core::marker::PhantomData;

    #[pallet::pallet]
    pub struct Pallet<T>(PhantomData<T>);

    #[pallet::config]
    pub trait Config: frame_system::Config {
        type RuntimeEvent: From<Event<Self>> + IsType<<Self as frame_system::Config>::RuntimeEvent>;
        /// Governance origin used for gating administrative calls
        type GovernanceOrigin: EnsureOrigin<Self::RuntimeOrigin>;
    }

    #[pallet::storage]
    /// Registry of operators -> OperatorRecord
    pub type OperatorRegistry<T: Config> = StorageMap<_, Identity, T::AccountId, OperatorRecord<T::AccountId>>;

    #[pallet::storage]
    /// Certificates by nonce
    pub type Certificates<T> = StorageMap<_, Identity, u64, types::StakingCertificate>;

    #[pallet::storage]
    /// Cardano stake references indexed by certificate nonce
    pub type CardanoReferences<T> = StorageMap<_, Identity, u64, types::CardanoStakeReference>;

    #[pallet::storage]
    /// Governance authority config: admin keys and threshold
    pub type GovernanceAuthority<T: Config> = StorageValue<_, GovernanceConfig<T::AccountId>>;

    #[pallet::storage]
    /// Bootstrap operator count threshold used to determine when operator keys must be included
    pub type BootstrapOperatorCount<T: Config> = StorageValue<_, u32, ValueQuery>;

    #[pallet::storage]
    /// Slash proposals indexed by proposal id
    pub type SlashProposals<T> = StorageMap<_, Identity, u64, types::SlashProposal>;

    #[pallet::storage]
    /// Next slash proposal id
    pub type NextSlashProposalId<T: Config> = StorageValue<_, u64, ValueQuery>;

    #[pallet::storage]
    /// Per-proposal vote record: proposal_id -> account -> vote (0 none, 1 approve, 2 reject)
    pub type SlashVotes<T: Config> = StorageDoubleMap<_, Identity, u64, Identity, T::AccountId, u8>;

    #[derive(Encode, Decode, Clone, PartialEq, Eq, Debug, TypeInfo, MaxEncodedLen)]
    pub struct OperatorRecord<AccountId> {
        pub account: AccountId,
        pub status: types::OperatorStatus,
        pub certificate_nonces: types::BoundedVec<u64, types::ConstU32<128>>,
    }

    #[derive(Encode, Decode, Clone, PartialEq, Eq, Debug, TypeInfo, MaxEncodedLen)]
    pub struct GovernanceConfig<AccountId> {
        pub admins: types::BoundedVec<AccountId, types::ConstU32<16>>,
        pub threshold: u32,
    }

    #[pallet::event]
    #[pallet::generate_deposit(pub(super) fn deposit_event)]
    pub enum Event<T: Config> {
        JoinRequested { who: T::AccountId, when: frame_system::pallet_prelude::BlockNumberFor<T> },
        CertificateIssued { operator: T::AccountId, nonce: u64 },
        StakeConfirmed { operator: T::AccountId, nonce: u64 },
        NodeAuthorized { operator: T::AccountId },
        RetireRequested { operator: T::AccountId },
        RetireAuthorized { operator: T::AccountId, nonce: u64 },
        SlashProposed { proposal_id: u64, operator: T::AccountId },
        SlashApproved { proposal_id: u64, operator: T::AccountId, amount: u128 },
        SlashRejected { proposal_id: u64 },
        GovernanceAuthorityUpdated,
    }

    #[pallet::error]
    pub enum Error<T> {
        AlreadyRegistered,
        NotRegistered,
        InvalidStatus,
        NotAuthorized,
        ProposalNotFound,
        AlreadyVoted,
        NotGovernanceAdmin,
        InsufficientOperatorInclusion,
    }

    #[pallet::call]
    impl<T: Config> Pallet<T> {
        #[pallet::call_index(0)]
        #[pallet::weight((0, Pays::No))]
        pub fn request_join(
            origin: OriginFor<T>,
            cardano_stake_key: Vec<u8>,
            stake_amount: u128,
            sidechain_pubkey: Vec<u8>,
        ) -> DispatchResult {
            let who = ensure_signed(origin)?;

            ensure!(
                !OperatorRegistry::<T>::contains_key(&who),
                Error::<T>::AlreadyRegistered
            );

            let record = OperatorRecord {
                account: who.clone(),
                status: types::OperatorStatus::Requested,
                certificate_nonces: types::BoundedVec::default(),
            };

            OperatorRegistry::<T>::insert(&who, record);

            let when = <frame_system::Pallet<T>>::block_number();
            Self::deposit_event(Event::JoinRequested { who, when });

            Ok(())
        }
        
        #[pallet::call_index(1)]
        #[pallet::weight((0, Pays::No))]
        pub fn issue_certificate(
            origin: OriginFor<T>,
            operator: T::AccountId,
            certificate: types::StakingCertificate,
        ) -> DispatchResult {
            // Ensure the call is coming from the configured governance origin
            T::GovernanceOrigin::ensure_origin(origin).map_err(|_| Error::<T>::NotAuthorized)?;

            // Operator must be registered
            ensure!(OperatorRegistry::<T>::contains_key(&operator), Error::<T>::NotRegistered);

            // Check operator is in Requested state
            let mut record = OperatorRegistry::<T>::get(&operator).ok_or(Error::<T>::NotRegistered)?;
            ensure!(record.status == types::OperatorStatus::Requested, Error::<T>::InvalidStatus);

            // Verify the certificate nonce is unused
            ensure!(!Certificates::<T>::contains_key(&certificate.nonce), Error::<T>::InvalidStatus);

            // Verify the certificate's embedded operator_account matches the provided operator
            let encoded_op = operator.encode();
            ensure!(encoded_op.len() <= certificate.operator_account.capacity() as usize, Error::<T>::InvalidStatus);
            ensure!(encoded_op.as_slice() == &certificate.operator_account[..], Error::<T>::InvalidStatus);

            // Persist certificate
            Certificates::<T>::insert(certificate.nonce, certificate.clone());

            // Update operator record
            record.status = types::OperatorStatus::CertificateIssued;
            {
                let mut cn = record.certificate_nonces.clone();
                cn.try_push(certificate.nonce).map_err(|_| Error::<T>::InvalidStatus)?;
                record.certificate_nonces = cn;
            }
            OperatorRegistry::<T>::insert(&operator, record);

            // Emit event
            Self::deposit_event(Event::CertificateIssued { operator, nonce: certificate.nonce });

            Ok(())
        }

        #[pallet::call_index(2)]
        #[pallet::weight((0, Pays::No))]
        pub fn confirm_stake(
            origin: OriginFor<T>,
            tx_hash: Vec<u8>,
            output_index: u32,
            policy_id: Vec<u8>,
            asset_name: Vec<u8>,
            certificate_nonce: u64,
        ) -> DispatchResult {
            let who = ensure_signed(origin)?;

            // Ensure operator is registered
            ensure!(OperatorRegistry::<T>::contains_key(&who), Error::<T>::NotRegistered);

            // Ensure certificate exists
            ensure!(Certificates::<T>::contains_key(&certificate_nonce), Error::<T>::InvalidStatus);

            // Ensure operator currently in CertificateIssued state
            let mut record = OperatorRegistry::<T>::get(&who).ok_or(Error::<T>::NotRegistered)?;
            ensure!(record.status == types::OperatorStatus::CertificateIssued, Error::<T>::InvalidStatus);

            // Ensure the certificate_nonce belongs to this operator (was issued to them)
            ensure!(record.certificate_nonces.contains(&certificate_nonce), Error::<T>::InvalidStatus);

            // Persist Cardano reference keyed by certificate nonce (convert to bounded types)
            let tx_bv: types::BoundedVec<u8, types::ConstU32<64>> = tx_hash.try_into().map_err(|_| Error::<T>::InvalidStatus)?;
            let policy_bv: types::BoundedVec<u8, types::ConstU32<64>> = policy_id.try_into().map_err(|_| Error::<T>::InvalidStatus)?;
            let asset_bv: types::BoundedVec<u8, types::ConstU32<64>> = asset_name.try_into().map_err(|_| Error::<T>::InvalidStatus)?;

            let cref = types::CardanoStakeReference {
                tx_hash: tx_bv,
                output_index,
                policy_id: policy_bv,
                asset_name: asset_bv,
            };

            CardanoReferences::<T>::insert(certificate_nonce, cref);

            // Transition operator to StakeConfirmed
            record.status = types::OperatorStatus::StakeConfirmed;
            OperatorRegistry::<T>::insert(&who, record);

            // Emit event
            Self::deposit_event(Event::StakeConfirmed { operator: who, nonce: certificate_nonce });

            Ok(())
        }

        #[pallet::call_index(3)]
        #[pallet::weight((0, Pays::No))]
        pub fn authorize_node(
            origin: OriginFor<T>,
            operator: T::AccountId,
        ) -> DispatchResult {
            // Ensure caller is the configured governance origin
            T::GovernanceOrigin::ensure_origin(origin).map_err(|_| Error::<T>::NotAuthorized)?;

            // Operator must exist
            ensure!(OperatorRegistry::<T>::contains_key(&operator), Error::<T>::NotRegistered);

            let mut record = OperatorRegistry::<T>::get(&operator).ok_or(Error::<T>::NotRegistered)?;
            // Must be in StakeConfirmed state
            ensure!(record.status == types::OperatorStatus::StakeConfirmed, Error::<T>::InvalidStatus);

            // Transition to Active
            record.status = types::OperatorStatus::Active;
            OperatorRegistry::<T>::insert(&operator, record);

            // Emit event
            Self::deposit_event(Event::NodeAuthorized { operator });

            Ok(())
        }

        #[pallet::call_index(4)]
        #[pallet::weight((0, Pays::No))]
        pub fn request_retire(
            origin: OriginFor<T>,
        ) -> DispatchResult {
            let who = ensure_signed(origin)?;

            // Ensure operator exists and is Active
            ensure!(OperatorRegistry::<T>::contains_key(&who), Error::<T>::NotRegistered);
            let mut record = OperatorRegistry::<T>::get(&who).ok_or(Error::<T>::NotRegistered)?;
            ensure!(record.status == types::OperatorStatus::Active, Error::<T>::InvalidStatus);

            // Transition to PendingRetire
            record.status = types::OperatorStatus::PendingRetire;
            OperatorRegistry::<T>::insert(&who, record);

            Self::deposit_event(Event::RetireRequested { operator: who });

            Ok(())
        }

        #[pallet::call_index(5)]
        #[pallet::weight((0, Pays::No))]
        pub fn authorize_retire(
            origin: OriginFor<T>,
            operator: T::AccountId,
            certificate_nonce: u64,
            lock_until_unix_ms: u64,
        ) -> DispatchResult {
            // Governance only
            T::GovernanceOrigin::ensure_origin(origin).map_err(|_| Error::<T>::NotAuthorized)?;

            // Operator must exist and be PendingRetire
            ensure!(OperatorRegistry::<T>::contains_key(&operator), Error::<T>::NotRegistered);
            let mut record = OperatorRegistry::<T>::get(&operator).ok_or(Error::<T>::NotRegistered)?;
            ensure!(record.status == types::OperatorStatus::PendingRetire, Error::<T>::InvalidStatus);

            // Ensure nonce unused
            ensure!(!Certificates::<T>::contains_key(&certificate_nonce), Error::<T>::InvalidStatus);

            // Build a retire certificate (reusing StakingCertificate with expiry = lock_until)
            let op_encoded = operator.encode();
            let op_bounded: types::BoundedVec<u8, types::ConstU32<64>> =
                op_encoded.try_into().map_err(|_| Error::<T>::InvalidStatus)?;

            let empty_bounded: types::BoundedVec<u8, types::ConstU32<64>> = types::BoundedVec::default();

            let cert = types::StakingCertificate {
                operator_account: op_bounded,
                cardano_stake_key_hash: empty_bounded.clone(),
                sidechain_pubkey: empty_bounded,
                stake_amount: 0u128,
                expiry: lock_until_unix_ms,
                nonce: certificate_nonce,
            };

            Certificates::<T>::insert(certificate_nonce, cert.clone());

            // Update operator record to Retired and store nonce
            record.status = types::OperatorStatus::Retired;
            {
                let mut cn = record.certificate_nonces.clone();
                cn.try_push(certificate_nonce).map_err(|_| Error::<T>::InvalidStatus)?;
                record.certificate_nonces = cn;
            }
            OperatorRegistry::<T>::insert(&operator, record);

            Self::deposit_event(Event::RetireAuthorized { operator, nonce: certificate_nonce });

            Ok(())
        }

        #[pallet::call_index(6)]
        #[pallet::weight((0, Pays::No))]
        pub fn request_slash(
            origin: OriginFor<T>,
            operator: T::AccountId,
            amount: u128,
            reason: Vec<u8>,
        ) -> DispatchResult {
            let who = ensure_signed(origin)?;

            // ensure operator exists
            ensure!(OperatorRegistry::<T>::contains_key(&operator), Error::<T>::NotRegistered);

            // build proposal
            let pid = NextSlashProposalId::<T>::get();

            // encode proposer and operator into bounded vecs
            let proposer_enc = who.encode();
            let proposer_bv: types::BoundedVec<u8, types::ConstU32<64>> =
                proposer_enc.try_into().map_err(|_| Error::<T>::InvalidStatus)?;

            let operator_enc = operator.encode();
            let operator_bv: types::BoundedVec<u8, types::ConstU32<64>> =
                operator_enc.try_into().map_err(|_| Error::<T>::InvalidStatus)?;

            let reason_bv: types::BoundedVec<u8, types::ConstU32<256>> =
                reason.try_into().map_err(|_| Error::<T>::InvalidStatus)?;

            let proposal = types::SlashProposal {
                proposer: proposer_bv,
                target_operator: operator_bv,
                amount,
                reason: reason_bv,
                approvals: 0u32,
                rejections: 0u32,
            };

            SlashProposals::<T>::insert(pid, proposal);
            NextSlashProposalId::<T>::put(pid.wrapping_add(1));

            Self::deposit_event(Event::SlashProposed { proposal_id: pid, operator });

            Ok(())
        }

        #[pallet::call_index(7)]
        #[pallet::weight((0, Pays::No))]
        pub fn approve_slash(
            origin: OriginFor<T>,
            proposal_id: u64,
        ) -> DispatchResult {
            let who = ensure_signed(origin)?;

            // governance authority must exist
            let gconf = GovernanceAuthority::<T>::get().ok_or(Error::<T>::NotGovernanceAdmin)?;
            // ensure signer is an admin
            ensure!(gconf.admins.contains(&who), Error::<T>::NotGovernanceAdmin);

            // proposal must exist
            ensure!(SlashProposals::<T>::contains_key(&proposal_id), Error::<T>::ProposalNotFound);

            // ensure not already voted
            ensure!(!SlashVotes::<T>::contains_key(&proposal_id, &who), Error::<T>::AlreadyVoted);

            // mark vote
            SlashVotes::<T>::insert(&proposal_id, &who, 1u8);

            // increment approvals
            SlashProposals::<T>::mutate(&proposal_id, |maybe| {
                if let Some(p) = maybe {
                    p.approvals = p.approvals.saturating_add(1);
                }
            });

            // check threshold
            let p = SlashProposals::<T>::get(&proposal_id).ok_or(Error::<T>::ProposalNotFound)?;
            if p.approvals >= gconf.threshold {
                // emit approved event and remove proposal
                // need to decode target_operator back into AccountId
                let op_bytes: Vec<u8> = p.target_operator.clone().into();
                let op_account = T::AccountId::decode(&mut &op_bytes[..]).map_err(|_| Error::<T>::InvalidStatus)?;
                Self::deposit_event(Event::SlashApproved { proposal_id, operator: op_account, amount: p.amount });
                SlashProposals::<T>::remove(&proposal_id);
            }

            Ok(())
        }

        #[pallet::call_index(8)]
        #[pallet::weight((0, Pays::No))]
        pub fn reject_slash(
            origin: OriginFor<T>,
            proposal_id: u64,
        ) -> DispatchResult {
            let who = ensure_signed(origin)?;

            let gconf = GovernanceAuthority::<T>::get().ok_or(Error::<T>::NotGovernanceAdmin)?;
            ensure!(gconf.admins.contains(&who), Error::<T>::NotGovernanceAdmin);

            ensure!(SlashProposals::<T>::contains_key(&proposal_id), Error::<T>::ProposalNotFound);
            ensure!(!SlashVotes::<T>::contains_key(&proposal_id, &who), Error::<T>::AlreadyVoted);

            SlashVotes::<T>::insert(&proposal_id, &who, 2u8);

            SlashProposals::<T>::mutate(&proposal_id, |maybe| {
                if let Some(p) = maybe {
                    p.rejections = p.rejections.saturating_add(1);
                }
            });

            // if rejections >= threshold, remove proposal and emit rejected event
            let p = SlashProposals::<T>::get(&proposal_id).ok_or(Error::<T>::ProposalNotFound)?;
            if p.rejections >= gconf.threshold {
                Self::deposit_event(Event::SlashRejected { proposal_id });
                SlashProposals::<T>::remove(&proposal_id);
            }

            Ok(())
        }

        #[pallet::call_index(9)]
        #[pallet::weight((0, Pays::No))]
        pub fn update_governance_authority(
            origin: OriginFor<T>,
            admins: types::BoundedVec<T::AccountId, types::ConstU32<16>>,
            threshold: u32,
        ) -> DispatchResult {
            // Only governance origin can call
            T::GovernanceOrigin::ensure_origin(origin).map_err(|_| Error::<T>::NotAuthorized)?;

            // Convert admins into storage type
            let gconf = GovernanceConfig { admins: admins.clone(), threshold };

            // If active operator count >= bootstrap threshold, require that at least one active operator
            // is present in the new admin set.
            let bootstrap_n = BootstrapOperatorCount::<T>::get();
            let mut active_count: u32 = 0;
            for (_acc, rec) in OperatorRegistry::<T>::iter() {
                if rec.status == types::OperatorStatus::Active {
                    active_count = active_count.saturating_add(1);
                }
            }

            if active_count >= bootstrap_n {
                // ensure admins contains at least one active operator
                let mut found = false;
                for admin in admins.iter() {
                    if let Some(rec) = OperatorRegistry::<T>::get(admin) {
                        if rec.status == types::OperatorStatus::Active {
                            found = true;
                            break;
                        }
                    }
                }
                ensure!(found, Error::<T>::InsufficientOperatorInclusion);
            }

            GovernanceAuthority::<T>::put(gconf);
            Self::deposit_event(Event::GovernanceAuthorityUpdated);

            Ok(())
        }
    }

    #[pallet::genesis_config]
    pub struct GenesisConfig<T: Config> {
        pub initial_admins: types::BoundedVec<T::AccountId, types::ConstU32<16>>,
        pub initial_threshold: u32,
        pub bootstrap_operator_count: u32,
    }

    #[cfg(feature = "std")]
    impl<T: Config> Default for GenesisConfig<T> {
        fn default() -> Self {
            Self { initial_admins: types::BoundedVec::default(), initial_threshold: 1u32, bootstrap_operator_count: 0u32 }
        }
    }

    #[pallet::genesis_build]
    impl<T: Config> BuildGenesisConfig for GenesisConfig<T> {
        fn build(&self) {
            let gconf = GovernanceConfig { admins: self.initial_admins.clone(), threshold: self.initial_threshold };
            GovernanceAuthority::<T>::put(gconf);
            BootstrapOperatorCount::<T>::put(self.bootstrap_operator_count);
        }
    }
}

// crate-level re-exports
pub use types::*;
#[cfg(feature = "pallet")]
pub use pallet::*;

