//! Running a call through rustc's MIR interpreter: the machine [`super::evaluate`] runs on.
//!
//! **One semantics, rustc's.** Every rule of what a program does is `rustc_const_eval::interpret`
//! (`InterpCx`), the engine CTFE and Miri are built on, stepping the optimized MIR rustc built for
//! the source and for every library function it calls. What lives here is only the policy an
//! interpreter's `Machine` is for: which calls are served (heap allocation, from the
//! interpreter's own memory), which are refused by name (every foreign function, every syscall,
//! file and network), how a panic ends the run (its message, read and returned), and how the
//! finished value is handed to its own `Debug` to be rendered.
//!
//! **What of the operating system is served, and why only that.** A program's printing is its
//! output, not an effect on the world, so writes to standard output and standard error are
//! served and their bytes kept in the machine; every other descriptor, file and socket stays
//! refused. The one thread a run has gets its thread-local statics, as Miri gives them. The
//! random bytes std asks the OS for (the keys of a `HashMap`) are a fixed stream, so a run
//! reproduces: a verifier's answer must not change between two runs of the same call. Each of
//! these is the few foreign functions std reaches for it on the hosts frontend runs on (macOS
//! and Linux, read from std's own source), served the way Miri serves them.
//!
//! **Why not the compile-time machine.** CTFE refuses non-`const` functions and keeps pointers
//! relative to their allocation, so a pointer never becomes an integer. Ordinary library code
//! needs both: `fmt::Arguments::as_str` reads a pointer's low bit, a slice iterator compares two
//! pointers, `align_offset` reads an address. So this machine gives every allocation an absolute
//! address, as Miri does (`Prov` below: `OFFSET_IS_ADDR`), and calls any function whose MIR the
//! metadata carries. That is why the library read has a switch to write every function's MIR
//! (`CrateRead::all_mir`): rustc writes only generic and inline functions' MIR by default.

use alloc::borrow::Cow;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::any::Any;
use core::borrow::Borrow;
use core::cell::RefCell;
use core::fmt;
use core::hash::Hash;
use hashbrown::hash_map::Entry;

use crate::rustc_abi::{Align, FIRST_VARIANT, FieldIdx, Size};
use crate::rustc_ast::Mutability;
use crate::rustc_const_eval::interpret::{
    AllocBytes, AllocId, AllocInit, AllocMap, Allocation, AtomicRmwOp, CTFE_ALLOC_SALT,
    CtfeProvenance, FnArg, Frame, ImmTy, Immediate, InterpCx, InterpErrorInfo, InterpErrorKind,
    InterpResult, InvalidProgramInfo, MPlaceTy, Machine, MachineStopType, MayLeak, MemoryKind,
    OpTy, PlaceTy, Pointer, Projectable, Provenance, ResourceExhaustionInfo, ReturnContinuation,
    Scalar, interp_ok,
};
use crate::rustc_data_structures::fx::{FxHashMap, FxHashSet};
use crate::rustc_hir::attrs::Linkage;
use crate::rustc_hir::attrs::lang_items::LangItem;
use crate::rustc_hir::def::{DefKind, Res};
use crate::rustc_hir::def_id::{DefId, LocalDefId};
use crate::rustc_middle::mir;
use crate::rustc_middle::ty::layout::{HasTypingEnv, TyAndLayout, ValidityRequirement};
use crate::rustc_middle::ty::print::with_no_trimmed_paths;
use crate::rustc_middle::ty::{self, AtomicOrdering, Ty, TyCtxt};
use crate::rustc_span::sym;
use crate::rustc_target::callconv::FnAbi;

use super::Evaluation;

/// The name of the function [`super::evaluate`] appends to the source around the call.
pub(super) const ENTRY: &str = "__frontend_evaluate";

/// The names of the two functions [`RENDER`] defines: the generic one run on the call's value,
/// and the sink its text is written through.
pub(super) const DEBUG: &str = "__frontend_debug";
pub(super) const SINK: &str = "__frontend_sink";

/// Appended to a source that has a library, so a value is rendered by its type's own `Debug`,
/// run on the interpreter like the call itself: `{:?}` is the library's formatting, never a
/// second implementation of it. The text goes out through `__frontend_sink`, which the machine
/// serves by keeping what it is handed ([`Evaluator::rendered`]); `#[inline(never)]` keeps the
/// call to it in the MIR. Only `core` is named, by a relative path the extern prelude resolves
/// in every edition, so a `no_std` crate and an edition 2015 crate take it alike.
pub(super) const RENDER: &str = "\
#[allow(warnings)]
mod __frontend_render {
    pub fn __frontend_debug<T: core::fmt::Debug>(value: &T) -> core::fmt::Result {
        core::fmt::write(&mut Sink, format_args!(\"{:?}\", value))
    }
    struct Sink;
    impl core::fmt::Write for Sink {
        fn write_str(&mut self, text: &str) -> core::fmt::Result {
            __frontend_sink(text);
            core::result::Result::Ok(())
        }
    }
    #[inline(never)]
    pub fn __frontend_sink(_text: &str) {}
}
";

/// The two functions of [`RENDER`], found in the compiled source.
pub(super) struct Render {
    pub(super) debug: DefId,
    pub(super) sink: DefId,
}

/// The deepest call stack a run may build. A real thread's stack overflows somewhere near here
/// for small frames; past it the run is refused rather than growing the interpreter's own memory
/// without end.
const MAX_FRAMES: usize = 100_000;

/// Where the first allocation's address starts, and the gap left after each allocation, so that
/// no allocation sits at null or starts where another one ends.
const FIRST_ADDRESS: u64 = 0x1_0000;
const ADDRESS_GAP: u64 = 16;

type Ecx<'tcx> = InterpCx<'tcx, Evaluator<'tcx>>;

/// A pointer's provenance: the allocation it points into. The pointer's offset is the absolute
/// address (`OFFSET_IS_ADDR`), so a pointer turns into an integer the way it does on hardware.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Prov(AllocId);

impl fmt::Debug for Prov {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

impl Provenance for Prov {
    const OFFSET_IS_ADDR: bool = true;
    // An integer cast to a pointer gets no provenance and reads nothing.
    const WILDCARD: Option<Self> = None;

    fn fmt(ptr: &Pointer<Self>, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (prov, addr) = ptr.into_raw_parts();
        write!(f, "{:#x}[{:?}]", addr.bytes(), prov.0)
    }

    fn get_alloc_id(self) -> Option<AllocId> {
        Some(self.0)
    }
}

/// The machine's own memory kinds: the heap `__rust_alloc` serves, a global copied out of
/// `tcx` because the run reads or writes it, the run's one thread's copy of a thread-local
/// static, and what the machine provides itself (the null an absent weak symbol reads as).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Heap,
    Global,
    ThreadLocal,
    Machine,
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Kind::Heap => "heap allocation",
            Kind::Global => "global allocation",
            Kind::ThreadLocal => "thread-local static",
            Kind::Machine => "machine allocation",
        })
    }
}

impl MayLeak for Kind {
    // A run ends when its value is read; nothing checks for leaks.
    fn may_leak(self) -> bool {
        true
    }
}

/// Why a run stopped before its call returned. Raised as a machine stop, so it unwinds through
/// the interpreter like any error, and read back where the run is driven.
#[derive(Debug)]
enum Halt {
    /// The program panicked. `Some` when the message is already known; `None` when it is the
    /// `fmt::Arguments` in [`Evaluator::panic_arguments`], formatted after the run.
    Panicked(Option<String>),
    /// The program did something that ends it with no value: undefined behavior, a deadlock,
    /// a stack deeper than any run has. What the program does, and so its answer.
    Refused(String),
    /// This machine could not run it: a function whose MIR no metadata carries, an intrinsic or
    /// a foreign function it does not serve, an operation the interpreter does not support. Says
    /// nothing about the program; a run on a machine that serves it may well finish.
    Unsupported(String),
}

impl fmt::Display for Halt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Halt::Panicked(Some(message)) => write!(f, "panicked: {message}"),
            Halt::Panicked(None) => f.write_str("panicked"),
            Halt::Refused(why) | Halt::Unsupported(why) => f.write_str(why),
        }
    }
}

impl MachineStopType for Halt {}

fn halt<'tcx, T>(halt: Halt) -> InterpResult<'tcx, T> {
    Err(InterpErrorKind::MachineStop(Box::new(halt)).into())
}

fn refuse<'tcx, T>(why: String) -> InterpResult<'tcx, T> {
    halt(Halt::Refused(why))
}

/// Stop because this machine cannot do what the program asks, which says nothing of the program.
fn unsupported<'tcx, T>(why: String) -> InterpResult<'tcx, T> {
    halt(Halt::Unsupported(why))
}

/// The machine: the call stack, the addresses handed out, a panic's message, and the state of
/// what it serves of the operating system for the run's one thread.
pub(crate) struct Evaluator<'tcx> {
    stack: Vec<Frame<'tcx, Prov>>,
    /// Behind a `RefCell` because addresses are handed out from hooks that see `&InterpCx`.
    addresses: RefCell<Addresses>,
    /// The `fmt::Arguments` a panic carried, set when the panic runtime is entered.
    panic_arguments: Option<MPlaceTy<'tcx, Prov>>,
    /// Each thread-local static the run has reached, by the static: the one thread's copy.
    thread_locals: FxHashMap<DefId, Pointer<Prov>>,
    /// A pointer-sized null, made when the run starts, which every `extern_weak` static reads
    /// as: the symbol is absent, which a weak symbol may be, so std takes its fallback.
    absent_symbol: Option<Pointer<Prov>>,
    /// The pthread mutexes held, by address. One thread holds them, so locking one it already
    /// holds can never return: a deadlock, refused rather than run forever.
    held: FxHashSet<u64>,
    /// What the program wrote to standard output and to standard error, in order. Its output,
    /// kept here: the wire shape of an evaluation does not carry it.
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    /// The state of the stream served as the OS's random bytes: splitmix64 from a fixed seed,
    /// so a run reproduces, as Miri's seeded generator does.
    random: u64,
    /// [`RENDER`]'s sink, when the source has one, and what it has been handed.
    sink: Option<DefId>,
    rendered: String,
}

/// Absolute addresses, one per allocation, handed out in order and never reused, so two live
/// allocations never overlap and a dangling pointer never aliases a new allocation.
struct Addresses {
    next: u64,
    base: FxHashMap<AllocId, u64>,
}

impl<'tcx> Evaluator<'tcx> {
    fn new(sink: Option<DefId>) -> Self {
        Evaluator {
            stack: Vec::new(),
            addresses: RefCell::new(Addresses { next: FIRST_ADDRESS, base: FxHashMap::default() }),
            panic_arguments: None,
            thread_locals: FxHashMap::default(),
            absent_symbol: None,
            held: FxHashSet::default(),
            stdout: Vec::new(),
            stderr: Vec::new(),
            random: 0,
            sink,
            rendered: String::new(),
        }
    }

    /// The next eight bytes of the random stream (splitmix64).
    fn next_random(&mut self) -> u64 {
        self.random = self.random.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.random;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// `id`'s absolute address, handed out the first time it is asked for, aligned to the
    /// allocation's own alignment.
    fn base_address(ecx: &Ecx<'tcx>, id: AllocId) -> u64 {
        if let Some(&base) = ecx.machine.addresses.borrow().base.get(&id) {
            return base;
        }
        // Read before borrowing: the size and alignment come from memory or from `tcx`.
        let info = ecx.get_alloc_info(id);
        let mut addresses = ecx.machine.addresses.borrow_mut();
        let base = addresses.next.next_multiple_of(info.align.bytes().max(1));
        addresses.next = base + info.size.bytes().max(1) + ADDRESS_GAP;
        addresses.base.insert(id, base);
        base
    }
}

/// The allocation map: a map a shared reference can insert into, which `Memory` needs when a
/// global is read for the first time and has to be copied in, its pointers given addresses.
/// Each value is boxed, so a reference to it stays valid while the map grows.
#[derive(Clone)]
pub(crate) struct MonoMap<K: Hash + Eq, V>(RefCell<FxHashMap<K, Box<V>>>);

impl<K: Hash + Eq, V> Default for MonoMap<K, V> {
    fn default() -> Self {
        MonoMap(RefCell::new(FxHashMap::default()))
    }
}

impl<K: Hash + Eq, V> AllocMap<K, V> for MonoMap<K, V> {
    fn contains_key<Q: ?Sized + Hash + Eq>(&mut self, k: &Q) -> bool
    where
        K: Borrow<Q>,
    {
        self.0.get_mut().contains_key(k)
    }

    fn contains_key_ref<Q: ?Sized + Hash + Eq>(&self, k: &Q) -> bool
    where
        K: Borrow<Q>,
    {
        self.0.borrow().contains_key(k)
    }

    fn insert(&mut self, k: K, v: V) -> Option<V> {
        self.0.get_mut().insert(k, Box::new(v)).map(|old| *old)
    }

    fn remove<Q: ?Sized + Hash + Eq>(&mut self, k: &Q) -> Option<V>
    where
        K: Borrow<Q>,
    {
        self.0.get_mut().remove(k).map(|old| *old)
    }

    fn filter_map_collect<T>(&self, mut f: impl FnMut(&K, &V) -> Option<T>) -> Vec<T> {
        self.0.borrow().iter().filter_map(move |(k, v)| f(k, &**v)).collect()
    }

    fn get_or<E>(&self, k: K, vacant: impl FnOnce() -> Result<V, E>) -> Result<&V, E> {
        // The borrow is not held across `vacant`, which may look into this very map.
        if let Some(v) = self.0.borrow().get(&k) {
            let v: *const V = &**v;
            // SAFETY: `v` points into a `Box` the map owns. A box does not move when the map
            // grows, and nothing removes it while `&self` is borrowed: removal takes `&mut self`.
            return Ok(unsafe { &*v });
        }
        let new = Box::new(vacant()?);
        let v: *const V = &**self.0.borrow_mut().entry(k).or_insert(new);
        // SAFETY: as above.
        Ok(unsafe { &*v })
    }

    fn get_mut_or<E>(&mut self, k: K, vacant: impl FnOnce() -> Result<V, E>) -> Result<&mut V, E> {
        match self.0.get_mut().entry(k) {
            Entry::Occupied(entry) => Ok(&mut **entry.into_mut()),
            Entry::Vacant(entry) => {
                let v = vacant()?;
                Ok(&mut **entry.insert(Box::new(v)))
            }
        }
    }
}

impl<'tcx> Machine<'tcx> for Evaluator<'tcx> {
    type MemoryKind = Kind;
    type Provenance = Prov;
    type ProvenanceExtra = ();
    type ExtraFnVal = crate::Never;
    type FrameExtra = ();
    type AllocExtra = ();
    type Bytes = Box<[u8]>;
    type MemoryMap =
        MonoMap<AllocId, (MemoryKind<Kind>, Allocation<Prov, (), Box<[u8]>>)>;

    const GLOBAL_KIND: Option<Kind> = Some(Kind::Global);
    const PANIC_ON_ALLOC_FAIL: bool = false;
    // A library constant this run evaluates may fail; that is an error of the run, not a bug.
    const ALL_CONSTS_ARE_PRECHECKED: bool = false;

    fn enforce_alignment(_ecx: &Ecx<'tcx>) -> bool {
        true
    }

    // As the compile-time machine: a value of an uninhabited type is always undefined behavior.
    fn enforce_validity(_ecx: &Ecx<'tcx>, layout: TyAndLayout<'tcx>) -> bool {
        layout.is_uninhabited()
    }

    // A debug build: overflow is checked, including in library functions that inherit the check.
    fn ignore_optional_overflow_checks(_ecx: &Ecx<'tcx>) -> bool {
        false
    }

    fn find_mir_or_eval_fn(
        ecx: &mut Ecx<'tcx>,
        instance: ty::Instance<'tcx>,
        _abi: &FnAbi<'tcx, Ty<'tcx>>,
        args: &[FnArg<'tcx, Prov>],
        destination: &PlaceTy<'tcx, Prov>,
        target: Option<mir::BasicBlock>,
        _unwind: mir::UnwindAction,
    ) -> InterpResult<'tcx, Option<(&'tcx mir::Body<'tcx>, ty::Instance<'tcx>)>> {
        let tcx = *ecx.tcx;
        if let ty::InstanceKind::Item(def_id) = instance.def {
            // A tuple struct's or a variant's constructor called as a function: `.map(Some)`,
            // `ControlFlow::Break` in `Iterator::eq`'s `try_fold`. It has no function body a
            // crate's metadata carries; rustc's MIR for it is the shim `build_adt_ctor` writes,
            // the variant's aggregate of the arguments, and that is written here, so every
            // constructor runs alike, a library's or the source's own.
            if tcx.is_constructor(def_id) {
                construct(ecx, def_id, args, destination)?;
                ecx.return_to_block(target)?;
                return interp_ok(None);
            }
            // `RENDER`'s sink: the value's `Debug` output, kept.
            if ecx.machine.sink == Some(def_id) {
                let Some(text) = args.first() else {
                    return unsupported("the rendering sink was called with no text".to_string());
                };
                let text = ecx.deref_pointer(&text.copy_fn_arg())?;
                let text = ecx.read_str(&text)?.to_string();
                ecx.machine.rendered.push_str(&text);
                ecx.return_to_block(target)?;
                return interp_ok(None);
            }
            // Every panic reaches the panic runtime with its message: core declares it as the
            // foreign `panic_impl`, and std defines it as the `#[panic_handler]`.
            let foreign = tcx.is_foreign_item(def_id);
            if tcx.is_lang_item(def_id, LangItem::PanicImpl)
                || (foreign && tcx.item_name(def_id).as_str() == "panic_impl")
            {
                return panic_from_info(ecx, args);
            }
            // `panic!("text")` in editions before 2021 goes to std's `begin_panic` with the
            // payload itself, not a `fmt::Arguments`.
            if tcx.is_lang_item(def_id, LangItem::BeginPanic) {
                return begin_panic(&*ecx, args);
            }
            if foreign {
                foreign_call(ecx, def_id, args, destination)?;
                ecx.return_to_block(target)?;
                return interp_ok(None);
            }
        }
        interp_ok(Some((body_of(ecx, instance)?, instance)))
    }

    fn call_extra_fn(
        _ecx: &mut Ecx<'tcx>,
        fn_val: crate::Never,
        _abi: &FnAbi<'tcx, Ty<'tcx>>,
        _args: &[FnArg<'tcx, Prov>],
        _destination: &PlaceTy<'tcx, Prov>,
        _target: Option<mir::BasicBlock>,
        _unwind: mir::UnwindAction,
    ) -> InterpResult<'tcx> {
        match fn_val {}
    }

    fn call_intrinsic(
        ecx: &mut Ecx<'tcx>,
        instance: ty::Instance<'tcx>,
        args: &[OpTy<'tcx, Prov>],
        destination: &PlaceTy<'tcx, Prov>,
        target: Option<mir::BasicBlock>,
        _unwind: mir::UnwindAction,
    ) -> InterpResult<'tcx, Option<ty::Instance<'tcx>>> {
        // The intrinsics every machine shares.
        if ecx.eval_intrinsic(instance, args, destination, target)? {
            return interp_ok(None);
        }
        let name = ecx.tcx.item_name(instance.def_id());
        match name {
            // At run time a pointer comparison is known: the addresses are real.
            sym::ptr_guaranteed_cmp => {
                let size = ecx.tcx.data_layout.pointer_size();
                let a = ecx.read_scalar(&args[0])?.to_bits(size)?;
                let b = ecx.read_scalar(&args[1])?.to_bits(size)?;
                ecx.write_scalar(Scalar::from_u8(u8::from(a == b)), destination)?;
            }
            // Nothing is known to an optimizer here, as in the compile-time machine.
            sym::is_val_statically_known => {
                ecx.write_scalar(Scalar::from_bool(false), destination)?;
            }
            sym::abort => return halt(Halt::Panicked(Some("the program aborted".to_string()))),
            sym::assert_inhabited
            | sym::assert_zero_valid
            | sym::assert_mem_uninitialized_valid => {
                let ty = instance.args.type_at(0);
                let requirement = ValidityRequirement::from_intrinsic(name)
                    .expect("one of the three validity intrinsics");
                let valid = ecx
                    .tcx
                    .check_validity_requirement((requirement, ecx.typing_env().as_query_input(ty)))
                    .unwrap_or(true);
                if !valid {
                    return halt(Halt::Panicked(Some(format!(
                        "aborted execution: attempted to create an invalid value of type `{ty}`"
                    ))));
                }
            }
            _ => {
                // An intrinsic with a body of its own runs that body.
                let must_be_overridden =
                    ecx.tcx.intrinsic(instance.def_id()).is_none_or(|i| i.must_be_overridden);
                if must_be_overridden {
                    return unsupported(format!(
                        "calls the intrinsic `{name}`, which is not served"
                    ));
                }
                return interp_ok(Some(ty::Instance {
                    def: ty::InstanceKind::Item(instance.def_id()),
                    args: instance.args,
                }));
            }
        }
        ecx.return_to_block(target)?;
        interp_ok(None)
    }

    fn call_llvm_intrinsic(
        ecx: &mut Ecx<'tcx>,
        instance: ty::Instance<'tcx>,
        _args: &[OpTy<'tcx, Prov>],
        _destination: &PlaceTy<'tcx, Prov>,
        _target: Option<mir::BasicBlock>,
    ) -> InterpResult<'tcx> {
        let name = shown_path(*ecx.tcx, instance.def_id());
        unsupported(format!("calls the LLVM intrinsic `{name}`, which is not served"))
    }

    fn check_fn_target_features(
        _ecx: &Ecx<'tcx>,
        _instance: ty::Instance<'tcx>,
    ) -> InterpResult<'tcx> {
        interp_ok(())
    }

    /// A failed `Assert` (overflow, a bounds check, division by zero) calls the runtime's own
    /// panic function for it, as compiled code does, so the message is the one a run prints.
    /// Without that function (a `no_core` crate) the message is the compiler's description.
    fn assert_panic(
        ecx: &mut Ecx<'tcx>,
        msg: &mir::AssertMessage<'tcx>,
        unwind: mir::UnwindAction,
    ) -> InterpResult<'tcx> {
        use crate::rustc_middle::mir::AssertKind;
        let tcx = *ecx.tcx;
        let operand = |ecx: &Ecx<'tcx>, op: &mir::Operand<'tcx>| {
            ecx.read_immediate(&ecx.eval_operand(op, None)?)
        };
        // The runtime's functions for these two take the operands, in this order.
        let (item, args) = match msg {
            AssertKind::BoundsCheck { len, index } => {
                let args = [operand(&*ecx, index)?, operand(&*ecx, len)?];
                (tcx.lang_items().get(LangItem::PanicBoundsCheck), Vec::from(args))
            }
            AssertKind::MisalignedPointerDereference { required, found } => {
                let args = [operand(&*ecx, required)?, operand(&*ecx, found)?];
                (tcx.lang_items().get(LangItem::PanicMisalignedPointerDereference), Vec::from(args))
            }
            _ => (tcx.lang_items().get(msg.panic_function()), Vec::new()),
        };
        let runtime = item.map(|item| ty::Instance::mono(tcx, item)).filter(|instance| {
            let def_id = instance.def_id();
            def_id.is_local() || tcx.is_mir_available(def_id)
        });
        let Some(instance) = runtime else {
            let message = describe_assert(&*ecx, msg)?;
            return halt(Halt::Panicked(Some(message)));
        };
        let body = body_of(&*ecx, instance)?;
        let fn_abi = ecx.fn_abi_of_instance_no_deduced_attrs(instance, ty::List::empty())?;
        // The function returns `!`, so nothing is ever written here.
        let unit = ecx.layout_of(tcx.types.unit)?;
        let destination = MPlaceTy::fake_alloc_zst(unit);
        let args: Vec<FnArg<'tcx, Prov>> =
            args.into_iter().map(|arg| FnArg::Copy(arg.into())).collect();
        ecx.init_stack_frame(
            instance,
            body,
            fn_abi,
            &args,
            instance.def.requires_caller_location(tcx),
            &destination.into(),
            ReturnContinuation::Goto { ret: None, unwind },
        )
    }

    fn panic_nounwind(_ecx: &mut Ecx<'tcx>, msg: &str) -> InterpResult<'tcx> {
        halt(Halt::Panicked(Some(msg.to_string())))
    }

    // Nothing unwinds here: a panic ends the run where it starts.
    fn unwind_terminate(
        _ecx: &mut Ecx<'tcx>,
        _reason: mir::UnwindTerminateReason,
    ) -> InterpResult<'tcx> {
        halt(Halt::Panicked(Some("panic in a function that cannot unwind".to_string())))
    }

    /// Pointers compare by address, as they do on hardware; a wide pointer compares its address
    /// and then its metadata.
    fn binary_ptr_op(
        ecx: &Ecx<'tcx>,
        bin_op: mir::BinOp,
        left: &ImmTy<'tcx, Prov>,
        right: &ImmTy<'tcx, Prov>,
    ) -> InterpResult<'tcx, ImmTy<'tcx, Prov>> {
        use crate::rustc_middle::mir::BinOp::*;
        let size = ecx.tcx.data_layout.pointer_size();
        let bits = |imm: &ImmTy<'tcx, Prov>| -> InterpResult<'tcx, (u128, u128)> {
            interp_ok(match **imm {
                Immediate::Scalar(a) => (a.to_bits(size)?, 0),
                Immediate::ScalarPair(a, b) => (a.to_bits(size)?, b.to_bits(size)?),
                Immediate::Uninit => {
                    use crate::rustc_middle::mir::interpret::UndefinedBehaviorInfo;
                    let ub = UndefinedBehaviorInfo::InvalidUninitBytes(None);
                    return Err(InterpErrorKind::UndefinedBehavior(ub).into());
                }
            })
        };
        let result = match bin_op {
            Eq | Ne | Lt | Le | Gt | Ge => {
                let (l, r) = (bits(left)?, bits(right)?);
                match bin_op {
                    Eq => l == r,
                    Ne => l != r,
                    Lt => l < r,
                    Le => l <= r,
                    Gt => l > r,
                    _ => l >= r,
                }
            }
            _ => return unsupported(format!("pointer arithmetic `{bin_op:?}` is not served")),
        };
        interp_ok(ImmTy::from_bool(result, *ecx.tcx))
    }

    // As the compile-time machine.
    fn float_fuse_mul_add(_ecx: &Ecx<'tcx>) -> bool {
        true
    }

    // One thread: atomics are plain loads and stores.
    fn atomic_load(
        ecx: &Ecx<'tcx>,
        place: &MPlaceTy<'tcx, Prov>,
        _ordering: AtomicOrdering,
    ) -> InterpResult<'tcx, Scalar<Prov>> {
        ecx.read_scalar(place)
    }

    fn atomic_store(
        ecx: &mut Ecx<'tcx>,
        place: &MPlaceTy<'tcx, Prov>,
        val: &ImmTy<'tcx, Prov>,
        _ordering: AtomicOrdering,
    ) -> InterpResult<'tcx> {
        ecx.write_scalar(val.to_scalar(), place)
    }

    fn atomic_rmw(
        ecx: &mut Ecx<'tcx>,
        place: &MPlaceTy<'tcx, Prov>,
        op: AtomicRmwOp,
        operand: &ImmTy<'tcx, Prov>,
        _ordering: AtomicOrdering,
    ) -> InterpResult<'tcx, Scalar<Prov>> {
        let old = ecx.read_immediate(place)?;
        let new = ecx.atomic_rmw_op(op, &old, operand)?;
        ecx.write_immediate(*new, place)?;
        interp_ok(old.to_scalar())
    }

    fn atomic_compare_exchange(
        ecx: &mut Ecx<'tcx>,
        place: &MPlaceTy<'tcx, Prov>,
        expected_old: &ImmTy<'tcx, Prov>,
        new: &ImmTy<'tcx, Prov>,
        _can_fail_spuriously: bool,
        _success_ordering: AtomicOrdering,
        _failure_ordering: AtomicOrdering,
    ) -> InterpResult<'tcx, (Scalar<Prov>, bool)> {
        let actual = ecx.read_immediate(place)?;
        let equal =
            ecx.binary_op(mir::BinOp::Eq, &actual, expected_old)?.to_scalar().to_bool()?;
        if equal {
            ecx.write_immediate(**new, place)?;
        }
        interp_ok((actual.to_scalar(), equal))
    }

    fn atomic_fence(
        _ecx: &Ecx<'tcx>,
        _ordering: AtomicOrdering,
        _singlethread: bool,
    ) -> InterpResult<'tcx> {
        interp_ok(())
    }

    /// `UbChecks` is off: the interpreter checks every access for undefined behavior itself, at
    /// the access, which is stricter than the library's debug precondition checks and costs no
    /// steps. Overflow checks are on, as in a debug build. Contracts are off, as in rustc.
    fn runtime_checks(_ecx: &Ecx<'tcx>, r: mir::RuntimeChecks) -> InterpResult<'tcx, bool> {
        interp_ok(matches!(r, mir::RuntimeChecks::OverflowChecks))
    }

    /// A thread-local static, for the one thread a run has: a copy of its initial value made the
    /// first time the run reaches it, and the same copy every time after, as Miri does (the
    /// interpreter's default refuses every thread-local). The thread does not exit during a run,
    /// so the copy is never freed and no destructor registered for it runs.
    fn thread_local_static_pointer(
        ecx: &mut Ecx<'tcx>,
        def_id: DefId,
    ) -> InterpResult<'tcx, Pointer<Prov>> {
        if let Some(&pointer) = ecx.machine.thread_locals.get(&def_id) {
            return interp_ok(pointer);
        }
        if ecx.tcx.is_foreign_item(def_id) {
            let name = shown_path(*ecx.tcx, def_id);
            return unsupported(format!(
                "reads the foreign thread-local static `{name}`, which is not served"
            ));
        }
        let initial = ecx.ctfe_query(|tcx| tcx.eval_static_initializer(def_id))?;
        let this: &Ecx<'tcx> = ecx;
        let mut copy = initial.inner().adjust_from_tcx(
            this,
            |bytes, align| interp_ok(<Box<[u8]> as AllocBytes>::from_bytes(bytes, align, ())),
            |ptr| this.global_root_pointer(ptr),
        )?;
        // The thread writes its own copy; the initializer itself is read-only memory.
        copy.mutability = Mutability::Mut;
        let pointer = ecx.insert_allocation(copy, MemoryKind::Machine(Kind::ThreadLocal))?;
        ecx.machine.thread_locals.insert(def_id, pointer);
        interp_ok(pointer)
    }

    /// A foreign static. An `extern_weak` one is absent (null), which a weak symbol is allowed to
    /// be, so std takes the fallback it has for that (on Linux, `getrandom` through `syscall`);
    /// every other foreign static is refused by name.
    fn extern_static_pointer(
        ecx: &Ecx<'tcx>,
        def_id: DefId,
    ) -> InterpResult<'tcx, Pointer<Prov>> {
        let weak = ecx.tcx.codegen_fn_attrs(def_id).import_linkage == Some(Linkage::ExternalWeak);
        if weak && let Some(absent) = ecx.machine.absent_symbol {
            return interp_ok(absent);
        }
        let name = shown_path(*ecx.tcx, def_id);
        unsupported(format!("reads the foreign static `{name}`, which is not served"))
    }

    fn ptr_from_addr_cast(
        _ecx: &Ecx<'tcx>,
        addr: u64,
    ) -> InterpResult<'tcx, Pointer<Option<Prov>>> {
        interp_ok(Pointer::without_provenance(addr))
    }

    fn expose_provenance(_ecx: &Ecx<'tcx>, _provenance: Prov) -> InterpResult<'tcx> {
        interp_ok(())
    }

    fn ptr_get_alloc(
        ecx: &Ecx<'tcx>,
        ptr: Pointer<Prov>,
        _size: i64,
    ) -> Option<(AllocId, Size, ())> {
        let (prov, addr) = ptr.into_raw_parts();
        let base = Evaluator::base_address(ecx, prov.0);
        Some((prov.0, Size::from_bytes(addr.bytes().wrapping_sub(base)), ()))
    }

    fn adjust_alloc_root_pointer(
        ecx: &Ecx<'tcx>,
        ptr: Pointer<CtfeProvenance>,
        _kind: Option<MemoryKind<Kind>>,
    ) -> InterpResult<'tcx, Pointer<Prov>> {
        let (prov, offset) = ptr.prov_and_relative_offset();
        let id = prov.alloc_id();
        let base = Evaluator::base_address(ecx, id);
        interp_ok(Pointer::new(Prov(id), Size::from_bytes(base.wrapping_add(offset.bytes()))))
    }

    /// A global's bytes, copied, with every pointer in it given its target's address.
    fn adjust_global_allocation<'b>(
        ecx: &Ecx<'tcx>,
        _id: AllocId,
        alloc: &'b Allocation,
    ) -> InterpResult<'tcx, Cow<'b, Allocation<Prov, (), Box<[u8]>>>> {
        let adjusted = alloc.adjust_from_tcx(
            ecx,
            |bytes, align| interp_ok(<Box<[u8]> as AllocBytes>::from_bytes(bytes, align, ())),
            |ptr| ecx.global_root_pointer(ptr),
        )?;
        interp_ok(Cow::Owned(adjusted))
    }

    fn init_local_allocation(
        _ecx: &Ecx<'tcx>,
        _id: AllocId,
        _kind: MemoryKind<Kind>,
        _size: Size,
        _align: Align,
    ) -> InterpResult<'tcx> {
        interp_ok(())
    }

    fn init_frame(
        ecx: &mut Ecx<'tcx>,
        frame: Frame<'tcx, Prov>,
    ) -> InterpResult<'tcx, Frame<'tcx, Prov>> {
        if ecx.machine.stack.len() >= MAX_FRAMES {
            return Err(
                InterpErrorKind::ResourceExhaustion(ResourceExhaustionInfo::StackFrameLimitReached)
                    .into(),
            );
        }
        interp_ok(frame)
    }

    fn stack<'a>(ecx: &'a Ecx<'tcx>) -> &'a [Frame<'tcx, Prov>] {
        &ecx.machine.stack
    }

    fn stack_mut<'a>(ecx: &'a mut Ecx<'tcx>) -> &'a mut Vec<Frame<'tcx, Prov>> {
        &mut ecx.machine.stack
    }

    fn get_global_alloc_salt(
        _ecx: &Ecx<'tcx>,
        _instance: Option<ty::Instance<'tcx>>,
    ) -> usize {
        CTFE_ALLOC_SALT
    }

    fn get_default_alloc_params(&self) -> <Self::Bytes as AllocBytes>::AllocParams {}
}

/// `def_id` printed in full, for a refusal or a panic message.
fn shown_path(tcx: TyCtxt<'_>, def_id: DefId) -> String {
    with_no_trimmed_paths!(tcx.def_path_str(def_id))
}

/// The MIR a call runs. A function of another crate has MIR only when that crate's metadata
/// carries it; one that does not is refused by name rather than guessed at.
fn body_of<'tcx>(
    ecx: &Ecx<'tcx>,
    instance: ty::Instance<'tcx>,
) -> InterpResult<'tcx, &'tcx mir::Body<'tcx>> {
    if let ty::InstanceKind::Item(def_id) = instance.def
        && !def_id.is_local()
        && !ecx.tcx.is_mir_available(def_id)
    {
        let name = shown_path(*ecx.tcx, def_id);
        return unsupported(format!(
            "calls `{name}`, whose MIR its crate's metadata does not carry (read the crate with \
             every function's MIR)"
        ));
    }
    // A body rustc built from source with an error in it is refused by the interpreter as "an
    // error has already been reported elsewhere". For a library function that report is in
    // another session, the library read of its crate, so this names the function and the crate
    // whose read recorded the error.
    if ecx.tcx.instance_mir(instance.def).tainted_by_errors.is_some() {
        let name = match instance.def {
            ty::InstanceKind::Item(def_id) => shown_path(*ecx.tcx, def_id),
            _ => instance.to_string(),
        };
        let krate = ecx.tcx.crate_name(instance.def_id().krate);
        return unsupported(format!(
            "calls `{name}`, whose MIR rustc built from a body with an error in it: the error is \
             among the diagnostics recorded when `{krate}` was read"
        ));
    }
    ecx.load_mir(instance.def, None)
}

/// A constructor call, `ctor(args..)`: the aggregate of its variant written into `destination`,
/// as the interpreter writes an aggregate (`write_aggregate`) and as the shim rustc builds for a
/// constructor (`build_adt_ctor`) does: each argument into its field of the variant, in order,
/// then the discriminant.
fn construct<'tcx>(
    ecx: &mut Ecx<'tcx>,
    ctor: DefId,
    args: &[FnArg<'tcx, Prov>],
    destination: &PlaceTy<'tcx, Prov>,
) -> InterpResult<'tcx> {
    let ty::Adt(adt, _) = destination.layout.ty.kind() else {
        let name = shown_path(*ecx.tcx, ctor);
        return unsupported(format!(
            "calls the constructor `{name}`, whose result is not a struct or an enum"
        ));
    };
    let variant = if adt.is_enum() { adt.variant_index_with_ctor_id(ctor) } else { FIRST_VARIANT };
    let variant_place = ecx.project_downcast(destination, variant)?;
    for (index, arg) in args.iter().enumerate() {
        let field = ecx.project_field(&variant_place, FieldIdx::from_usize(index))?;
        ecx.copy_op(&arg.copy_fn_arg(), &field)?;
    }
    ecx.write_discriminant(variant, destination)
}

/// A call to a foreign function. The allocator's entry points are served from the
/// interpreter's own memory, and the operating system's as far as [`system_call`] serves it;
/// every other foreign function (a syscall, a C library, file and network) is refused by name.
fn foreign_call<'tcx>(
    ecx: &mut Ecx<'tcx>,
    def_id: DefId,
    args: &[FnArg<'tcx, Prov>],
    destination: &PlaceTy<'tcx, Prov>,
) -> InterpResult<'tcx> {
    let name = ecx.tcx.item_name(def_id);
    let args: Vec<OpTy<'tcx, Prov>> = args.iter().map(FnArg::copy_fn_arg).collect();
    let heap = MemoryKind::Machine(Kind::Heap);
    let align = |ecx: &Ecx<'tcx>, op: &OpTy<'tcx, Prov>| -> InterpResult<'tcx, Align> {
        let bytes = ecx.read_target_usize(op)?;
        match Align::from_bytes(bytes) {
            Ok(align) => interp_ok(align),
            Err(_) => refuse(format!("asks the allocator for an alignment of {bytes}")),
        }
    };
    match name.as_str() {
        "__rust_alloc" | "__rust_alloc_zeroed" if args.len() == 2 => {
            let size = Size::from_bytes(ecx.read_target_usize(&args[0])?);
            let align = align(&*ecx, &args[1])?;
            let init =
                if name.as_str() == "__rust_alloc" { AllocInit::Uninit } else { AllocInit::Zero };
            let ptr = ecx.allocate_ptr(size, align, heap, init)?;
            ecx.write_pointer(ptr, destination)?;
        }
        "__rust_dealloc" if args.len() == 3 => {
            let ptr = ecx.read_pointer(&args[0])?;
            let size = Size::from_bytes(ecx.read_target_usize(&args[1])?);
            let align = align(&*ecx, &args[2])?;
            ecx.deallocate_ptr(ptr, Some((size, align)), heap)?;
        }
        "__rust_realloc" if args.len() == 4 => {
            let ptr = ecx.read_pointer(&args[0])?;
            let old_size = Size::from_bytes(ecx.read_target_usize(&args[1])?);
            let align = align(&*ecx, &args[2])?;
            let new_size = Size::from_bytes(ecx.read_target_usize(&args[3])?);
            let ptr = ecx.reallocate_ptr(
                ptr,
                Some((old_size, align)),
                new_size,
                align,
                heap,
                AllocInit::Uninit,
            )?;
            ecx.write_pointer(ptr, destination)?;
        }
        // Only a link-time marker that the allocator shim exists; it does nothing.
        "__rust_no_alloc_shim_is_unstable_v2" => {}
        _ => return system_call(ecx, def_id, &args, destination),
    }
    interp_ok(())
}

/// The operating system, as far as std's printing, its locks and its hash map keys reach it on
/// macOS and Linux (read from std's source: `io::stdio`, `sys::stdio::unix`, `sys::fd::unix`,
/// `sys::pal::unix::sync::mutex`, `sys::random`), each served as Miri serves it. A foreign
/// function is matched by the symbol it links to (`#[link_name]`), which is what the OS sees.
///
/// - `write` to descriptor 1 or 2: the bytes are kept in the machine and all are written. Any
///   other descriptor is refused by name.
/// - `pthread_mutex*` (macOS's `Mutex`; Linux's is atomics until two threads contend): one
///   thread's bookkeeping, by address. Relocking a held mutex is a deadlock and is refused.
/// - `CCRandomGenerateBytes` (macOS) and `syscall(SYS_getrandom)` (Linux, once the weak
///   `getrandom` reads as absent): the machine's fixed stream, so a run reproduces.
/// - `_tlv_atexit` (macOS): a destructor for the thread's locals. The thread does not exit
///   during a run, so it is not kept.
fn system_call<'tcx>(
    ecx: &mut Ecx<'tcx>,
    def_id: DefId,
    args: &[OpTy<'tcx, Prov>],
    destination: &PlaceTy<'tcx, Prov>,
) -> InterpResult<'tcx> {
    let tcx = *ecx.tcx;
    let link = tcx.codegen_fn_attrs(def_id).symbol_name.unwrap_or_else(|| tcx.item_name(def_id));
    let size = destination.layout.size;
    let int = |value: u128| -> Scalar<Prov> { Scalar::from_uint(value, size) };
    match link.as_str() {
        "write" if args.len() == 3 => {
            let fd = ecx.read_scalar(&args[0])?.to_i32()?;
            let buffer = ecx.read_pointer(&args[1])?;
            let count = ecx.read_target_usize(&args[2])?;
            let bytes = ecx.read_bytes_ptr_strip_provenance(buffer, Size::from_bytes(count))?;
            let bytes = bytes.to_vec();
            let stream = match fd {
                1 => &mut ecx.machine.stdout,
                2 => &mut ecx.machine.stderr,
                _ => {
                    return unsupported(format!(
                        "writes to file descriptor {fd}, which is not served: only standard \
                         output and standard error are"
                    ));
                }
            };
            stream.extend_from_slice(&bytes);
            ecx.write_scalar(int(u128::from(count)), destination)?;
        }
        "pthread_mutexattr_init" | "pthread_mutexattr_settype" | "pthread_mutexattr_destroy" => {
            ecx.write_scalar(int(0), destination)?;
        }
        "pthread_mutex_init" | "pthread_mutex_destroy" if !args.is_empty() => {
            let mutex = ecx.read_pointer(&args[0])?.addr().bytes();
            ecx.machine.held.remove(&mutex);
            ecx.write_scalar(int(0), destination)?;
        }
        "pthread_mutex_lock" if !args.is_empty() => {
            let mutex = ecx.read_pointer(&args[0])?.addr().bytes();
            if !ecx.machine.held.insert(mutex) {
                return refuse(
                    "deadlocks: the one thread locks a mutex it already holds".to_string(),
                );
            }
            ecx.write_scalar(int(0), destination)?;
        }
        "pthread_mutex_trylock" if !args.is_empty() => {
            let mutex = ecx.read_pointer(&args[0])?.addr().bytes();
            let code =
                if ecx.machine.held.insert(mutex) { 0 } else { libc_constant(ecx, "EBUSY")? };
            ecx.write_scalar(int(code), destination)?;
        }
        "pthread_mutex_unlock" if !args.is_empty() => {
            let mutex = ecx.read_pointer(&args[0])?.addr().bytes();
            if !ecx.machine.held.remove(&mutex) {
                return refuse("unlocks a mutex that is not locked".to_string());
            }
            ecx.write_scalar(int(0), destination)?;
        }
        "CCRandomGenerateBytes" if args.len() == 2 => {
            let buffer = ecx.read_pointer(&args[0])?;
            let count = ecx.read_target_usize(&args[1])?;
            fill_random(ecx, buffer, count)?;
            // `kCCSuccess`.
            ecx.write_scalar(int(0), destination)?;
        }
        "syscall" if !args.is_empty() => {
            let number = ecx.read_scalar(&args[0])?.to_bits(args[0].layout.size)?;
            if number != libc_constant(ecx, "SYS_getrandom")? || args.len() < 3 {
                return unsupported(format!(
                    "makes the system call {number}, which is not served"
                ));
            }
            let buffer = ecx.read_pointer(&args[1])?;
            let count = ecx.read_target_usize(&args[2])?;
            fill_random(ecx, buffer, count)?;
            ecx.write_scalar(int(u128::from(count)), destination)?;
        }
        "_tlv_atexit" => {}
        _ => {
            let path = shown_path(tcx, def_id);
            return unsupported(format!(
                "calls the foreign function `{path}`, which is not served"
            ));
        }
    }
    interp_ok(())
}

/// `count` bytes of the machine's random stream, written at `buffer`.
fn fill_random<'tcx>(
    ecx: &mut Ecx<'tcx>,
    buffer: Pointer<Option<Prov>>,
    count: u64,
) -> InterpResult<'tcx> {
    // Checked before anything is made, so a count past the buffer allocates nothing here.
    ecx.get_ptr_alloc(buffer, Size::from_bytes(count))?;
    let mut bytes = Vec::new();
    while (bytes.len() as u64) < count {
        let word = ecx.machine.next_random();
        bytes.extend_from_slice(&word.to_le_bytes());
    }
    bytes.truncate(usize::try_from(count).unwrap_or(usize::MAX));
    ecx.write_bytes_ptr(buffer, bytes)
}

/// The value of the constant `libc::{name}` for the target, evaluated from the loaded `libc`
/// crate, as Miri reads it: the library's own number, not a table of ours.
fn libc_constant<'tcx>(ecx: &Ecx<'tcx>, name: &str) -> InterpResult<'tcx, u128> {
    let tcx = *ecx.tcx;
    let constant = tcx
        .crates(())
        .iter()
        .filter(|&&krate| tcx.crate_name(krate).as_str() == "libc")
        .find_map(|&krate| {
            tcx.module_children(krate.as_def_id()).iter().find_map(|child| match child.res {
                Res::Def(DefKind::Const { .. }, id) if child.ident.name.as_str() == name => {
                    Some(id)
                }
                _ => None,
            })
        });
    let Some(constant) = constant else {
        return unsupported(format!(
            "needs `libc::{name}`, and no loaded `libc` crate defines it"
        ));
    };
    match tcx.const_eval_poly(constant).ok().and_then(|value| value.try_to_scalar_int()) {
        Some(value) => interp_ok(value.to_bits_unchecked()),
        None => unsupported(format!("needs `libc::{name}`, which did not evaluate to an integer")),
    }
}

/// The panic runtime entered with a `&PanicInfo`: keep its `fmt::Arguments` for the message,
/// which is formatted once the run has stopped (see [`panic_message`]), and stop.
fn panic_from_info<'tcx, T>(
    ecx: &mut Ecx<'tcx>,
    args: &[FnArg<'tcx, Prov>],
) -> InterpResult<'tcx, T> {
    if let Some(info) = args.first() {
        let info = ecx.deref_pointer(&info.copy_fn_arg())?;
        if let Some(message) = field_index(&info, "message") {
            let message = ecx.project_field(&info, message)?;
            if message.layout.ty.is_ref() {
                ecx.machine.panic_arguments = Some(ecx.deref_pointer(&message)?);
            }
        }
    }
    halt(Halt::Panicked(None))
}

/// std's `begin_panic` with its payload: a string is the message, and anything else is what std
/// prints for it.
fn begin_panic<'tcx, T>(ecx: &Ecx<'tcx>, args: &[FnArg<'tcx, Prov>]) -> InterpResult<'tcx, T> {
    let message = match args.first().map(FnArg::copy_fn_arg) {
        Some(payload) => match payload.layout.ty.kind() {
            ty::Ref(_, inner, _) if inner.is_str() => {
                let text = ecx.deref_pointer(&payload)?;
                Some(ecx.read_str(&text)?.to_string())
            }
            ty::Adt(def, _) if is_string(*ecx.tcx, def.did()) => {
                // A `String` is never an immediate: it is always in memory.
                payload.as_mplace_or_imm().left().map(|place| {
                    string_of(ecx, &place)
                        .unwrap_or_else(|why| format!("a `String` not read: {why}"))
                })
            }
            _ => Some("Box<dyn Any>".to_string()),
        },
        None => None,
    };
    halt(Halt::Panicked(message))
}

/// The compiler's own description of a failed `Assert`, with its operands' values: what is
/// reported when no panic runtime is loaded to print the runtime's message.
fn describe_assert<'tcx>(
    ecx: &Ecx<'tcx>,
    msg: &mir::AssertMessage<'tcx>,
) -> InterpResult<'tcx, String> {
    use crate::rustc_middle::mir::AssertKind::*;
    let int = |op: &mir::Operand<'tcx>| {
        ecx.read_immediate(&ecx.eval_operand(op, None)?).map(|x| x.to_const_int())
    };
    let described = match msg {
        BoundsCheck { len, index } => BoundsCheck { len: int(len)?, index: int(index)? },
        Overflow(op, l, r) => Overflow(*op, int(l)?, int(r)?),
        OverflowNeg(op) => OverflowNeg(int(op)?),
        DivisionByZero(op) => DivisionByZero(int(op)?),
        RemainderByZero(op) => RemainderByZero(int(op)?),
        ResumedAfterReturn(kind) => ResumedAfterReturn(*kind),
        ResumedAfterPanic(kind) => ResumedAfterPanic(*kind),
        ResumedAfterDrop(kind) => ResumedAfterDrop(*kind),
        MisalignedPointerDereference { required, found } => {
            MisalignedPointerDereference { required: int(required)?, found: int(found)? }
        }
        NullPointerDereference => NullPointerDereference,
        NullReferenceConstructed => NullReferenceConstructed,
        InvalidEnumConstruction(source) => InvalidEnumConstruction(int(source)?),
    };
    interp_ok(described.to_string())
}

/// The index of the field named `name` in a struct place, if it has one.
fn field_index(place: &MPlaceTy<'_, Prov>, name: &str) -> Option<FieldIdx> {
    let ty::Adt(def, _) = place.layout.ty.kind() else { return None };
    if !def.is_struct() {
        return None;
    }
    def.non_enum_variant()
        .fields
        .iter_enumerated()
        .find(|(_, field)| field.name.as_str() == name)
        .map(|(index, _)| index)
}

/// Run `entry`, a function of no arguments in the local crate, to its value, and render it:
/// with [`RENDER`]'s functions when the source has them (it has a library), and otherwise, for
/// a `no_core` source, with [`render_primitive`].
///
/// Every path printed on the way (a type, a refusal, an error) is printed in full. A trimmed
/// path is computed from the whole crate's imports and is only for diagnostics: a session that
/// trims one and then emits no diagnostic panics when it ends, and a run that succeeds emits
/// none.
pub(super) fn run_entry(
    tcx: TyCtxt<'_>,
    entry: LocalDefId,
    render: Option<Render>,
    budget: Option<u64>,
) -> Evaluation {
    with_no_trimmed_paths!(run(tcx, entry, render, budget))
}

fn run<'tcx>(
    tcx: TyCtxt<'tcx>,
    entry: LocalDefId,
    render: Option<Render>,
    budget: Option<u64>,
) -> Evaluation {
    let instance = ty::Instance::mono(tcx, entry.to_def_id());
    let typing_env = ty::TypingEnv::fully_monomorphized();
    let machine = Evaluator::new(render.as_ref().map(|render| render.sink));
    let mut ecx = InterpCx::new(tcx, tcx.def_span(entry.to_def_id()), typing_env, machine);
    let mut steps = 0;
    let started = prepare(&mut ecx).and_then(|()| start(&mut ecx, instance));
    let finished =
        started.and_then(|place| drive(&mut ecx, &mut steps, budget).map(|done| (place, done)));
    match finished {
        Ok((_, false)) => Evaluation::Exhausted { steps },
        Ok((place, true)) => {
            let ty = place.layout.ty.to_string();
            match render {
                Some(render) => render_by_debug(&mut ecx, &render, &place, ty, steps),
                None => match render_primitive(&ecx, &place, 0) {
                    Ok(rendered) => Evaluation::Value { rendered, ty, steps, render_steps: 0 },
                    Err(why) => Evaluation::Refused {
                        why: format!("the call returned a `{ty}`, which is not rendered: {why}"),
                    },
                },
            }
        }
        Err(err) => ended(&mut ecx, err, steps),
    }
}

/// What the machine makes before a run: the null an absent weak symbol reads as.
fn prepare<'tcx>(ecx: &mut Ecx<'tcx>) -> InterpResult<'tcx> {
    let layout = &ecx.tcx.data_layout;
    let null = alloc::vec![0u8; layout.pointer_size().bytes_usize()];
    let align = layout.pointer_align().abi;
    let kind = MemoryKind::Machine(Kind::Machine);
    let absent = ecx.allocate_bytes_ptr(&null, align, kind, Mutability::Not)?;
    ecx.machine.absent_symbol = Some(absent);
    interp_ok(())
}

/// The outcome of a run that stopped with `err` after `steps` steps.
///
/// A panic's message is formatted outside the budget, as a value is rendered: the budget
/// stops a call that runs away, and the call has already stopped. `steps` are the call's.
fn ended<'tcx>(ecx: &mut Ecx<'tcx>, err: InterpErrorInfo<'tcx>, steps: u64) -> Evaluation {
    match stopped(ecx, err) {
        Halt::Refused(why) => Evaluation::Refused { why },
        Halt::Unsupported(why) => Evaluation::Unsupported { why },
        Halt::Panicked(Some(message)) => Evaluation::Panicked { message, steps },
        Halt::Panicked(None) => Evaluation::Panicked { message: panic_message(ecx, None), steps },
    }
}

/// The value at `place` as its type's own `Debug` prints it: [`RENDER`]'s generic function run
/// on it, on the same interpreter. `ty` stays the call's type, `steps` the call's own, and
/// `render_steps` what the rendering took.
///
/// **Outside the budget.** The budget stops a call that runs away (a range walk to
/// `isize::MAX`); it is not a limit on how long a finished value takes to print. A rendering
/// counted against it made two calls whose values differ only in how they print (a negative
/// number takes more steps than a positive one) differ in whether they finished, and an answer
/// dropped for that hid the one input two readings disagree on. What there is to render is what
/// the call built within its budget.
///
/// A type with no `Debug` is refused, and so is one whose `Debug` fails or panics: the call ran,
/// but there is no rendering of its value to give.
fn render_by_debug<'tcx>(
    ecx: &mut Ecx<'tcx>,
    render: &Render,
    place: &MPlaceTy<'tcx, Prov>,
    ty: String,
    steps: u64,
) -> Evaluation {
    if !implements_debug(*ecx.tcx, place.layout.ty) {
        return Evaluation::Refused {
            why: format!("the call returned a `{ty}`, which does not implement `Debug`"),
        };
    }
    let mut rendering = 0;
    ecx.machine.rendered.clear();
    // With no budget the drive runs until the rendering returns; `done` is always true.
    let finished = start_render(ecx, render.debug, place)
        .and_then(|result| drive(ecx, &mut rendering, None).map(|_| result));
    match finished {
        Ok(result) => match ecx.read_discriminant(&result) {
            Ok(variant) if variant == FIRST_VARIANT => {
                let rendered = core::mem::take(&mut ecx.machine.rendered);
                Evaluation::Value { rendered, ty, steps, render_steps: rendering }
            }
            Ok(_) => Evaluation::Refused {
                why: format!("the call returned a `{ty}`, whose `Debug` returned an error"),
            },
            Err(err) => Evaluation::Refused {
                why: format!("the call returned a `{ty}`, not rendered: {}", stopped(ecx, err)),
            },
        },
        Err(err) => match ended(ecx, err, rendering) {
            Evaluation::Panicked { message, .. } => Evaluation::Refused {
                why: format!("the call returned a `{ty}`, whose `Debug` panicked: {message}"),
            },
            Evaluation::Refused { why } => Evaluation::Refused {
                why: format!("the call returned a `{ty}`, not rendered: {why}"),
            },
            other => other,
        },
    }
}

/// A root frame calling `debug::<T>(&value)`, `T` the type of the value at `place`.
fn start_render<'tcx>(
    ecx: &mut Ecx<'tcx>,
    debug: DefId,
    place: &MPlaceTy<'tcx, Prov>,
) -> InterpResult<'tcx, MPlaceTy<'tcx, Prov>> {
    let tcx = *ecx.tcx;
    let ty = place.layout.ty;
    let instance = ty::Instance::new_raw(debug, tcx.mk_args(&[ty.into()]));
    let layout = ecx.layout_of(Ty::new_imm_ref(tcx, tcx.lifetimes.re_erased, ty))?;
    let reference = ImmTy::from_immediate(place.to_ref(&*ecx), layout);
    start_call(ecx, instance, &[FnArg::Copy(reference.into())])
}

/// Whether `ty` implements `core::fmt::Debug`: `<ty as Debug>::fmt` resolves to a function.
fn implements_debug<'tcx>(tcx: TyCtxt<'tcx>, ty: Ty<'tcx>) -> bool {
    let Some(debug) = tcx.get_diagnostic_item(sym::Debug) else { return false };
    let methods = tcx.associated_item_def_ids(debug);
    let Some(&fmt) = methods.iter().find(|&&id| tcx.item_name(id) == sym::fmt) else {
        return false;
    };
    let typing_env = ty::TypingEnv::fully_monomorphized();
    let resolved = ty::Instance::try_resolve(tcx, typing_env, fmt, tcx.mk_args(&[ty.into()]));
    matches!(resolved, Ok(Some(_)))
}

/// The root frame: `instance`'s body, returning into a fresh place of its return type, as the
/// compile-time interpreter starts a constant's body.
fn start<'tcx>(
    ecx: &mut Ecx<'tcx>,
    instance: ty::Instance<'tcx>,
) -> InterpResult<'tcx, MPlaceTy<'tcx, Prov>> {
    let body = body_of(ecx, instance)?;
    let tcx = *ecx.tcx;
    // The entry returns `impl Sized`; revealing it gives the call's own type.
    let ty = tcx.normalize_erasing_regions(
        ecx.typing_env(),
        ty::Unnormalized::new_wip(body.local_decls[mir::RETURN_PLACE].ty),
    );
    let layout = ecx.layout_of(ty)?;
    let place = ecx.allocate(layout, MemoryKind::Stack)?;
    ecx.push_stack_frame_raw(
        instance,
        body,
        &place.clone().into(),
        ReturnContinuation::Stop { cleanup: false },
    )?;
    ecx.push_stack_frame_done()?;
    interp_ok(place)
}

/// Step until the stack is empty (`true`) or `budget` steps have run (`false`). A step is one
/// MIR statement or terminator.
fn drive<'tcx>(
    ecx: &mut Ecx<'tcx>,
    steps: &mut u64,
    budget: Option<u64>,
) -> InterpResult<'tcx, bool> {
    loop {
        if ecx.machine.stack.is_empty() {
            return interp_ok(true);
        }
        if budget.is_some_and(|budget| *steps >= budget) {
            return interp_ok(false);
        }
        if !ecx.step()? {
            return interp_ok(true);
        }
        *steps += 1;
    }
}

/// What stopped a run, as the outcome it is. An error of the interpreter's own (not a stop of
/// this machine's) names the function it happened in, the innermost frame still on the stack.
fn stopped<'tcx>(ecx: &Ecx<'tcx>, err: InterpErrorInfo<'tcx>) -> Halt {
    let within = match ecx.machine.stack.last() {
        Some(frame) => format!(" (in `{}`)", frame.instance()),
        None => String::new(),
    };
    match err.into_kind() {
        InterpErrorKind::MachineStop(stop) => {
            let shown = stop.to_string();
            let stop: Box<dyn Any> = stop;
            match stop.downcast::<Halt>() {
                Ok(halt) => *halt,
                Err(_) => Halt::Unsupported(shown),
            }
        }
        // What the program does: its answer, and the candidate's to be wrong about.
        InterpErrorKind::UndefinedBehavior(ub) => {
            Halt::Refused(format!("undefined behavior{within}: {ub}"))
        }
        InterpErrorKind::ResourceExhaustion(ResourceExhaustionInfo::StackFrameLimitReached) => {
            Halt::Refused(format!("the call stack grew past {MAX_FRAMES} frames"))
        }
        // The interpreter's words for this, "an error has already been reported elsewhere",
        // hide the error. It is a constant that failed or a type with an error in it: rustc
        // reported it into this session, and `evaluate` adds what it said.
        InterpErrorKind::InvalidProgram(InvalidProgramInfo::AlreadyReported(_)) => {
            Halt::Refused(format!("rustc reported an error{within}, which stops the run"))
        }
        // What this interpreter could not do: an operation it does not support, a program it
        // cannot lay out generically, memory it ran out of. None says anything of the program.
        other @ (InterpErrorKind::Unsupported(_)
        | InterpErrorKind::InvalidProgram(_)
        | InterpErrorKind::ResourceExhaustion(_)) => {
            Halt::Unsupported(format!("{other}{within}"))
        }
    }
}

/// The message of a panic whose `fmt::Arguments` the run kept, formatted by the library's own
/// `alloc::fmt::format` on the same interpreter, so a `{}` in it prints as the program would.
fn panic_message(ecx: &mut Ecx<'_>, budget: Option<u64>) -> String {
    let Some(arguments) = ecx.machine.panic_arguments.take() else {
        return "explicit panic".to_string();
    };
    format_arguments(ecx, &arguments, budget)
        .unwrap_or_else(|why| format!("a panic whose message was not formatted: {why}"))
}

fn format_arguments<'tcx>(
    ecx: &mut Ecx<'tcx>,
    arguments: &MPlaceTy<'tcx, Prov>,
    budget: Option<u64>,
) -> Result<String, String> {
    let tcx = *ecx.tcx;
    let format = alloc_format(tcx).ok_or("the `alloc` crate is not loaded")?;
    // The panicking frames stay unpopped: their locals, which the arguments point into, are
    // still in memory. Only the stack is dropped, so a new root frame can start.
    ecx.machine.stack.clear();
    let mut steps = 0;
    let instance = ty::Instance::mono(tcx, format);
    let place = match start_call(ecx, instance, &[FnArg::Copy(arguments.clone().into())]) {
        Ok(place) => place,
        Err(err) => return Err(stopped(ecx, err).to_string()),
    };
    match drive(ecx, &mut steps, budget) {
        Ok(true) => string_of(ecx, &place),
        Ok(false) => Err("the step budget ran out while formatting it".to_string()),
        Err(err) => Err(stopped(ecx, err).to_string()),
    }
}

/// A root frame calling `instance` with `args`, returning into a fresh place.
fn start_call<'tcx>(
    ecx: &mut Ecx<'tcx>,
    instance: ty::Instance<'tcx>,
    args: &[FnArg<'tcx, Prov>],
) -> InterpResult<'tcx, MPlaceTy<'tcx, Prov>> {
    let body = body_of(ecx, instance)?;
    let fn_abi = ecx.fn_abi_of_instance_no_deduced_attrs(instance, ty::List::empty())?;
    let place = ecx.allocate(fn_abi.ret.layout, MemoryKind::Stack)?;
    ecx.init_stack_frame(
        instance,
        body,
        fn_abi,
        args,
        false,
        &place.clone().into(),
        ReturnContinuation::Stop { cleanup: false },
    )?;
    interp_ok(place)
}

/// `alloc::fmt::format`, found by path in the loaded `alloc` crate.
fn alloc_format(tcx: TyCtxt<'_>) -> Option<DefId> {
    let child = |module: DefId, name: &str, kind: DefKind| {
        tcx.module_children(module).iter().find_map(|child| match child.res {
            Res::Def(found, id) if found == kind && child.ident.name.as_str() == name => Some(id),
            _ => None,
        })
    };
    tcx.crates(()).iter().filter(|&&krate| tcx.crate_name(krate) == sym::alloc).find_map(|&krate| {
        let fmt = child(krate.as_def_id(), "fmt", DefKind::Mod)?;
        child(fmt, "format", DefKind::Fn)
    })
}

type Rendered = Result<String, String>;

fn read<'tcx, T>(result: InterpResult<'tcx, T>) -> Result<T, String> {
    result.map_err(|err| err.to_string())
}

/// A value of a `no_core` source, as `{:?}` prints it, read from the interpreter's memory by its
/// type's layout: integers, `bool`, `char`, `f32` and `f64`, `str`, references, arrays, slices
/// and tuples, nested. Any other type is refused rather than printed some other way.
///
/// Why it is still here: a `no_core` source has no `core::fmt`, so there is no `Debug` to run
/// and [`render_by_debug`] cannot serve it. Every source with a library is rendered by its
/// type's own `Debug` instead; this covers only the primitive shapes a `no_core` source can
/// build, and derives nothing a library defines.
fn render_primitive<'tcx>(ecx: &Ecx<'tcx>, place: &MPlaceTy<'tcx, Prov>, depth: u32) -> Rendered {
    if depth > 256 {
        return Err("the value is nested too deeply".to_string());
    }
    let ty = place.layout.ty;
    let scalar = || read(ecx.read_scalar(place));
    match ty.kind() {
        ty::Bool => Ok(read(scalar()?.to_bool())?.to_string()),
        ty::Char => Ok(format!("{:?}", read(scalar()?.to_char())?)),
        ty::Int(_) => Ok(read(scalar()?.to_int(place.layout.size))?.to_string()),
        ty::Uint(_) => Ok(read(scalar()?.to_uint(place.layout.size))?.to_string()),
        ty::Float(ty::FloatTy::F32) => {
            Ok(format!("{:?}", f32::from_bits(read(scalar()?.to_u32())?)))
        }
        ty::Float(ty::FloatTy::F64) => {
            Ok(format!("{:?}", f64::from_bits(read(scalar()?.to_u64())?)))
        }
        ty::Str => Ok(format!("{:?}", read(ecx.read_str(place))?)),
        ty::Ref(..) => render_primitive(ecx, &read(ecx.deref_pointer(place))?, depth + 1),
        ty::Array(..) | ty::Slice(_) => {
            let len = read(place.len(ecx))?;
            let mut parts = Vec::new();
            for index in 0..len {
                let element = read(ecx.project_index(place, index))?;
                parts.push(render_primitive(ecx, &element, depth + 1)?);
            }
            Ok(format!("[{}]", parts.join(", ")))
        }
        ty::Tuple(fields) => {
            let mut parts = Vec::with_capacity(fields.len());
            for index in 0..fields.len() {
                let field = read(ecx.project_field(place, FieldIdx::from_usize(index)))?;
                parts.push(render_primitive(ecx, &field, depth + 1)?);
            }
            Ok(match parts.len() {
                1 => format!("({},)", parts[0]),
                _ => format!("({})", parts.join(", ")),
            })
        }
        _ => Err(format!("`{ty}` is not a type whose value is rendered without a library")),
    }
}

/// Whether `did` is `alloc::string::String`, which the library marks as a lang item (`Vec` is a
/// diagnostic item instead); the diagnostic item is accepted too, for libraries that mark it so.
fn is_string(tcx: TyCtxt<'_>, did: DefId) -> bool {
    tcx.is_lang_item(did, LangItem::String) || tcx.is_diagnostic_item(sym::String, did)
}

/// A `String`'s text: its `vec` field's bytes.
fn string_of<'tcx>(ecx: &Ecx<'tcx>, place: &MPlaceTy<'tcx, Prov>) -> Result<String, String> {
    let vec = field_index(place, "vec").ok_or("a `String` with no `vec` field")?;
    let vec = read(ecx.project_field(place, vec))?;
    let (pointer, len) = vec_parts(ecx, &vec)?;
    let bytes = read(ecx.read_bytes_ptr_strip_provenance(pointer, Size::from_bytes(len)))?;
    String::from_utf8(bytes.to_vec()).map_err(|err| err.to_string())
}

/// A `Vec`'s buffer and length: its `len` field, and the one raw pointer its `buf` holds. The
/// buffer's own shape (`RawVec`, `RawVecInner`, `Unique`, `NonNull`) differs between library
/// versions; the first raw pointer inside it is the buffer in every one of them.
fn vec_parts<'tcx>(
    ecx: &Ecx<'tcx>,
    place: &MPlaceTy<'tcx, Prov>,
) -> Result<(Pointer<Option<Prov>>, u64), String> {
    let len = field_index(place, "len").ok_or("a `Vec` with no `len` field")?;
    let len = read(ecx.read_target_usize(&read(ecx.project_field(place, len))?))?;
    let buf = field_index(place, "buf").ok_or("a `Vec` with no `buf` field")?;
    let buf = read(ecx.project_field(place, buf))?;
    let pointer = first_pointer(ecx, &buf, 0)?.ok_or("a `Vec` whose buffer holds no pointer")?;
    Ok((pointer, len))
}

fn first_pointer<'tcx>(
    ecx: &Ecx<'tcx>,
    place: &MPlaceTy<'tcx, Prov>,
    depth: u32,
) -> Result<Option<Pointer<Option<Prov>>>, String> {
    match place.layout.ty.kind() {
        ty::RawPtr(..) => Ok(Some(read(ecx.read_pointer(place))?)),
        // `NonNull`'s field is a pattern type now, `pattern_type!(*const T is !null)`: a raw
        // pointer with a restricted range, laid out as the pointer itself.
        ty::Pat(base, _) if base.is_raw_ptr() => Ok(Some(read(ecx.read_pointer(place))?)),
        ty::Adt(def, _) if def.is_struct() && depth < 8 => {
            for index in 0..def.non_enum_variant().fields.len() {
                let field = read(ecx.project_field(place, FieldIdx::from_usize(index)))?;
                if let Some(pointer) = first_pointer(ecx, &field, depth + 1)? {
                    return Ok(Some(pointer));
                }
            }
            Ok(None)
        }
        _ => Ok(None),
    }
}
