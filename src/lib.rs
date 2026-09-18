//! rustc's frontend, as one crate.
//!
//! Upstream ships this as sixty-odd crates because bootstrap builds them together and the count
//! costs nothing there. Published, each name is permanent and each is a dependency somebody has to
//! take, so they are modules here instead. The split survives as the module names: what upstream
//! calls `rustc_middle` is `frontend::rustc_middle`, and a path that read `rustc_middle::ty::Ty`
//! upstream reads `crate::rustc_middle::ty::Ty` inside this crate.
//!
//! Two things could not come with them. `frontend_macros` holds the proc macros, because the
//! language compiles a proc-macro crate separately for the host and will not let it merge into a
//! normal one. `frontend_diag_template` holds the diagnostic-template parser, because both the
//! proc macros and this crate parse the same templates and a proc-macro crate cannot export a
//! normal item for this one to use.
//!
//! See `UPSTREAM.md` for provenance and `README.md` for the sysroot this needs in order to run.

#![no_std]
#![recursion_limit = "512"]
#![feature(allocator_api)]
#![feature(allow_internal_unstable)]
#![feature(arbitrary_self_types)]
#![feature(array_try_map)]
#![feature(ascii_char)]
#![feature(ascii_char_variants)]
#![feature(assert_matches)]
#![feature(associated_type_defaults)]
#![feature(auto_traits)]
#![feature(closure_track_caller)]
#![feature(const_default)]
#![feature(const_trait_impl)]
#![feature(const_type_name)]
#![feature(control_flow_into_value)]
#![feature(core_intrinsics)]
#![feature(core_io_borrowed_buf)]
#![feature(cow_is_borrowed)]
#![feature(debug_closure_helpers)]
#![feature(decl_macro)]
#![feature(default_field_values)]
#![feature(deref_patterns)]
#![feature(derive_const)]
#![feature(diagnostic_on_unknown)]
#![feature(discriminant_kind)]
#![feature(dropck_eyepatch)]
#![feature(error_iter)]
#![feature(exact_size_is_empty)]
#![feature(exhaustive_patterns)]
#![feature(extend_one)]
#![feature(extern_types)]
#![feature(f16)]
#![feature(gen_blocks)]
#![feature(impl_trait_in_assoc_type)]
#![feature(iter_intersperse)]
#![feature(iter_is_partitioned)]
#![feature(iter_order_by)]
#![feature(iterator_try_collect)]
#![feature(iterator_try_reduce)]
#![feature(macro_metavar_expr)]
#![feature(macro_metavar_expr_concat)]
#![feature(map_try_insert)]
#![feature(mem_conjure_zst)]
#![feature(min_specialization)]
#![feature(negative_impls)]
#![feature(nonzero_internals)]
#![feature(once_cell_get_mut)]
#![feature(option_into_flat_iter)]
#![feature(option_reference_flattening)]
#![feature(pattern_type_macro)]
#![feature(pattern_types)]
// `proc_macro_diagnostic`, `proc_macro_internals` and `proc_macro_quote` were here and are gone
// with `staged_api`. They were never core's: `proc_macro` declared them itself, in the
// `#[unstable(feature = "...")]` attributes on its own items, and gating against your own
// declarations is only meaningful for a crate that ships in the sysroot. With the attributes
// removed the names exist nowhere, and naming them is E0635, "unknown feature".
#![feature(ptr_alignment_type)]
#![feature(range_bounds_is_empty)]
#![feature(rustc_attrs)]
#![feature(rustdoc_internals)]
// Required by `rustc_data_structures`, `rustc_middle` and `rustc_serialize`, which spell
// `PointeeSized` in about twenty-five bounds. Upstream only those three crates enable it; here it
// is necessarily on for all seventy, which is why `RawList` needs the inherent slice methods in
// `rustc_middle/ty/list.rs`. Measured, not assumed: turning it off leaves the dropck failure in
// `rustc_interface::passes` exactly as it was, so that is a separate problem and this is not it.
#![feature(sized_hierarchy)]
#![feature(slice_partition_dedup)]
#![feature(slice_ptr_get)]
// Both are core library features these modules use and no crate in the union declared, because
// upstream they are supplied by bootstrap rather than by the crate. `step_trait` is
// `core::iter::Step`, which `rustc_abi` implements; `hasher_prefixfree_extras` is
// `Hasher::write_length_prefix`. They surfaced only once `staged_api` came off, because the pass
// that reports them is the one that was aborting on 39,258 missing stability attributes.
#![feature(step_trait)]
#![feature(hasher_prefixfree_extras)]
// `staged_api` is deliberately absent from this list, and it was in it.
//
// It arrived by unioning the feature lists of the crates that became modules here: `proc_macro`
// ships in the sysroot, so upstream it is a staged crate and every public item in it carries
// `#[stable]` or `#[unstable]`. Turning that feature on turns stability checking on for the whole
// crate, and the other sixty-nine modules have no such attributes - 39,258 errors, all of them
// "missing stability attribute", none of them a real defect.
//
// This is a normal library and not part of any sysroot, so it is not staged. The 211 stability
// attributes in `rustc_proc_macro` were removed with the feature; keeping them without it is
// E0734, "stability attributes may not be used outside of the standard library".
#![feature(stmt_expr_attributes)]
#![feature(titlecase)]
#![feature(trait_alias)]
#![feature(trim_prefix_suffix)]
#![feature(trusted_len)]
#![feature(try_blocks)]
#![feature(try_trait_v2)]
#![feature(try_trait_v2_residual)]
#![feature(try_trait_v2_yeet)]
#![feature(type_alias_impl_trait)]
#![feature(unqualified_local_imports)]
#![feature(unwrap_infallible)]
#![feature(variant_count)]
#![feature(yeet_expr)]
// Proc macros generate paths rooted at `frontend::`. This alias makes those resolve inside
// this crate too, so one generated path works for us and for a consumer alike.
extern crate self as frontend;


#[macro_use]
extern crate alloc;
// `pub`, because `rustc_log` re-exports it as part of its own interface: a consumer that
// configures logging needs the same `tracing` this crate emits into, not whichever version their
// own graph resolved. An `extern crate` binding is private by default and re-exporting one is
// E0365, a future-incompatibility that is already deny-by-default.
#[macro_use]
pub extern crate tracing;

pub mod datafrog;
pub mod ena;
pub mod frontend_facts;
pub mod frontend_semantics;
pub mod odht;
pub mod polonius_engine;
pub mod punycode_no_std;
pub mod rustc_abi;
// The arenas are a crate, not a module, and `frontend_arena/Cargo.toml` says why: merging them in
// makes dropck stop honouring `TypedArena`'s `#[may_dangle]` destructor. Re-exported under the
// name the tree already uses, so `crate::rustc_arena::…` resolves unchanged at all eighteen sites.
pub use frontend_arena as rustc_arena;
pub mod rustc_ast;
pub mod rustc_ast_ir;
pub mod rustc_ast_lowering;
pub mod rustc_ast_passes;
pub mod rustc_ast_pretty;
pub mod rustc_attr_ir;
pub mod rustc_attr_parsing;
pub mod rustc_borrowck;
pub mod rustc_builtin_macros;
pub mod rustc_const_eval;
pub mod rustc_crate_store;
pub mod rustc_data_structures;
// `#[macro_use]` so that `error_codes!` is in textual scope for `rustc_errors::codes`, which is
// declared below it and is the only caller. Upstream that call is `rustc_error_codes::error_codes!`,
// a path through the crate name, and there is no such path here: `#[macro_export]` puts the macro
// at *this* crate's root, and reaching a macro-expanded `macro_export` macro by absolute path from
// inside its own crate is a hard error. The module holds exactly one macro, so this brings in one
// name.
#[macro_use]
pub mod rustc_error_codes;
pub mod rustc_error_messages;
pub mod rustc_errors;
pub mod rustc_expand;
pub mod rustc_feature;
pub mod rustc_fs_util;
pub mod rustc_graphviz;
pub mod rustc_hashes;
pub mod rustc_hir;
pub mod rustc_hir_analysis;
pub mod rustc_hir_id;
pub mod rustc_hir_pretty;
pub mod rustc_hir_typeck;
pub mod rustc_index;
pub mod rustc_infer;
pub mod rustc_interface;
pub mod rustc_lexer;
pub mod rustc_lint;
pub mod rustc_lint_defs;
pub mod rustc_log;
pub mod rustc_metadata;
pub mod rustc_middle;
pub mod rustc_mir_build;
pub mod rustc_mir_dataflow;
pub mod rustc_mir_transform;
pub mod rustc_monomorphize;
pub mod rustc_next_trait_solver;
pub mod rustc_parse;
pub mod rustc_parse_format;
pub mod rustc_passes;
pub mod rustc_pattern_analysis;
pub mod rustc_privacy;
pub mod rustc_proc_macro;
pub mod rustc_query_impl;
pub mod rustc_resolve;
pub mod rustc_serialize;
pub mod rustc_session;
pub mod rustc_span;
pub mod rustc_stable_hash;
pub mod rustc_structures;
pub mod rustc_symbol_mangling;
pub mod rustc_target;
pub mod rustc_trait_selection;
pub mod rustc_traits;
pub mod rustc_transmute;
pub mod rustc_ty_utils;
pub mod rustc_ty_walk;
pub mod rustc_type_ir;
pub mod unwind_janky;
