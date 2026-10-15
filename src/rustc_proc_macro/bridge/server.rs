//! Server-side traits.

// `with_api!` is defined in `bridge/mod.rs` and names `String` and `Vec` in the signatures it
// generates. A `macro_rules!` body resolves those at the expansion site, which is here.
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::Cell;
use core::hash::Hash;
use core::ops::{Bound, Range};
use core::sync::atomic::AtomicU32;

// A prelude macro names no path, which is why a `std::` search never saw this one.
use eko::thread_local;

use crate::rustc_proc_macro::bridge::{
    ApiTags, BridgeConfig, Buffer, Decode, Diagnostic, Encode, ExpnGlobals, Literal, Mark, Marked,
    PanicMessage, TokenTree, client, handle,
};

pub(super) struct HandleStore<S: Server> {
    token_stream: handle::OwnedStore<MarkedTokenStream<S>>,
    span: handle::InternedStore<MarkedSpan<S>>,
}

impl<S: Server> HandleStore<S> {
    fn new() -> Self {
        static TOKEN_STREAM: AtomicU32 = AtomicU32::new(1);
        static SPAN: AtomicU32 = AtomicU32::new(1);

        HandleStore {
            token_stream: handle::OwnedStore::new(&TOKEN_STREAM),
            span: handle::InternedStore::new(&SPAN),
        }
    }
}

pub(super) type MarkedTokenStream<S> = Marked<<S as Server>::TokenStream, client::TokenStream>;
pub(super) type MarkedSpan<S> = Marked<<S as Server>::Span, client::Span>;
pub(super) type MarkedSymbol<S> = Marked<<S as Server>::Symbol, client::Symbol>;

impl<S: Server> Encode<HandleStore<S>> for MarkedTokenStream<S> {
    fn encode(self, w: &mut Buffer, s: &mut HandleStore<S>) {
        s.token_stream.alloc(self).encode(w, s);
    }
}

impl<S: Server> Decode<'_, '_, HandleStore<S>> for MarkedTokenStream<S> {
    fn decode(r: &mut &[u8], s: &mut HandleStore<S>) -> Self {
        s.token_stream.take(handle::Handle::decode(r, &mut ()))
    }
}

impl<'s, S: Server> Decode<'_, 's, HandleStore<S>> for &'s MarkedTokenStream<S> {
    fn decode(r: &mut &[u8], s: &'s mut HandleStore<S>) -> Self {
        &s.token_stream[handle::Handle::decode(r, &mut ())]
    }
}

impl<S: Server> Encode<HandleStore<S>> for MarkedSpan<S> {
    fn encode(self, w: &mut Buffer, s: &mut HandleStore<S>) {
        s.span.alloc(self).encode(w, s);
    }
}

impl<S: Server> Decode<'_, '_, HandleStore<S>> for MarkedSpan<S> {
    fn decode(r: &mut &[u8], s: &mut HandleStore<S>) -> Self {
        s.span.copy(handle::Handle::decode(r, &mut ()))
    }
}

macro_rules! define_server {
    (
        $(fn $method:ident($($arg:ident: $arg_ty:ty),* $(,)?) $(-> $ret_ty:ty)?;)*
    ) => {
        #[expect(clippy::result_unit_err, reason = "the bridge protocol uses unit failure sentinels for expansion and identifier validation")]
        pub trait Server {
            type TokenStream: 'static + Clone + Default;
            type Span: 'static + Copy + Eq + Hash;
            type Symbol: 'static;

            fn globals(&mut self) -> ExpnGlobals<Self::Span>;

            /// Intern a symbol received from RPC
            fn intern_symbol(ident: &str) -> Self::Symbol;

            /// Recover the string value of a symbol, and invoke a callback with it.
            fn with_symbol_string(symbol: &Self::Symbol, f: impl FnOnce(&str));

            $(fn $method(&mut self, $($arg: $arg_ty),*) $(-> $ret_ty)?;)*
        }
    }
}
with_api!(define_server, Self::TokenStream, Self::Span, Self::Symbol);

// FIXME(eddyb) `pub` only for `ExecutionStrategy` below.
pub struct Dispatcher<S: Server> {
    handle_store: HandleStore<S>,
    server: S,
}

macro_rules! define_dispatcher {
    (
        $(fn $method:ident($($arg:ident: $arg_ty:ty),* $(,)?) $(-> $ret_ty:ty)?;)*
    ) => {
        impl<S: Server> Dispatcher<S> {
            fn dispatch(&mut self, mut buf: Buffer) -> Buffer {
                let Dispatcher { handle_store, server } = self;

                let mut reader = &buf[..];
                match ApiTags::decode(&mut reader, &mut ()) {
                    $(ApiTags::$method => {
                        let call_method = || {
                            $(let $arg = <$arg_ty>::decode(&mut reader, handle_store).unmark();)*
                            let r = server.$method($($arg),*);
                            $(let r: $ret_ty = Mark::mark(r);)?
                            r
                        };
                        // Upstream guarded this with `thread::panicking()`, because
                        // `std::panic::catch_unwind` asserts the panic counter is zero on entry
                        // and client and server may share one `std`. `crate::unwind_janky::catch` is
                        // the bare `core::intrinsics::catch_unwind` with no such assertion and
                        // no counter to consult, so the guard has nothing left to prevent and
                        // the branch is gone rather than reproduced against a counter we do not
                        // keep. See `crates/unwind-janky`.
                        let r = crate::unwind_janky::catch(call_method).map_err(PanicMessage::from);

                        buf.clear();
                        r.encode(&mut buf, handle_store);
                    })*
                }
                buf
            }
        }
    }
}
with_api!(define_dispatcher, MarkedTokenStream<S>, MarkedSpan<S>, MarkedSymbol<S>);

/// Runs a proc-macro client through the existing bridge protocol.
pub trait ExecutionStrategy {
    fn run_bridge_and_client(
        &self,
        dispatcher: &mut Dispatcher<impl Server>,
        input: Buffer,
        run_client: extern "C" fn(BridgeConfig<'_>) -> Buffer,
        force_show_panics: bool,
    ) -> Buffer;
}

thread_local! {
    // BRIDGE_STATE restores its previous pointer, but the client also clears
    // its separate symbol interner on entry and exit. Refuse nesting before
    // entering the client so the outer invocation's symbols remain valid.
    static ACTIVE_CLIENT: Cell<bool> = const { Cell::new(false) };
}

struct ActiveClientGuard;

impl ActiveClientGuard {
    fn enter() -> Result<Self, PanicMessage> {
        if ACTIVE_CLIENT.get() {
            return Err(PanicMessage::StaticStr(
                "frontend_proc_macro_nested_bridge: nested proc-macro execution is unsupported because the client symbol interner is not reentrant",
            ));
        }
        ACTIVE_CLIENT.set(true);
        Ok(Self)
    }
}

impl Drop for ActiveClientGuard {
    fn drop(&mut self) {
        ACTIVE_CLIENT.set(false);
    }
}

/// Executes a client on the caller's thread, without a message pipe or worker.
pub struct SameThread;

fn bridge_error(mut input: Buffer, error: PanicMessage) -> Buffer {
    input.clear();
    // The Err arm has no success value, so its wire encoding is independent
    // of the token-stream result the caller will decode.
    Result::<(), PanicMessage>::Err(error).encode(&mut input, &mut ());
    input
}

impl ExecutionStrategy for SameThread {
    fn run_bridge_and_client(
        &self,
        dispatcher: &mut Dispatcher<impl Server>,
        input: Buffer,
        run_client: extern "C" fn(BridgeConfig<'_>) -> Buffer,
        force_show_panics: bool,
    ) -> Buffer {
        let _guard = match ActiveClientGuard::enter() {
            Ok(guard) => guard,
            Err(error) => return bridge_error(input, error),
        };
        let mut dispatch = |buf| dispatcher.dispatch(buf);
        run_client(BridgeConfig { input, dispatch: (&mut dispatch).into(), force_show_panics })
    }
}

/// Compatibility wrapper for callers compiled against the published strategy API.
/// A cross-thread request is refused; it is never converted into same-thread work.
pub struct MaybeCrossThread {
    /// Legacy request, retained for source compatibility. A true value returns
    /// frontend_proc_macro_cross_thread_removed through the bridge protocol.
    pub cross_thread: bool,
}

/// Published same-thread strategy value, retaining its original type.
pub const SAME_THREAD: MaybeCrossThread = MaybeCrossThread { cross_thread: false };
/// Published legacy value. Execution returns a named refusal without entering the client.
pub const CROSS_THREAD: MaybeCrossThread = MaybeCrossThread { cross_thread: true };

impl ExecutionStrategy for MaybeCrossThread {
    fn run_bridge_and_client(
        &self,
        dispatcher: &mut Dispatcher<impl Server>,
        input: Buffer,
        run_client: extern "C" fn(BridgeConfig<'_>) -> Buffer,
        force_show_panics: bool,
    ) -> Buffer {
        if self.cross_thread {
            return bridge_error(
                input,
                PanicMessage::StaticStr(
                    "frontend_proc_macro_cross_thread_removed: cross-thread proc-macro execution has been removed",
                ),
            );
        }
        SameThread.run_bridge_and_client(dispatcher, input, run_client, force_show_panics)
    }
}

fn run_server<
    S: Server,
    I: Encode<HandleStore<S>>,
    O: for<'a, 's> Decode<'a, 's, HandleStore<S>>,
>(
    strategy: &impl ExecutionStrategy,
    server: S,
    input: I,
    run_client: extern "C" fn(BridgeConfig<'_>) -> Buffer,
    force_show_panics: bool,
) -> Result<O, PanicMessage> {
    let mut dispatcher = Dispatcher { handle_store: HandleStore::new(), server };

    let globals = dispatcher.server.globals();

    let mut buf = Buffer::new();
    (<ExpnGlobals<MarkedSpan<S>> as Mark>::mark(globals), input)
        .encode(&mut buf, &mut dispatcher.handle_store);

    buf = strategy.run_bridge_and_client(&mut dispatcher, buf, run_client, force_show_panics);

    Result::decode(&mut &buf[..], &mut dispatcher.handle_store)
}

impl client::Client {
    pub fn run1<S>(
        &self,
        strategy: &impl ExecutionStrategy,
        server: S,
        input: S::TokenStream,
        force_show_panics: bool,
    ) -> Result<S::TokenStream, PanicMessage>
    where
        S: Server,
    {
        let client::Client { run } = *self;
        run_server(strategy, server, <MarkedTokenStream<S>>::mark(input), run, force_show_panics)
            .map(|s| <Option<MarkedTokenStream<S>>>::unmark(s).unwrap_or_default())
    }

    pub fn run2<S>(
        &self,
        strategy: &impl ExecutionStrategy,
        server: S,
        input: S::TokenStream,
        input2: S::TokenStream,
        force_show_panics: bool,
    ) -> Result<S::TokenStream, PanicMessage>
    where
        S: Server,
    {
        let client::Client { run } = *self;
        run_server(
            strategy,
            server,
            (<MarkedTokenStream<S>>::mark(input), <MarkedTokenStream<S>>::mark(input2)),
            run,
            force_show_panics,
        )
        .map(|s| <Option<MarkedTokenStream<S>>>::unmark(s).unwrap_or_default())
    }
}
