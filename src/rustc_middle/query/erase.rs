//! To improve compile times and code size for the compiler itself, query
//! values are "erased" in some contexts (e.g. inside in-memory cache types),
//! to reduce the number of generic instantiations created during codegen.
//!
//! See <https://github.com/rust-lang/rust/pull/151715> for some bootstrap-time
//! and performance benchmarks.

// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use core::marker::PhantomData;
use core::mem::transmute_copy;
use core::mem::MaybeUninit;

use crate::rustc_ast::tokenstream::TokenStream;
use crate::rustc_data_structures::steal::Steal;
use crate::rustc_data_structures::sync::{DynSend, DynSync};
use crate::rustc_span::def_id::ModId;
use crate::rustc_span::{ErrorGuaranteed, Spanned};

use crate::rustc_middle::mono::{MonoItem, NormalizationErrorInMono};
use crate::rustc_middle::ty::{self, Ty, TyCtxt};
use crate::rustc_middle::{mir, thir, traits};

/// Opts `ErasedData` out of `Send` and `Sync`, which would be wrong for an erased type.
///
/// Upstream this is an extern type (`extern { type NoAutoTraits; }`), which is unstable.
/// A raw pointer is neither `Send` nor `Sync`, which covers the auto traits that matter
/// here; unlike the extern type it leaves `Unpin` and the unwind-safety traits in place,
/// which nothing in this crate reads off erased values.
type NoAutoTraits = *const ();

/// Internal implementation detail of [`Erased`].
#[derive(Copy, Clone)]
pub struct ErasedData<Storage: Copy> {
    /// We use `MaybeUninit` here to make sure it's legal to store a transmuted
    /// value that isn't actually of type `Storage`.
    data: MaybeUninit<Storage>,
    /// `Storage` is an erased type, so we use an external type here to opt-out of auto traits
    /// as those would be incorrect.
    no_auto_traits: PhantomData<NoAutoTraits>,
}

// Upstream grants `DynSync`/`DynSend` here explicitly because the extern-type marker
// opted `ErasedData` out of the auto impls. Both are now blanket-implemented (see
// `rustc_data_structures/marker.rs`), so a second impl would conflict.

/// Trait for types that can be erased into [`Erased<Self>`].
///
/// Erasing and unerasing values is performed by [`erase_val`] and [`restore_val`].
///
/// FIXME: This whole trait could potentially be replaced by `T: Copy` and the
/// storage type `[u8; size_of::<T>()]` when support for that is more mature.
pub trait Erasable: Copy {
    /// Storage type to used for erased values of this type.
    /// Should be `[u8; N]`, where N is equal to `size_of::<Self>`.
    ///
    /// [`ErasedData`] wraps this storage type in `MaybeUninit` to ensure that
    /// transmutes to/from erased storage are well-defined.
    type Storage: Copy;
}

/// A value of `T` that has been "erased" into some opaque storage type.
///
/// This is helpful for reducing the number of concrete instantiations needed
/// during codegen when building the compiler.
///
/// Upstream this is an opaque alias, `ErasedData<impl Copy>` (`type_alias_impl_trait`, unstable),
/// so that `Erased<T>` and `Erased<U>` are distinct types while monomorphization can still see
/// the shared storage. A plain `ErasedData<T::Storage>` alias would merge them and stop callers
/// such as `restore_val(*erased_value)` from inferring `T`, so this is a newtype over the same
/// storage instead. Layout and auto traits are unchanged; what is lost is only the codegen
/// sharing between instantiations with equal storage.
pub struct Erased<T: Erasable> {
    data: ErasedData<<T as Erasable>::Storage>,
    _type: PhantomData<fn() -> T>,
}

impl<T: Erasable> Copy for Erased<T> {}

impl<T: Erasable> Clone for Erased<T> {
    #[inline(always)]
    fn clone(&self) -> Self {
        *self
    }
}

/// Erases a value of type `T` into `Erased<T>`.
///
/// `Erased<T>` and `Erased<U>` are type-checked as distinct types, but codegen
/// can see whether they actually have the same storage type.
#[inline(always)]
// The `DynSend` and `DynSync` bounds on `T` are used to
// justify the safety of the implementations of these traits for `ErasedData`.
pub fn erase_val<T: Erasable + DynSend + DynSync>(value: T) -> Erased<T> {
    // Ensure the sizes match
    const {
        if size_of::<T>() != size_of::<T::Storage>() {
            panic!("size of T must match erased type <T as Erasable>::Storage")
        }
    };

    Erased {
        data: ErasedData::<<T as Erasable>::Storage> {
            // `transmute` cannot be used: its size check does not see through the generic
            // `T::Storage`. Upstream used the `transmute_unchecked` intrinsic, which is unstable;
            // `transmute_copy` does the same here because `Erasable: Copy`, and the `const` block
            // above has already checked that the sizes match.
            //
            // SAFETY: It is safe to transmute to MaybeUninit for types with the same sizes.
            data: unsafe { transmute_copy::<T, MaybeUninit<T::Storage>>(&value) },
            no_auto_traits: PhantomData,
        },
        _type: PhantomData,
    }
}

/// Restores an erased value to its real type.
///
/// This relies on the fact that `Erased<T>` and `Erased<U>` are type-checked
/// as distinct types, even if they use the same storage type.
#[inline(always)]
pub fn restore_val<T: Erasable>(erased_value: Erased<T>) -> T {
    let Erased { data: ErasedData { data, .. }, .. } = erased_value;
    // See comment in `erase_val` for why we use `transmute_copy`.
    //
    // SAFETY: Due to the use of impl Trait in `Erased` the only way to safely create an instance
    // of `Erased` is to call `erase_val`, so we know that `erased_value.data` is a valid instance
    // of `T` of the right size.
    unsafe { transmute_copy::<MaybeUninit<T::Storage>, T>(&data) }
}

impl<T> Erasable for &'_ T {
    type Storage = [u8; size_of::<&'_ ()>()];
}

impl<T> Erasable for &'_ [T] {
    type Storage = [u8; size_of::<&'_ [()]>()];
}

// Upstream has a separate `impl Erasable for &'_ ty::RawList<H, T>` here, which did not overlap
// the `&'_ T` impl only because `RawList` ended in an extern type and was unsized. Extern types
// are unstable, `RawList` is `Sized` now (see `ty/list.rs`), so that impl would conflict. The
// `&'_ T` impl above covers it with the same storage: `&RawList` is a thin pointer either way.

impl<T> Erasable for Result<&'_ T, traits::query::NoSolution> {
    type Storage = [u8; size_of::<Result<&'_ (), traits::query::NoSolution>>()];
}

impl<T> Erasable for Result<&'_ T, ErrorGuaranteed> {
    type Storage = [u8; size_of::<Result<&'_ (), ErrorGuaranteed>>()];
}

impl<T> Erasable for Option<&'_ T> {
    type Storage = [u8; size_of::<Option<&'_ ()>>()];
}

impl<T: Erasable> Erasable for ty::EarlyBinder<'_, T> {
    type Storage = T::Storage;
}

impl<T0, T1> Erasable for (&'_ T0, &'_ T1) {
    type Storage = [u8; size_of::<(&'_ (), &'_ ())>()];
}

impl<T0, T1, T2> Erasable for (&'_ T0, &'_ T1, &'_ T2) {
    type Storage = [u8; size_of::<(&'_ (), &'_ (), &'_ ())>()];
}

impl<T0, T1> Erasable for (&'_ [T0], &'_ [T1]) {
    type Storage = [u8; size_of::<(&'_ [()], &'_ [()])>()];
}

macro_rules! impl_erasable_for_types_with_no_type_params {
    ($($ty:ty),+ $(,)?) => {
        $(
            impl Erasable for $ty {
                type Storage = [u8; size_of::<$ty>()];
            }
        )*
    }
}

// For types with no type parameters the erased storage for `Foo` is
// `[u8; size_of::<Foo>()]`. ('_ lifetimes are allowed.)
impl_erasable_for_types_with_no_type_params! {
    // tidy-alphabetical-start
    (&'_ ty::CrateInherentImpls, Result<(), ErrorGuaranteed>),
    (),
    (traits::solve::QueryResult<'_>, &'_ traits::solve::inspect::Probe<TyCtxt<'_>>, ty::RequiredDepth),
    Option<&'_ [crate::rustc_hir::PreciseCapturingArgKind<crate::rustc_span::Symbol, crate::rustc_span::Symbol>]>,
    // Was `Option<&'_ OsStr>`. The `env_var_os` query returns `Option<&'tcx [u8]>` now; both are
    // unsized, so neither is covered by the generic `impl<T> Erasable for Option<&'_ T>`.
    Option<&'_ [u8]>,
    Option<(mir::ConstValue, Ty<'_>)>,
    Option<(crate::rustc_span::def_id::DefId, crate::rustc_session::config::EntryFnType)>,
    Option<crate::rustc_abi::Align>,
    Option<crate::rustc_ast::expand::allocator::AllocatorKind>,
    Option<crate::rustc_data_structures::svh::Svh>,
    Option<crate::rustc_hir::ConstStability>,
    Option<crate::rustc_hir::CoroutineKind>,
    Option<crate::rustc_hir::DefaultBodyStability>,
    Option<crate::rustc_hir::Stability>,
    Option<crate::rustc_middle::middle::stability::DeprecationEntry>,
    Option<crate::rustc_middle::ty::AsyncDestructor>,
    Option<crate::rustc_middle::ty::Destructor>,
    Option<crate::rustc_middle::ty::IntrinsicDef>,
    Option<crate::rustc_middle::ty::ScalarInt>,
    Option<crate::rustc_span::Span>,
    Option<crate::rustc_span::def_id::CrateNum>,
    Option<crate::rustc_span::def_id::DefId>,
    Option<crate::rustc_span::def_id::LocalDefId>,
    Option<crate::rustc_target::spec::PanicStrategy>,
    Option<ty::EarlyBinder<'_, Ty<'_>>>,
    Option<ty::Value<'_>>,
    Option<usize>,
    Result<&'_ TokenStream, ()>,
    Result<&'_ crate::rustc_target::callconv::FnAbi<'_, Ty<'_>>, &'_ ty::layout::FnAbiError<'_>>,
    Result<&'_ traits::ImplSource<'_, ()>, traits::CodegenObligationError>,
    Result<&'_ ty::List<Ty<'_>>, ty::util::AlwaysRequiresDrop>,
    Result<(&'_ Steal<thir::Thir<'_>>, thir::ExprId), ErrorGuaranteed>,
    Result<(&'_ [Spanned<MonoItem<'_>>], &'_ [Spanned<MonoItem<'_>>]), NormalizationErrorInMono>,
    Result<(), ErrorGuaranteed>,
    Result<Option<ty::EarlyBinder<'_, ty::Const<'_>>>, ErrorGuaranteed>,
    Result<Option<ty::Instance<'_>>, ErrorGuaranteed>,
    Result<bool, &ty::layout::LayoutError<'_>>,
    Result<mir::ConstAlloc<'_>, mir::interpret::ErrorHandled>,
    Result<mir::ConstValue, mir::interpret::ErrorHandled>,
    Result<crate::rustc_abi::TyAndLayout<'_, Ty<'_>>, &ty::layout::LayoutError<'_>>,
    Result<crate::rustc_middle::traits::EvaluationResult, crate::rustc_middle::traits::OverflowError>,
    Result<crate::rustc_middle::ty::adjustment::CoerceUnsizedInfo, ErrorGuaranteed>,
    Result<ty::GenericArg<'_>, traits::query::NoSolution>,
    Ty<'_>,
    bool,
    crate::rustc_crate_store::CrateDepKind,
    crate::rustc_data_structures::svh::Svh,
    crate::rustc_hir::Constness,
    crate::rustc_hir::Defaultness,
    crate::rustc_hir::HirId,
    crate::rustc_hir::MaybeOwner<'_>,
    crate::rustc_hir::OpaqueTyOrigin<crate::rustc_hir::def_id::DefId>,
    crate::rustc_hir::def::DefKind,
    crate::rustc_hir::def_id::DefId,
    crate::rustc_middle::hir::ProjectedMaybeOwner<'_>,
    crate::rustc_middle::middle::codegen_fn_attrs::SanitizerFnAttrs,
    crate::rustc_middle::middle::resolve_bound_vars::ObjectLifetimeDefault,
    crate::rustc_middle::mir::ConstQualifs,
    crate::rustc_middle::mir::ConstValue,
    crate::rustc_middle::mir::interpret::AllocId,
    crate::rustc_middle::mir::interpret::EvalStaticInitializerRawResult<'_>,
    crate::rustc_middle::mir::interpret::EvalToValTreeResult<'_>,
    crate::rustc_middle::mono::MonoItemPartitions<'_>,
    crate::rustc_middle::traits::query::MethodAutoderefStepsResult<'_>,
    crate::rustc_middle::ty::AdtDef<'_>,
    crate::rustc_middle::ty::AnonConstKind,
    crate::rustc_middle::ty::AssocItem,
    crate::rustc_middle::ty::Asyncness,
    crate::rustc_middle::ty::Binder<'_, ty::CoroutineWitnessTypes<TyCtxt<'_>>>,
    crate::rustc_middle::ty::Binder<'_, ty::FnSig<'_>>,
    crate::rustc_middle::ty::ClosureTypeInfo<'_>,
    crate::rustc_middle::ty::Const<'_>,
    crate::rustc_middle::ty::ConstConditions<'_>,
    crate::rustc_middle::ty::GenericClauses<'_>,
    crate::rustc_middle::ty::ImplTraitHeader<'_>,
    crate::rustc_middle::ty::ParamEnv<'_>,
    crate::rustc_middle::ty::SymbolName<'_>,
    crate::rustc_middle::ty::TypingEnv<'_>,
    crate::rustc_middle::ty::Visibility<ModId>,
    crate::rustc_middle::ty::inhabitedness::InhabitedPredicate<'_>,
    crate::rustc_session::Limits,
    crate::rustc_session::config::OptLevel,
    crate::rustc_session::config::SymbolManglingVersion,
    crate::rustc_span::ExpnId,
    crate::rustc_span::Span,
    crate::rustc_span::Symbol,
    crate::rustc_target::spec::PanicStrategy,
    usize,
    // tidy-alphabetical-end
}
