#![allow(internal_features, dead_code, non_snake_case, non_camel_case_types, non_upper_case_globals)]
#![feature(lang_items)]
#[lang = "pointee_sized"] pub trait PointeeSized {}
#[lang = "meta_sized"] pub trait MetaSized: PointeeSized {}
#[lang = "sized"] pub trait Sized: MetaSized {}
#[lang = "copy"] pub trait Copy {}
#[lang = "legacy_receiver"] pub trait LegacyReceiver {}
impl<T: ?Sized> LegacyReceiver for &T {}
impl Copy for u32 {}
impl Copy for bool {}

// The lines above are `tests/parallel.rs`'s `LANG` prelude, verbatim, after one `allow` line: a
// corpus file names items `PointQc3x17`, `larger_Qc3x17` and `LIMIT_Qc3x17`, which the case
// lints would flag on every item, and `lang_items` is an internal feature, which warns. Everything below is what the corpus units need beyond that prelude, and nothing
// more: `&mut self` receivers, a second integer type, and the operator traits that integer
// arithmetic and comparison are type checked through. Each operator impl is written with the
// operator itself; type checking resolves it to the builtin operation on two integers, as
// rustc's own `minicore` test prelude does. No `Not` (so no `!`), no `Div` or `Rem` (so no `/`
// or `%`), no statics (a static needs `Sync`), no closures, no slices, no `dyn`.

impl<T: ?Sized> LegacyReceiver for &mut T {}
impl Copy for u64 {}

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

/// `==` looks up `eq` and `!=` looks up `ne` on this trait, so both are required methods: a
/// default `ne` would need `!`.
#[lang = "eq"]
pub trait PartialEq<Rhs: ?Sized = Self> {
    fn eq(&self, other: &Rhs) -> bool;
    fn ne(&self, other: &Rhs) -> bool;
}

/// `<`, `<=`, `>` and `>=` look up these four by name. The real trait's `partial_cmp` is left
/// out: it needs `Option` and `Ordering`, and no operator asks for it.
#[lang = "partial_ord"]
pub trait PartialOrd<Rhs: ?Sized = Self>: PartialEq<Rhs> {
    fn lt(&self, other: &Rhs) -> bool;
    fn le(&self, other: &Rhs) -> bool;
    fn gt(&self, other: &Rhs) -> bool;
    fn ge(&self, other: &Rhs) -> bool;
}

impl Add for u32 {
    type Output = u32;
    fn add(self, rhs: u32) -> u32 {
        self + rhs
    }
}

impl Sub for u32 {
    type Output = u32;
    fn sub(self, rhs: u32) -> u32 {
        self - rhs
    }
}

impl Mul for u32 {
    type Output = u32;
    fn mul(self, rhs: u32) -> u32 {
        self * rhs
    }
}

impl PartialEq for u32 {
    fn eq(&self, other: &u32) -> bool {
        *self == *other
    }
    fn ne(&self, other: &u32) -> bool {
        *self != *other
    }
}

impl PartialOrd for u32 {
    fn lt(&self, other: &u32) -> bool {
        *self < *other
    }
    fn le(&self, other: &u32) -> bool {
        *self <= *other
    }
    fn gt(&self, other: &u32) -> bool {
        *self > *other
    }
    fn ge(&self, other: &u32) -> bool {
        *self >= *other
    }
}

impl Add for u64 {
    type Output = u64;
    fn add(self, rhs: u64) -> u64 {
        self + rhs
    }
}

impl Sub for u64 {
    type Output = u64;
    fn sub(self, rhs: u64) -> u64 {
        self - rhs
    }
}

impl Mul for u64 {
    type Output = u64;
    fn mul(self, rhs: u64) -> u64 {
        self * rhs
    }
}

impl PartialEq for u64 {
    fn eq(&self, other: &u64) -> bool {
        *self == *other
    }
    fn ne(&self, other: &u64) -> bool {
        *self != *other
    }
}

impl PartialOrd for u64 {
    fn lt(&self, other: &u64) -> bool {
        *self < *other
    }
    fn le(&self, other: &u64) -> bool {
        *self <= *other
    }
    fn gt(&self, other: &u64) -> bool {
        *self > *other
    }
    fn ge(&self, other: &u64) -> bool {
        *self >= *other
    }
}
