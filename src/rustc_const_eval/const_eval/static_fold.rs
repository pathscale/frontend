use alloc::vec;
use alloc::vec::Vec;
use core::cmp::Ordering;

use crate::rustc_abi::Size;
use crate::rustc_data_structures::fx::FxHashMap;
use crate::rustc_hir::def::DefKind;
use crate::rustc_middle::mir::interpret::{
    AllocId, Allocation, GlobalId, Pointer, Scalar, alloc_range,
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
    Reference { ty: Ty<'tcx>, pointee_ty: Ty<'tcx>, alloc_id: AllocId },
    Aggregate { ty: Ty<'tcx>, fields: &'tcx [FoldValue<'tcx>] },
    Slice { ty: Ty<'tcx>, alloc_id: AllocId, len: u64 },
}

impl<'tcx> FoldValue<'tcx> {
    fn ty(self) -> Ty<'tcx> {
        match self {
            FoldValue::Scalar { ty, .. }
            | FoldValue::ZeroSized { ty }
            | FoldValue::Reference { ty, .. }
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
            FoldValue::Reference { alloc_id, .. } => Ok(ConstValue::Scalar(Scalar::from_pointer(
                Pointer::new(alloc_id.into(), Size::ZERO),
                &self.tcx,
            ))),
            FoldValue::Slice { alloc_id, len, .. } => Ok(ConstValue::Slice { alloc_id, meta: len }),
            FoldValue::Aggregate { ty, fields } => {
                let layout = self
                    .tcx
                    .layout_of(self.typing_env.as_query_input(ty))
                    .map_err(|_| "aggregate layout could not be computed")?;
                if layout.is_zst() {
                    return Ok(ConstValue::ZeroSized);
                }
                if layout.backend_repr.is_scalar() {
                    for field in fields {
                        if self.layout(field.ty())?.0.bytes() != 0 {
                            return self.to_const(*field);
                        }
                    }
                }
                Ok(ConstValue::Indirect { alloc_id: self.allocate(value)?, offset: Size::ZERO })
            }
        }
    }

    fn allocate(&self, value: FoldValue<'tcx>) -> Result<AllocId, &'static str> {
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
        allocation.mutability = mir::Mutability::Not;
        Ok(self.tcx.reserve_and_set_memory_alloc(self.tcx.mk_const_alloc(allocation)))
    }

    fn write_value(
        &self,
        allocation: &mut Allocation,
        offset: Size,
        value: FoldValue<'tcx>,
    ) -> Result<(), &'static str> {
        if let FoldValue::Aggregate { ty, fields } = value {
            let layout = self
                .tcx
                .layout_of(self.typing_env.as_query_input(ty))
                .map_err(|_| "aggregate layout could not be computed")?;
            for (index, field) in fields.iter().enumerate() {
                self.write_value(allocation, offset + layout.fields.offset(index), *field)?;
            }
            return Ok(());
        }
        let size = self.layout(value.ty())?.0;
        if size.bytes() == 0 {
            return Ok(());
        }
        let scalar = match value {
            FoldValue::Reference { alloc_id, .. } => {
                Scalar::from_pointer(Pointer::new(alloc_id.into(), Size::ZERO), &self.tcx)
            }
            FoldValue::Slice { alloc_id, len, .. } => {
                let pointer_size = self.tcx.data_layout.pointer_size();
                allocation
                    .write_scalar(
                        &self.tcx,
                        alloc_range(offset, pointer_size),
                        Scalar::from_pointer(Pointer::new(alloc_id.into(), Size::ZERO), &self.tcx),
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
        FoldValue::Aggregate { ty, fields: self.tcx.arena.dropless.alloc_slice(fields) }
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
        if matches!(scalar.primitive(), crate::rustc_abi::Primitive::Pointer(_))
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
        if let Some(scalar) = constant.try_to_scalar_int() {
            let ty = constant.ty();
            let (size, _) = self.layout(ty)?;
            return self.scalar_value(scalar.to_bits(size), ty);
        }

        match constant {
            mir::Const::Val(ConstValue::ZeroSized, ty) => self.zero_sized(ty),
            mir::Const::Val(ConstValue::Scalar(_), _) => Err("raw pointer constant is unsupported"),
            mir::Const::Val(ConstValue::Slice { alloc_id, meta }, ty) => {
                Ok(FoldValue::Slice { ty, alloc_id, len: meta })
            }
            mir::Const::Val(ConstValue::Indirect { .. }, _) => {
                Err("allocation-backed constant is unsupported")
            }
            mir::Const::Ty(value_ty, type_const) => match type_const.kind() {
                ty::ConstKind::Value(_) => {
                    if self.layout(value_ty)?.0.bytes() == 0 {
                        self.zero_sized(value_ty)
                    } else {
                        Err("aggregate type-level constant is unsupported")
                    }
                }
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
                    return Err("unreachable terminator is unsupported");
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
        &self,
        _body: &mir::Body<'tcx>,
        locals: &FxHashMap<Local, FoldValue<'tcx>>,
        place: &mir::Place<'tcx>,
    ) -> Result<FoldValue<'tcx>, &'static str> {
        let mut value =
            *locals.get(&place.local).ok_or("MIR reads a local without a folded value")?;
        for projection in place.projection {
            match (projection, value) {
                (mir::ProjectionElem::Field(index, _), FoldValue::Aggregate { fields, .. }) => {
                    value =
                        *fields.get(index.as_usize()).ok_or("constant field is out of bounds")?;
                }
                _ => return Err("unsupported constant place projection"),
            }
        }
        Ok(value)
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
            Rvalue::Ref(_, _, place) => {
                if !place.projection.is_empty() {
                    return Err("reference to a projected place is unsupported");
                }
                let pointee_ty = place.ty(&body.local_decls, self.tcx).ty;
                let ty::Ref(_, target_pointee, _) = *destination_ty.kind() else {
                    return Err("reference rvalue has a non-reference type");
                };
                if target_pointee != pointee_ty {
                    return Err("reference rvalue has a mismatched pointee type");
                }
                let value = self.eval_place(body, locals, place)?;
                let alloc_id = self.allocate(value)?;
                Ok(FoldValue::Reference { ty: destination_ty, pointee_ty, alloc_id })
            }
            Rvalue::Cast(mir::CastKind::IntToInt, operand, cast_ty)
                if *cast_ty == destination_ty =>
            {
                let value = self.eval_operand(body, locals, operand, depth)?;
                self.cast_integer(value, destination_ty)
            }
            Rvalue::Cast(mir::CastKind::Subtype, operand, cast_ty)
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
            Rvalue::RawPtr(..) => Err("raw pointer construction is unsupported"),
            Rvalue::Cast(
                mir::CastKind::PointerCoercion(ty::adjustment::PointerCoercion::Unsize, _),
                operand,
                _,
            ) => {
                let value = self.eval_operand(body, locals, operand, depth)?;
                let FoldValue::Reference { pointee_ty, alloc_id, .. } = value else {
                    return Err("unsizing requires a folded reference");
                };
                let ty::Array(element, count) = *pointee_ty.kind() else {
                    return Err("only array-to-slice unsizing is supported");
                };
                let ty::Ref(_, target, _) = *destination_ty.kind() else {
                    return Err("unsizing target is not a reference");
                };
                if !matches!(*target.kind(), ty::Slice(ty) if ty == element) {
                    return Err("unsizing changed the element type");
                }
                let len =
                    count.try_to_target_usize(self.tcx).ok_or("unsizing length is unresolved")?;
                Ok(FoldValue::Slice { ty: destination_ty, alloc_id, len })
            }
            Rvalue::Cast(mir::CastKind::Transmute, operand, _) => {
                let value = self.eval_operand(body, locals, operand, depth)?;
                self.retype(value, destination_ty)
            }
            Rvalue::Cast(..) => Err("non-integer cast is unsupported"),
            Rvalue::Discriminant(..) => Err("discriminant read is unsupported"),
            Rvalue::Aggregate(kind, operands) => {
                if operands.len() > MAX_FOLD_STEPS {
                    return Err("aggregate exceeded its field bound");
                }
                let supported = match &**kind {
                    mir::AggregateKind::Array(_) | mir::AggregateKind::Tuple => true,
                    mir::AggregateKind::Adt(def, _, _, _, _) => self.tcx.adt_def(*def).is_struct(),
                    _ => false,
                };
                if !supported {
                    return Err("unsupported aggregate construction");
                }
                let fields = operands
                    .iter()
                    .map(|operand| self.eval_operand(body, locals, operand, depth))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(self.aggregate(destination_ty, &fields))
            }
            Rvalue::CopyForDeref(..) => Err("deref copy is unsupported"),
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
        if !is_size && !is_align && !is_size_val && !is_align_val && !is_needs_drop {
            if depth + 1 >= MAX_FOLD_DEPTH {
                return Err("constant call exceeded its depth bound");
            }
            let values = args
                .iter()
                .map(|arg| self.eval_operand(body, locals, &arg.node, depth))
                .collect::<Result<Vec<_>, _>>()?;
            if self.tcx.is_intrinsic(def_id, sym::transmute) {
                let [value] = values.as_slice() else {
                    return Err("transmute requires one argument");
                };
                return self.retype(*value, destination_ty);
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
        if !ty.is_sized(self.tcx, self.typing_env) {
            return Err("layout intrinsic has an unsized type");
        }

        if is_size_val || is_align_val {
            if args.len() != 1 {
                return Err("size_of_val or align_of_val call has an unexpected argument count");
            }
            let argument = self.eval_operand(body, locals, &args[0].node, depth)?;
            let FoldValue::Reference { pointee_ty, .. } = argument else {
                return Err("size_of_val or align_of_val argument is not a folded local reference");
            };
            if pointee_ty != ty {
                return Err("size_of_val or align_of_val argument has a mismatched pointee type");
            }
        } else if !args.is_empty() {
            return Err("size_of or align_of call has arguments");
        }

        let (size, align) = self.layout(ty)?;
        let value = if is_align || is_align_val { align as u128 } else { size.bytes() as u128 };
        self.scalar_value(value, destination_ty)
    }

    fn eval_unary(
        &self,
        op: mir::UnOp,
        value: FoldValue<'tcx>,
        result_ty: Ty<'tcx>,
    ) -> Result<FoldValue<'tcx>, &'static str> {
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
                Err("overflow-reporting arithmetic produces an aggregate")
            }
            mir::BinOp::Cmp => Err("three-way comparison result is unsupported"),
            mir::BinOp::Offset => Err("pointer offset is unsupported"),
        }
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
            FoldValue::Reference { pointee_ty, alloc_id, .. } => {
                let ty::Ref(_, destination_pointee, _) = *destination_ty.kind() else {
                    return Err("subtype cast changed a reference into a non-reference");
                };
                if pointee_ty != destination_pointee {
                    return Err("subtype cast changed a reference pointee");
                }
                Ok(FoldValue::Reference { ty: destination_ty, pointee_ty, alloc_id })
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
