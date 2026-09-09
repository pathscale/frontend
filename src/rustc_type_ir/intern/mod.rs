use core::hash::Hash;

use crate::rustc_type_ir::fmt::Debug;

pub trait Interned<I>: Copy + Debug + Hash + Eq + PartialEq {
    type Value;
    fn get(self) -> Self::Value;
}
