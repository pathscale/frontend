use crate::rustc_data_structures::fx::FxIndexMap;
use crate::rustc_span::Symbol;
use crate::rustc_span::def_id::{DefId, DefIdMap};
use rustc_macros::StableHash;

#[derive(Debug, Default, StableHash)]
pub struct DiagnosticItems {
    #[stable_hash(ignore)]
    pub id_to_name: DefIdMap<Symbol>,
    pub name_to_id: FxIndexMap<Symbol, DefId>,
}
