//! Server-side traits.

// `with_api!` is defined in `bridge/mod.rs` and names `String` and `Vec` in the signatures it
// generates. A `macro_rules!` body resolves those at the expansion site, which is here.
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::Cell;
use core::hash::Hash;
use core::ops::{Bound, Range};
use core::sync::atomic::AtomicU32;

use eko::channel;
use eko::thread;
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

// This trait is currently only implemented and used once, inside of this crate.
// We keep it public to allow implementing more complex execution strategies in
// the future, such as wasm proc-macros.
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
    /// While running a proc-macro with the same-thread executor, this flag will
    /// be set, forcing nested proc-macro invocations (e.g. due to
    /// `TokenStream::expand_expr`) to be run using a cross-thread executor.
    ///
    /// This is required as the thread-local state in the proc_macro client does
    /// not handle being re-entered, and will invalidate all `Symbol`s when
    /// entering a nested macro.
    static ALREADY_RUNNING_SAME_THREAD: Cell<bool> = const { Cell::new(false) };
}

/// Keep `ALREADY_RUNNING_SAME_THREAD` (see also its documentation)
/// set to `true`, preventing same-thread reentrance.
struct RunningSameThreadGuard(());

impl RunningSameThreadGuard {
    fn new() -> Self {
        let already_running = ALREADY_RUNNING_SAME_THREAD.replace(true);
        assert!(
            !already_running,
            "same-thread nesting (\"reentrance\") of proc macro executions is not supported"
        );
        RunningSameThreadGuard(())
    }
}

impl Drop for RunningSameThreadGuard {
    fn drop(&mut self) {
        ALREADY_RUNNING_SAME_THREAD.set(false);
    }
}

pub struct MaybeCrossThread {
    pub cross_thread: bool,
}

pub const SAME_THREAD: MaybeCrossThread = MaybeCrossThread { cross_thread: false };
pub const CROSS_THREAD: MaybeCrossThread = MaybeCrossThread { cross_thread: true };

impl ExecutionStrategy for MaybeCrossThread {
    fn run_bridge_and_client(
        &self,
        dispatcher: &mut Dispatcher<impl Server>,
        input: Buffer,
        run_client: extern "C" fn(BridgeConfig<'_>) -> Buffer,
        force_show_panics: bool,
    ) -> Buffer {
        if self.cross_thread || ALREADY_RUNNING_SAME_THREAD.get() {
            let (mut server, mut client) = MessagePipe::new();

            let join_handle = thread::spawn(move || {
                let mut dispatch = |b: Buffer| -> Buffer {
                    client.send(b);
                    client.recv().expect("server died while client waiting for reply")
                };

                run_client(BridgeConfig {
                    input,
                    dispatch: (&mut dispatch).into(),
                    force_show_panics,
                })
            })
            // `eko`'s `spawn` hands back the `pthread_create` errno where
            // `std::thread::spawn` panics on its own. Same outcome, said here.
            .expect("could not start a thread to run the proc macro on");

            while let Some(b) = server.recv() {
                server.send(dispatcher.dispatch(b));
            }

            // `Err` is a failed `pthread_join`, not a panicking client: a panic inside the
            // client is caught by `run_client` and comes back through the pipe as a
            // `PanicMessage`, which is what the RPC boundary is for.
            join_handle.join().expect("could not join the thread running the proc macro")
        } else {
            let _guard = RunningSameThreadGuard::new();

            let mut dispatch = |buf| dispatcher.dispatch(buf);

            run_client(BridgeConfig { input, dispatch: (&mut dispatch).into(), force_show_panics })
        }
    }
}

/// A message pipe used for communicating between server and client threads.
struct MessagePipe<T> {
    tx: channel::SyncSender<T>,
    rx: channel::Receiver<T>,
}

impl<T> MessagePipe<T> {
    /// Creates a new pair of endpoints for the message pipe.
    fn new() -> (Self, Self) {
        let (tx1, rx1) = channel::sync_channel(1);
        let (tx2, rx2) = channel::sync_channel(1);
        (MessagePipe { tx: tx1, rx: rx2 }, MessagePipe { tx: tx2, rx: rx1 })
    }

    /// Send a message to the other endpoint of this pipe.
    fn send(&mut self, value: T) {
        // Not `unwrap`: the error carries the undelivered `T` back, and `T` here is a `Buffer`,
        // which has no `Debug`.
        if self.tx.send(value).is_err() {
            panic!("the other end of the proc macro message pipe is gone");
        }
    }

    /// Receive a message from the other endpoint of this pipe.
    ///
    /// Returns `None` if the other end of the pipe has been destroyed, and no
    /// message was received.
    fn recv(&mut self) -> Option<T> {
        self.rx.recv().ok()
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
