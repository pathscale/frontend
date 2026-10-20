use alloc::vec;
use alloc::vec::Vec;
use core::cmp::Ordering;
use rustc_apfloat::Float;
use rustc_apfloat::ieee::{Double, Single};

use crate::rustc_abi::{Size, TagEncoding, VariantIdx, Variants};
use crate::rustc_data_structures::fx::FxHashMap;
use crate::rustc_hir::def::DefKind;
use crate::rustc_middle::mir::interpret::{
    AllocId, AllocInit, Allocation, CTFE_ALLOC_SALT, ConstAllocation, GlobalAlloc, GlobalId,
    Pointer, Scalar, alloc_range, read_target_uint, write_target_uint,
};
use crate::rustc_middle::mir::{self, ConstValue, Local, Operand, Rvalue};
use crate::rustc_middle::ty::layout::LayoutCx;
use crate::rustc_middle::ty::{self, Instance, Ty, TyCtxt, TypeVisitableExt};
use crate::rustc_span::{Spanned, sym};

const MAX_FOLD_DEPTH: usize = 32;
const MAX_FOLD_STEPS: usize = 4096;

#[derive(Clone, Copy)]
enum FoldValue<'tcx> {
    Scalar { bits: u128, ty: Ty<'tcx> },
    ZeroSized { ty: Ty<'tcx> },
    // Thin references and raw pointers share allocation provenance and a pointee.
    Reference { ty: Ty<'tcx>, pointee: &'tcx FoldValue<'tcx>, alloc_id: AllocId, offset: Size },
    // Thin pointers with deferred pointee reads, including statics and TypeId fragments.
    Pointer { ty: Ty<'tcx>, alloc_id: AllocId, offset: Size },
    Aggregate { ty: Ty<'tcx>, fields: &'tcx [FoldValue<'tcx>], variant: Option<VariantIdx> },
    // A slice/string pointer, or its unsized pointee during place projection.
    Slice { ty: Ty<'tcx>, alloc_id: AllocId, offset: Size, len: u64 },
}

// A projected borrow keeps the original allocation and its byte offset. Keeping
// the folded pointee too lets subsequent dereferences use values, without
// interpreting arbitrary pointers or reading unknown memory.
struct FoldPlace<'tcx> {
    value: FoldValue<'tcx>,
    root: FoldValue<'tcx>,
    alloc_id: Option<AllocId>,
    offset: Size,
}

impl<'tcx> FoldValue<'tcx> {
    fn ty(self) -> Ty<'tcx> {
        match self {
            FoldValue::Scalar { ty, .. }
            | FoldValue::ZeroSized { ty }
            | FoldValue::Reference { ty, .. }
            | FoldValue::Pointer { ty, .. }
            | FoldValue::Aggregate { ty, .. }
            | FoldValue::Slice { ty, .. } => ty,
        }
    }
}

#[derive(Clone, Copy)]
struct ScalarInfo<'tcx> {
    bits: u128,
    ty: Ty<'tcx>,
    size: Size,
    width: u32,
    signed: bool,
}

struct Folder<'tcx> {
    tcx: TyCtxt<'tcx>,
    typing_env: ty::TypingEnv<'tcx>,
    active: Vec<GlobalId<'tcx>>,
    cache: FxHashMap<GlobalId<'tcx>, Result<FoldValue<'tcx>, &'static str>>,
    remaining_steps: usize,
}

pub(super) fn fold_scalar<'tcx>(
    tcx: TyCtxt<'tcx>,
    typing_env: ty::TypingEnv<'tcx>,
    id: GlobalId<'tcx>,
) -> Result<Scalar, &'static str> {
    match fold_const(tcx, typing_env, id)? {
        ConstValue::Scalar(value) => Ok(value),
        _ => Err("constant requires an aggregate query result"),
    }
}

pub(super) fn fold_const<'tcx>(
    tcx: TyCtxt<'tcx>,
    typing_env: ty::TypingEnv<'tcx>,
    id: GlobalId<'tcx>,
) -> Result<ConstValue, &'static str> {
    let mut folder = Folder {
        tcx,
        typing_env,
        active: Vec::new(),
        cache: FxHashMap::default(),
        remaining_steps: MAX_FOLD_STEPS,
    };
    let value = folder.eval_instance(id, 0)?;
    folder.to_const(value)
}

pub(super) fn fold_static_initializer<'tcx>(
    tcx: TyCtxt<'tcx>,
    def_id: crate::rustc_hir::def_id::LocalDefId,
) -> Result<ConstAllocation<'tcx>, &'static str> {
    let typing_env = ty::TypingEnv::post_analysis(tcx, def_id);
    let mut folder = Folder {
        tcx,
        typing_env,
        active: Vec::new(),
        cache: FxHashMap::default(),
        remaining_steps: MAX_FOLD_STEPS,
    };
    let id = GlobalId { instance: Instance::mono(tcx, def_id.to_def_id()), promoted: None };
    let value = folder.eval_instance(id, 0)?;
    let mutable =
        tcx.is_mutable_static(def_id.to_def_id()) || !value.ty().is_freeze(tcx, typing_env);
    folder.allocation(value, if mutable { mir::Mutability::Mut } else { mir::Mutability::Not })
}

pub(super) fn fold_allocation<'tcx>(
    tcx: TyCtxt<'tcx>,
    typing_env: ty::TypingEnv<'tcx>,
    id: GlobalId<'tcx>,
) -> Result<mir::ConstAlloc<'tcx>, &'static str> {
    let mut folder = Folder {
        tcx,
        typing_env,
        active: Vec::new(),
        cache: FxHashMap::default(),
        remaining_steps: MAX_FOLD_STEPS,
    };
    let value = folder.eval_instance(id, 0)?;
    Ok(mir::ConstAlloc { alloc_id: folder.allocate(value)?, ty: value.ty() })
}

pub(super) fn fold_valtree<'tcx>(
    tcx: TyCtxt<'tcx>,
    typing_env: ty::TypingEnv<'tcx>,
    value: ty::Value<'tcx>,
) -> Result<ConstValue, &'static str> {
    let mut folder = Folder {
        tcx,
        typing_env,
        active: Vec::new(),
        cache: FxHashMap::default(),
        remaining_steps: MAX_FOLD_STEPS,
    };
    let value = folder.eval_valtree(value.ty, value.valtree, 0)?;
    folder.to_const(value)
}

impl<'tcx> Folder<'tcx> {
    // Pattern endpoints can themselves need folding. Asking layout_of for a
    // transparent pattern wrapper would send those endpoints through valtrees,
    // which this scalar folder does not require. Its storage is the base type;
    // its extra validity restriction is checked separately below.
    fn scalar_storage_ty(&self, mut ty: Ty<'tcx>) -> Result<Ty<'tcx>, &'static str> {
        for _ in 0..MAX_FOLD_DEPTH {
            match *ty.kind() {
                ty::Pat(base, _) => ty = base,
                ty::Adt(def, args)
                    if def.is_struct()
                        && def.repr().transparent()
                        && def.non_enum_variant().fields.len() == 1 =>
                {
                    ty = def.non_enum_variant().fields[crate::rustc_abi::FieldIdx::from_usize(0)]
                        .ty(self.tcx, args)
                        .skip_norm_wip();
                }
                _ => return Ok(ty),
            }
        }
        Err("scalar wrapper exceeded its depth bound")
    }

    fn validate_scalar_type(
        &self,
        ty: Ty<'tcx>,
        bits: u128,
        depth: usize,
    ) -> Result<(), &'static str> {
        if depth >= MAX_FOLD_DEPTH {
            return Err("scalar validity exceeded its depth bound");
        }
        match *ty.kind() {
            ty::Adt(def, args)
                if def.is_struct()
                    && def.repr().transparent()
                    && def.non_enum_variant().fields.len() == 1 =>
            {
                self.validate_scalar_type(
                    def.non_enum_variant().fields[crate::rustc_abi::FieldIdx::from_usize(0)]
                        .ty(self.tcx, args)
                        .skip_norm_wip(),
                    bits,
                    depth + 1,
                )
            }
            ty::Pat(base, pattern) => {
                let ty::PatternKind::Range { start, end } = *pattern else {
                    return Err("unsupported scalar validity pattern");
                };
                let mut bounds = Folder {
                    tcx: self.tcx,
                    typing_env: self.typing_env,
                    active: Vec::new(),
                    cache: FxHashMap::default(),
                    remaining_steps: MAX_FOLD_STEPS,
                };
                let start = bounds.eval_const(
                    mir::Const::Ty(base, start),
                    crate::rustc_span::DUMMY_SP,
                    depth + 1,
                )?;
                let start = bounds.scalar_info(start)?.bits;
                let end = bounds.eval_const(
                    mir::Const::Ty(base, end),
                    crate::rustc_span::DUMMY_SP,
                    depth + 1,
                )?;
                let end = bounds.scalar_info(end)?.bits;
                let size = self.layout(base)?.0;
                let valid = if base.is_signed() {
                    let bits = size.sign_extend(bits);
                    size.sign_extend(start) <= bits && bits <= size.sign_extend(end)
                } else {
                    start <= bits && bits <= end
                };
                if !valid {
                    return Err("constant scalar violates its pattern range");
                }
                self.validate_scalar_type(base, bits, depth + 1)
            }
            _ => Ok(()),
        }
    }

    fn to_const(&self, value: FoldValue<'tcx>) -> Result<ConstValue, &'static str> {
        match value {
            FoldValue::Scalar { .. } => Ok(ConstValue::Scalar(self.to_scalar(value)?)),
            FoldValue::ZeroSized { .. } => Ok(ConstValue::ZeroSized),
            FoldValue::Reference { alloc_id, offset, .. }
            | FoldValue::Pointer { alloc_id, offset, .. } => Ok(ConstValue::Scalar(
                Scalar::from_pointer(Pointer::new(alloc_id.into(), offset), &self.tcx),
            )),
            FoldValue::Slice { ty, alloc_id, offset: Size::ZERO, len }
                if matches!(ty.kind(), ty::Ref(..)) =>
            {
                Ok(ConstValue::Slice { alloc_id, meta: len })
            }
            FoldValue::Slice { .. } => {
                Ok(ConstValue::Indirect { alloc_id: self.allocate(value)?, offset: Size::ZERO })
            }
            FoldValue::Aggregate { ty, fields, variant } => {
                let layout = self
                    .tcx
                    .layout_of(self.typing_env.as_query_input(ty))
                    .map_err(|_| "aggregate layout could not be computed")?;
                if layout.is_zst() {
                    return Ok(ConstValue::ZeroSized);
                }
                if variant.is_none() && layout.backend_repr.is_scalar() {
                    for field in fields {
                        if self.layout(field.ty())?.0.bytes() != 0 {
                            return self.to_const(*field);
                        }
                    }
                }
                let alloc_id = self.allocate(value)?;
                if let crate::rustc_abi::BackendRepr::Scalar(scalar) = layout.backend_repr {
                    let read_provenance =
                        matches!(scalar.primitive(), crate::rustc_abi::Primitive::Pointer(_));
                    let scalar = self
                        .tcx
                        .global_alloc(alloc_id)
                        .unwrap_memory()
                        .inner()
                        .read_scalar(
                            &self.tcx,
                            alloc_range(Size::ZERO, layout.size),
                            read_provenance,
                        )
                        .map_err(|_| "constant aggregate scalar read failed")?;
                    return Ok(ConstValue::Scalar(scalar));
                }
                Ok(ConstValue::Indirect { alloc_id, offset: Size::ZERO })
            }
        }
    }

    fn allocate(&self, value: FoldValue<'tcx>) -> Result<AllocId, &'static str> {
        Ok(self.tcx.reserve_and_set_memory_alloc(self.allocation(value, mir::Mutability::Not)?))
    }

    fn allocation(
        &self,
        value: FoldValue<'tcx>,
        mutability: mir::Mutability,
    ) -> Result<ConstAllocation<'tcx>, &'static str> {
        let layout = self
            .tcx
            .layout_of(self.typing_env.as_query_input(value.ty()))
            .map_err(|_| "allocation layout could not be computed")?;
        let len = usize::try_from(layout.size.bytes()).map_err(|_| "allocation is too large")?;
        if len > MAX_FOLD_STEPS * 16 {
            return Err("constant allocation exceeded its size bound");
        }
        let mut allocation =
            Allocation::from_bytes(vec![0; len], layout.align.abi, mir::Mutability::Mut, ());
        self.write_value(&mut allocation, Size::ZERO, value)?;
        allocation.mutability = mutability;
        Ok(self.tcx.mk_const_alloc(allocation))
    }

    fn write_value(
        &self,
        allocation: &mut Allocation,
        offset: Size,
        value: FoldValue<'tcx>,
    ) -> Result<(), &'static str> {
        if let FoldValue::Aggregate { ty, fields, variant } = value {
            let layout = self
                .tcx
                .layout_of(self.typing_env.as_query_input(ty))
                .map_err(|_| "aggregate layout could not be computed")?;
            let cx = LayoutCx::new(self.tcx, self.typing_env);
            let field_layout = variant.map_or(layout, |variant| layout.for_variant(&cx, variant));
            for (index, field) in fields.iter().enumerate() {
                self.write_value(allocation, offset + field_layout.fields.offset(index), *field)?;
            }
            if let Some(variant) = variant
                && let Variants::Multiple { tag, tag_encoding, tag_field, .. } = &layout.variants
            {
                let bits = match tag_encoding {
                    TagEncoding::Direct => {
                        let ty::Adt(def, _) = *ty.kind() else {
                            return Err("enum aggregate has a non-ADT type");
                        };
                        def.discriminant_for_variant(self.tcx, variant).val
                    }
                    TagEncoding::Niche { untagged_variant, niche_variants, niche_start } => {
                        if variant == *untagged_variant {
                            return Ok(());
                        }
                        if !niche_variants.contains(&variant) {
                            return Err("enum variant has no representable tag");
                        }
                        ((variant.as_u32() - niche_variants.start.as_u32()) as u128)
                            .wrapping_add(*niche_start)
                    }
                };
                let size = tag.size(&self.tcx);
                allocation
                    .write_scalar(
                        &self.tcx,
                        alloc_range(offset + layout.fields.offset(tag_field.as_usize()), size),
                        Scalar::from_uint(size.truncate(bits), size),
                    )
                    .map_err(|_| "constant enum tag write failed")?;
            }
            return Ok(());
        }
        let size = self.layout(value.ty())?.0;
        if size.bytes() == 0 {
            return Ok(());
        }
        let scalar = match value {
            FoldValue::Reference { alloc_id, offset, .. }
            | FoldValue::Pointer { alloc_id, offset, .. } => {
                Scalar::from_pointer(Pointer::new(alloc_id.into(), offset), &self.tcx)
            }
            FoldValue::Slice { alloc_id, offset: pointee_offset, len, .. } => {
                let pointer_size = self.tcx.data_layout.pointer_size();
                allocation
                    .write_scalar(
                        &self.tcx,
                        alloc_range(offset, pointer_size),
                        Scalar::from_pointer(
                            Pointer::new(alloc_id.into(), pointee_offset),
                            &self.tcx,
                        ),
                    )
                    .map_err(|_| "constant pointer write failed")?;
                allocation
                    .write_scalar(
                        &self.tcx,
                        alloc_range(offset + pointer_size, pointer_size),
                        Scalar::from_target_usize(len, &self.tcx),
                    )
                    .map_err(|_| "constant metadata write failed")?;
                return Ok(());
            }
            _ => self.to_scalar(value)?,
        };
        allocation
            .write_scalar(&self.tcx, alloc_range(offset, size), scalar)
            .map_err(|_| "constant scalar write failed")
    }

    fn aggregate(&self, ty: Ty<'tcx>, fields: &[FoldValue<'tcx>]) -> FoldValue<'tcx> {
        FoldValue::Aggregate {
            ty,
            fields: self.tcx.arena.dropless.alloc_slice(fields),
            variant: None,
        }
    }

    fn layout(&self, ty: Ty<'tcx>) -> Result<(Size, u64), &'static str> {
        let ty = self.scalar_storage_ty(ty)?;
        let layout = self
            .tcx
            .layout_of(self.typing_env.as_query_input(ty))
            .map_err(|_| "type layout could not be computed")?;
        Ok((layout.size, layout.align.bytes()))
    }

    fn scalar_value(&self, bits: u128, ty: Ty<'tcx>) -> Result<FoldValue<'tcx>, &'static str> {
        let layout = self
            .tcx
            .layout_of(self.typing_env.as_query_input(self.scalar_storage_ty(ty)?))
            .map_err(|_| "scalar layout could not be computed")?;
        let crate::rustc_abi::BackendRepr::Scalar(scalar) = layout.backend_repr else {
            return Err("constant does not have a scalar layout");
        };

        let (size, _) = self.layout(ty)?;
        let width = bit_width(size)?;
        let bits = bits & bit_mask(width);
        self.validate_scalar_type(ty, bits, 0)?;
        if (matches!(scalar.primitive(), crate::rustc_abi::Primitive::Pointer(_))
            && !matches!(ty.kind(), ty::RawPtr(..)))
            || !scalar.valid_range(&self.tcx).contains(bits)
        {
            return Err("constant scalar violates its type's validity range");
        }
        if ty.is_bool() && bits > 1 {
            return Err("boolean scalar has an invalid value");
        }

        Ok(FoldValue::Scalar { bits, ty })
    }

    fn zero_sized(&self, ty: Ty<'tcx>) -> Result<FoldValue<'tcx>, &'static str> {
        let (size, _) = self.layout(ty)?;
        if size.bytes() != 0 {
            return Err("zero-sized constant has a nonzero layout");
        }
        Ok(FoldValue::ZeroSized { ty })
    }

    fn scalar_info(&self, value: FoldValue<'tcx>) -> Result<ScalarInfo<'tcx>, &'static str> {
        let FoldValue::Scalar { bits, ty } = value else {
            return Err("operation requires a scalar operand");
        };
        if !ty.is_bool() && !is_integer_ty(ty) {
            return Err("operation requires an integer or boolean operand");
        }

        let (size, _) = self.layout(ty)?;
        let width = bit_width(size)?;
        Ok(ScalarInfo { bits: bits & bit_mask(width), ty, size, width, signed: ty.is_signed() })
    }

    fn to_scalar(&self, value: FoldValue<'tcx>) -> Result<Scalar, &'static str> {
        let FoldValue::Scalar { bits, ty } = value else {
            return Err("constant result is not a scalar");
        };
        let (size, _) = self.layout(ty)?;
        if ty.is_bool() {
            if bits > 1 {
                return Err("boolean result has an invalid value");
            }
            Ok(Scalar::from_bool(bits == 1))
        } else if self
            .tcx
            .layout_of(self.typing_env.as_query_input(self.scalar_storage_ty(ty)?))
            .map_err(|_| "scalar layout could not be computed")?
            .backend_repr
            .is_scalar()
        {
            Ok(Scalar::from_uint(bits, size))
        } else {
            Err("constant result is not an integer or boolean scalar")
        }
    }

    fn eval_instance(
        &mut self,
        id: GlobalId<'tcx>,
        depth: usize,
    ) -> Result<FoldValue<'tcx>, &'static str> {
        self.spend_step()?;
        if depth >= MAX_FOLD_DEPTH {
            return Err("nested constant fold exceeded its depth bound");
        }
        let instance = id.instance;
        if let Some(result) = self.cache.get(&id) {
            return *result;
        }
        if self.active.contains(&id) {
            return Err("cyclic constant reference");
        }

        self.active.push(id);
        let result = if let Some(promoted) = id.promoted {
            let body = &self.tcx.promoted_mir(instance.def_id())[promoted];
            let body = instance.instantiate_mir_and_normalize_erasing_regions(
                self.tcx,
                self.typing_env,
                ty::EarlyBinder::bind(self.tcx, body.clone()),
            );
            self.eval_body(&body, depth)
        } else {
            self.eval_instance_uncached(instance, depth)
        };
        self.active.pop();
        self.cache.insert(id, result);
        result
    }

    fn eval_instance_uncached(
        &mut self,
        instance: Instance<'tcx>,
        depth: usize,
    ) -> Result<FoldValue<'tcx>, &'static str> {
        let def_id = instance.def_id();

        if let Some((value, ty)) = self.tcx.trivial_const(def_id) {
            return self.eval_const(
                mir::Const::from_value(value, ty),
                self.tcx.def_span(def_id),
                depth,
            );
        }

        if instance.args.has_non_region_param() {
            return Err("constant instance still has generic parameters");
        }

        if !matches!(
            self.tcx.def_kind(def_id),
            DefKind::Const { is_type_const: false }
                | DefKind::AssocConst { is_type_const: false }
                | DefKind::AnonConst
                | DefKind::Static { .. }
        ) {
            return Err("unevaluated item is not a constant body");
        }
        // Constant bodies use the CTFE table. is_mir_available only asks for
        // optimized function MIR and is false for external associated constants.
        let uninstantiated_body = self.tcx.mir_for_ctfe(def_id);
        self.spend_step()?;
        let body = instance.instantiate_mir_and_normalize_erasing_regions(
            self.tcx,
            self.typing_env,
            ty::EarlyBinder::bind(self.tcx, uninstantiated_body.clone()),
        );
        if body.has_non_region_param() {
            return Err("constant MIR still has generic parameters after substitution");
        }
        if body.arg_count != 0 {
            return Err("constant body has arguments");
        }

        self.eval_body(&body, depth)
    }

    fn eval_const(
        &mut self,
        constant: mir::Const<'tcx>,
        span: crate::rustc_span::Span,
        depth: usize,
    ) -> Result<FoldValue<'tcx>, &'static str> {
        if let mir::Const::Val(ConstValue::Scalar(Scalar::Int(scalar)), ty) = constant {
            let (size, _) = self.layout(ty)?;
            return self.scalar_value(scalar.to_bits(size), ty);
        }

        match constant {
            mir::Const::Val(ConstValue::ZeroSized, ty) => self.zero_sized(ty),
            mir::Const::Val(ConstValue::Scalar(scalar), ty) => {
                self.read_pointer(scalar, ty, None, depth)
            }
            mir::Const::Val(ConstValue::Slice { alloc_id, meta }, ty) => {
                Ok(FoldValue::Slice { ty, alloc_id, offset: Size::ZERO, len: meta })
            }
            mir::Const::Val(ConstValue::Indirect { alloc_id, offset }, ty) => {
                self.read_value(alloc_id, offset, ty, depth)
            }
            mir::Const::Ty(value_ty, type_const) => match type_const.kind() {
                ty::ConstKind::Value(value) => self.eval_valtree(value_ty, value.valtree, depth),
                ty::ConstKind::Alias(_, alias) => {
                    if type_const.has_non_region_param() {
                        return Err("type-level constant alias has generic parameters");
                    }
                    let Some(def) = alias.kind.opt_def_id() else {
                        return Err("type-level constant alias has no item definition");
                    };
                    self.eval_unevaluated(
                        mir::UnevaluatedConst { def, args: alias.args, promoted: None },
                        span,
                        depth + 1,
                    )
                }
                ty::ConstKind::Param(..)
                | ty::ConstKind::Infer(..)
                | ty::ConstKind::Bound(..)
                | ty::ConstKind::Placeholder(..) => {
                    Err("type-level constant still has unresolved parameters")
                }
                ty::ConstKind::Expr(..) => {
                    Err("unevaluated generic constant expression is unsupported")
                }
                ty::ConstKind::Error(..) => Err("type-level constant already contains an error"),
            },
            mir::Const::Unevaluated(unevaluated, _) => {
                self.eval_unevaluated(unevaluated, span, depth + 1)
            }
        }
    }

    fn eval_valtree(
        &mut self,
        ty: Ty<'tcx>,
        tree: ty::ValTree<'tcx>,
        depth: usize,
    ) -> Result<FoldValue<'tcx>, &'static str> {
        self.spend_step()?;
        if depth >= MAX_FOLD_DEPTH {
            return Err("type-level constant exceeded its depth bound");
        }
        if let ty::Pat(base, _) = *ty.kind() {
            let value = self.eval_valtree(base, tree, depth + 1)?;
            return self.retype(value, ty);
        }
        if tree.try_to_branch().is_some_and(|branches| branches.len() > MAX_FOLD_STEPS) {
            return Err("type-level aggregate exceeded its field bound");
        }
        if let ty::Ref(_, target, _) = *ty.kind() {
            let len = || {
                tree.try_to_branch()
                    .map(|branches| branches.len() as u64)
                    .ok_or("slice valtree requires a branch")
            };
            let (storage_ty, len) = match *target.kind() {
                ty::Slice(element) => (Ty::new_array(self.tcx, element, len()?), Some(len()?)),
                ty::Str => (Ty::new_array(self.tcx, self.tcx.types.u8, len()?), Some(len()?)),
                _ => (target, None),
            };
            let pointee = self.eval_valtree(storage_ty, tree, depth + 1)?;
            let alloc_id = self.allocate(pointee)?;
            return Ok(match len {
                Some(len) => FoldValue::Slice { ty, alloc_id, offset: Size::ZERO, len },
                None => FoldValue::Reference {
                    ty,
                    pointee: self.tcx.arena.dropless.alloc(pointee),
                    alloc_id,
                    offset: Size::ZERO,
                },
            });
        }
        if let Some(leaf) = tree.try_to_leaf() {
            return self.scalar_value(leaf.to_bits(self.layout(ty)?.0), ty);
        }
        let branches = tree.to_branch();
        let layout = self
            .tcx
            .layout_of(self.typing_env.as_query_input(ty))
            .map_err(|_| "type-level aggregate layout is unavailable")?;
        if layout.is_zst() {
            return self.zero_sized(ty);
        }
        let (variant, branches) = if let ty::Adt(def, _) = *ty.kind()
            && def.is_enum()
        {
            let (index, fields) = branches.split_first().ok_or("enum valtree has no variant")?;
            let index = index.to_value().to_leaf().to_bits(Size::from_bytes(4)) as u32;
            let variant = VariantIdx::from_u32(index);
            if index as usize >= def.variants().len() {
                return Err("enum valtree variant is out of bounds");
            }
            (Some(variant), fields)
        } else {
            (None, &branches[..])
        };
        let cx = LayoutCx::new(self.tcx, self.typing_env);
        let layout = variant.map_or(layout, |variant| layout.for_variant(&cx, variant));
        if branches.len() != layout.fields.count() {
            return Err("type-level aggregate has a mismatched field count");
        }
        let fields = branches
            .iter()
            .enumerate()
            .map(|(index, field)| {
                self.eval_valtree(layout.field(&cx, index).ty, field.to_value().valtree, depth + 1)
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(FoldValue::Aggregate {
            ty,
            fields: self.tcx.arena.dropless.alloc_slice(&fields),
            variant,
        })
    }

    fn transmute(
        &mut self,
        value: FoldValue<'tcx>,
        destination_ty: Ty<'tcx>,
        depth: usize,
    ) -> Result<FoldValue<'tcx>, &'static str> {
        let source = self
            .tcx
            .layout_of(self.typing_env.as_query_input(value.ty()))
            .map_err(|_| "transmute source layout is unavailable")?;
        let destination = self
            .tcx
            .layout_of(self.typing_env.as_query_input(destination_ty))
            .map_err(|_| "transmute destination layout is unavailable")?;
        if source.size != destination.size {
            return Err("transmute changed the value size");
        }
        if destination.is_uninhabited() {
            return Err("transmute destination has no valid values");
        }
        if source.size.bytes() > (MAX_FOLD_STEPS * 16) as u64 {
            return Err("constant allocation exceeded its size bound");
        }
        // Atomic constructors transmute integers into structs containing UnsafeCell.
        // Re-read their fields using the destination layout, preserving relocations.
        // Leave padding uninitialized so a transmute cannot invent initialized bytes.
        let mut allocation = Allocation::new(
            source.size,
            source.align.abi.max(destination.align.abi),
            AllocInit::Uninit,
            (),
        );
        self.write_value(&mut allocation, Size::ZERO, value)?;
        allocation.mutability = mir::Mutability::Not;
        let alloc_id = self.tcx.reserve_and_set_memory_alloc(self.tcx.mk_const_alloc(allocation));
        self.read_value(alloc_id, Size::ZERO, destination_ty, depth + 1)
    }

    fn read_memory(
        &self,
        alloc_id: AllocId,
        offset: Size,
        size: Size,
        align: u64,
        provenance: bool,
    ) -> Result<Scalar, &'static str> {
        let Some(GlobalAlloc::Memory(allocation)) = self.tcx.try_get_global_alloc(alloc_id) else {
            return Err("constant pointer does not name known memory");
        };
        let allocation = allocation.inner();
        if allocation.mutability != mir::Mutability::Not {
            return Err("constant pointer names mutable memory");
        }
        if offset.bytes() % align != 0 || allocation.align.bytes() < align {
            return Err("constant memory read is misaligned");
        }
        if offset
            .bytes()
            .checked_add(size.bytes())
            .is_none_or(|end| end > allocation.size().bytes())
        {
            return Err("constant memory read is out of bounds");
        }
        allocation
            .read_scalar(&self.tcx, alloc_range(offset, size), provenance)
            .map_err(|_| "constant memory read is uninitialized or has invalid provenance")
    }

    fn read_pointer(
        &mut self,
        scalar: Scalar,
        ty: Ty<'tcx>,
        len: Option<u64>,
        depth: usize,
    ) -> Result<FoldValue<'tcx>, &'static str> {
        if matches!(ty.kind(), ty::FnPtr(..)) {
            let (provenance, offset) = scalar.to_pointer(&self.tcx).into_raw_parts();
            let alloc_id = provenance.ok_or("function pointer has no provenance")?.alloc_id();
            if offset != Size::ZERO
                || !matches!(
                    self.tcx.try_get_global_alloc(alloc_id),
                    Some(GlobalAlloc::Function { .. })
                )
            {
                return Err("function pointer does not name a function");
            }
            return Ok(FoldValue::Pointer { ty, alloc_id, offset });
        }
        let target = pointer_target(ty).ok_or("pointer constant has a non-pointer type")?;
        if len.is_none()
            && matches!(ty.kind(), ty::RawPtr(..))
            && let Ok(integer) = scalar.try_to_scalar_int()
        {
            return self.scalar_value(integer.to_bits(self.tcx.data_layout.pointer_size()), ty);
        }
        let (provenance, offset) = scalar.to_pointer(&self.tcx).into_raw_parts();
        let alloc_id =
            provenance.ok_or("constant pointer has no allocation provenance")?.alloc_id();
        if matches!(self.tcx.try_get_global_alloc(alloc_id), Some(GlobalAlloc::TypeId { .. })) {
            if !matches!(ty.kind(), ty::RawPtr(..)) || !target.is_unit() || len.is_some() {
                return Err("type id provenance requires a thin raw unit pointer");
            }
            return Ok(FoldValue::Pointer { ty, alloc_id, offset });
        }
        // Taking a static's address does not read its initializer or current contents.
        // Preserve its identity even for mutable, foreign or self-referential statics.
        if len.is_none()
            && matches!(self.tcx.try_get_global_alloc(alloc_id), Some(GlobalAlloc::Static(_)))
        {
            return Ok(FoldValue::Pointer { ty, alloc_id, offset });
        }
        if let Some(len) = len {
            if !matches!(target.kind(), ty::Slice(_) | ty::Str) {
                return Err("unsupported constant pointer metadata");
            }
            return Ok(FoldValue::Slice { ty, alloc_id, offset, len });
        }
        if matches!(ty.kind(), ty::RawPtr(..)) {
            return Ok(FoldValue::Pointer { ty, alloc_id, offset });
        }
        let pointee = self.read_value(alloc_id, offset, target, depth + 1)?;
        Ok(FoldValue::Reference {
            ty,
            pointee: self.tcx.arena.dropless.alloc(pointee),
            alloc_id,
            offset,
        })
    }

    fn read_value(
        &mut self,
        alloc_id: AllocId,
        offset: Size,
        ty: Ty<'tcx>,
        depth: usize,
    ) -> Result<FoldValue<'tcx>, &'static str> {
        self.spend_step()?;
        if depth >= MAX_FOLD_DEPTH {
            return Err("constant memory read exceeded its depth bound");
        }
        let layout = self
            .tcx
            .layout_of(self.typing_env.as_query_input(ty))
            .map_err(|_| "constant memory layout is unavailable")?;
        let Some(GlobalAlloc::Memory(allocation)) = self.tcx.try_get_global_alloc(alloc_id) else {
            return Err("constant pointer does not name known memory");
        };
        let allocation = allocation.inner();
        if allocation.mutability != mir::Mutability::Not {
            return Err("constant pointer names mutable memory");
        }
        let align = layout.align.abi.bytes();
        if offset.bytes() % align != 0 || allocation.align.bytes() < align {
            return Err("constant memory read is misaligned");
        }
        if layout.size.bytes() > (MAX_FOLD_STEPS * 16) as u64 {
            return Err("constant memory read exceeded its size bound");
        }
        if offset
            .bytes()
            .checked_add(layout.size.bytes())
            .is_none_or(|end| end > allocation.size().bytes())
        {
            return Err("constant memory read is out of bounds");
        }
        if layout.is_zst() {
            return self.zero_sized(ty);
        }
        if matches!(ty.kind(), ty::FnPtr(..)) {
            let scalar = self.read_memory(alloc_id, offset, layout.size, align, true)?;
            return self.read_pointer(scalar, ty, None, depth);
        }
        if let Some(target) = pointer_target(ty) {
            let size = self.tcx.data_layout.pointer_size();
            let scalar = self.read_memory(alloc_id, offset, size, size.bytes(), true)?;
            let len = if matches!(target.kind(), ty::Slice(_) | ty::Str) {
                let len = self.read_memory(alloc_id, offset + size, size, size.bytes(), false)?;
                Some(
                    len.try_to_scalar_int()
                        .map_err(|_| "pointer length has provenance")?
                        .to_bits(size) as u64,
                )
            } else {
                None
            };
            return self.read_pointer(scalar, ty, len, depth);
        }
        let cx = LayoutCx::new(self.tcx, self.typing_env);
        let variant = if let ty::Adt(def, _) = *ty.kind()
            && def.is_enum()
        {
            Some(match &layout.variants {
                Variants::Empty => return Err("constant enum has no inhabited variants"),
                Variants::Single { index } => *index,
                Variants::Multiple { tag, tag_encoding, tag_field, .. } => {
                    let size = tag.size(&self.tcx);
                    let scalar = self.read_memory(
                        alloc_id,
                        offset + layout.fields.offset(tag_field.as_usize()),
                        size,
                        1,
                        matches!(tag.primitive(), crate::rustc_abi::Primitive::Pointer(_)),
                    )?;
                    match tag_encoding {
                        TagEncoding::Direct => {
                            let bits = scalar
                                .try_to_scalar_int()
                                .map_err(|_| "enum tag has pointer provenance")?
                                .to_bits(size);
                            def.discriminants(self.tcx)
                                .find(|(_, discr)| size.truncate(discr.val) == bits)
                                .map(|(variant, _)| variant)
                                .ok_or("constant enum has an invalid tag")?
                        }
                        TagEncoding::Niche { untagged_variant, niche_variants, niche_start } => {
                            if let Ok(integer) = scalar.try_to_scalar_int() {
                                let relative =
                                    size.truncate(integer.to_bits(size).wrapping_sub(*niche_start));
                                let count =
                                    niche_variants.last.as_u32() - niche_variants.start.as_u32();
                                if relative <= count as u128 {
                                    VariantIdx::from_u32(
                                        niche_variants.start.as_u32() + relative as u32,
                                    )
                                } else {
                                    *untagged_variant
                                }
                            } else {
                                *untagged_variant
                            }
                        }
                    }
                }
            })
        } else {
            None
        };
        let layout = variant.map_or(layout, |variant| layout.for_variant(&cx, variant));
        let count = match *ty.kind() {
            ty::Array(_, count) => {
                count.try_to_target_usize(self.tcx).ok_or("constant array length is unresolved")?
                    as usize
            }
            ty::Tuple(fields) => fields.len(),
            ty::Adt(def, _) if def.is_struct() => def.non_enum_variant().fields.len(),
            ty::Adt(def, _) if def.is_enum() => def.variant(variant.unwrap()).fields.len(),
            ty::Closure(_, args) => args.as_closure().upvar_tys().len(),
            _ => {
                if !layout.backend_repr.is_scalar() {
                    return Err("unsupported allocation-backed constant type");
                }
                let scalar = self.read_memory(
                    alloc_id,
                    offset,
                    layout.size,
                    layout.align.abi.bytes(),
                    false,
                )?;
                let bits = scalar
                    .try_to_scalar_int()
                    .map_err(|_| "scalar memory has pointer provenance")?
                    .to_bits(layout.size);
                return self.scalar_value(bits, ty);
            }
        };
        if count > MAX_FOLD_STEPS {
            return Err("constant memory aggregate exceeded its field bound");
        }
        let fields = (0..count)
            .map(|index| {
                self.read_value(
                    alloc_id,
                    offset + layout.fields.offset(index),
                    layout.field(&cx, index).ty,
                    depth + 1,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(FoldValue::Aggregate {
            ty,
            fields: self.tcx.arena.dropless.alloc_slice(&fields),
            variant,
        })
    }

    fn eval_unevaluated(
        &mut self,
        unevaluated: mir::UnevaluatedConst<'tcx>,
        _span: crate::rustc_span::Span,
        depth: usize,
    ) -> Result<FoldValue<'tcx>, &'static str> {
        if depth >= MAX_FOLD_DEPTH {
            return Err("nested constant fold exceeded its depth bound");
        }
        if unevaluated.args.has_non_region_param() {
            return Err("nested constant still has generic parameters");
        }

        let instance =
            ty::Instance::try_resolve(self.tcx, self.typing_env, unevaluated.def, unevaluated.args)
                .map_err(|_| "nested constant instance resolution reported an error")?
                .ok_or("nested constant instance is unresolved")?;

        self.eval_instance(GlobalId { instance, promoted: unevaluated.promoted }, depth)
    }

    fn eval_body(
        &mut self,
        body: &mir::Body<'tcx>,
        depth: usize,
    ) -> Result<FoldValue<'tcx>, &'static str> {
        self.eval_body_with_args(body, depth, &[])
    }

    fn eval_body_with_args(
        &mut self,
        body: &mir::Body<'tcx>,
        depth: usize,
        args: &[FoldValue<'tcx>],
    ) -> Result<FoldValue<'tcx>, &'static str> {
        let mut locals: FxHashMap<Local, FoldValue<'tcx>> = FxHashMap::default();
        for (index, value) in args.iter().enumerate() {
            locals.insert(Local::from_usize(index + 1), *value);
        }
        let mut visited = Vec::new();
        let mut block = mir::START_BLOCK;

        loop {
            if visited.contains(&block) {
                return Err("loop detected in constant MIR");
            }
            visited.push(block);

            let data = &body.basic_blocks[block];
            for statement in &data.statements {
                self.spend_step()?;
                match &statement.kind {
                    mir::StatementKind::Assign(assignment) => {
                        let (destination, rvalue) = &**assignment;
                        if !destination.projection.is_empty() {
                            return Err("projected place assignment is unsupported");
                        }
                        let destination_ty = body.local_decls[destination.local].ty;
                        let value =
                            self.eval_rvalue(body, &locals, rvalue, destination_ty, depth)?;
                        if value.ty() != destination_ty {
                            return Err("MIR assignment changed the value type");
                        }
                        locals.insert(destination.local, value);
                    }
                    mir::StatementKind::StorageLive(local)
                    | mir::StatementKind::StorageDead(local) => {
                        locals.remove(local);
                    }
                    mir::StatementKind::Intrinsic(intrinsic) => match &**intrinsic {
                        mir::NonDivergingIntrinsic::Assume(operand) => {
                            let condition = self.eval_operand(body, &locals, operand, depth)?;
                            if !self.bool_value(condition)? {
                                return Err("false assume intrinsic");
                            }
                        }
                        mir::NonDivergingIntrinsic::CopyNonOverlapping(..) => {
                            return Err("copy_nonoverlapping memory intrinsic is unsupported");
                        }
                    },
                    mir::StatementKind::FakeRead(..)
                    | mir::StatementKind::AscribeUserType(..)
                    | mir::StatementKind::Coverage(..)
                    | mir::StatementKind::ConstEvalCounter
                    | mir::StatementKind::Nop
                    | mir::StatementKind::BackwardIncompatibleDropHint { .. } => {}
                    mir::StatementKind::SetDiscriminant { .. } => {
                        return Err("enum discriminant write is unsupported");
                    }
                    mir::StatementKind::PlaceMention(..) => {
                        return Err("place mention is unsupported");
                    }
                }
            }

            self.spend_step()?;
            match &data.terminator().kind {
                mir::TerminatorKind::Goto { target } => block = *target,
                mir::TerminatorKind::SwitchInt { discr, targets } => {
                    let value = self.eval_operand(body, &locals, discr, depth)?;
                    let value = self.scalar_info(value)?;
                    block = targets.target_for_value(value.bits);
                }
                mir::TerminatorKind::Assert { cond, expected, target, .. } => {
                    let condition = self.eval_operand(body, &locals, cond, depth)?;
                    if self.bool_value(condition)? != *expected {
                        return Err("constant MIR assertion failed");
                    }
                    block = *target;
                }
                mir::TerminatorKind::Call { func, args, destination, target, .. } => {
                    let Some(next_block) = *target else {
                        return Err("diverging call is unsupported");
                    };
                    if !destination.projection.is_empty() {
                        return Err("projected call destination is unsupported");
                    }
                    let destination_ty = body.local_decls[destination.local].ty;
                    let value = self.eval_layout_intrinsic(
                        body,
                        &locals,
                        func,
                        args,
                        destination_ty,
                        depth,
                    )?;
                    if value.ty() != destination_ty {
                        return Err("intrinsic call changed the destination type");
                    }
                    locals.insert(destination.local, value);
                    block = next_block;
                }
                mir::TerminatorKind::Return => {
                    return locals
                        .get(&mir::RETURN_PLACE)
                        .copied()
                        .ok_or("constant return place has no value");
                }
                mir::TerminatorKind::Unreachable => {
                    // Other blocks may contain unreachable alternatives of a
                    // valid const match. Reject only the executed terminator.
                    return Err("constant execution reached an unreachable terminator");
                }
                mir::TerminatorKind::Drop { .. } => {
                    return Err("drop terminator is unsupported");
                }
                mir::TerminatorKind::UnwindResume | mir::TerminatorKind::UnwindTerminate(..) => {
                    return Err("unwind terminator is unsupported");
                }
                _ => return Err("unsupported control-flow terminator"),
            }
        }
    }

    fn spend_step(&mut self) -> Result<(), &'static str> {
        self.remaining_steps =
            self.remaining_steps.checked_sub(1).ok_or("constant MIR exceeded its step bound")?;
        Ok(())
    }

    fn eval_operand(
        &mut self,
        body: &mir::Body<'tcx>,
        locals: &FxHashMap<Local, FoldValue<'tcx>>,
        operand: &Operand<'tcx>,
        depth: usize,
    ) -> Result<FoldValue<'tcx>, &'static str> {
        match operand {
            Operand::Copy(place) | Operand::Move(place) => self.eval_place(body, locals, place),
            Operand::Constant(constant) => self.eval_const(constant.const_, constant.span, depth),
            Operand::RuntimeChecks(_) => Err("runtime-check operand is unsupported"),
        }
    }

    fn eval_place(
        &mut self,
        body: &mir::Body<'tcx>,
        locals: &FxHashMap<Local, FoldValue<'tcx>>,
        place: &mir::Place<'tcx>,
    ) -> Result<FoldValue<'tcx>, &'static str> {
        Ok(self.project_place(body, locals, place)?.value)
    }

    fn project_place(
        &mut self,
        _body: &mir::Body<'tcx>,
        locals: &FxHashMap<Local, FoldValue<'tcx>>,
        place: &mir::Place<'tcx>,
    ) -> Result<FoldPlace<'tcx>, &'static str> {
        if place.projection.len() > MAX_FOLD_DEPTH {
            return Err("constant place projection exceeded its depth bound");
        }
        let value = *locals.get(&place.local).ok_or("MIR reads a local without a folded value")?;
        let mut place_value = FoldPlace { value, root: value, alloc_id: None, offset: Size::ZERO };
        let mut downcast = None;
        let cx = LayoutCx::new(self.tcx, self.typing_env);
        for projection in place.projection {
            let value = place_value.value;
            match projection {
                mir::ProjectionElem::Deref => {
                    if let FoldValue::Pointer { ty, alloc_id, offset } = value {
                        let target =
                            pointer_target(ty).ok_or("dereference requires a pointer type")?;
                        let pointee = self.read_value(alloc_id, offset, target, 0)?;
                        place_value = FoldPlace {
                            value: pointee,
                            root: pointee,
                            alloc_id: Some(alloc_id),
                            offset,
                        };
                        downcast = None;
                        continue;
                    }
                    if let FoldValue::Slice { ty, alloc_id, offset, len } = value {
                        let target =
                            pointer_target(ty).ok_or("dereference requires a pointer type")?;
                        let pointee = FoldValue::Slice { ty: target, alloc_id, offset, len };
                        place_value = FoldPlace {
                            value: pointee,
                            root: pointee,
                            alloc_id: Some(alloc_id),
                            offset,
                        };
                        downcast = None;
                        continue;
                    }
                    let FoldValue::Reference { pointee, alloc_id, offset, .. } = value else {
                        return Err("dereference requires a folded pointer");
                    };
                    place_value = FoldPlace {
                        value: *pointee,
                        root: *pointee,
                        alloc_id: Some(alloc_id),
                        offset,
                    };
                    downcast = None;
                }
                mir::ProjectionElem::Downcast(_, variant) => {
                    let FoldValue::Aggregate { variant: Some(active), .. } = value else {
                        return Err("downcast requires a folded enum variant");
                    };
                    if variant != active {
                        return Err("constant downcast selects an inactive variant");
                    }
                    downcast = Some(variant);
                }
                mir::ProjectionElem::Field(index, field_ty) => {
                    let FoldValue::Aggregate { ty, fields, variant } = value else {
                        return Err("field projection requires a folded aggregate");
                    };
                    if variant != downcast {
                        return Err("enum field projection requires its active downcast");
                    }
                    let field =
                        *fields.get(index.as_usize()).ok_or("constant field is out of bounds")?;
                    if field.ty() != field_ty {
                        return Err("constant field projection has a mismatched type");
                    }
                    let layout = self
                        .tcx
                        .layout_of(self.typing_env.as_query_input(ty))
                        .map_err(|_| "projected aggregate layout could not be computed")?;
                    let layout = variant.map_or(layout, |variant| layout.for_variant(&cx, variant));
                    place_value.offset += layout.fields.offset(index.as_usize());
                    place_value.value = field;
                    downcast = None;
                }
                mir::ProjectionElem::ConstantIndex { offset, min_length, from_end } => {
                    let len = self.array_len(value)?;
                    if len < min_length {
                        return Err("constant index minimum length exceeds the array length");
                    }
                    let index = if from_end {
                        len.checked_sub(offset).ok_or("constant index is out of bounds")?
                    } else {
                        offset
                    };
                    self.project_index(&mut place_value, index)?;
                    downcast = None;
                }
                mir::ProjectionElem::Index(local) => {
                    let index = *locals.get(&local).ok_or("constant index has no folded value")?;
                    let index = self.scalar_info(index)?;
                    if index.ty != self.tcx.types.usize {
                        return Err("constant index is not a usize");
                    }
                    let index =
                        u64::try_from(index.bits).map_err(|_| "constant index is too large")?;
                    self.project_index(&mut place_value, index)?;
                    downcast = None;
                }
                _ => return Err("unsupported constant place projection"),
            }
        }
        Ok(place_value)
    }

    fn array_len(&self, value: FoldValue<'tcx>) -> Result<u64, &'static str> {
        if let FoldValue::Slice { ty, len, .. } = value
            && matches!(ty.kind(), ty::Slice(_) | ty::Str)
        {
            return Ok(len);
        }
        let ty::Array(_, count) = *value.ty().kind() else {
            return Err("constant index requires a folded array");
        };
        count.try_to_target_usize(self.tcx).ok_or("constant array length is unresolved")
    }

    fn project_index(
        &mut self,
        place: &mut FoldPlace<'tcx>,
        index: u64,
    ) -> Result<(), &'static str> {
        let len = self.array_len(place.value)?;
        if index >= len {
            return Err("constant index is out of bounds");
        }
        if let FoldValue::Slice { ty, alloc_id, offset, .. } = place.value {
            let element = match *ty.kind() {
                ty::Slice(element) => element,
                ty::Str => self.tcx.types.u8,
                _ => return Err("constant index requires an array or slice"),
            };
            let stride = self.layout(element)?.0.bytes();
            let offset = offset
                .bytes()
                .checked_add(index.checked_mul(stride).ok_or("slice index overflow")?)
                .ok_or("slice offset overflow")?;
            place.offset = Size::from_bytes(offset);
            place.value = self.read_value(alloc_id, place.offset, element, 0)?;
            return Ok(());
        }
        let FoldValue::Aggregate { ty, fields, variant: None } = place.value else {
            return Err("constant index requires a folded array value");
        };
        if fields.len() as u64 != len {
            return Err("folded array has a mismatched element count");
        }
        let index = usize::try_from(index).map_err(|_| "constant index is too large")?;
        let layout = self
            .tcx
            .layout_of(self.typing_env.as_query_input(ty))
            .map_err(|_| "projected array layout could not be computed")?;
        place.offset += layout.fields.offset(index);
        place.value = fields[index];
        Ok(())
    }

    fn eval_rvalue(
        &mut self,
        body: &mir::Body<'tcx>,
        locals: &FxHashMap<Local, FoldValue<'tcx>>,
        rvalue: &Rvalue<'tcx>,
        destination_ty: Ty<'tcx>,
        depth: usize,
    ) -> Result<FoldValue<'tcx>, &'static str> {
        match rvalue {
            Rvalue::Use(operand, _) => self.eval_operand(body, locals, operand, depth),
            Rvalue::UnaryOp(op, operand) => {
                let value = self.eval_operand(body, locals, operand, depth)?;
                self.eval_unary(*op, value, destination_ty)
            }
            Rvalue::BinaryOp(op, operands) => {
                let (left, right) = &**operands;
                let left = self.eval_operand(body, locals, left, depth)?;
                let right = self.eval_operand(body, locals, right, depth)?;
                self.eval_binary(*op, left, right, destination_ty)
            }
            Rvalue::Ref(_, _, place) | Rvalue::RawPtr(_, place) => {
                let pointee_ty = place.ty(&body.local_decls, self.tcx).ty;
                let target_pointee =
                    pointer_target(destination_ty).ok_or("borrow rvalue has a non-pointer type")?;
                if target_pointee != pointee_ty {
                    return Err("reference rvalue has a mismatched pointee type");
                }
                // A reborrow of a deferred pointer needs only its address. In
                // particular, &STATIC and &raw mut STATIC must not load static memory.
                if let Some((mir::ProjectionElem::Deref, prefix)) = place.projection.split_last() {
                    let base = mir::Place {
                        local: place.local,
                        projection: self.tcx.mk_place_elems(prefix),
                    };
                    if let FoldValue::Pointer { alloc_id, offset, .. } =
                        self.eval_place(body, locals, &base)?
                        && matches!(
                            self.tcx.try_get_global_alloc(alloc_id),
                            Some(GlobalAlloc::Static(_))
                        )
                    {
                        return Ok(FoldValue::Pointer { ty: destination_ty, alloc_id, offset });
                    }
                }
                let place = self.project_place(body, locals, place)?;
                if place.value.ty() != pointee_ty {
                    return Err("folded reference has a mismatched pointee type");
                }
                if let FoldValue::Slice { alloc_id, offset, len, .. } = place.value
                    && matches!(pointee_ty.kind(), ty::Slice(_) | ty::Str)
                {
                    return Ok(FoldValue::Slice { ty: destination_ty, alloc_id, offset, len });
                }
                let alloc_id = match place.alloc_id {
                    Some(alloc_id) => alloc_id,
                    None => self.allocate(place.root)?,
                };
                Ok(FoldValue::Reference {
                    ty: destination_ty,
                    pointee: self.tcx.arena.dropless.alloc(place.value),
                    alloc_id,
                    offset: place.offset,
                })
            }
            Rvalue::Cast(mir::CastKind::IntToInt, operand, cast_ty)
                if *cast_ty == destination_ty =>
            {
                let value = self.eval_operand(body, locals, operand, depth)?;
                self.cast_integer(value, destination_ty)
            }
            Rvalue::Cast(mir::CastKind::PointerWithExposedProvenance, operand, cast_ty)
                if *cast_ty == destination_ty && self.is_thin_raw_pointer(destination_ty) =>
            {
                let value = self.eval_operand(body, locals, operand, depth)?;
                let value = self.scalar_info(value)?;
                if !is_integer_ty(value.ty) {
                    return Err("raw address cast requires an integer");
                }
                let bits = if value.signed {
                    value.size.sign_extend(value.bits) as u128
                } else {
                    value.bits
                };
                self.scalar_value(bits, destination_ty)
            }
            Rvalue::Cast(mir::CastKind::PtrToPtr, operand, cast_ty)
                if *cast_ty == destination_ty
                    && self.is_thin_raw_pointer(destination_ty)
                    && self.is_thin_raw_pointer(operand.ty(&body.local_decls, self.tcx)) =>
            {
                let value = self.eval_operand(body, locals, operand, depth)?;
                match value {
                    FoldValue::Scalar { bits, .. } => self.scalar_value(bits, destination_ty),
                    FoldValue::Reference { pointee, alloc_id, offset, .. }
                        if pointer_target(destination_ty) != Some(pointee.ty()) =>
                    {
                        Ok(FoldValue::Pointer { ty: destination_ty, alloc_id, offset })
                    }
                    FoldValue::Pointer { alloc_id, offset, .. } => {
                        Ok(FoldValue::Pointer { ty: destination_ty, alloc_id, offset })
                    }
                    _ => self.retype(value, destination_ty),
                }
            }
            Rvalue::Cast(mir::CastKind::Subtype | mir::CastKind::PtrToPtr, operand, cast_ty)
                if *cast_ty == destination_ty =>
            {
                let value = self.eval_operand(body, locals, operand, depth)?;
                self.retype(value, destination_ty)
            }
            Rvalue::Repeat(operand, count) => {
                let count = count
                    .try_to_target_usize(self.tcx)
                    .ok_or("array repeat length is unresolved")?;
                if count > MAX_FOLD_STEPS as u64 {
                    return Err("array repeat exceeded its size bound");
                }
                let value = self.eval_operand(body, locals, operand, depth)?;
                Ok(self.aggregate(destination_ty, &vec![value; count as usize]))
            }
            Rvalue::ThreadLocalRef(..) => Err("thread-local static access is unsupported"),
            Rvalue::Cast(
                mir::CastKind::PointerCoercion(ty::adjustment::PointerCoercion::Unsize, _),
                operand,
                _,
            ) => {
                let value = self.eval_operand(body, locals, operand, depth)?;
                let FoldValue::Reference { pointee, alloc_id, offset, .. } = value else {
                    return Err("unsizing requires a folded reference");
                };
                let ty::Array(element, count) = *pointee.ty().kind() else {
                    return Err("only array-to-slice unsizing is supported");
                };
                let target =
                    pointer_target(destination_ty).ok_or("unsizing target is not a pointer")?;
                if !matches!(*target.kind(), ty::Slice(ty) if ty == element) {
                    return Err("unsizing changed the element type");
                }
                let len =
                    count.try_to_target_usize(self.tcx).ok_or("unsizing length is unresolved")?;
                Ok(FoldValue::Slice { ty: destination_ty, alloc_id, offset, len })
            }
            Rvalue::Cast(mir::CastKind::Transmute, operand, _) => {
                let value = self.eval_operand(body, locals, operand, depth)?;
                self.transmute(value, destination_ty, depth)
            }
            Rvalue::Cast(mir::CastKind::PointerCoercion(coercion, _), operand, cast_ty)
                if *cast_ty == destination_ty && matches!(destination_ty.kind(), ty::FnPtr(..)) =>
            {
                use ty::adjustment::PointerCoercion;
                let value = self.eval_operand(body, locals, operand, depth)?;
                let instance = match (*coercion, *value.ty().kind()) {
                    (PointerCoercion::ReifyFnPointer(_), ty::FnDef(def, args)) => {
                        Instance::resolve_for_fn_ptr(
                            self.tcx,
                            self.typing_env,
                            def,
                            args.skip_binder(),
                        )
                        .ok_or("function pointer instance is unresolved")?
                    }
                    (PointerCoercion::ClosureFnPointer(_), ty::Closure(def, args))
                        if self.layout(value.ty())?.0 == Size::ZERO =>
                    {
                        Instance::resolve_closure(self.tcx, def, args, ty::ClosureKind::FnOnce)
                    }
                    (PointerCoercion::UnsafeFnPointer, ty::FnPtr(..)) => {
                        return self.retype(value, destination_ty);
                    }
                    _ => return Err("unsupported function pointer coercion"),
                };
                if instance.args.has_non_region_param() {
                    return Err("function pointer instance still has generic parameters");
                }
                let alloc_id = self.tcx.reserve_and_set_fn_alloc(instance, CTFE_ALLOC_SALT);
                Ok(FoldValue::Pointer { ty: destination_ty, alloc_id, offset: Size::ZERO })
            }
            Rvalue::Cast(mir::CastKind::FnPtrToPtr, operand, cast_ty)
                if *cast_ty == destination_ty && self.is_thin_raw_pointer(destination_ty) =>
            {
                let value = self.eval_operand(body, locals, operand, depth)?;
                let FoldValue::Pointer { alloc_id, offset, .. } = value else {
                    return Err("function address cast requires a function pointer");
                };
                Ok(FoldValue::Pointer { ty: destination_ty, alloc_id, offset })
            }
            Rvalue::Cast(..) => Err("non-integer cast is unsupported"),
            Rvalue::Discriminant(place) => {
                let value = self.eval_place(body, locals, place)?;
                let FoldValue::Aggregate { ty, variant: Some(variant), .. } = value else {
                    return Err("discriminant read requires a folded enum variant");
                };
                let ty::Adt(def, _) = *ty.kind() else {
                    return Err("enum aggregate has a non-ADT type");
                };
                self.scalar_value(
                    def.discriminant_for_variant(self.tcx, variant).val,
                    destination_ty,
                )
            }
            Rvalue::Aggregate(kind, operands) => {
                if operands.len() > MAX_FOLD_STEPS {
                    return Err("aggregate exceeded its field bound");
                }
                let variant = match &**kind {
                    mir::AggregateKind::Array(_)
                    | mir::AggregateKind::Tuple
                    | mir::AggregateKind::Closure(..) => None,
                    mir::AggregateKind::Adt(def, variant, _, _, None) => {
                        let def = self.tcx.adt_def(*def);
                        if def.is_union() {
                            return Err("union aggregate construction is unsupported");
                        }
                        if def.variant(*variant).fields.len() != operands.len() {
                            return Err("aggregate has a mismatched field count");
                        }
                        def.is_enum().then_some(*variant)
                    }
                    _ => return Err("unsupported aggregate construction"),
                };
                let fields = operands
                    .iter()
                    .map(|operand| self.eval_operand(body, locals, operand, depth))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(FoldValue::Aggregate {
                    ty: destination_ty,
                    fields: self.tcx.arena.dropless.alloc_slice(&fields),
                    variant,
                })
            }
            Rvalue::CopyForDeref(place) => self.eval_place(body, locals, place),
            Rvalue::WrapUnsafeBinder(..) => Err("unsafe binder wrapping is unsupported"),
            Rvalue::Reborrow(..) => Err("reborrow is unsupported"),
        }
    }

    fn eval_layout_intrinsic(
        &mut self,
        body: &mir::Body<'tcx>,
        locals: &FxHashMap<Local, FoldValue<'tcx>>,
        func: &Operand<'tcx>,
        args: &[Spanned<Operand<'tcx>>],
        destination_ty: Ty<'tcx>,
        depth: usize,
    ) -> Result<FoldValue<'tcx>, &'static str> {
        let Some((def_id, generic_args)) = func.const_fn_def() else {
            return Err("call to a non-intrinsic function");
        };

        let is_size = self.tcx.is_intrinsic(def_id, sym::size_of);
        let is_align = self.tcx.is_intrinsic(def_id, sym::align_of);
        let is_size_val = self.tcx.is_intrinsic(def_id, sym::size_of_val);
        let is_align_val = self.tcx.is_intrinsic(def_id, sym::align_of_val);
        let is_needs_drop = self.tcx.is_intrinsic(def_id, sym::needs_drop);
        if self.tcx.is_intrinsic(def_id, sym::offset_of) {
            let ty = crate::rustc_const_eval::util::intrinsic_type_param(
                self.tcx,
                def_id,
                generic_args,
                0,
            )
            .ok_or("offset_of requires a concrete type")?;
            if args.len() != 2 || destination_ty != self.tcx.types.usize {
                return Err("offset_of has an invalid signature");
            }
            let variant = self.eval_operand(body, locals, &args[0].node, depth)?;
            let field = self.eval_operand(body, locals, &args[1].node, depth)?;
            let variant = self.scalar_info(variant)?;
            let field = self.scalar_info(field)?;
            if variant.ty != self.tcx.types.u32 || field.ty != self.tcx.types.u32 {
                return Err("offset_of indices must be u32");
            }
            let layout = self
                .tcx
                .layout_of(self.typing_env.as_query_input(ty))
                .map_err(|_| "offset_of layout is unavailable")?;
            let variant = VariantIdx::from_u32(variant.bits as u32);
            match layout.variants {
                Variants::Single { index } if variant == index => {}
                Variants::Multiple { ref variants, .. } if variant.as_usize() < variants.len() => {}
                _ => return Err("offset_of variant is out of bounds"),
            }
            let cx = LayoutCx::new(self.tcx, self.typing_env);
            let layout = layout.for_variant(&cx, variant);
            let field = field.bits as usize;
            if field >= layout.fields.count() {
                return Err("offset_of field is out of bounds");
            }
            // This is a compile-time intrinsic. Its placeholder MIR body is
            // unreachable and must never be executed as an ordinary const fn.
            return self.scalar_value(layout.fields.offset(field).bytes().into(), destination_ty);
        }
        if !is_size && !is_align && !is_size_val && !is_align_val && !is_needs_drop {
            if depth + 1 >= MAX_FOLD_DEPTH {
                return Err("constant call exceeded its depth bound");
            }
            let values = args
                .iter()
                .map(|arg| self.eval_operand(body, locals, &arg.node, depth))
                .collect::<Result<Vec<_>, _>>()?;
            if self.tcx.is_intrinsic(def_id, sym::type_id) {
                if !values.is_empty() {
                    return Err("type_id takes no value arguments");
                }
                let ty = crate::rustc_const_eval::util::intrinsic_type_param(
                    self.tcx,
                    def_id,
                    generic_args,
                    0,
                )
                .ok_or("type_id requires a concrete type")?;
                return self.type_id_value(ty, destination_ty);
            }
            if self.tcx.is_intrinsic(def_id, sym::ctpop) {
                let [value] = values.as_slice() else {
                    return Err("ctpop requires one argument");
                };
                let value = self.scalar_info(*value)?;
                if !is_integer_ty(value.ty) || !is_integer_ty(destination_ty) {
                    return Err("ctpop requires integer types");
                }
                return self.scalar_value(value.bits.count_ones().into(), destination_ty);
            }
            if self.tcx.is_intrinsic(def_id, sym::transmute) {
                let [value] = values.as_slice() else {
                    return Err("transmute requires one argument");
                };
                return self.transmute(*value, destination_ty, depth);
            }
            if destination_ty
                .ty_adt_def()
                .is_some_and(|def| self.tcx.lang_items().alloc_layout() == Some(def.did()))
                && self.tcx.item_name(def_id).as_str() == "from_size_align_unchecked"
                && self.tcx.associated_item(def_id).impl_container(self.tcx).is_some_and(|owner| {
                    self.tcx.type_of(owner).instantiate_identity().skip_norm_wip() == destination_ty
                })
            {
                let [size, align] = values.as_slice() else {
                    return Err("Layout constructor requires two arguments");
                };
                let size = self.scalar_info(*size)?.bits;
                let align = self.scalar_info(*align)?.bits;
                let pointer_bits = self.tcx.data_layout.pointer_size().bits();
                let max = (1u128 << (pointer_bits - 1)) - 1;
                if !align.is_power_of_two()
                    || size.checked_add(align - 1).is_none_or(|rounded| rounded > max)
                {
                    return Err("Layout constructor precondition failed");
                }
                let layout = self
                    .tcx
                    .layout_of(self.typing_env.as_query_input(destination_ty))
                    .map_err(|_| "Layout constructor result layout is unavailable")?;
                let cx = LayoutCx::new(self.tcx, self.typing_env);
                let size = self.scalar_value(size, layout.field(&cx, 0).ty)?;
                let align = self.scalar_value(align, layout.field(&cx, 1).ty)?;
                return Ok(self.aggregate(destination_ty, &[size, align]));
            }
            if !self.tcx.is_const_fn(def_id) {
                return Err("call to a non-const function");
            }
            let instance = Instance::try_resolve(self.tcx, self.typing_env, def_id, generic_args)
                .map_err(|_| "constant call resolution failed")?
                .ok_or("constant call is unresolved")?;
            if instance.args.has_non_region_param() {
                return Err("constant call still has generic parameters");
            }
            let uninstantiated = self.tcx.mir_for_ctfe(instance.def_id());
            let body = instance.instantiate_mir_and_normalize_erasing_regions(
                self.tcx,
                self.typing_env,
                ty::EarlyBinder::bind(self.tcx, uninstantiated.clone()),
            );
            if body.arg_count != values.len() {
                return Err("constant call argument count differs");
            }
            return self.eval_body_with_args(&body, depth + 1, &values);
        }

        let Some(ty) =
            crate::rustc_const_eval::util::intrinsic_type_param(self.tcx, def_id, generic_args, 0)
        else {
            return Err("layout intrinsic has no concrete type argument");
        };
        if is_needs_drop {
            if !args.is_empty() || !destination_ty.is_bool() {
                return Err("needs_drop has an invalid scalar signature");
            }
            return self
                .scalar_value(ty.needs_drop(self.tcx, self.typing_env) as u128, destination_ty);
        }
        if is_size_val || is_align_val {
            if args.len() != 1 {
                return Err("size_of_val or align_of_val call has an unexpected argument count");
            }
            let argument = self.eval_operand(body, locals, &args[0].node, depth)?;
            if pointer_target(argument.ty()) != Some(ty) {
                return Err("size_of_val or align_of_val argument has a mismatched pointee type");
            }
            if let FoldValue::Slice { len, .. } = argument {
                let (stride, align) = match *ty.kind() {
                    ty::Slice(element) => self.layout(element)?,
                    ty::Str => (Size::from_bytes(1), 1),
                    _ => return Err("unsupported dynamic layout"),
                };
                let size = stride.bytes().checked_mul(len).ok_or("dynamic layout size overflow")?;
                if size >= self.tcx.data_layout.obj_size_bound() {
                    return Err("dynamic layout exceeds the target object size bound");
                }
                return self
                    .scalar_value(if is_align_val { align } else { size }.into(), destination_ty);
            }
            if !matches!(argument, FoldValue::Reference { .. }) {
                return Err("layout argument is not a folded pointer");
            }
        } else if !args.is_empty() {
            return Err("size_of or align_of call has arguments");
        }

        if !ty.is_sized(self.tcx, self.typing_env) {
            return Err("layout intrinsic has an unsized type");
        }

        let (size, align) = self.layout(ty)?;
        let value = if is_align || is_align_val { align as u128 } else { size.bytes() as u128 };
        self.scalar_value(value, destination_ty)
    }

    fn type_id_value(
        &self,
        ty: Ty<'tcx>,
        result_ty: Ty<'tcx>,
    ) -> Result<FoldValue<'tcx>, &'static str> {
        if result_ty
            .ty_adt_def()
            .is_none_or(|def| Some(def.did()) != self.tcx.lang_items().type_id())
        {
            return Err("type_id has an invalid result type");
        }
        let layout = self
            .tcx
            .layout_of(self.typing_env.as_query_input(result_ty))
            .map_err(|_| "type_id result layout is unavailable")?;
        if layout.size.bytes() != 16 || layout.fields.count() != 1 {
            return Err("unsupported TypeId representation");
        }
        let cx = LayoutCx::new(self.tcx, self.typing_env);
        let array_ty = layout.field(&cx, 0).ty;
        let ty::Array(element, count) = *array_ty.kind() else {
            return Err("TypeId field is not an array");
        };
        let pointer_size = self.tcx.data_layout.pointer_size().bytes() as usize;
        if !matches!(pointer_size, 4 | 8)
            || count.try_to_target_usize(self.tcx) != Some((16 / pointer_size) as u64)
            || !matches!(element.kind(), ty::RawPtr(target, _) if target.is_unit())
        {
            return Err("unsupported TypeId hash fragment representation");
        }
        let ty = self.tcx.erase_and_anonymize_regions(ty);
        let mut bytes = [0; 16];
        write_target_uint(
            self.tcx.data_layout.endian,
            &mut bytes,
            self.tcx.type_id_hash(ty).as_u128(),
        )
        .map_err(|_| "TypeId hash could not be encoded")?;
        let alloc_id = self.tcx.reserve_and_set_type_id_alloc(ty);
        // Each target-sized hash fragment retains the type's symbolic provenance.
        let fields = bytes
            .chunks_exact(pointer_size)
            .map(|chunk| {
                let bits = read_target_uint(self.tcx.data_layout.endian, chunk)
                    .map_err(|_| "TypeId hash fragment could not be decoded")?;
                Ok(FoldValue::Pointer {
                    ty: element,
                    alloc_id,
                    offset: Size::from_bytes(bits as u64),
                })
            })
            .collect::<Result<Vec<_>, &'static str>>()?;
        Ok(self.aggregate(result_ty, &[self.aggregate(array_ty, &fields)]))
    }

    fn eval_unary(
        &self,
        op: mir::UnOp,
        value: FoldValue<'tcx>,
        result_ty: Ty<'tcx>,
    ) -> Result<FoldValue<'tcx>, &'static str> {
        if op == mir::UnOp::PtrMetadata {
            return match value {
                FoldValue::Slice { len, .. } if result_ty == self.tcx.types.usize => {
                    self.scalar_value(len.into(), result_ty)
                }
                FoldValue::Reference { .. } | FoldValue::Pointer { .. } => {
                    self.zero_sized(result_ty)
                }
                _ => Err("pointer metadata requires a folded pointer"),
            };
        }
        if let FoldValue::Scalar { bits, ty } = value
            && let ty::Float(float) = ty.kind()
        {
            if ty != result_ty || op != mir::UnOp::Neg {
                return Err("unsupported floating-point unary operation");
            }
            let bits = match float {
                ty::FloatTy::F32 => (-Single::from_bits(bits)).to_bits(),
                ty::FloatTy::F64 => (-Double::from_bits(bits)).to_bits(),
                _ => return Err("unsupported floating-point width"),
            };
            return self.scalar_value(bits, result_ty);
        }
        let value = self.scalar_info(value)?;
        if value.ty != result_ty {
            return Err("unary operation changed the operand type");
        }

        match op {
            mir::UnOp::Not => {
                let bits = if value.ty.is_bool() {
                    value.bits ^ 1
                } else {
                    !value.bits & bit_mask(value.width)
                };
                self.scalar_value(bits, result_ty)
            }
            mir::UnOp::Neg => {
                if !value.signed {
                    return Err("negation of a non-signed integer is unsupported");
                }
                let integer = value.size.sign_extend(value.bits);
                let Some(negated) = integer.checked_neg() else {
                    return Err("integer negation overflow");
                };
                if !fits_signed(negated, value.width) {
                    return Err("integer negation overflow");
                }
                self.scalar_value(negated as u128, result_ty)
            }
            mir::UnOp::PtrMetadata => Err("pointer metadata operation is unsupported"),
        }
    }

    fn eval_binary(
        &self,
        op: mir::BinOp,
        left: FoldValue<'tcx>,
        right: FoldValue<'tcx>,
        result_ty: Ty<'tcx>,
    ) -> Result<FoldValue<'tcx>, &'static str> {
        if let (FoldValue::Scalar { bits: a, ty }, FoldValue::Scalar { bits: b, ty: rhs }) =
            (left, right)
            && let ty::Float(float) = ty.kind()
        {
            if ty != rhs {
                return Err("floating-point operation has mismatched operand types");
            }
            return match float {
                ty::FloatTy::F32 => self.eval_float_binary::<Single>(op, a, b, ty, result_ty),
                ty::FloatTy::F64 => self.eval_float_binary::<Double>(op, a, b, ty, result_ty),
                _ => Err("unsupported floating-point width"),
            };
        }
        let left = self.scalar_info(left)?;
        let right = self.scalar_info(right)?;

        if left.ty.is_bool() || right.ty.is_bool() {
            if left.ty != right.ty || !left.ty.is_bool() {
                return Err("boolean operation has mismatched operand types");
            }
            return match op {
                mir::BinOp::BitAnd => self.scalar_value(left.bits & right.bits, result_ty),
                mir::BinOp::BitOr => self.scalar_value(left.bits | right.bits, result_ty),
                mir::BinOp::BitXor => self.scalar_value(left.bits ^ right.bits, result_ty),
                mir::BinOp::Eq
                | mir::BinOp::Ne
                | mir::BinOp::Lt
                | mir::BinOp::Le
                | mir::BinOp::Ge
                | mir::BinOp::Gt => self.compare(op, left, right, result_ty),
                _ => Err("unsupported boolean arithmetic operator"),
            };
        }

        if !is_integer_ty(left.ty) || !is_integer_ty(right.ty) {
            return Err("binary operation requires integer operands");
        }

        match op {
            mir::BinOp::Eq
            | mir::BinOp::Ne
            | mir::BinOp::Lt
            | mir::BinOp::Le
            | mir::BinOp::Ge
            | mir::BinOp::Gt => self.compare(op, left, right, result_ty),
            mir::BinOp::BitAnd => {
                self.require_same_integer_types(left, right, result_ty)?;
                self.scalar_value(left.bits & right.bits, result_ty)
            }
            mir::BinOp::BitOr => {
                self.require_same_integer_types(left, right, result_ty)?;
                self.scalar_value(left.bits | right.bits, result_ty)
            }
            mir::BinOp::BitXor => {
                self.require_same_integer_types(left, right, result_ty)?;
                self.scalar_value(left.bits ^ right.bits, result_ty)
            }
            mir::BinOp::Shl
            | mir::BinOp::ShlUnchecked
            | mir::BinOp::Shr
            | mir::BinOp::ShrUnchecked => self.shift(op, left, right, result_ty),
            mir::BinOp::Add
            | mir::BinOp::AddUnchecked
            | mir::BinOp::Sub
            | mir::BinOp::SubUnchecked
            | mir::BinOp::Mul
            | mir::BinOp::MulUnchecked => {
                self.require_same_integer_types(left, right, result_ty)?;
                self.checked_arithmetic(op, left, right, result_ty)
            }
            mir::BinOp::Div | mir::BinOp::Rem => {
                self.require_same_integer_types(left, right, result_ty)?;
                self.divide_or_remainder(op, left, right, result_ty)
            }
            mir::BinOp::AddWithOverflow
            | mir::BinOp::SubWithOverflow
            | mir::BinOp::MulWithOverflow => {
                self.overflowing_arithmetic(op, left, right, result_ty)
            }
            mir::BinOp::Cmp => Err("three-way comparison result is unsupported"),
            mir::BinOp::Offset => Err("pointer offset is unsupported"),
        }
    }

    fn overflowing_arithmetic(
        &self,
        op: mir::BinOp,
        left: ScalarInfo<'tcx>,
        right: ScalarInfo<'tcx>,
        result_ty: Ty<'tcx>,
    ) -> Result<FoldValue<'tcx>, &'static str> {
        let ty::Tuple(fields) = result_ty.kind() else {
            return Err("overflow-reporting arithmetic requires a tuple result");
        };
        if left.ty != right.ty || fields.len() != 2 || fields[0] != left.ty || !fields[1].is_bool()
        {
            return Err("overflow-reporting arithmetic has mismatched types");
        }
        let (bits, overflow) = if left.signed {
            let a = left.size.sign_extend(left.bits);
            let b = right.size.sign_extend(right.bits);
            let (value, overflow) = match op {
                mir::BinOp::AddWithOverflow => a.overflowing_add(b),
                mir::BinOp::SubWithOverflow => a.overflowing_sub(b),
                mir::BinOp::MulWithOverflow => a.overflowing_mul(b),
                _ => return Err("unsupported overflow-reporting arithmetic"),
            };
            (value as u128, overflow || !fits_signed(value, left.width))
        } else {
            let (value, overflow) = match op {
                mir::BinOp::AddWithOverflow => left.bits.overflowing_add(right.bits),
                mir::BinOp::SubWithOverflow => left.bits.overflowing_sub(right.bits),
                mir::BinOp::MulWithOverflow => left.bits.overflowing_mul(right.bits),
                _ => return Err("unsupported overflow-reporting arithmetic"),
            };
            (value, overflow || value > bit_mask(left.width))
        };
        Ok(self.aggregate(
            result_ty,
            &[self.scalar_value(bits, left.ty)?, self.scalar_value(overflow.into(), fields[1])?],
        ))
    }

    fn eval_float_binary<F: Float>(
        &self,
        op: mir::BinOp,
        a: u128,
        b: u128,
        operand_ty: Ty<'tcx>,
        result_ty: Ty<'tcx>,
    ) -> Result<FoldValue<'tcx>, &'static str> {
        let (a, b) = (F::from_bits(a), F::from_bits(b));
        let comparison = match op {
            mir::BinOp::Eq => Some(a == b),
            mir::BinOp::Ne => Some(a != b),
            mir::BinOp::Lt => Some(a < b),
            mir::BinOp::Le => Some(a <= b),
            mir::BinOp::Gt => Some(a > b),
            mir::BinOp::Ge => Some(a >= b),
            _ => None,
        };
        if let Some(result) = comparison {
            if !result_ty.is_bool() {
                return Err("floating-point comparison has a non-boolean result");
            }
            return self.scalar_value(result.into(), result_ty);
        }
        if operand_ty != result_ty {
            return Err("floating-point arithmetic changed the result type");
        }
        // IEEE exceptions produce infinities or NaNs, not integer-style errors.
        let result = match op {
            mir::BinOp::Add => a + b,
            mir::BinOp::Sub => a - b,
            mir::BinOp::Mul => a * b,
            mir::BinOp::Div => a / b,
            mir::BinOp::Rem => a % b,
            _ => return Err("unsupported floating-point binary operation"),
        };
        self.scalar_value(result.value.to_bits(), result_ty)
    }

    fn require_same_integer_types(
        &self,
        left: ScalarInfo<'tcx>,
        right: ScalarInfo<'tcx>,
        result_ty: Ty<'tcx>,
    ) -> Result<(), &'static str> {
        if left.ty != right.ty || left.ty != result_ty {
            return Err("integer operation has mismatched operand or result types");
        }
        Ok(())
    }

    fn compare(
        &self,
        op: mir::BinOp,
        left: ScalarInfo<'tcx>,
        right: ScalarInfo<'tcx>,
        result_ty: Ty<'tcx>,
    ) -> Result<FoldValue<'tcx>, &'static str> {
        if left.ty != right.ty || !result_ty.is_bool() {
            return Err("comparison has mismatched operand or result types");
        }

        let ordering = if left.signed {
            left.size.sign_extend(left.bits).cmp(&right.size.sign_extend(right.bits))
        } else {
            left.bits.cmp(&right.bits)
        };
        let result = match op {
            mir::BinOp::Eq => ordering == Ordering::Equal,
            mir::BinOp::Ne => ordering != Ordering::Equal,
            mir::BinOp::Lt => ordering == Ordering::Less,
            mir::BinOp::Le => ordering != Ordering::Greater,
            mir::BinOp::Ge => ordering != Ordering::Less,
            mir::BinOp::Gt => ordering == Ordering::Greater,
            _ => return Err("unsupported comparison operator"),
        };
        self.scalar_value(result as u128, result_ty)
    }

    fn checked_arithmetic(
        &self,
        op: mir::BinOp,
        left: ScalarInfo<'tcx>,
        right: ScalarInfo<'tcx>,
        result_ty: Ty<'tcx>,
    ) -> Result<FoldValue<'tcx>, &'static str> {
        let overflow_reason = match op {
            mir::BinOp::Add | mir::BinOp::AddUnchecked => "integer addition overflow",
            mir::BinOp::Sub | mir::BinOp::SubUnchecked => "integer subtraction overflow",
            mir::BinOp::Mul | mir::BinOp::MulUnchecked => "integer multiplication overflow",
            _ => "unsupported integer arithmetic operator",
        };

        let bits = if left.signed {
            let a = left.size.sign_extend(left.bits);
            let b = right.size.sign_extend(right.bits);
            let result = match op {
                mir::BinOp::Add | mir::BinOp::AddUnchecked => a.checked_add(b),
                mir::BinOp::Sub | mir::BinOp::SubUnchecked => a.checked_sub(b),
                mir::BinOp::Mul | mir::BinOp::MulUnchecked => a.checked_mul(b),
                _ => None,
            }
            .ok_or(overflow_reason)?;
            if !fits_signed(result, left.width) {
                return Err(overflow_reason);
            }
            result as u128
        } else {
            let result = match op {
                mir::BinOp::Add | mir::BinOp::AddUnchecked => left.bits.checked_add(right.bits),
                mir::BinOp::Sub | mir::BinOp::SubUnchecked => left.bits.checked_sub(right.bits),
                mir::BinOp::Mul | mir::BinOp::MulUnchecked => left.bits.checked_mul(right.bits),
                _ => None,
            }
            .ok_or(overflow_reason)?;
            if result > bit_mask(left.width) {
                return Err(overflow_reason);
            }
            result
        };

        self.scalar_value(bits, result_ty)
    }

    fn divide_or_remainder(
        &self,
        op: mir::BinOp,
        left: ScalarInfo<'tcx>,
        right: ScalarInfo<'tcx>,
        result_ty: Ty<'tcx>,
    ) -> Result<FoldValue<'tcx>, &'static str> {
        if right.bits == 0 {
            return Err(if op == mir::BinOp::Div {
                "integer division by zero"
            } else {
                "integer remainder by zero"
            });
        }

        let bits = if left.signed {
            let a = left.size.sign_extend(left.bits);
            let b = right.size.sign_extend(right.bits);
            if op == mir::BinOp::Rem && a == signed_min(left.width) && b == -1 {
                return Err("signed remainder overflow");
            }
            let result = if op == mir::BinOp::Div { a.checked_div(b) } else { a.checked_rem(b) }
                .ok_or(if op == mir::BinOp::Div {
                    "signed division overflow"
                } else {
                    "signed remainder overflow"
                })?;
            if !fits_signed(result, left.width) {
                return Err(if op == mir::BinOp::Div {
                    "signed division overflow"
                } else {
                    "signed remainder overflow"
                });
            }
            result as u128
        } else if op == mir::BinOp::Div {
            left.bits.checked_div(right.bits).ok_or("integer division overflow")?
        } else {
            left.bits.checked_rem(right.bits).ok_or("integer remainder overflow")?
        };

        self.scalar_value(bits, result_ty)
    }

    fn shift(
        &self,
        op: mir::BinOp,
        left: ScalarInfo<'tcx>,
        right: ScalarInfo<'tcx>,
        result_ty: Ty<'tcx>,
    ) -> Result<FoldValue<'tcx>, &'static str> {
        if left.ty != result_ty {
            return Err("shift operation changed the left operand type");
        }
        if !is_integer_ty(right.ty) {
            return Err("shift amount is not an integer");
        }

        let strict = matches!(op, mir::BinOp::ShlUnchecked | mir::BinOp::ShrUnchecked);
        let amount = self.shift_amount(right, left.width, strict)?;
        let bits = match op {
            mir::BinOp::Shl | mir::BinOp::ShlUnchecked => {
                (left.bits << amount) & bit_mask(left.width)
            }
            mir::BinOp::Shr | mir::BinOp::ShrUnchecked if left.signed => {
                (left.size.sign_extend(left.bits) >> amount) as u128 & bit_mask(left.width)
            }
            mir::BinOp::Shr | mir::BinOp::ShrUnchecked => left.bits >> amount,
            _ => return Err("unsupported shift operator"),
        };
        self.scalar_value(bits, result_ty)
    }

    fn shift_amount(
        &self,
        value: ScalarInfo<'tcx>,
        width: u32,
        strict: bool,
    ) -> Result<u32, &'static str> {
        if value.signed {
            let amount = value.size.sign_extend(value.bits);
            if strict {
                if amount < 0 || amount >= i128::from(width) {
                    return Err("unchecked shift amount is out of range");
                }
                Ok(amount as u32)
            } else {
                Ok(amount.rem_euclid(i128::from(width)) as u32)
            }
        } else if strict {
            if value.bits >= u128::from(width) {
                return Err("unchecked shift amount is out of range");
            }
            Ok(value.bits as u32)
        } else {
            Ok((value.bits % u128::from(width)) as u32)
        }
    }

    fn is_thin_raw_pointer(&self, ty: Ty<'tcx>) -> bool {
        matches!(ty.kind(), ty::RawPtr(..))
            && self.layout(ty).is_ok_and(|(size, _)| size == self.tcx.data_layout.pointer_size())
    }

    fn cast_integer(
        &self,
        value: FoldValue<'tcx>,
        destination_ty: Ty<'tcx>,
    ) -> Result<FoldValue<'tcx>, &'static str> {
        let value = self.scalar_info(value)?;
        if !is_integer_ty(destination_ty) {
            return Err("integer cast has a non-integer destination");
        }

        let (destination_size, _) = self.layout(destination_ty)?;
        let destination_width = bit_width(destination_size)?;
        let bits = if value.signed && destination_width > value.width {
            value.size.sign_extend(value.bits) as u128
        } else {
            value.bits
        };
        self.scalar_value(bits, destination_ty)
    }

    fn retype(
        &self,
        value: FoldValue<'tcx>,
        destination_ty: Ty<'tcx>,
    ) -> Result<FoldValue<'tcx>, &'static str> {
        match value {
            FoldValue::Scalar { bits, ty } => {
                let (source_size, _) = self.layout(ty)?;
                let (destination_size, _) = self.layout(destination_ty)?;
                if source_size != destination_size {
                    return Err("subtype cast changed scalar size");
                }
                self.scalar_value(bits, destination_ty)
            }
            FoldValue::ZeroSized { .. } => self.zero_sized(destination_ty),
            FoldValue::Reference { pointee, alloc_id, offset, .. } => {
                let destination_pointee = pointer_target(destination_ty)
                    .ok_or("pointer cast has a non-pointer target")?;
                if pointee.ty() != destination_pointee {
                    return Err("subtype cast changed a reference pointee");
                }
                Ok(FoldValue::Reference { ty: destination_ty, pointee, alloc_id, offset })
            }
            FoldValue::Slice { ty, alloc_id, offset, len } => {
                let source = pointer_target(ty);
                let target = pointer_target(destination_ty);
                // str and [u8] share their byte storage and length metadata.
                let byte_slice = |ty: Option<Ty<'tcx>>| {
                    ty.is_some_and(|ty| matches!(ty.kind(), ty::Str)
                        || matches!(ty.kind(), ty::Slice(element) if *element == self.tcx.types.u8))
                };
                if target.is_none()
                    || (source != target && !(byte_slice(source) && byte_slice(target)))
                {
                    return Err("pointer cast changed a slice pointee");
                }
                Ok(FoldValue::Slice { ty: destination_ty, alloc_id, offset, len })
            }
            FoldValue::Pointer { ty, alloc_id, offset } => {
                if matches!(ty.kind(), ty::FnPtr(..))
                    && matches!(destination_ty.kind(), ty::FnPtr(..))
                {
                    return Ok(FoldValue::Pointer { ty: destination_ty, alloc_id, offset });
                }
                let static_reference = matches!(destination_ty.kind(), ty::Ref(..))
                    && matches!(
                        self.tcx.try_get_global_alloc(alloc_id),
                        Some(GlobalAlloc::Static(_))
                    );
                if pointer_target(ty) != pointer_target(destination_ty)
                    || !(matches!(destination_ty.kind(), ty::RawPtr(..)) || static_reference)
                {
                    return Err("symbolic pointer cast changed its pointee");
                }
                Ok(FoldValue::Pointer { ty: destination_ty, alloc_id, offset })
            }
            _ => Err("aggregate reinterpretation is unsupported"),
        }
    }

    fn bool_value(&self, value: FoldValue<'tcx>) -> Result<bool, &'static str> {
        let value = self.scalar_info(value)?;
        if !value.ty.is_bool() {
            return Err("control-flow condition is not boolean");
        }
        Ok(value.bits == 1)
    }
}

fn pointer_target(ty: Ty<'_>) -> Option<Ty<'_>> {
    match *ty.kind() {
        ty::Ref(_, target, _) | ty::RawPtr(target, _) => Some(target),
        _ => None,
    }
}

fn is_integer_ty(ty: Ty<'_>) -> bool {
    matches!(ty.kind(), ty::Int(_) | ty::Uint(_))
}

fn bit_width(size: Size) -> Result<u32, &'static str> {
    let bits = size.bits();
    if bits == 0 || bits > 128 {
        return Err("integer width is outside the supported range");
    }
    Ok(bits as u32)
}

fn bit_mask(width: u32) -> u128 {
    if width == 128 { u128::MAX } else { (1u128 << width) - 1 }
}

fn signed_min(width: u32) -> i128 {
    if width == 128 { i128::MIN } else { -(1i128 << (width - 1)) }
}

fn fits_signed(value: i128, width: u32) -> bool {
    if width == 128 {
        true
    } else {
        let min = signed_min(width);
        let max = (1i128 << (width - 1)) - 1;
        value >= min && value <= max
    }
}
