// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.

pub mod discriminator;
pub mod llvm_siphash;

pub use discriminator::{
    FnPtrDiscriminatorSource, FnPtrTypeDiscriminatorInput, ptrauth_clone_discriminated_schema_for,
    ptrauth_compute_fn_ptr_type_discriminator_for,
};
