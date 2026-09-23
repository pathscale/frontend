//! Client-side types.

// `with_api!` is defined in `bridge/mod.rs` and names `String` and `Vec` in the signatures it
// generates. A `macro_rules!` body resolves those at the expansion site, which is here.
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::marker::PhantomData;
use core::ops::{Bound, Range};
use core::{fmt, mem};

use eko::thread::Once;

use crate::rustc_proc_macro::bridge::{
    ApiTags, BridgeConfig, Buffer, Decode, Diagnostic, Encode, ExpnGlobals, Literal, PanicMessage,
    TokenTree, closure, handle,
};

pub(crate) struct TokenStream {
    handle: handle::Handle,
    // Upstream: `impl !Send` and `impl !Sync` (unstable negative impls). A raw-pointer
    // marker makes the type neither, on stable. The public `TokenStream`, `Group` and
    // `TokenTree` contain this one and inherit it.
    _not_send_sync: PhantomData<*const ()>,
}

// Forward `Drop::drop` to the inherent `drop` method.
impl Drop for TokenStream {
    fn drop(&mut self) {
        Methods::ts_drop(TokenStream { handle: self.handle, _not_send_sync: PhantomData });
    }
}

impl<S> Encode<S> for TokenStream {
    #[inline]
    fn encode(self, w: &mut Buffer, s: &mut S) {
        mem::ManuallyDrop::new(self).handle.encode(w, s);
    }
}

impl<S> Encode<S> for &TokenStream {
    #[inline]
    fn encode(self, w: &mut Buffer, s: &mut S) {
        self.handle.encode(w, s);
    }
}

impl<S> Decode<'_, '_, S> for TokenStream {
    #[inline]
    fn decode(r: &mut &[u8], s: &mut S) -> Self {
        TokenStream { handle: handle::Handle::decode(r, s), _not_send_sync: PhantomData }
    }
}

impl Encode<()> for crate::rustc_proc_macro::TokenStream {
    #[inline]
    fn encode(self, w: &mut Buffer, s: &mut ()) {
        self.0.encode(w, s)
    }
}

impl Decode<'_, '_, ()> for crate::rustc_proc_macro::TokenStream {
    #[inline]
    fn decode(r: &mut &[u8], s: &mut ()) -> Self {
        crate::rustc_proc_macro::TokenStream(Some(Decode::decode(r, s)))
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Span {
    handle: handle::Handle,
    // Upstream: `impl !Send` and `impl !Sync` (unstable negative impls); see `TokenStream`.
    // The public `Span`, `Punct`, `Ident` and `Literal` contain this one and inherit it.
    _not_send_sync: PhantomData<*const ()>,
}

impl<S> Encode<S> for Span {
    #[inline]
    fn encode(self, w: &mut Buffer, s: &mut S) {
        self.handle.encode(w, s);
    }
}

impl<S> Decode<'_, '_, S> for Span {
    #[inline]
    fn decode(r: &mut &[u8], s: &mut S) -> Self {
        Span { handle: handle::Handle::decode(r, s), _not_send_sync: PhantomData }
    }
}

impl Clone for TokenStream {
    fn clone(&self) -> Self {
        Methods::ts_clone(self)
    }
}

impl Span {
    pub(crate) fn def_site() -> Span {
        Bridge::with(|bridge| bridge.globals.def_site)
    }

    pub(crate) fn call_site() -> Span {
        Bridge::with(|bridge| bridge.globals.call_site)
    }

    pub(crate) fn mixed_site() -> Span {
        Bridge::with(|bridge| bridge.globals.mixed_site)
    }
}

impl fmt::Debug for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&Methods::span_debug(*self))
    }
}

pub(crate) use super::Methods;
pub(crate) use super::symbol::Symbol;

macro_rules! define_client_side {
    (
        $(fn $method:ident($($arg:ident: $arg_ty:ty),* $(,)?) $(-> $ret_ty:ty)?;)*
    ) => {
        impl Methods {
            $(pub(crate) fn $method($($arg: $arg_ty),*) $(-> $ret_ty)? {
                Bridge::with(|bridge| {
                    let mut buf = bridge.cached_buffer.take();

                    buf.clear();
                    ApiTags::$method.encode(&mut buf, &mut ());
                    $($arg.encode(&mut buf, &mut ());)*

                    buf = bridge.dispatch.call(buf);

                    let r = Result::<_, PanicMessage>::decode(&mut &buf[..], &mut ());

                    bridge.cached_buffer = buf;

                    // `crate::unwind_janky::resume` cannot re-raise the original object, so the
                    // message is raised afresh here rather than boxed and thrown away. What is
                    // lost is the payload's *type*: a proc macro that panicked with
                    // `panic_any(MyError)` reaches its own `catch_unwind` as a string.
                    r.unwrap_or_else(|e| match e.into_string() {
                        Some(message) => panic!("{message}"),
                        None => panic!("proc macro panicked"),
                    })
                })
            })*
        }
    }
}
with_api!(define_client_side, TokenStream, Span, Symbol);

struct Bridge<'a> {
    /// Reusable buffer (only `clear`-ed, never shrunk), primarily
    /// used for making requests.
    cached_buffer: Buffer,

    /// Server-side function that the client uses to make requests.
    dispatch: closure::Closure<'a>,

    /// Provided globals for this macro expansion.
    globals: ExpnGlobals<Span>,
}

// Upstream also wrote `impl !Send` / `impl !Sync` here (unstable negative impls). They are
// redundant: `closure::Closure` holds a raw-pointer marker, so `Bridge` is neither already.

#[allow(unsafe_code)]
mod state {
    // An inline module does not inherit the enclosing module's `use` items, and
    // `thread_local!` is a prelude macro that names no path - so both have to be said again.
    use core::cell::{Cell, RefCell};
    use core::ptr;

    use eko::thread_local;

    use super::Bridge;

    thread_local! {
        static BRIDGE_STATE: Cell<*const ()> = const { Cell::new(ptr::null()) };
    }

    pub(super) fn set<'bridge, R>(state: &RefCell<Bridge<'bridge>>, f: impl FnOnce() -> R) -> R {
        struct RestoreOnDrop(*const ());
        impl Drop for RestoreOnDrop {
            fn drop(&mut self) {
                BRIDGE_STATE.set(self.0);
            }
        }

        let inner = ptr::from_ref(state).cast();
        let outer = BRIDGE_STATE.replace(inner);
        let _restore = RestoreOnDrop(outer);

        f()
    }

    pub(super) fn with<R>(
        f: impl for<'bridge> FnOnce(Option<&RefCell<Bridge<'bridge>>>) -> R,
    ) -> R {
        let state = BRIDGE_STATE.get();
        // SAFETY: the only place where the pointer is set is in `set`. It puts
        // back the previous value after the inner call has returned, so we know
        // that as long as the pointer is not null, it came from a reference to
        // a `RefCell<Bridge>` that outlasts the call to this function. Since `f`
        // works the same for any lifetime of the bridge, including the actual
        // one, we can lie here and say that the lifetime is `'static` without
        // anyone noticing.
        let bridge = unsafe { state.cast::<RefCell<Bridge<'static>>>().as_ref() };
        f(bridge)
    }
}

impl Bridge<'_> {
    fn with<R>(f: impl FnOnce(&mut Bridge<'_>) -> R) -> R {
        state::with(|state| {
            let bridge = state.expect("procedural macro API is used outside of a procedural macro");
            let mut bridge = bridge
                .try_borrow_mut()
                .expect("procedural macro API is used while it's already in use");
            f(&mut bridge)
        })
    }
}

pub(crate) fn is_available() -> bool {
    state::with(|s| s.is_some())
}

/// A client-side RPC entry-point, which may be using a different `proc_macro`
/// from the one used by the server, but can be invoked compatibly.
///
/// Note that the input and output type parameters are erased. They do not
/// participate in the ABI, so while using the wrong runN method will likely
/// result in a panic, it will not result in UB.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct Client {
    pub(super) run: extern "C" fn(BridgeConfig<'_>) -> Buffer,
}

/// Hide the default panic output within `proc_macro` expansions.
///
/// **Blocked without `std`, and left as the shape to restore.** What stood here chained the panic
/// hook: `take_hook` kept the previous one and `set_hook` installed one that stayed silent for a
/// panic the bridge was going to report anyway, deferring to the old hook when the panic could
/// not unwind or when `force_show_panics` was set. There is no hook registry outside `std` -
/// `core` has `PanicInfo` and no way to install anything - and `crates/unwind-janky` deliberately
/// does not define `#[panic_handler]`, because the one already linked owns that symbol. So the
/// two calls are gone and the `Once` only records that we got here.
///
/// The cost is confined to the client side, which is the half of this crate that does not run in
/// this compiler: a proc macro that panics prints the runtime's own line as well as the error the
/// compiler reports from the `PanicMessage`. Nothing is swallowed and nothing is invented.
/// Restore this together with a real panic runtime; see `crates/unwind-janky`'s header for what
/// that costs.
fn maybe_install_panic_hook(force_show_panics: bool) {
    static HIDE_PANICS_DURING_EXPANSION: Once = Once::new();
    let _ = force_show_panics;
    HIDE_PANICS_DURING_EXPANSION.call_once(|| {});
}

/// Client-side helper for handling client panics, entering the bridge,
/// deserializing input and serializing output.
fn run_client<A: for<'a, 's> Decode<'a, 's, ()>>(
    config: BridgeConfig<'_>,
    f: impl FnOnce(A) -> crate::rustc_proc_macro::TokenStream,
) -> Buffer {
    let BridgeConfig { input: mut buf, dispatch, force_show_panics, .. } = config;

    // `crate::unwind_janky::catch` is `std::panic::catch_unwind` reaching the panic runtime without
    // naming `std`. It takes no `UnwindSafe` bound, so the `AssertUnwindSafe` that stood here
    // is gone with it - every caller in this tree wrapped its closure in one, so the bound was
    // carrying no information. See `crates/unwind-janky`.
    let res = crate::unwind_janky::catch(|| {
        maybe_install_panic_hook(force_show_panics);

        // Make sure the symbol store is empty before decoding inputs.
        Symbol::invalidate_all();

        let reader = &mut &buf[..];
        let (globals, input) = <(ExpnGlobals<Span>, A)>::decode(reader, &mut ());

        // Put the buffer we used for input back in the `Bridge` for requests.
        let state = RefCell::new(Bridge { cached_buffer: buf.take(), dispatch, globals });

        let output = state::set(&state, || f(input));

        // Take the `cached_buffer` back out, for the output value.
        buf = RefCell::into_inner(state).cached_buffer;

        output
    });

    // Serialize response of type `Result<R, PanicMessage>`.
    buf.clear();
    res.map_err(PanicMessage::from).encode(&mut buf, &mut ());

    // Now that a response has been serialized, invalidate all symbols
    // registered with the interner.
    Symbol::invalidate_all();
    buf
}

impl Client {
    pub const fn expand1(f: impl Fn(crate::rustc_proc_macro::TokenStream) -> crate::rustc_proc_macro::TokenStream + Copy) -> Self {
        Client {
            run: super::selfless_reify::reify_to_extern_c_fn_hrt_bridge(move |bridge| {
                run_client(bridge, f)
            }),
        }
    }

    pub const fn expand2(
        f: impl Fn(crate::rustc_proc_macro::TokenStream, crate::rustc_proc_macro::TokenStream) -> crate::rustc_proc_macro::TokenStream + Copy,
    ) -> Self {
        Client {
            run: super::selfless_reify::reify_to_extern_c_fn_hrt_bridge(move |bridge| {
                run_client(bridge, |(input, input2)| f(input, input2))
            }),
        }
    }
}
