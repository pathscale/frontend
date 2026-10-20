#![no_std]

extern crate alloc;

use alloc::sync::Arc;
use core::sync::atomic::AtomicBool;

use frontend::frontend_facts::Setup;
use frontend::rustc_abi::Size;
use frontend::rustc_hir::def::DefKind;
use frontend::rustc_hir::def_id::{CRATE_DEF_INDEX, DefId};
use frontend::rustc_interface::{Config, create_and_enter_global_ctxt, parse, run_compiler};
use frontend::rustc_middle::mir::interpret::{AllocId, GlobalAlloc, GlobalId, alloc_range};
use frontend::rustc_middle::mir::visit::Visitor;
use frontend::rustc_middle::mir::{self, ConstValue};
use frontend::rustc_middle::ty::{self, TyCtxt};
use frontend::rustc_session::config::Input;
use frontend::rustc_span::FileName;

#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

const SOURCE: &str = r#"
#![feature(no_core, lang_items, auto_traits)]
#![allow(internal_features, dead_code, unconditional_panic, static_mut_refs)]
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
#[lang = "not"] pub trait Not {
    type Output;
    fn not(self) -> Self::Output;
}
impl Not for usize { type Output = usize; fn not(self) -> usize { 0 } }
#[lang = "neg"] pub trait Neg {
    type Output;
    fn neg(self) -> Self::Output;
}
impl Neg for i8 { type Output = i8; fn neg(self) -> i8 { loop {} } }
impl Neg for i32 { type Output = i32; fn neg(self) -> i32 { loop {} } }

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
pub const MAX_ADDRESS: *mut u8 = !0usize as *mut u8;
pub const NULL_ADDRESS: *const usize = 0 as *const usize;
pub const RECAST_ADDRESS: *const usize = MAX_ADDRESS as *const usize;
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
    let mut opts =
        Setup { sysroot, ..Setup::plain("static_fold") }.options(&input).expect("session options");
    // These sessions request no object output. Still collect callers when a
    // regression explicitly asks for monomorphization below.
    opts.cg.link_dead_code = Some(true);
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
#[ignore = "requires FRONTEND_FOLD_SYSROOT naming the caller's compiled library"]
fn static_aggregates_atomics_and_function_pointers_against_a_named_library() {
    let sysroot = eko::env::var("FRONTEND_FOLD_SYSROOT").expect("explicit test sysroot");
    let source = r#"
        #![no_std]
        use core::cell::UnsafeCell;
        use core::sync::atomic::{AtomicU64, AtomicUsize};
        pub static COUNTER: AtomicU64 = AtomicU64::new(0);
        pub static COUNT: AtomicUsize = AtomicUsize::new(0);
        pub fn answer() -> i64 { 73 }
        pub fn generic<T>() -> i64 { 91 }
        #[repr(C)]
        pub struct State {
            prefix: u8,
            count: AtomicUsize,
            cells: [UnsafeCell<usize>; 2],
            pair: (UnsafeCell<usize>, fn() -> i64),
            init: fn() -> i64,
            raw: *const (),
            reference: &'static usize,
            slice: &'static [usize],
            optional: Option<fn() -> i64>,
            empty: (),
        }
        unsafe impl Sync for State {}
        pub static STATE: State = State {
            prefix: 5,
            count: AtomicUsize::new(17),
            cells: [UnsafeCell::new(23), UnsafeCell::new(29)],
            pair: (UnsafeCell::new(31), generic::<u8>),
            init: answer,
            raw: answer as fn() -> i64 as *const (),
            reference: &41,
            slice: &[43, 47],
            optional: Some(answer),
            empty: (),
        };
        pub struct LocalKey<T: 'static> { count: AtomicUsize, init: fn() -> T }
        unsafe impl<T> Sync for LocalKey<T> {}
        impl<T> LocalKey<T> {
            pub const fn new(init: fn() -> T) -> Self {
                Self { count: AtomicUsize::new(0), init }
            }
        }
        pub static TLS: LocalKey<i64> = LocalKey::new(answer);
        pub static SCOPED: LocalKey<core::cell::Cell<*const i64>> =
            LocalKey::new(|| core::cell::Cell::new(core::ptr::null()));
        pub static mut ARRAY: [fn() -> i64; 2] = [answer; 2];
        pub static mut UNSAFE: unsafe fn() -> i64 = answer as fn() -> i64;
        pub static ROUNDTRIP: Option<fn() -> i64> = unsafe { core::mem::transmute(Some(answer as fn() -> i64)) };
        pub static NONE: Option<fn() -> i64> = unsafe { core::mem::transmute(None::<fn() -> i64>) };
        pub static FUNCTION_BACK: fn() -> i64 = unsafe { core::mem::transmute(answer as fn() -> i64 as *const ()) };
        pub static NESTED: &&fn() -> i64 = &&(answer as fn() -> i64);
        #[repr(C)]
        pub enum Direct { First(usize), Second(u8, usize) }
        pub static DIRECT: Direct = unsafe { core::mem::transmute(Direct::Second(2, 73)) };
        pub const FUNCTIONS: (fn() -> i64, fn() -> i64) = (answer, generic::<u8>);
        pub const EMPTY: [u8; 0] = [];
    "#;
    with_source_tcx(source, Some(&sysroot), |tcx| {
        use frontend::rustc_middle::ty::layout::LayoutCx;
        let env = ty::TypingEnv::fully_monomorphized();
        let cx = LayoutCx::new(tcx, env);
        let static_def = |name: &str| {
            tcx.hir_body_owners()
                .find(|&owner| {
                    matches!(tcx.def_kind(owner), DefKind::Static { .. })
                        && tcx.item_name(owner.to_def_id()).as_str() == name
                })
                .expect("fixture static")
        };
        let word = tcx.data_layout.pointer_size();
        for name in ["COUNTER", "COUNT"] {
            let allocation = tcx.eval_static_initializer(static_def(name)).expect("atomic folds");
            assert!(allocation.inner().mutability.is_mut(), "{name}");
            let size = if name == "COUNTER" { Size::from_bytes(8) } else { word };
            assert_eq!(
                allocation
                    .inner()
                    .read_scalar(&tcx, alloc_range(Size::ZERO, size), false)
                    .unwrap()
                    .try_to_scalar_int()
                    .unwrap()
                    .to_bits(size),
                0,
                "{name}"
            );
        }
        let state = static_def("STATE");
        let allocation = tcx.eval_static_initializer(state).expect("nested aggregate folds");
        assert!(allocation.inner().mutability.is_mut());
        let layout = tcx
            .layout_of(
                env.as_query_input(tcx.type_of(state).instantiate_identity().skip_norm_wip()),
            )
            .unwrap();
        let offset = |path: &[usize]| {
            let mut layout = layout;
            let mut offset = Size::ZERO;
            for &index in path {
                offset += layout.fields.offset(index);
                layout = layout.field(&cx, index);
            }
            offset
        };
        for (path, expected) in
            [(&[1][..], 17), (&[2, 0][..], 23), (&[2, 1][..], 29), (&[3, 0][..], 31)]
        {
            assert_eq!(
                allocation
                    .inner()
                    .read_scalar(&tcx, alloc_range(offset(path), word), false)
                    .unwrap()
                    .try_to_scalar_int()
                    .unwrap()
                    .to_bits(word),
                expected
            );
        }
        let function_at =
            |allocation: frontend::rustc_middle::mir::interpret::ConstAllocation<'_>,
             offset,
             expected| {
                let scalar =
                    allocation.inner().read_scalar(&tcx, alloc_range(offset, word), true).unwrap();
                let (provenance, address) = scalar.to_pointer(&tcx).into_raw_parts();
                assert_eq!(address, Size::ZERO);
                let GlobalAlloc::Function { instance } =
                    tcx.global_alloc(provenance.unwrap().alloc_id())
                else {
                    panic!("function provenance expected");
                };
                if let Some(expected) = expected {
                    assert_eq!(tcx.item_name(instance.def_id()).as_str(), expected);
                } else {
                    assert!(matches!(
                        instance.def,
                        ty::InstanceKind::Shim(ty::ShimKind::ClosureOnce { .. })
                    ));
                }
                instance
            };
        let generic = function_at(allocation, offset(&[3, 1]), Some("generic"));
        assert_eq!(generic.args.type_at(0), tcx.types.u8);
        for index in [4, 5, 8] {
            function_at(allocation, offset(&[index]), Some("answer"));
        }
        for (index, expected) in [(6, 41), (7, 43)] {
            let scalar = allocation
                .inner()
                .read_scalar(&tcx, alloc_range(offset(&[index]), word), true)
                .unwrap();
            let (provenance, address) = scalar.to_pointer(&tcx).into_raw_parts();
            assert_eq!(read_word(tcx, provenance.unwrap().alloc_id(), address), expected);
        }
        assert_eq!(
            allocation
                .inner()
                .read_scalar(&tcx, alloc_range(offset(&[7]) + word, word), false)
                .unwrap()
                .try_to_scalar_int()
                .unwrap()
                .to_bits(word),
            2
        );
        for (name, expected) in [("TLS", Some("answer")), ("SCOPED", None)] {
            let def = static_def(name);
            let allocation = tcx.eval_static_initializer(def).expect("local key folds");
            assert!(allocation.inner().mutability.is_mut());
            let layout = tcx
                .layout_of(
                    env.as_query_input(tcx.type_of(def).instantiate_identity().skip_norm_wip()),
                )
                .unwrap();
            function_at(allocation, layout.fields.offset(1), expected);
        }
        let array = tcx.eval_static_initializer(static_def("ARRAY")).unwrap();
        for index in 0..2 {
            function_at(array, word * index, Some("answer"));
        }
        function_at(
            tcx.eval_static_initializer(static_def("UNSAFE")).unwrap(),
            Size::ZERO,
            Some("answer"),
        );
        for name in ["ROUNDTRIP", "FUNCTION_BACK"] {
            function_at(
                tcx.eval_static_initializer(static_def(name)).unwrap(),
                Size::ZERO,
                Some("answer"),
            );
        }
        let none = tcx.eval_static_initializer(static_def("NONE")).unwrap();
        assert_eq!(
            none.inner()
                .read_scalar(&tcx, alloc_range(Size::ZERO, word), false)
                .unwrap()
                .try_to_scalar_int()
                .unwrap()
                .to_bits(word),
            0
        );
        let direct = tcx.eval_static_initializer(static_def("DIRECT")).unwrap();
        assert_eq!(
            direct
                .inner()
                .read_scalar(&tcx, alloc_range(word * 2, word), false)
                .unwrap()
                .try_to_scalar_int()
                .unwrap()
                .to_bits(word),
            73
        );
        let mut nested = tcx.eval_static_initializer(static_def("NESTED")).unwrap();
        for _ in 0..2 {
            let scalar =
                nested.inner().read_scalar(&tcx, alloc_range(Size::ZERO, word), true).unwrap();
            let (provenance, offset) = scalar.to_pointer(&tcx).into_raw_parts();
            assert_eq!(offset, Size::ZERO);
            nested = tcx.global_alloc(provenance.unwrap().alloc_id()).unwrap_memory();
        }
        function_at(nested, Size::ZERO, Some("answer"));
        for name in ["FUNCTIONS", "EMPTY"] {
            let def = tcx
                .hir_body_owners()
                .find(|&owner| {
                    matches!(tcx.def_kind(owner), DefKind::Const { .. })
                        && tcx.item_name(owner.to_def_id()).as_str() == name
                })
                .unwrap();
            let result =
                tcx.const_eval_poly_to_alloc(def.to_def_id()).expect("raw allocation query folds");
            let allocation = tcx.global_alloc(result.alloc_id).unwrap_memory();
            if name == "FUNCTIONS" {
                function_at(allocation, Size::ZERO, Some("answer"));
                function_at(allocation, word, Some("generic"));
            } else {
                assert_eq!(allocation.inner().size(), Size::ZERO);
            }
        }
        let items = tcx.collect_and_partition_mono_items(());
        assert!(
            items.codegen_units.iter().flat_map(|unit| unit.items().keys()).any(|item| {
                matches!(item, frontend::rustc_middle::mono::MonoItem::Fn(instance)
                if matches!(instance.def, ty::InstanceKind::Shim(ty::ShimKind::ClosureOnce { .. })))
            }),
            "the closure function pointer must keep its callable shim reachable"
        );
        assert!(tcx.dcx().has_errors().is_none());
    });
}

#[test]
#[ignore = "requires FRONTEND_FOLD_SYSROOT naming the caller's compiled library"]
fn transmute_rejects_uninitialized_padding_and_uninhabited_values() {
    let sysroot = eko::env::var("FRONTEND_FOLD_SYSROOT").expect("explicit test sysroot");
    let source = r#"
        #![no_std]
        #[repr(C)]
        pub struct Padded { byte: u8, word: u32 }
        pub enum Void {}
        pub const PADDING: u64 = unsafe { core::mem::transmute(Padded { byte: 1, word: 2 }) };
        pub const VOID: Void = unsafe { core::mem::transmute(()) };
    "#;
    with_source_tcx(source, Some(&sysroot), |tcx| {
        for name in ["PADDING", "VOID"] {
            let def = tcx
                .hir_body_owners()
                .find(|&owner| {
                    matches!(tcx.def_kind(owner), DefKind::Const { .. })
                        && tcx.item_name(owner.to_def_id()).as_str() == name
                })
                .unwrap();
            assert!(tcx.const_eval_poly(def.to_def_id()).is_err(), "{name}");
        }
        tcx.dcx().reset_err_count();
    });
}

#[test]
fn aggregate_valtrees_use_layout_offsets_and_reference_provenance() {
    with_tcx(|tcx| {
        let env = ty::TypingEnv::fully_monomorphized();
        let word = tcx.data_layout.pointer_size();
        let leaf = |bits: u128, ty| {
            let size = tcx.layout_of(env.as_query_input(ty)).unwrap().size;
            ty::Const::new_value(
                tcx,
                ty::ValTree::from_scalar_int(
                    tcx,
                    ty::ScalarInt::try_from_uint(bits, size).unwrap(),
                ),
                ty,
            )
        };
        let tree =
            ty::ValTree::from_branches(tcx, [leaf(11, tcx.types.usize), leaf(22, tcx.types.usize)]);
        let scalar_reference = ty::Ty::new_imm_ref(tcx, tcx.lifetimes.re_static, tcx.types.usize);
        let scalar = tcx
            .valtree_to_const_val(ty::Value {
                ty: scalar_reference,
                valtree: leaf(37, tcx.types.usize).to_value().valtree,
            })
            .unwrap();
        let (provenance, address) =
            scalar.try_to_scalar().unwrap().to_pointer(&tcx).into_raw_parts();
        assert_eq!(read_word(tcx, provenance.unwrap().alloc_id(), address), 37);
        let array_ty = ty::Ty::new_array(tcx, tcx.types.usize, 2);
        let array = tcx.valtree_to_const_val(ty::Value { ty: array_ty, valtree: tree }).unwrap();
        let ConstValue::Indirect { alloc_id, offset } = array else { panic!("array allocation") };
        assert_eq!(read_word(tcx, alloc_id, offset), 11);
        assert_eq!(read_word(tcx, alloc_id, offset + word), 22);
        let reference = ty::Ty::new_imm_ref(tcx, tcx.lifetimes.re_static, array_ty);
        let value = tcx.valtree_to_const_val(ty::Value { ty: reference, valtree: tree }).unwrap();
        let (provenance, offset) = value.try_to_scalar().unwrap().to_pointer(&tcx).into_raw_parts();
        assert_eq!(read_word(tcx, provenance.unwrap().alloc_id(), offset + word), 22);
        let slice = ty::Ty::new_imm_ref(
            tcx,
            tcx.lifetimes.re_static,
            ty::Ty::new_slice(tcx, tcx.types.usize),
        );
        let value = tcx.valtree_to_const_val(ty::Value { ty: slice, valtree: tree }).unwrap();
        let ConstValue::Slice { alloc_id, meta } = value else { panic!("slice allocation") };
        assert_eq!(meta, 2);
        assert_eq!(read_word(tcx, alloc_id, word), 22);
        for (name, tree) in [
            (
                "PAIR",
                ty::ValTree::from_branches(tcx, [leaf(5, tcx.types.u8), leaf(47, tcx.types.usize)]),
            ),
            (
                "ENUM",
                ty::ValTree::from_branches(
                    tcx,
                    [leaf(1, tcx.types.u32), leaf(2, tcx.types.u8), leaf(73, tcx.types.usize)],
                ),
            ),
        ] {
            let def = tcx
                .hir_body_owners()
                .find(|&owner| {
                    matches!(tcx.def_kind(owner), DefKind::Const { .. })
                        && tcx.item_name(owner.to_def_id()).as_str() == name
                })
                .unwrap();
            let ty = tcx.type_of(def).instantiate_identity().skip_norm_wip();
            let value = tcx.valtree_to_const_val(ty::Value { ty, valtree: tree }).unwrap();
            let ConstValue::Indirect { alloc_id, offset } = value else {
                panic!("aggregate allocation")
            };
            assert_eq!(
                read_word(tcx, alloc_id, offset + word * if name == "ENUM" { 2 } else { 1 }),
                if name == "ENUM" { 73 } else { 47 }
            );
        }
    });
}

#[test]
#[ignore = "requires FRONTEND_FOLD_SYSROOT naming the caller's compiled library"]
fn pthread_initializer_aggregates_against_a_named_library() {
    let sysroot = eko::env::var("FRONTEND_FOLD_SYSROOT").expect("explicit test sysroot");
    let source = r#"
        #![no_std]
        #![feature(rustc_private)]
        extern crate libc;
        use core::cell::UnsafeCell;
        pub struct Mutex<T> { raw: UnsafeCell<libc::pthread_mutex_t>, value: UnsafeCell<T> }
        unsafe impl<T: Send> Sync for Mutex<T> {}
        impl<T> Mutex<T> {
            pub const fn new(value: T) -> Self {
                Self { raw: UnsafeCell::new(libc::PTHREAD_MUTEX_INITIALIZER), value: UnsafeCell::new(value) }
            }
        }
        pub struct Condvar(UnsafeCell<libc::pthread_cond_t>);
        unsafe impl Sync for Condvar {}
        pub struct RwLock<T> { raw: UnsafeCell<libc::pthread_rwlock_t>, value: UnsafeCell<T> }
        unsafe impl<T: Send + Sync> Sync for RwLock<T> {}
        pub static MTX: Mutex<usize> = Mutex::new(59);
        pub static CV: Condvar = Condvar(UnsafeCell::new(libc::PTHREAD_COND_INITIALIZER));
        pub static RW: RwLock<usize> = RwLock {
            raw: UnsafeCell::new(libc::PTHREAD_RWLOCK_INITIALIZER), value: UnsafeCell::new(61)
        };
    "#;
    with_source_tcx(source, Some(&sysroot), |tcx| {
        let env = ty::TypingEnv::fully_monomorphized();
        for (name, expected) in [("MTX", Some(59)), ("CV", None), ("RW", Some(61))] {
            let def = tcx
                .hir_body_owners()
                .find(|&owner| {
                    matches!(tcx.def_kind(owner), DefKind::Static { .. })
                        && tcx.item_name(owner.to_def_id()).as_str() == name
                })
                .unwrap();
            let allocation = tcx.eval_static_initializer(def).expect("pthread initializer folds");
            assert!(allocation.inner().mutability.is_mut());
            if let Some(expected) = expected {
                let layout = tcx
                    .layout_of(
                        env.as_query_input(tcx.type_of(def).instantiate_identity().skip_norm_wip()),
                    )
                    .unwrap();
                let word = tcx.data_layout.pointer_size();
                assert_eq!(
                    allocation
                        .inner()
                        .read_scalar(&tcx, alloc_range(layout.fields.offset(1), word), false)
                        .unwrap()
                        .try_to_scalar_int()
                        .unwrap()
                        .to_bits(word),
                    expected
                );
            }
        }
    });
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

fn check_ordering_discriminants<'tcx>(tcx: TyCtxt<'tcx>, def: ty::AdtDef<'tcx>) {
    assert_eq!(def.variants().len(), 3);
    for ((index, discr), expected) in def.discriminants(tcx).zip([-1i128, 0, 1]) {
        let ty::VariantDiscr::Explicit(expr) = def.variant(index).discr else {
            panic!("explicit discriminant expected");
        };
        let explicit = def.eval_explicit_discr(tcx, expr).expect("explicit discriminant folds");
        assert_eq!((explicit.val, explicit.ty), (discr.val, discr.ty));
        let size = tcx
            .layout_of(ty::TypingEnv::fully_monomorphized().as_query_input(discr.ty))
            .expect("discriminant layout")
            .size;
        assert_eq!(size.sign_extend(discr.val), expected);
        let direct = def.discriminant_for_variant(tcx, index);
        assert_eq!((direct.val, direct.ty), (discr.val, discr.ty));
    }
}

#[test]
fn explicit_signed_discriminants_fold_without_a_library() {
    let source = alloc::format!(
        r#"{SOURCE}
        #[repr(i8)] pub enum Ordering {{ Less = -1, Equal = 0, Greater = 1 }}
        pub const LESS: Ordering = Ordering::Less;
        pub const EQUAL: Ordering = Ordering::Equal;
        pub const GREATER: Ordering = Ordering::Greater;
        "#
    );
    with_source_tcx(&source, None, |tcx| {
        let owner = tcx
            .hir_body_owners()
            .find(|&owner| {
                matches!(tcx.def_kind(owner), DefKind::Const { .. })
                    && tcx.item_name(owner.to_def_id()).as_str() == "LESS"
            })
            .expect("ordering constant");
        let ty::Adt(def, _) = *tcx.type_of(owner).instantiate_identity().skip_norm_wip().kind()
        else {
            panic!("ordering type expected");
        };
        check_ordering_discriminants(tcx, def);
        for (name, bits) in [("LESS", 255), ("EQUAL", 0), ("GREATER", 1)] {
            assert_eq!(constant(tcx, name).try_to_bits(Size::from_bytes(1)), Some(bits), "{name}");
        }
    });
}

#[test]
#[ignore = "requires FRONTEND_FOLD_SYSROOT naming the caller's compiled library"]
fn core_ordering_explicit_discriminants_against_a_named_library() {
    let sysroot = eko::env::var("FRONTEND_FOLD_SYSROOT").expect("explicit test sysroot");
    let source = r#"
        pub const LESS: core::cmp::Ordering = core::cmp::Ordering::Less;
        pub const EQUAL: core::cmp::Ordering = core::cmp::Ordering::Equal;
        pub const GREATER: core::cmp::Ordering = core::cmp::Ordering::Greater;
    "#;
    with_source_tcx(source, Some(&sysroot), |tcx| {
        let owner = tcx
            .hir_body_owners()
            .find(|&owner| {
                matches!(tcx.def_kind(owner), DefKind::Const { .. })
                    && tcx.item_name(owner.to_def_id()).as_str() == "LESS"
            })
            .expect("ordering constant");
        let ty::Adt(def, _) = *tcx.type_of(owner).instantiate_identity().skip_norm_wip().kind()
        else {
            panic!("core ordering type expected");
        };
        assert!(!def.did().is_local());
        check_ordering_discriminants(tcx, def);
        for (name, bits) in [("LESS", 255), ("EQUAL", 0), ("GREATER", 1)] {
            assert_eq!(constant(tcx, name).try_to_bits(Size::from_bytes(1)), Some(bits), "{name}");
        }
    });
}

fn static_pointer_target(tcx: TyCtxt<'_>, mut value: ConstValue) -> DefId {
    for _ in 0..4 {
        let pointer = value.try_to_scalar().expect("pointer scalar").to_pointer(&tcx);
        let (provenance, offset) = pointer.into_raw_parts();
        assert_eq!(offset, Size::ZERO);
        match tcx.global_alloc(provenance.expect("static pointer provenance").alloc_id()) {
            GlobalAlloc::Static(def) => return def,
            GlobalAlloc::Memory(memory) => {
                value = ConstValue::Scalar(
                    memory
                        .inner()
                        .read_scalar(
                            &tcx,
                            alloc_range(offset, tcx.data_layout.pointer_size()),
                            true,
                        )
                        .expect("nested pointer"),
                );
            }
            _ => panic!("static or promoted pointer storage expected"),
        }
    }
    panic!("static pointer was not reached");
}

#[test]
fn static_addresses_and_initializers_preserve_identity_and_mutability() {
    let source = alloc::format!(
        r#"{SOURCE}
        pub static VALUE: i32 = -1;
        pub static OTHER: i32 = -1;
        pub static mut WRITE_END: i32 = -1;
        pub const STATIC_REF: &i32 = &VALUE;
        pub const OTHER_REF: &i32 = &OTHER;
        pub const STATIC_RAW: *const i32 = &raw const VALUE;
        pub const MUT_STATIC_RAW: *mut i32 = &raw mut WRITE_END;
        pub const MUT_STATIC_CONST_RAW: *const i32 = &raw const WRITE_END;
        pub const MUT_STATIC_REF: &i32 = unsafe {{ &WRITE_END }};
        pub const REBORROW_STATIC: &i32 = &*STATIC_REF;
        pub static mut SAVED_ADDRESS: *mut i32 = &raw mut WRITE_END;
        pub static mut SELF_ADDRESS: *mut *mut () = &raw mut SELF_ADDRESS as *mut *mut ();
        fn accept<T>(_: T) {{}}
        pub fn promoted_statics() {{
            accept(&STATIC_REF);
            accept(&STATIC_RAW);
            accept(&MUT_STATIC_RAW);
            accept(&MUT_STATIC_REF);
        }}
        "#
    );
    with_source_tcx(&source, None, |tcx| {
        let static_def = |name: &str| {
            tcx.hir_body_owners()
                .find(|&owner| {
                    matches!(tcx.def_kind(owner), DefKind::Static { .. })
                        && tcx.item_name(owner.to_def_id()).as_str() == name
                })
                .expect("fixture static")
                .to_def_id()
        };
        let value = static_def("VALUE");
        let write_end = static_def("WRITE_END");
        for name in ["STATIC_REF", "STATIC_RAW", "REBORROW_STATIC"] {
            assert_eq!(static_pointer_target(tcx, constant(tcx, name)), value, "{name}");
        }
        assert_eq!(static_pointer_target(tcx, constant(tcx, "OTHER_REF")), static_def("OTHER"));
        assert_ne!(value, static_def("OTHER"));
        for name in ["MUT_STATIC_RAW", "MUT_STATIC_CONST_RAW", "MUT_STATIC_REF"] {
            assert_eq!(static_pointer_target(tcx, constant(tcx, name)), write_end, "{name}");
        }
        for (def, mutable) in [(value, false), (write_end, true)] {
            let allocation = tcx.eval_static_initializer(def).expect("static initializer folds");
            assert_eq!(allocation.inner().mutability.is_mut(), mutable);
            let scalar = allocation
                .inner()
                .read_scalar(&tcx, alloc_range(Size::ZERO, Size::from_bytes(4)), false)
                .expect("static integer");
            assert_eq!(
                scalar
                    .try_to_scalar_int()
                    .expect("integer initializer")
                    .to_bits(Size::from_bytes(4)),
                0xffff_ffff
            );
        }
        for (name, target) in
            [("SAVED_ADDRESS", write_end), ("SELF_ADDRESS", static_def("SELF_ADDRESS"))]
        {
            let allocation = tcx
                .eval_static_initializer(static_def(name))
                .expect("static pointer initializer folds");
            assert!(allocation.inner().mutability.is_mut());
            let scalar = allocation
                .inner()
                .read_scalar(&tcx, alloc_range(Size::ZERO, tcx.data_layout.pointer_size()), true)
                .expect("static pointer");
            assert_eq!(static_pointer_target(tcx, ConstValue::Scalar(scalar)), target);
        }
        let owner = tcx
            .hir_body_owners()
            .find(|&owner| {
                tcx.def_kind(owner) == DefKind::Fn
                    && tcx.item_name(owner.to_def_id()).as_str() == "promoted_statics"
            })
            .expect("promoted static caller");
        let mut count = 0;
        let mut saw_mutable = false;
        let mut saw_immutable = false;
        for (promoted, _) in tcx.promoted_mir(owner).iter_enumerated() {
            let result = tcx
                .const_eval_global_id(
                    ty::TypingEnv::fully_monomorphized(),
                    GlobalId {
                        instance: ty::Instance::mono(tcx, owner.to_def_id()),
                        promoted: Some(promoted),
                    },
                    frontend::rustc_span::DUMMY_SP,
                )
                .expect("promoted static address folds");
            let target = static_pointer_target(tcx, result);
            assert!(target == value || target == write_end);
            saw_mutable |= target == write_end;
            saw_immutable |= target == value;
            count += 1;
        }
        assert!(count >= 4 && saw_mutable && saw_immutable, "static references must be promoted");
    });
}

#[test]
fn thin_raw_addresses_fold_without_a_library_or_provenance() {
    with_tcx(|tcx| {
        let size = tcx.data_layout.pointer_size();
        for (name, bits) in [
            ("MAX_ADDRESS", size.truncate(u128::MAX)),
            ("NULL_ADDRESS", 0),
            ("RECAST_ADDRESS", size.truncate(u128::MAX)),
        ] {
            let scalar = constant(tcx, name).try_to_scalar().expect("raw address");
            assert_eq!(
                scalar.to_pointer(&tcx).into_raw_parts(),
                (None, Size::from_bytes(bits as u64))
            );
        }
    });
}

#[test]
#[ignore = "requires FRONTEND_FOLD_SYSROOT naming the caller's compiled library"]
fn mmap_addresses_and_read_unaligned_assertions_against_a_named_library() {
    let sysroot = eko::env::var("FRONTEND_FOLD_SYSROOT").expect("explicit test sysroot");
    let source = r#"
        pub const MAP_FAILED: *mut core::ffi::c_void = !0usize as *mut core::ffi::c_void;
        pub const NULL: *const u8 = 0usize as *const u8;
        pub const NEGATIVE: *const u8 = -1i8 as *const u8;
        pub const TRUNCATED: *const u8 = u128::MAX as *const u8;
        pub const RECAST: *const u32 = MAP_FAILED as *const u8 as *const u32;
        pub const SIGNED_MAP_FAILED: *mut core::ffi::c_void = !0 as *mut core::ffi::c_void;
        pub const STORAGE: u64 = 73;
        pub const STORAGE_CAST: *const u8 = &STORAGE as *const u64 as *const u8;
        pub const STORAGE_READ: u64 = unsafe { *(STORAGE_CAST as *const u64) };
        pub const POINTER_ARRAY: [*const u8; 2] = [0 as *const u8, !0usize as *const u8];
        pub const POINTER_READ: *const u8 = POINTER_ARRAY[1];
        #[repr(C)]
        pub struct Padded { byte: u8, word: u64 }
        #[repr(C, packed)]
        pub struct Packed { byte: u8, word: u64 }
        pub const PADDED_OFFSET: usize = core::mem::offset_of!(Padded, word);
        pub const PACKED_OFFSET: usize = core::mem::offset_of!(Packed, word);
        pub const NESTED_OFFSET: usize = core::mem::offset_of!((u64, Padded), 1.word);
        pub const unsafe fn read_word(src: *const usize) -> usize {
            unsafe { core::ptr::read_unaligned(src) }
        }
        pub fn contains(bytes: &[u8], needle: &u8) -> bool { bytes.contains(needle) }
    "#;
    with_source_tcx(source, Some(&sysroot), |tcx| {
        let size = tcx.data_layout.pointer_size();
        for name in [
            "MAP_FAILED",
            "NULL",
            "NEGATIVE",
            "TRUNCATED",
            "RECAST",
            "SIGNED_MAP_FAILED",
            "POINTER_READ",
        ] {
            let value = constant(tcx, name).try_to_scalar().expect("raw address scalar");
            let (provenance, offset) = value.to_pointer(&tcx).into_raw_parts();
            assert!(provenance.is_none(), "{name}");
            assert_eq!(
                offset.bytes() as u128,
                if name == "NULL" { 0 } else { size.truncate(u128::MAX) }
            );
        }
        assert_eq!(constant(tcx, "STORAGE_READ").try_to_bits(Size::from_bytes(8)), Some(73));
        let scalar = constant(tcx, "STORAGE_CAST").try_to_scalar().expect("cast reference");
        assert!(scalar.to_pointer(&tcx).into_raw_parts().0.is_some());
        let owner = tcx
            .hir_body_owners()
            .find(|&owner| {
                tcx.def_kind(owner) == DefKind::Fn
                    && tcx.item_name(owner.to_def_id()).as_str() == "read_word"
            })
            .expect("read_unaligned caller");
        let mut constants = Constants(alloc::vec![]);
        constants.visit_body(tcx.mir_for_ctfe(owner));
        let read = constants
            .0
            .iter()
            .find_map(|constant| {
                if let ty::FnDef(def, _) = *constant.ty().kind()
                    && tcx.item_name(def).as_str() == "read_unaligned"
                {
                    Some(def)
                } else {
                    None
                }
            })
            .expect("read_unaligned function");
        let typing_env = ty::TypingEnv::fully_monomorphized();
        for ty in [tcx.types.usize, tcx.types.u64, tcx.types.u8, tcx.types.unit] {
            let instance = ty::Instance::new_raw(read, tcx.mk_args(&[ty.into()]));
            let body = instance.instantiate_mir_and_normalize_erasing_regions(
                tcx,
                typing_env,
                ty::EarlyBinder::bind(tcx, tcx.mir_for_ctfe(read).clone()),
            );
            let mut constants = Constants(alloc::vec![]);
            constants.visit_body(&body);
            let assertion = constants
                .0
                .into_iter()
                .find(|value| {
                    matches!(value, mir::Const::Unevaluated(unevaluated, _)
                if tcx.def_path_str(unevaluated.def).ends_with("read_unaligned::{constant#0}"))
                })
                .expect("exact read_unaligned inline assertion");
            assert!(matches!(
                assertion
                    .eval(tcx, typing_env, frontend::rustc_span::DUMMY_SP)
                    .expect("read_unaligned::{constant#0} folds"),
                ConstValue::ZeroSized
            ));
        }
        for (name, bits) in [("PADDED_OFFSET", 8), ("PACKED_OFFSET", 1), ("NESTED_OFFSET", 16)] {
            assert_eq!(constant(tcx, name).try_to_bits(size), Some(bits), "{name}");
        }
        let items = tcx.collect_and_partition_mono_items(());
        assert!(items.all_mono_items.contains(&owner.to_def_id()), "read_word is instantiated");
        let contains = tcx
            .hir_body_owners()
            .find(|&owner| {
                tcx.def_kind(owner) == DefKind::Fn
                    && tcx.item_name(owner.to_def_id()).as_str() == "contains"
            })
            .expect("slice contains caller");
        assert!(
            items.all_mono_items.contains(&contains.to_def_id()),
            "[u8]::contains is instantiated"
        );
        assert!(tcx.dcx().has_errors().is_none(), "contains instantiation has no diagnostics");
    });
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
