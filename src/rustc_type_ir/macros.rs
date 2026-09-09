/// Used for types that are `Copy` and which **do not care arena
/// allocated data** (i.e., don't need to be folded).
#[macro_export]
macro_rules! TrivialTypeTraversalImpls {
    ($($ty:ty,)+) => {
        $(
            impl<I: $crate::rustc_type_ir::Interner> $crate::rustc_type_ir::TypeFoldable<I> for $ty {
                fn try_fold_with<F: $crate::rustc_type_ir::FallibleTypeFolder<I>>(
                    self,
                    _: &mut F,
                ) -> ::core::result::Result<Self, F::Error> {
                    Ok(self)
                }

                #[inline]
                fn fold_with<F: $crate::rustc_type_ir::TypeFolder<I>>(
                    self,
                    _: &mut F,
                ) -> Self {
                    self
                }
            }

            impl<I: $crate::rustc_type_ir::Interner> $crate::rustc_type_ir::TypeVisitable<I> for $ty {
                #[inline]
                fn visit_with<F: $crate::rustc_type_ir::TypeVisitor<I>>(
                    &self,
                    _: &mut F)
                    -> F::Result
                {
                    <F::Result as $crate::rustc_type_ir::VisitorResult>::output()
                }
            }
        )+
    };
}

///////////////////////////////////////////////////////////////////////////
// Atomic structs
//
// For things that don't carry any arena-allocated data (and are
// copy...), just add them to this list.

TrivialTypeTraversalImpls! {
    (),
    bool,
    usize,
    u8,
    u16,
    u32,
    u64,
    // tidy-alphabetical-start
    crate::rustc_type_ir::BoundConstness,
    crate::rustc_type_ir::ClausePolarity,
    crate::rustc_type_ir::DebruijnIndex,
    crate::rustc_type_ir::UniverseIndex,
    crate::rustc_type_ir::Variance,
    crate::rustc_type_ir::solve::BuiltinImplSource,
    crate::rustc_type_ir::solve::Certainty,
    crate::rustc_type_ir::solve::GoalSource,
    crate::rustc_type_ir::solve::VisibleForLeakCheck,
    crate::rustc_ast_ir::Mutability,
    // tidy-alphabetical-end
}

macro_rules! TrivialLiftImpls {
    ($($ty:ty),+ $(,)?) => {
        $(
            impl<I: $crate::rustc_type_ir::Interner> $crate::rustc_type_ir::lift::Lift<I> for $ty {
                type Lifted = Self;
                fn lift_to_interner(self, _: I) -> Self {
                    self
                }
            }
        )+
    };
}

TrivialLiftImpls! {
    crate::rustc_type_ir::LateParamRegion<I>
}
