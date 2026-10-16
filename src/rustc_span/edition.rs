//! `Edition` lives in `frontend_symbol`, with the keywords it decides; this module re-exports it
//! and adds what needs this crate: serialization (the one-byte variant tag the derive wrote) and
//! stable hashing (its discriminant, as the derive hashed it).

use crate::rustc_data_structures::stable_hash::{StableHash, StableHashCtxt, StableHasher};
use crate::rustc_serialize::{Decodable, Encodable, Encoder};
use crate::rustc_span::BlobDecoder;

pub use frontend_symbol::edition::*;

impl<E: Encoder> Encodable<E> for Edition {
    fn encode(&self, e: &mut E) {
        e.emit_u8(*self as u8);
    }
}

impl<D: BlobDecoder> Decodable<D> for Edition {
    fn decode(d: &mut D) -> Edition {
        let tag = d.read_u8();
        Edition::from_index(tag).unwrap_or_else(|| {
            panic!(
                "invalid enum variant tag while decoding `Edition`, expected 0..{}, actual {tag}",
                ALL_EDITIONS.len()
            )
        })
    }
}

impl StableHash for Edition {
    #[inline]
    fn stable_hash<Hcx: StableHashCtxt>(&self, hcx: &mut Hcx, hasher: &mut StableHasher) {
        core::mem::discriminant(self).stable_hash(hcx, hasher);
    }
}
