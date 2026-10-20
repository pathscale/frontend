// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.

pub mod codegen_fn_attrs;
pub mod dead_code;
pub mod debugger_visualizer;
pub mod deduced_param_attrs;
pub mod dependency_format;
pub mod exported_symbols;
pub mod lang_items;
pub mod lib_features {
    use crate::rustc_data_structures::unord::UnordMap;
    use crate::rustc_span::{Span, Symbol};
    use alloc::vec::Vec;
    use rustc_macros::{BlobDecodable, Encodable, StableHash};

    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    #[derive(StableHash, Encodable, BlobDecodable)]
    pub enum FeatureStability {
        AcceptedSince(Symbol),
        Unstable { old_name: Option<Symbol> },
    }

    #[derive(StableHash, Debug, Default)]
    pub struct LibFeatures {
        pub stability: UnordMap<Symbol, (FeatureStability, Span)>,
    }

    impl LibFeatures {
        pub fn to_sorted_vec(&self) -> Vec<(Symbol, FeatureStability)> {
            self.stability
                .to_sorted_stable_ord()
                .iter()
                .map(|&(&sym, &(stab, _))| (sym, stab))
                .collect()
        }
    }
}
pub mod privacy;
pub mod region;
pub mod resolve_bound_vars;
pub mod stability;
