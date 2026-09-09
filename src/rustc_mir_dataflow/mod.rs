// tidy-alphabetical-start
// tidy-alphabetical-end

// `framework/graphviz.rs` dumps dataflow graphs to .dot files behind -Zdump-mir-dataflow:
// `OsString`, `fs`, `io`. Debug output, so it goes with the rest of the writing surface.

// ---------------------------------------------------------------------------------------------
// STD IS BANNED IN THIS CRATE.
//
// `#![no_std]` above is the ban and the compiler is the enforcer: without `extern crate std;`
// there is no `std` in the extern prelude, so any `std::` path fails to resolve and the build
// stops. Do not add that line back to make an error go away - the error is the point. Whatever
// needed `std` either has a `core`/`alloc` equivalent, belongs in `ekostd`, or is a
// dependency that has to be replaced.
//
// The prelude is the part a grep cannot see: `Vec`, `String`, `Box`, `format!`, `vec!`,
// `thread_local!` and `println!` name no path. Under `#![no_std]` they resolve through `alloc`
// and `eko` instead, which is why those imports appear at the top of every file here.
// ---------------------------------------------------------------------------------------------
#[macro_use]
use crate::rustc_middle::ty;

// Please change the public `use` directives cautiously, as they might be used by external tools.
// See issue #120130.
pub use self::drop_flag_effects::{
    DropFlagState, drop_flag_effects_for_function_entry, drop_flag_effects_for_location,
    move_path_children_matching, on_all_children_bits, on_lookup_result_bits,
};
pub use self::framework::{
    Analysis, Backward, Direction, EntryStates, Forward, GenKill, JoinSemiLattice, MaybeReachable,
    Results, ResultsCursor, ResultsVisitor, SwitchTargetIndex, fmt, lattice,
    visit_results,
};
use self::move_paths::MoveData;

pub mod debuginfo;
mod diagnostics;
mod drop_flag_effects;
mod framework;
pub mod impls;
pub mod move_paths;
pub mod points;
pub mod rustc_peek;
mod un_derefer;
pub mod value_analysis;

pub struct MoveDataTypingEnv<'tcx> {
    pub move_data: MoveData<'tcx>,
    pub typing_env: ty::TypingEnv<'tcx>,
}
