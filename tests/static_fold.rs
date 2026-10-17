#![no_std]

extern crate alloc;

use alloc::sync::Arc;
use core::sync::atomic::AtomicBool;

use frontend::frontend_facts::Setup;
use frontend::rustc_abi::Size;
use frontend::rustc_hir::def::DefKind;
use frontend::rustc_hir::def_id::{CRATE_DEF_INDEX, DefId};
use frontend::rustc_interface::{Config, create_and_enter_global_ctxt, parse, run_compiler};
use frontend::rustc_middle::mir::interpret::{AllocId, alloc_range};
use frontend::rustc_middle::mir::visit::Visitor;
use frontend::rustc_middle::mir::{self, ConstValue};
use frontend::rustc_middle::ty::{self, TyCtxt};
use frontend::rustc_session::config::Input;
use frontend::rustc_span::FileName;

#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

const SOURCE: &str = r#"
#![feature(no_core, lang_items, auto_traits)]
#![allow(internal_features, dead_code, unconditional_panic)]
#![no_core]
#[lang = "pointee_sized"] pub trait PointeeSized {}
#[lang = "meta_sized"] pub trait MetaSized: PointeeSized {}
#[lang = "sized"] pub trait Sized: MetaSized {}
#[lang = "sync"] pub unsafe auto trait Sync {}
#[lang = "freeze"] pub unsafe auto trait Freeze {}
#[lang = "unsize"] pub trait Unsize<T: PointeeSized>: PointeeSized {}
#[lang = "coerce_unsized"] pub trait CoerceUnsized<T: PointeeSized>: Sized {}
impl<'a, T: ?Sized + Unsize<U>, U: ?Sized> CoerceUnsized<&'a U> for &'a T {}
#[lang = "clone"] pub trait Clone: Sized {
    #[lang = "clone_fn"] fn clone(&self) -> Self;
}
#[lang = "copy"] pub trait Copy: Clone {}
#[lang = "legacy_receiver"] pub trait LegacyReceiver {}
impl<T: ?Sized> LegacyReceiver for &T {}
impl<T: ?Sized> Clone for &T { fn clone(&self) -> Self { *self } }
impl<T: ?Sized> Copy for &T {}
impl Clone for u8 { fn clone(&self) -> Self { *self } }
impl Copy for u8 {}
impl Clone for usize { fn clone(&self) -> Self { *self } }
impl Copy for usize {}
#[lang = "index"] pub trait Index<Idx> {
    type Output: ?Sized;
    fn index(&self, index: Idx) -> &Self::Output;
}
impl<T, const N: usize> Index<usize> for [T; N] {
    type Output = T;
    fn index(&self, _: usize) -> &T { loop {} }
}
#[lang = "rem"] pub trait Rem<Rhs = Self> {
    type Output;
    fn rem(self, rhs: Rhs) -> Self::Output;
}
impl Rem for usize { type Output = usize; fn rem(self, _: usize) -> usize { 0 } }

#[repr(C)]
pub struct Pair { byte: u8, word: usize }
pub const PAIR: Pair = Pair { byte: 1, word: 47 };
pub const FIELD: &usize = &PAIR.word;
pub const REBORROW: &usize = { let pair = &PAIR; &pair.word };
pub const READ: usize = { let pair = &PAIR; *&pair.word };
pub const INDEX: &usize = &[11, 22, 33][1];
pub const PATTERN: &usize = { let [_, ref middle, _] = [11, 22, 33]; middle };
pub const FROM_END: &usize = { let [.., ref last] = [11, 22, 33]; last };
pub const NESTED: &usize = &(&PAIR).word;
pub const EMPTY: [usize; 0] = [];
pub const EMPTY_REPEAT: [usize; 0] = [7; 0];

#[repr(C)]
pub enum Choice { First(usize), Second(u8, usize) }
pub const fn choose(value: &Choice) -> &usize {
    match value { Choice::First(v) => v, Choice::Second(_, v) => v }
}
pub const VARIANT: &usize = choose(&Choice::Second(2, 73));
pub const ENUM: Choice = Choice::Second(2, 73);
pub enum Maybe<'a> { No, Yes(&'a usize) }
pub const NONE: Maybe<'static> = Maybe::No;
pub const SOME: Maybe<'static> = Maybe::Yes(&PAIR.word);
#[repr(u8)]
pub enum Flag { First, Second }
pub const FLAG: Flag = Flag::Second;

#[repr(transparent)]
pub struct Alignment(usize);
impl Alignment { pub const fn get(&self) -> usize { self.0 } }
pub struct Layout { size: usize, align: Alignment }
impl Layout {
    pub const fn size(&self) -> usize { self.size }
    pub const fn align(&self) -> usize { self.align.get() }
}
pub trait Properties { const LAYOUT: Layout; }
impl Properties for usize {
    const LAYOUT: Layout = Layout { size: 24, align: Alignment(8) };
}
pub const fn new_in<T: Properties>() -> usize {
    const { T::LAYOUT.size() % T::LAYOUT.align() }
}
pub const RAW_VEC_LAYOUT_CHECK: usize = new_in::<usize>();

#[repr(C)]
pub struct Buffer { prefix: usize, words: [usize; 3] }
pub const SLICE: &[usize] = &Buffer { prefix: 9, words: [11, 22, 33] }.words;

pub const OUT_OF_BOUNDS: &usize = &[11, 22, 33][3];
pub const RAW_POINTER: usize = { let pointer = &PAIR.word as *const usize; unsafe { *pointer } };
pub const MUTATION: usize = { let mut pair = Pair { byte: 1, word: 47 }; pair.word = 9; pair.word };
"#;

fn with_tcx(f: impl for<'tcx> FnOnce(TyCtxt<'tcx>) + Send) {
    with_source_tcx(SOURCE, None, f);
}

fn with_source_tcx(
    source: &str,
    sysroot: Option<&str>,
    f: impl for<'tcx> FnOnce(TyCtxt<'tcx>) + Send,
) {
    static INTERNAL: AtomicBool = AtomicBool::new(false);
    let input = Input::Str {
        name: FileName::Custom("static-fold.rs".into()),
        input: Arc::new(source.into()),
    };
    let opts =
        Setup { sysroot, ..Setup::plain("static_fold") }.options(&input).expect("session options");
    run_compiler(
        Config {
            opts,
            input,
            psess_created: None,
            using_internal_features: &INTERNAL,
            crate_cfg: alloc::vec![],
        },
        |compiler| {
            let krate = parse(&compiler.sess);
            create_and_enter_global_ctxt(compiler, krate, f);
        },
    );
}

fn constant<'tcx>(tcx: TyCtxt<'tcx>, name: &str) -> ConstValue {
    let owner = tcx
        .hir_body_owners()
        .find(|owner| {
            matches!(tcx.def_kind(*owner), DefKind::Const { .. })
                && tcx.item_name(owner.to_def_id()).as_str() == name
        })
        .expect("fixture constant");
    tcx.const_eval_poly(owner.to_def_id()).expect("constant folds")
}

fn read_word(tcx: TyCtxt<'_>, alloc_id: AllocId, offset: Size) -> u128 {
    tcx.global_alloc(alloc_id)
        .unwrap_memory()
        .inner()
        .read_scalar(&tcx, alloc_range(offset, tcx.data_layout.pointer_size()), false)
        .expect("initialized word")
        .try_to_scalar_int()
        .expect("integer word")
        .to_bits(tcx.data_layout.pointer_size())
}

#[test]
fn projected_references_preserve_storage_offsets_and_folded_values() {
    with_tcx(|tcx| {
        for (name, expected, offset) in [
            ("FIELD", 47, 8),
            ("REBORROW", 47, 8),
            ("NESTED", 47, 8),
            ("INDEX", 22, 8),
            ("PATTERN", 22, 8),
            ("FROM_END", 33, 16),
            ("VARIANT", 73, 16),
        ] {
            let pointer =
                constant(tcx, name).try_to_scalar().expect("reference scalar").to_pointer(&tcx);
            let (provenance, actual_offset) = pointer.into_raw_parts();
            assert_eq!(actual_offset.bytes(), offset, "{name}");
            assert_eq!(
                read_word(
                    tcx,
                    provenance.expect("allocation provenance").alloc_id(),
                    actual_offset
                ),
                expected,
                "{name}"
            );
        }
        assert_eq!(constant(tcx, "READ").try_to_bits(tcx.data_layout.pointer_size()), Some(47));
        assert_eq!(
            constant(tcx, "RAW_VEC_LAYOUT_CHECK").try_to_bits(tcx.data_layout.pointer_size()),
            Some(0)
        );
        assert!(matches!(constant(tcx, "EMPTY"), ConstValue::ZeroSized));
        assert!(matches!(constant(tcx, "EMPTY_REPEAT"), ConstValue::ZeroSized));
    });
}

#[test]
fn folded_enum_storage_has_direct_and_niche_tags() {
    with_tcx(|tcx| {
        let ConstValue::Indirect { alloc_id, offset } = constant(tcx, "ENUM") else {
            panic!("enum allocation expected");
        };
        assert_eq!(read_word(tcx, alloc_id, offset), 1);
        assert_eq!(read_word(tcx, alloc_id, offset + Size::from_bytes(16)), 73);
        assert_eq!(constant(tcx, "NONE").try_to_bits(tcx.data_layout.pointer_size()), Some(0));
        assert_eq!(constant(tcx, "FLAG").try_to_bits(Size::from_bytes(1)), Some(1));
        let pointer =
            constant(tcx, "SOME").try_to_scalar().expect("niche enum scalar").to_pointer(&tcx);
        let (provenance, offset) = pointer.into_raw_parts();
        assert_eq!(
            read_word(tcx, provenance.expect("reference provenance").alloc_id(), offset),
            47
        );
    });
}

#[test]
fn unsizing_a_projected_array_preserves_the_slice_pointer_and_length() {
    with_tcx(|tcx| {
        let ConstValue::Indirect { alloc_id, offset } = constant(tcx, "SLICE") else {
            panic!("slice descriptor allocation expected");
        };
        let pointer_size = tcx.data_layout.pointer_size();
        assert_eq!(read_word(tcx, alloc_id, offset + pointer_size), 3);
        let pointer = tcx
            .global_alloc(alloc_id)
            .unwrap_memory()
            .inner()
            .read_scalar(&tcx, alloc_range(offset, pointer_size), true)
            .expect("slice pointer")
            .to_pointer(&tcx);
        let (provenance, offset) = pointer.into_raw_parts();
        assert_eq!(offset.bytes(), 8);
        assert_eq!(read_word(tcx, provenance.expect("slice provenance").alloc_id(), offset), 11);
    });
}

#[test]
#[ignore = "requires FRONTEND_FOLD_SYSROOT naming the caller's compiled library"]
fn raw_vec_layout_assertion_against_a_named_library() {
    let sysroot = eko::env::var("FRONTEND_FOLD_SYSROOT").expect("explicit test sysroot");
    let source = r#"
        #![feature(sized_type_properties)]
        extern crate alloc;
        pub const CHECK: () = const {
            assert!(<usize as core::mem::SizedTypeProperties>::LAYOUT.size()
                % <usize as core::mem::SizedTypeProperties>::LAYOUT.align() == 0)
        };
    "#;
    with_source_tcx(source, Some(&sysroot), |tcx| {
        assert!(matches!(constant(tcx, "CHECK"), ConstValue::ZeroSized));
        let krate = *tcx
            .crates(())
            .iter()
            .find(|&&krate| tcx.crate_name(krate).as_str() == "alloc")
            .expect("explicit alloc dependency");
        let root = DefId { krate, index: CRATE_DEF_INDEX };
        let child = |parent, name| {
            tcx.module_children(parent)
                .iter()
                .find(|child| child.ident.name.as_str() == name)
                .expect("library item")
                .res
                .def_id()
        };
        let raw_vec = child(child(root, "raw_vec"), "RawVec");
        let global = child(child(root, "alloc"), "Global");
        let global_ty = tcx.type_of(global).instantiate_identity().skip_norm_wip();
        let new_in = tcx
            .inherent_impls(raw_vec)
            .iter()
            .flat_map(|&owner| tcx.associated_item_def_ids(owner))
            .copied()
            .find(|&item| tcx.item_name(item).as_str() == "new_in")
            .expect("RawVec::new_in");

        struct Constants<'tcx>(alloc::vec::Vec<mir::Const<'tcx>>);
        impl<'tcx> Visitor<'tcx> for Constants<'tcx> {
            fn visit_const_operand(
                &mut self,
                constant: &mir::ConstOperand<'tcx>,
                _: mir::Location,
            ) {
                self.0.push(constant.const_);
            }
        }
        for element in [tcx.types.u8, tcx.types.usize, tcx.types.unit] {
            let args = tcx.mk_args(&[element.into(), global_ty.into()]);
            let instance = ty::Instance::new_raw(new_in, args);
            let typing_env = ty::TypingEnv::fully_monomorphized();
            let body = instance.instantiate_mir_and_normalize_erasing_regions(
                tcx,
                typing_env,
                ty::EarlyBinder::bind(tcx, tcx.mir_for_ctfe(new_in).clone()),
            );
            let mut constants = Constants(alloc::vec![]);
            constants.visit_body(&body);
            let assertion = constants
                .0
                .into_iter()
                .find(|value| {
                    matches!(value, mir::Const::Unevaluated(unevaluated, _)
                    if tcx.def_path_str(unevaluated.def).ends_with("new_in::{constant#0}"))
                })
                .expect("the exact RawVec inline assertion");
            assert!(matches!(
                assertion
                    .eval(tcx, typing_env, frontend::rustc_span::DUMMY_SP)
                    .expect("RawVec::new_in::{constant#0} folds"),
                ConstValue::ZeroSized
            ));
        }
    });
}

#[test]
fn unknown_memory_bounds_errors_and_mutation_remain_unsupported() {
    with_tcx(|tcx| {
        for name in ["OUT_OF_BOUNDS", "RAW_POINTER", "MUTATION"] {
            let owner = tcx
                .hir_body_owners()
                .find(|owner| {
                    matches!(tcx.def_kind(*owner), DefKind::Const { .. })
                        && tcx.item_name(owner.to_def_id()).as_str() == name
                })
                .expect("fixture constant");
            assert!(tcx.const_eval_poly(owner.to_def_id()).is_err(), "{name}");
        }
        // Refusals are expected diagnostics, so leave this test session clean.
        tcx.dcx().reset_err_count();
    });
}
