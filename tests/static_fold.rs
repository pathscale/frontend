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
impl<T> Index<usize> for [T] {
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
pub const RAW_ADDRESS: usize = { let pair = Pair { byte: 1, word: 47 }; let pointer = &raw const pair.word; unsafe { *pointer } };
pub const RAW_SLICE: *const [usize] = SLICE as *const [usize];
pub const SLICE_REBORROW: &[usize] = &*SLICE;
pub const RAW_SLICE_READ: usize = unsafe { (&*RAW_SLICE)[2] };
pub const UNKNOWN_MEMORY: usize = unsafe { *(8usize as *const usize) };
pub const PROMOTED: (&'static &'static str, &'static &'static [u8; 3]) = (&"abc", &b"abc");
pub const LITERAL_READ: u8 = { let bytes = &b"abc"; bytes[1] };
pub const MANGLE_BUFFER: [u8; 4096] = [0; 4096];
pub const MANGLE_COMPUTED: [u8; 4096usize % 4097] = [0; 4096];
pub fn mangle_argument<T>(_: T) {}
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

fn check_type_id<'tcx>(tcx: TyCtxt<'tcx>, value: ConstValue, expected_ty: ty::Ty<'tcx>) {
    let ConstValue::Indirect { alloc_id, offset } = value else {
        panic!("TypeId allocation expected");
    };
    let allocation = tcx.global_alloc(alloc_id).unwrap_memory();
    let size = tcx.data_layout.pointer_size();
    let count = 16 / size.bytes();
    let hash = tcx.type_id_hash(expected_ty).as_u128();
    for index in 0..count {
        let scalar = allocation
            .inner()
            .read_scalar(
                &tcx,
                alloc_range(offset + Size::from_bytes(index * size.bytes()), size),
                true,
            )
            .expect("TypeId fragment");
        let (provenance, fragment) = scalar.to_pointer(&tcx).into_raw_parts();
        let id = provenance.expect("symbolic type provenance").alloc_id();
        assert!(
            matches!(tcx.global_alloc(id), frontend::rustc_middle::mir::interpret::GlobalAlloc::TypeId { ty } if ty == expected_ty)
        );
        let shift = match tcx.data_layout.endian {
            frontend::rustc_abi::Endian::Little => index,
            frontend::rustc_abi::Endian::Big => count - 1 - index,
        } * size.bits();
        assert_eq!(fragment.bytes() as u128, size.truncate(hash >> shift));
    }
}

struct Constants<'tcx>(alloc::vec::Vec<mir::Const<'tcx>>);
impl<'tcx> Visitor<'tcx> for Constants<'tcx> {
    fn visit_const_operand(&mut self, constant: &mir::ConstOperand<'tcx>, _: mir::Location) {
        self.0.push(constant.const_);
    }
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
            constant(tcx, "RAW_POINTER").try_to_bits(tcx.data_layout.pointer_size()),
            Some(47)
        );
        assert_eq!(
            constant(tcx, "RAW_ADDRESS").try_to_bits(tcx.data_layout.pointer_size()),
            Some(47)
        );
        assert_eq!(
            constant(tcx, "RAW_SLICE_READ").try_to_bits(tcx.data_layout.pointer_size()),
            Some(33)
        );
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
fn literal_and_promoted_references_keep_their_allocation_provenance() {
    with_tcx(|tcx| {
        assert_eq!(constant(tcx, "LITERAL_READ").try_to_bits(Size::from_bytes(1)), Some(98));
        assert!(matches!(constant(tcx, "PROMOTED"), ConstValue::Indirect { .. }));
        let ConstValue::Indirect { alloc_id, offset } = constant(tcx, "RAW_SLICE") else {
            panic!("raw slice descriptor expected");
        };
        assert_eq!(read_word(tcx, alloc_id, offset + tcx.data_layout.pointer_size()), 3);
        let scalar = tcx
            .global_alloc(alloc_id)
            .unwrap_memory()
            .inner()
            .read_scalar(&tcx, alloc_range(offset, tcx.data_layout.pointer_size()), true)
            .expect("raw slice pointer");
        let (provenance, offset) = scalar.to_pointer(&tcx).into_raw_parts();
        assert_eq!(offset.bytes(), 8);
        assert_eq!(read_word(tcx, provenance.expect("provenance").alloc_id(), offset), 11);
        assert!(matches!(constant(tcx, "SLICE_REBORROW"), ConstValue::Indirect { .. }));
    });
}

#[test]
#[ignore = "requires FRONTEND_FOLD_SYSROOT naming the caller's compiled library"]
fn raw_vec_layout_assertion_against_a_named_library() {
    let sysroot = eko::env::var("FRONTEND_FOLD_SYSROOT").expect("explicit test sysroot");
    let source = r#"
        #![feature(sized_type_properties, ptr_alignment_type)]
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
#[ignore = "requires FRONTEND_FOLD_SYSROOT naming the caller's compiled library"]
fn core_constants_against_a_named_library() {
    let sysroot = eko::env::var("FRONTEND_FOLD_SYSROOT").expect("explicit test sysroot");
    let source = r#"
        #![feature(sized_type_properties, ptr_alignment_type)]
        pub const ALIGN: usize = <u64 as core::mem::SizedTypeProperties>::ALIGNMENT.as_usize();
        pub const fn id() -> core::any::TypeId { core::any::TypeId::of::<u64>() }
        pub const TYPE_ID: core::any::TypeId = core::any::TypeId::of::<u64>();
        pub const OTHER_TYPE_ID: core::any::TypeId = core::any::TypeId::of::<u8>();
        pub const INFINITY: f64 = f64::INFINITY;
        pub const COUNT: u32 = 0xf0u8.count_ones();
    "#;
    with_source_tcx(source, Some(&sysroot), |tcx| {
        assert_eq!(constant(tcx, "ALIGN").try_to_bits(tcx.data_layout.pointer_size()), Some(8));
        let id = tcx
            .hir_body_owners()
            .find(|&owner| {
                tcx.def_kind(owner) == DefKind::Fn
                    && tcx.item_name(owner.to_def_id()).as_str() == "id"
            })
            .expect("TypeId caller");
        let mut constants = Constants(alloc::vec![]);
        constants.visit_body(tcx.mir_for_ctfe(id));
        let (of, args) = constants
            .0
            .iter()
            .find_map(|constant| {
                if let ty::FnDef(def, args) = *constant.ty().kind()
                    && tcx.item_name(def).as_str() == "of"
                {
                    Some((def, args.skip_binder()))
                } else {
                    None
                }
            })
            .expect("TypeId::of call");
        let instance = ty::Instance::new_raw(of, args);
        let typing_env = ty::TypingEnv::fully_monomorphized();
        let body = instance.instantiate_mir_and_normalize_erasing_regions(
            tcx,
            typing_env,
            ty::EarlyBinder::bind(tcx, tcx.mir_for_ctfe(of).clone()),
        );
        let mut constants = Constants(alloc::vec![]);
        constants.visit_body(&body);
        let assertion = constants
            .0
            .into_iter()
            .find(|value| {
                matches!(value, mir::Const::Unevaluated(unevaluated, _)
                if tcx.def_path_str(unevaluated.def).ends_with("TypeId::of::{constant#0}"))
            })
            .expect("exact TypeId inline constant");
        check_type_id(
            tcx,
            assertion
                .eval(tcx, typing_env, frontend::rustc_span::DUMMY_SP)
                .expect("TypeId::of::{constant#0} folds"),
            tcx.types.u64,
        );
        check_type_id(tcx, constant(tcx, "TYPE_ID"), tcx.types.u64);
        check_type_id(tcx, constant(tcx, "OTHER_TYPE_ID"), tcx.types.u8);
        assert_eq!(constant(tcx, "COUNT").try_to_bits(Size::from_bytes(4)), Some(4));
        assert_eq!(
            constant(tcx, "INFINITY").try_to_bits(Size::from_bytes(8)),
            Some(f64::INFINITY.to_bits().into())
        );
    });
}

#[test]
fn an_unselected_unreachable_block_does_not_reject_a_constant() {
    with_tcx(|tcx| {
        let choose = tcx
            .hir_body_owners()
            .find(|&owner| {
                tcx.def_kind(owner) == DefKind::Fn
                    && tcx.item_name(owner.to_def_id()).as_str() == "choose"
            })
            .expect("enum match function");
        assert!(
            tcx.mir_for_ctfe(choose).basic_blocks.iter().any(|block| {
                matches!(block.terminator().kind, mir::TerminatorKind::Unreachable)
            })
        );
        let pointer =
            constant(tcx, "VARIANT").try_to_scalar().expect("folded live branch").to_pointer(&tcx);
        let (provenance, offset) = pointer.into_raw_parts();
        assert_eq!(read_word(tcx, provenance.expect("provenance").alloc_id(), offset), 73);
    });
}

#[test]
#[ignore = "requires FRONTEND_FOLD_SYSROOT naming the caller's compiled library"]
fn ieee_float_operations_against_a_named_library() {
    let sysroot = eko::env::var("FRONTEND_FOLD_SYSROOT").expect("explicit test sysroot");
    for (float, size) in [("f32", Size::from_bytes(4)), ("f64", Size::from_bytes(8))] {
        let source = alloc::format!(
            r#"
            pub const INF: {float} = 1.0 / 0.0;
            pub const NEG_INF: {float} = -INF;
            pub const NEG: {float} = -ADD;
            pub const ADD: {float} = 1.5 + 2.0;
            pub const SUB: {float} = 1.5 - 2.0;
            pub const MUL: {float} = 1.5 * 2.0;
            pub const DIV: {float} = 1.5 / 2.0;
            pub const REM: {float} = 5.5 % 2.0;
            pub const ZERO: {float} = 0.0;
            pub const NEG_ZERO: {float} = -ZERO;
            pub const NAN: {float} = 0.0 / 0.0;
            pub const EQ: bool = 1.0_{float} == 1.0;
            pub const NE: bool = 1.0_{float} != 2.0;
            pub const LT: bool = 1.0_{float} < 2.0;
            pub const LE: bool = 1.0_{float} <= 1.0;
            pub const GT: bool = 2.0_{float} > 1.0;
            pub const GE: bool = 1.0_{float} >= 1.0;
            pub const NAN_EQ: bool = NAN == NAN;
            pub const NAN_NE: bool = NAN != NAN;
            pub const NAN_LT: bool = NAN < 1.0;
            pub const NAN_LE: bool = NAN <= 1.0;
            pub const NAN_GT: bool = NAN > 1.0;
            pub const NAN_GE: bool = NAN >= 1.0;
            pub const ZERO_EQ: bool = NEG_ZERO == 0.0;
        "#
        );
        with_source_tcx(&source, Some(&sysroot), |tcx| {
            for (name, expected) in [
                ("INF", f64::INFINITY),
                ("NEG_INF", f64::NEG_INFINITY),
                ("NEG", -3.5),
                ("ADD", 3.5),
                ("SUB", -0.5),
                ("MUL", 3.0),
                ("DIV", 0.75),
                ("REM", 1.5),
                ("NEG_ZERO", -0.0),
            ] {
                let bits = if size.bytes() == 4 {
                    (expected as f32).to_bits() as u128
                } else {
                    expected.to_bits() as u128
                };
                assert_eq!(constant(tcx, name).try_to_bits(size), Some(bits), "{float} {name}");
            }
            let nan = constant(tcx, "NAN").try_to_bits(size).expect("NaN scalar");
            assert!(if size.bytes() == 4 {
                f32::from_bits(nan as u32).is_nan()
            } else {
                f64::from_bits(nan as u64).is_nan()
            });
            for (name, expected) in [
                ("EQ", true),
                ("NE", true),
                ("LT", true),
                ("LE", true),
                ("GT", true),
                ("GE", true),
                ("ZERO_EQ", true),
                ("NAN_EQ", false),
                ("NAN_NE", true),
                ("NAN_LT", false),
                ("NAN_LE", false),
                ("NAN_GT", false),
                ("NAN_GE", false),
            ] {
                assert_eq!(
                    constant(tcx, name).try_to_bits(Size::from_bytes(1)),
                    Some(expected as u128),
                    "{float} {name}"
                );
            }
        });
    }
}

#[test]
#[ignore = "requires FRONTEND_FOLD_SYSROOT naming the caller's compiled library"]
fn raw_pointer_layout_and_promoted_references_against_a_named_library() {
    let sysroot = eko::env::var("FRONTEND_FOLD_SYSROOT").expect("explicit test sysroot");
    let source = r#"
        #![allow(arithmetic_overflow)]
        pub const VALUE: [u32; 3] = [11, 22, 33];
        pub const LAYOUT_SUM: usize = core::mem::size_of_val(&VALUE) + core::mem::align_of_val(&VALUE);
        pub const SLICE: &[u32] = &VALUE;
        pub const SLICE_LAYOUT: usize = core::mem::size_of_val(SLICE) + core::mem::align_of_val(SLICE);
        pub const RAW: *const [u32] = SLICE as *const [u32];
        pub const RAW_SIZE: usize = unsafe { core::mem::size_of_val_raw(RAW) };
        pub const RAW_ALIGN: usize = unsafe { core::mem::align_of_val_raw(RAW) };
        pub const TEXT: &str = "hello";
        pub const TEXT_SIZE: usize = core::mem::size_of_val(TEXT);
        pub const RAW_TEXT_SIZE: usize = unsafe { core::mem::size_of_val_raw(TEXT as *const str) };
        pub const BYTES: u8 = { let bytes = &b"abc"; bytes[1] };
        pub const NESTED: usize = { let text = &"abc"; text.len() };
        pub const INTEGER: i8 = { let value = 4i8; (value - 1) * 2 + 1 };
        pub const OVERFLOW: u8 = { let value = 255u8; value + 1 };
        pub fn promoted() { assert_eq!(b"abc", b"abc"); assert_eq!("abc", "abc"); }
    "#;
    let source = alloc::format!("#![feature(layout_for_ptr)]\n{source}");
    with_source_tcx(&source, Some(&sysroot), |tcx| {
        for (name, expected) in [
            ("LAYOUT_SUM", 16),
            ("SLICE_LAYOUT", 16),
            ("RAW_SIZE", 12),
            ("RAW_ALIGN", 4),
            ("TEXT_SIZE", 5),
            ("RAW_TEXT_SIZE", 5),
            ("NESTED", 3),
        ] {
            assert_eq!(
                constant(tcx, name).try_to_bits(tcx.data_layout.pointer_size()),
                Some(expected),
                "{name}"
            );
        }
        assert_eq!(constant(tcx, "BYTES").try_to_bits(Size::from_bytes(1)), Some(98));
        assert_eq!(constant(tcx, "INTEGER").try_to_bits(Size::from_bytes(1)), Some(7));
        let owner = tcx
            .hir_body_owners()
            .find(|&owner| {
                tcx.def_kind(owner) == DefKind::Fn
                    && tcx.item_name(owner.to_def_id()).as_str() == "promoted"
            })
            .expect("promoted function");
        let instance = ty::Instance::mono(tcx, owner.to_def_id());
        for (promoted, _) in tcx.promoted_mir(owner).iter_enumerated() {
            tcx.const_eval_global_id(
                ty::TypingEnv::fully_monomorphized(),
                frontend::rustc_middle::mir::interpret::GlobalId {
                    instance,
                    promoted: Some(promoted),
                },
                frontend::rustc_span::DUMMY_SP,
            )
            .expect("promoted reference folds");
        }
        let overflow = tcx
            .hir_body_owners()
            .find(|&owner| {
                matches!(tcx.def_kind(owner), DefKind::Const { .. })
                    && tcx.item_name(owner.to_def_id()).as_str() == "OVERFLOW"
            })
            .expect("overflow fixture");
        assert!(tcx.const_eval_poly(overflow.to_def_id()).is_err());
        tcx.dcx().reset_err_count();
    });
}

#[test]
fn unknown_memory_bounds_errors_and_mutation_remain_unsupported() {
    with_tcx(|tcx| {
        for name in ["OUT_OF_BOUNDS", "UNKNOWN_MEMORY", "MUTATION"] {
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

#[test]
fn v0_symbol_mangling_evaluates_anonymous_array_lengths() {
    with_tcx(|tcx| {
        let function = tcx
            .hir_body_owners()
            .find(|&owner| {
                tcx.def_kind(owner) == DefKind::Fn
                    && tcx.item_name(owner.to_def_id()).as_str() == "mangle_argument"
            })
            .expect("generic function")
            .to_def_id();
        for name in ["MANGLE_BUFFER", "MANGLE_COMPUTED"] {
            let length = tcx
                .hir_body_owners()
                .find(|&owner| {
                    tcx.def_kind(owner) == DefKind::AnonConst
                        && tcx.item_name(tcx.parent(owner.to_def_id())).as_str() == name
                })
                .expect("anonymous length")
                .to_def_id();
            let alias = ty::Const::new_alias(
                tcx,
                ty::IsRigid::No,
                ty::AliasConst::new(
                    tcx,
                    ty::AliasConstKind::new_from_def_id(tcx, length),
                    ty::GenericArgs::empty(),
                ),
            );
            let array = ty::Ty::new_array_with_const_len(tcx, tcx.types.u8, alias);
            let instance = ty::Instance::new_raw(function, tcx.mk_args(&[array.into()]));
            let symbol = tcx.symbol_name(instance).name;
            assert!(rustc_demangle::try_demangle(symbol).is_ok(), "{symbol}");
            assert!(symbol.contains("Ahj1000_"), "{symbol}");
            let evaluated = ty::Ty::new_array(tcx, tcx.types.u8, 4096);
            let instance = ty::Instance::new_raw(function, tcx.mk_args(&[evaluated.into()]));
            assert_eq!(symbol, tcx.symbol_name(instance).name);
            let index = tcx
                .hir_body_owners()
                .find(|&owner| {
                    if tcx.def_kind(owner) != DefKind::AssocFn
                        || tcx.item_name(owner.to_def_id()).as_str() != "index"
                    {
                        return false;
                    }
                    let parent = tcx.parent(owner.to_def_id());
                    matches!(tcx.def_kind(parent), DefKind::Impl { .. })
                        && matches!(
                            tcx.type_of(parent).instantiate_identity().skip_norm_wip().kind(),
                            ty::Array(..)
                        )
                })
                .expect("array Index impl")
                .to_def_id();
            let instance =
                ty::Instance::new_raw(index, tcx.mk_args(&[tcx.types.u8.into(), alias.into()]));
            let symbol = tcx.symbol_name(instance).name;
            assert!(rustc_demangle::try_demangle(symbol).is_ok(), "{symbol}");
            assert!(symbol.contains("Ahj1000_"), "{symbol}");
        }
    });
}
