#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use parity_scale_codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use scale_info::TypeInfo;
pub use frame_support::pallet_prelude::{BoundedVec, ConstU32};

/// Lifecycle status for operators
#[derive(Encode, Decode, Clone, PartialEq, Eq, Debug, MaxEncodedLen, TypeInfo)]
pub enum OperatorStatus {
    Requested,
    CertificateIssued,
    StakeConfirmed,
    Active,
    PendingRetire,
    Retired,
}

/// Staking certificate structure. Fields are generic byte arrays where runtime-specific
/// AccountId/Key types are SCALE-encoded into `operator_account`.
#[derive(Encode, Decode, DecodeWithMemTracking, Clone, PartialEq, Eq, Debug, MaxEncodedLen, TypeInfo)]
pub struct StakingCertificate {
    pub operator_account: BoundedVec<u8, ConstU32<64>>,
    pub cardano_stake_key_hash: BoundedVec<u8, ConstU32<64>>,
    pub sidechain_pubkey: BoundedVec<u8, ConstU32<64>>,
    pub stake_amount: u128,
    pub expiry: u64,
    pub nonce: u64,
}

impl StakingCertificate {
    /// Encode certificate to CBOR for Cardano interoperability.
    /// Numeric `stake_amount` is encoded as a byte string (big-endian) to safely
    /// represent u128 values; consumer policies should decode accordingly.
    pub fn to_cbor(&self) -> alloc::vec::Vec<u8> {
        let mut buf = alloc::vec::Vec::new();
        let mut e = minicbor::encode::Encoder::new(&mut buf);

        e.begin_array().ok();
        e.bytes(&self.operator_account[..]).ok();
        e.bytes(&self.cardano_stake_key_hash[..]).ok();
        e.bytes(&self.sidechain_pubkey[..]).ok();

        // Encode stake_amount as bytes (big-endian u128)
        let amt_be = self.stake_amount.to_be_bytes();
        e.bytes(&amt_be).ok();

        e.u64(self.expiry).ok();
        e.u64(self.nonce).ok();
        e.end().ok();
        buf
    }
}

#[derive(Encode, Decode, DecodeWithMemTracking, Clone, PartialEq, Eq, Debug, MaxEncodedLen, TypeInfo)]
pub struct SlashProposal {
    pub proposer: BoundedVec<u8, ConstU32<64>>,
    pub target_operator: BoundedVec<u8, ConstU32<64>>,
    pub amount: u128,
    pub reason: BoundedVec<u8, ConstU32<256>>,
    pub approvals: u32,
    pub rejections: u32,
}

/// Cardano UTXO and minted token reference stored when an operator confirms their stake.
#[derive(Encode, Decode, DecodeWithMemTracking, Clone, PartialEq, Eq, Debug, MaxEncodedLen, TypeInfo)]
pub struct CardanoStakeReference {
    pub tx_hash: BoundedVec<u8, ConstU32<64>>,
    pub output_index: u32,
    pub policy_id: BoundedVec<u8, ConstU32<64>>,
    pub asset_name: BoundedVec<u8, ConstU32<64>>,
}
