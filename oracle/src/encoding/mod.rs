extern crate alloc;
use alloc::vec::Vec;

pub trait CardanoCbor {
    fn to_cardano_cbor(&self) -> Vec<u8>;
}

pub trait CborHashable {
    fn cardano_cbor_hash(&self) -> [u8; 32];
}
