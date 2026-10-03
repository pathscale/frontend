/// Minimal declarations that let a no-core snippet type-check without a standard library.
pub const MINIMAL_LANG_ITEMS: &str = r#"
#[allow(dead_code, unused_imports)]
mod __analysis_lang_items {
    #[lang = "pointee_sized"]
    pub trait PointeeSized {}

    #[lang = "meta_sized"]
    pub trait MetaSized: PointeeSized {}

    #[lang = "sized"]
    pub trait Sized: MetaSized {}

    #[lang = "copy"]
    pub trait Copy: Clone {}

    #[lang = "clone"]
    pub const trait Clone: Sized {
        #[lang = "clone_fn"]
        fn clone(&self) -> Self;
    }

    #[lang = "drop"]
    pub const trait Drop {
        fn drop(&mut self);
    }

    #[lang = "deref"]
    pub const trait Deref: PointeeSized {
        #[lang = "deref_target"]
        type Target: ?Sized;

        fn deref(&self) -> &Self::Target;
    }

    #[lang = "deref_mut"]
    pub const trait DerefMut: [const] Deref + PointeeSized {
        fn deref_mut(&mut self) -> &mut Self::Target;
    }

    #[lang = "sync"]
    pub unsafe auto trait Sync {}

    #[lang = "unpin"]
    pub auto trait Unpin {}

    #[lang = "destruct"]
    pub const trait Destruct: PointeeSized {}

    #[lang = "structural_peq"]
    pub trait StructuralPartialEq {}

    pub trait Eq: PartialEq<Self> {}

    #[lang = "const_param_ty"]
    pub trait ConstParamTy: StructuralPartialEq + Eq {}

    #[lang = "unsize"]
    pub trait Unsize<T: PointeeSized>: PointeeSized {}

    #[lang = "coerce_unsized"]
    pub trait CoerceUnsized<T: PointeeSized>: Sized {}

    #[lang = "deref_pure"]
    pub unsafe trait DerefPure: PointeeSized {}

    #[lang = "reborrow"]
    pub trait Reborrow {}

    #[lang = "coerce_shared"]
    pub trait CoerceShared<Target: Copy>: Reborrow {}

    #[lang = "receiver"]
    pub trait Receiver: PointeeSized {
        #[lang = "receiver_target"]
        type Target: ?Sized;
    }

    #[lang = "legacy_receiver"]
    pub trait LegacyReceiver: PointeeSized {}

    #[lang = "va_arg_safe"]
    pub unsafe trait VaArgSafe: Copy {}

    #[lang = "Option"]
    pub enum Option<T> {
        #[lang = "Some"]
        Some(T),
        #[lang = "None"]
        None,
    }

    pub enum Result<T, E> {
        #[lang = "Ok"]
        Ok(T),
        #[lang = "Err"]
        Err(E),
    }

    pub enum ControlFlow<B, C = ()> {
        #[lang = "Break"]
        Break(B),
        #[lang = "Continue"]
        Continue(C),
    }

    pub const trait FromResidual<R = <Self as Try>::Residual> {
        #[lang = "from_residual"]
        fn from_residual(residual: R) -> Self;
    }

    #[lang = "Try"]
    pub const trait Try: [const] FromResidual {
        type Output;
        type Residual: Residual<Self::Output>;

        #[lang = "from_output"]
        fn from_output(output: Self::Output) -> Self;

        #[lang = "branch"]
        fn branch(self) -> ControlFlow<Self::Residual, Self::Output>;
    }

    pub const trait Residual<O>: Sized {
        type TryType: [const] Try<Output = O, Residual = Self>;
    }

    #[lang = "into_try_type"]
    pub const fn residual_into_try_type<R: [const] Residual<O>, O>(
        residual: R,
    ) -> <R as Residual<O>>::TryType {
        FromResidual::from_residual(residual)
    }

    pub struct Yeet<T>(pub T);

    #[lang = "from_yeet"]
    pub fn from_yeet<T, Y>(yeeted: Y) -> T
    where
        T: FromResidual<Yeet<Y>>,
    {
        FromResidual::from_residual(Yeet(yeeted))
    }

    pub trait IntoIterator {
        type Item;
        type IntoIter: Iterator<Item = Self::Item>;

        #[lang = "into_iter"]
        fn into_iter(self) -> Self::IntoIter;
    }

    #[lang = "iterator"]
    pub trait Iterator {
        type Item;

        #[lang = "next"]
        fn next(&mut self) -> Option<Self::Item>;
    }

    #[lang = "tuple_trait"]
    pub trait Tuple {}

    #[rustc_paren_sugar]
    #[lang = "fn_once"]
    pub trait FnOnce<Args: Tuple> {
        #[lang = "fn_once_output"]
        type Output;

        extern "rust-call" fn call_once(self, args: Args) -> Self::Output;
    }

    #[rustc_paren_sugar]
    #[lang = "fn_mut"]
    pub trait FnMut<Args: Tuple>: FnOnce<Args> {
        extern "rust-call" fn call_mut(&mut self, args: Args) -> Self::Output;
    }

    #[rustc_paren_sugar]
    #[lang = "fn"]
    pub trait Fn<Args: Tuple>: FnMut<Args> {
        extern "rust-call" fn call(&self, args: Args) -> Self::Output;
    }

    #[lang = "add"]
    pub trait Add<Rhs = Self> {
        type Output;
        fn add(self, rhs: Rhs) -> Self::Output;
    }

    #[lang = "sub"]
    pub trait Sub<Rhs = Self> {
        type Output;
        fn sub(self, rhs: Rhs) -> Self::Output;
    }

    #[lang = "mul"]
    pub trait Mul<Rhs = Self> {
        type Output;
        fn mul(self, rhs: Rhs) -> Self::Output;
    }

    #[lang = "div"]
    pub trait Div<Rhs = Self> {
        type Output;
        fn div(self, rhs: Rhs) -> Self::Output;
    }

    #[lang = "rem"]
    pub trait Rem<Rhs = Self> {
        type Output;
        fn rem(self, rhs: Rhs) -> Self::Output;
    }

    #[lang = "neg"]
    pub trait Neg {
        type Output;
        fn neg(self) -> Self::Output;
    }

    #[lang = "not"]
    pub trait Not {
        type Output;
        fn not(self) -> Self::Output;
    }

    #[lang = "bitxor"]
    pub trait BitXor<Rhs = Self> {
        type Output;
        fn bitxor(self, rhs: Rhs) -> Self::Output;
    }

    #[lang = "bitand"]
    pub trait BitAnd<Rhs = Self> {
        type Output;
        fn bitand(self, rhs: Rhs) -> Self::Output;
    }

    #[lang = "bitor"]
    pub trait BitOr<Rhs = Self> {
        type Output;
        fn bitor(self, rhs: Rhs) -> Self::Output;
    }

    #[lang = "shl"]
    pub trait Shl<Rhs = Self> {
        type Output;
        fn shl(self, rhs: Rhs) -> Self::Output;
    }

    #[lang = "shr"]
    pub trait Shr<Rhs = Self> {
        type Output;
        fn shr(self, rhs: Rhs) -> Self::Output;
    }

    #[lang = "add_assign"]
    pub trait AddAssign<Rhs = Self> {
        fn add_assign(&mut self, rhs: Rhs);
    }

    #[lang = "sub_assign"]
    pub trait SubAssign<Rhs = Self> {
        fn sub_assign(&mut self, rhs: Rhs);
    }

    #[lang = "mul_assign"]
    pub trait MulAssign<Rhs = Self> {
        fn mul_assign(&mut self, rhs: Rhs);
    }

    #[lang = "div_assign"]
    pub trait DivAssign<Rhs = Self> {
        fn div_assign(&mut self, rhs: Rhs);
    }

    #[lang = "rem_assign"]
    pub trait RemAssign<Rhs = Self> {
        fn rem_assign(&mut self, rhs: Rhs);
    }

    #[lang = "bitxor_assign"]
    pub trait BitXorAssign<Rhs = Self> {
        fn bitxor_assign(&mut self, rhs: Rhs);
    }

    #[lang = "bitand_assign"]
    pub trait BitAndAssign<Rhs = Self> {
        fn bitand_assign(&mut self, rhs: Rhs);
    }

    #[lang = "bitor_assign"]
    pub trait BitOrAssign<Rhs = Self> {
        fn bitor_assign(&mut self, rhs: Rhs);
    }

    #[lang = "shl_assign"]
    pub trait ShlAssign<Rhs = Self> {
        fn shl_assign(&mut self, rhs: Rhs);
    }

    #[lang = "shr_assign"]
    pub trait ShrAssign<Rhs = Self> {
        fn shr_assign(&mut self, rhs: Rhs);
    }

    #[lang = "eq"]
    pub const trait PartialEq<Rhs: PointeeSized = Self>: PointeeSized {
        fn eq(&self, other: &Rhs) -> bool;
        fn ne(&self, other: &Rhs) -> bool;
    }

    #[lang = "partial_ord"]
    pub const trait PartialOrd<Rhs: PointeeSized = Self>:
        [const] PartialEq<Rhs> + PointeeSized
    {
        fn partial_cmp(&self, other: &Rhs) -> Option<Ordering>;
        fn lt(&self, other: &Rhs) -> bool;
        fn le(&self, other: &Rhs) -> bool;
        fn gt(&self, other: &Rhs) -> bool;
        fn ge(&self, other: &Rhs) -> bool;
    }

    #[lang = "Ordering"]
    pub enum Ordering {
        Less,
        Equal,
        Greater,
    }

    #[lang = "index"]
    pub const trait Index<Idx: ?Sized> {
        type Output: ?Sized;
        fn index(&self, index: Idx) -> &Self::Output;
    }

    #[lang = "index_mut"]
    pub const trait IndexMut<Idx: ?Sized>: [const] Index<Idx> {
        fn index_mut(&mut self, index: Idx) -> &mut Self::Output;
    }

    pub const trait RangePattern {
        #[lang = "RangeMin"]
        const MIN: Self;
        #[lang = "RangeMax"]
        const MAX: Self;
        #[lang = "RangeSub"]
        fn sub_one(self) -> Self;
    }

    #[lang = "Range"]
    pub struct Range<Idx> {
        pub start: Idx,
        pub end: Idx,
    }

    #[lang = "RangeFrom"]
    pub struct RangeFrom<Idx> {
        pub start: Idx,
    }

    #[lang = "RangeTo"]
    pub struct RangeTo<Idx> {
        pub end: Idx,
    }

    #[lang = "RangeToInclusive"]
    pub struct RangeToInclusive<Idx> {
        pub end: Idx,
    }

    #[lang = "RangeFull"]
    pub struct RangeFull;

    #[lang = "RangeInclusive"]
    pub struct RangeInclusive<Idx> {
        start: Idx,
        end: Idx,
        exhausted: bool,
    }

    impl<Idx> RangeInclusive<Idx> {
        #[lang = "range_inclusive_new"]
        pub const fn new(start: Idx, end: Idx) -> Self {
            Self { start, end, exhausted: false }
        }
    }

    #[lang = "RangeCopy"]
    pub struct RangeCopy<Idx> {
        pub start: Idx,
        pub end: Idx,
    }

    #[lang = "RangeFromCopy"]
    pub struct RangeFromCopy<Idx> {
        pub start: Idx,
    }

    #[lang = "RangeInclusiveCopy"]
    pub struct RangeInclusiveCopy<Idx> {
        pub start: Idx,
        pub last: Idx,
    }

    #[lang = "RangeToInclusiveCopy"]
    pub struct RangeToInclusiveCopy<Idx> {
        pub last: Idx,
    }

    #[lang = "String"]
    pub struct String;

    #[lang = "CStr"]
    pub struct CStr {
        inner: [i8],
    }

    #[lang = "va_list"]
    pub struct VaList<'a> {
        marker: &'a (),
    }

    #[lang = "owned_box"]
    pub struct Box<T: ?Sized>(*mut T);

    #[lang = "manually_drop"]
    pub struct ManuallyDrop<T: ?Sized> {
        value: T,
    }

    #[lang = "maybe_uninit"]
    pub union MaybeUninit<T> {
        uninit: (),
        value: ManuallyDrop<T>,
    }

    #[lang = "pin"]
    pub struct Pin<Ptr> {
        pointer: Ptr,
    }

    impl<Ptr> Pin<Ptr> {
        #[lang = "new_unchecked"]
        pub const unsafe fn new_unchecked(pointer: Ptr) -> Self {
            Self { pointer }
        }
    }

    #[lang = "Poll"]
    pub enum Poll<T> {
        #[lang = "Ready"]
        Ready(T),
        #[lang = "Pending"]
        Pending,
    }

    #[lang = "Context"]
    pub struct Context<'a> {
        marker: &'a (),
    }

    #[lang = "future_trait"]
    pub trait Future {
        #[lang = "future_output"]
        type Output;

        #[lang = "poll"]
        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output>;
    }

    pub trait IntoFuture {
        type Output;
        type IntoFuture: Future<Output = Self::Output>;

        #[lang = "into_future"]
        fn into_future(self) -> Self::IntoFuture;
    }

    #[lang = "async_iterator"]
    pub trait AsyncIterator {
        type Item;

        #[lang = "async_iterator_poll_next"]
        fn poll_next(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<Option<Self::Item>>;
    }

    pub trait IntoAsyncIterator {
        type Item;
        type IntoAsyncIter: AsyncIterator<Item = Self::Item>;

        #[lang = "into_async_iter_into_iter"]
        fn into_async_iter(self) -> Self::IntoAsyncIter;
    }

    #[lang = "ResumeTy"]
    pub struct ResumeTy(*mut Context<'static>);

    #[lang = "get_context"]
    pub unsafe fn get_context<'a, 'b>(cx: ResumeTy) -> &'a mut Context<'b> {
        unsafe { &mut *(cx.0 as *mut Context<'b>) }
    }

    #[rustc_paren_sugar]
    #[lang = "async_fn_once"]
    pub trait AsyncFnOnce<Args: Tuple> {
        #[lang = "async_fn_once_output"]
        type Output;
        #[lang = "call_once_future"]
        type CallOnceFuture: Future<Output = Self::Output>;

        extern "rust-call" fn async_call_once(self, args: Args) -> Self::CallOnceFuture;
    }

    #[rustc_paren_sugar]
    #[lang = "async_fn_mut"]
    pub trait AsyncFnMut<Args: Tuple>: AsyncFnOnce<Args> {
        #[lang = "call_ref_future"]
        type CallRefFuture<'a>: Future<Output = Self::Output>
        where
            Self: 'a;

        extern "rust-call" fn async_call_mut(
            &mut self,
            args: Args,
        ) -> Self::CallRefFuture<'_>;
    }

    #[rustc_paren_sugar]
    #[lang = "async_fn"]
    pub trait AsyncFn<Args: Tuple>: AsyncFnMut<Args> {
        extern "rust-call" fn async_call(
            &self,
            args: Args,
        ) -> Self::CallRefFuture<'_>;
    }

    impl<T> Poll<Option<T>> {
        #[lang = "AsyncGenReady"]
        pub fn async_gen_ready(item: T) -> Self {
            Poll::Ready(Some(item))
        }

        #[lang = "AsyncGenPending"]
        pub const PENDING: Self = Poll::Pending;
    }

    #[lang = "From"]
    pub trait From<T>: Sized {
        #[lang = "from"]
        fn from(value: T) -> Self;
    }

    #[lang = "contract_check_requires"]
    pub fn contract_check_requires<C: Fn() -> bool + Copy>(_cond: C) {}

    #[lang = "contract_build_check_ensures"]
    pub fn build_check_ensures<Ret, C>(cond: C) -> C
    where
        C: Fn(&Ret) -> bool + Copy + 'static,
    {
        cond
    }

    #[lang = "contract_check_ensures"]
    pub fn contract_check_ensures<C, Ret>(cond: Option<C>, ret: Ret) -> Ret
    where
        C: Fn(&Ret) -> bool + Copy,
    {
        let _ = cond;
        ret
    }

    #[lang = "format_argument"]
    pub struct FormatArgument<'a> {
        marker: &'a (),
    }

    impl<'a> FormatArgument<'a> {
        pub const fn new_display<T>(x: &'a T) -> Self {
            let _ = x;
            Self { marker: &() }
        }

        pub const fn new_debug<T>(x: &'a T) -> Self {
            let _ = x;
            Self { marker: &() }
        }

        pub const fn new_debug_noop<T>(x: &'a T) -> Self {
            let _ = x;
            Self { marker: &() }
        }

        pub const fn new_octal<T>(x: &'a T) -> Self {
            let _ = x;
            Self { marker: &() }
        }

        pub const fn new_lower_hex<T>(x: &'a T) -> Self {
            let _ = x;
            Self { marker: &() }
        }

        pub const fn new_upper_hex<T>(x: &'a T) -> Self {
            let _ = x;
            Self { marker: &() }
        }

        pub const fn new_pointer<T>(x: &'a T) -> Self {
            let _ = x;
            Self { marker: &() }
        }

        pub const fn new_binary<T>(x: &'a T) -> Self {
            let _ = x;
            Self { marker: &() }
        }

        pub const fn new_lower_exp<T>(x: &'a T) -> Self {
            let _ = x;
            Self { marker: &() }
        }

        pub const fn new_upper_exp<T>(x: &'a T) -> Self {
            let _ = x;
            Self { marker: &() }
        }

        pub const fn from_usize(x: &'a usize) -> Self {
            let _ = x;
            Self { marker: &() }
        }
    }

    #[lang = "format_arguments"]
    pub struct FormatArguments<'a> {
        marker: &'a (),
    }

    impl<'a> FormatArguments<'a> {
        pub const fn from_str(s: &'static str) -> Self {
            let _ = s;
            Self { marker: &() }
        }

        pub fn from_str_nonconst(s: &'static str) -> Self {
            let _ = s;
            Self { marker: &() }
        }

        pub unsafe fn new<const N: usize, const M: usize>(
            template: &'a [u8; N],
            args: &'a [FormatArgument<'a>; M],
        ) -> Self {
            let _ = (template, args);
            Self { marker: &() }
        }
    }

    #[lang = "panic_info"]
    pub struct PanicInfo<'a> {
        marker: &'a (),
    }

    #[lang = "panic_location"]
    pub struct PanicLocation<'a> {
        marker: &'a (),
    }
}

#[allow(dead_code, unused_imports)]
mod __analysis_lang_item_prelude {
    pub use super::__analysis_lang_items::*;
    pub use super::__analysis_lang_items::Option::{None, Some};
    pub use super::__analysis_lang_items::Result::{Err, Ok};
}

#[allow(internal_features)]
#[prelude_import]
use __analysis_lang_item_prelude::*;
"#;

/// Check whether this source can receive the synthetic declarations.
pub fn should_inject(source: &str, no_sysroot: bool) -> bool {
    no_sysroot && !source.contains("#[lang") && !source.contains("__analysis_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injects_minimal_items_only_when_needed() {
        assert!(should_inject("fn inspect() {}", true));
        assert!(!should_inject("#[lang = \"sized\"] trait Sized {}", true));
        assert!(!should_inject("fn inspect() {}", false));
        assert!(MINIMAL_LANG_ITEMS.contains("#[lang = \"pointee_sized\"]"));
        assert!(MINIMAL_LANG_ITEMS.contains("#[lang = \"from_residual\"]"));
        assert!(MINIMAL_LANG_ITEMS.contains("#[lang = \"range_inclusive_new\"]"));
        assert!(MINIMAL_LANG_ITEMS.contains("#[lang = \"format_arguments\"]"));
        assert!(MINIMAL_LANG_ITEMS.contains("pub const fn new(start: Idx, end: Idx) -> Self"));
        assert!(MINIMAL_LANG_ITEMS.contains("Self { start, end, exhausted: false }"));

        assert!(MINIMAL_LANG_ITEMS.contains("#[prelude_import]"));

        let operator_source = include_str!("../rustc_hir_typeck/op.rs");
        let table_start = operator_source.find("fn lang_item_for_binop").unwrap();
        let mapping_source = &operator_source[table_start..];
        let table_end = mapping_source.find("/// Check if ").unwrap();
        let mapping_source = &mapping_source[..table_end];
        let mut checked = 0;

        for line in mapping_source.lines() {
            let Some((_, lookup)) = line.split_once("=> (sym::") else {
                continue;
            };
            let Some((method_name, trait_lookup)) = lookup.split_once(", lang.") else {
                continue;
            };
            let Some((trait_name, _)) = trait_lookup.split_once("_trait()") else {
                continue;
            };

            let lang_attribute = alloc::format!("#[lang = \"{trait_name}\"]");
            let (_, trait_source) = MINIMAL_LANG_ITEMS
                .split_once(&lang_attribute)
                .unwrap_or_else(|| panic!("missing operator lang item {trait_name}"));
            let trait_source = trait_source.split("\n    #[lang =").next().unwrap_or(trait_source);
            let method_signature = alloc::format!("fn {method_name}(");
            assert!(
                trait_source.contains(&method_signature),
                "missing {method_signature} in {trait_name}"
            );
            checked += 1;
        }

        assert!(checked > 0, "operator lookup table in op.rs was empty");
    }
}
