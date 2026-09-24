// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

pub use self::caches::{DefIdCache, DefaultCache, QueryCache, SingleCache, VecCache};
pub use self::calls::{TyCtxtAt, TyCtxtEnsureDone, TyCtxtEnsureOk, TyCtxtEnsureResult};
pub use self::into_query_key::IntoQueryKey;
pub use self::job::{
    ActiveKeyStatus, QueryCycle, QueryJob, QueryJobId, QueryLatch, QueryStackFrame, QueryState,
    QueryWaitGraph, QueryWaiter, thread_token,
};
pub use self::keys::{LocalCrate, QueryKey};
pub use self::node_diagnostics::NodeDiagnostics;
pub use self::system::{QueryMode, QuerySystem, QueryVTable};
pub use crate::rustc_middle::queries::Providers;

pub(crate) mod arena_cached;
mod caches;
pub(crate) mod calls;
pub mod erase;
mod into_query_key;
mod job;
mod keys;
mod node_diagnostics;
pub(crate) mod modifiers;
pub mod on_disk_cache;
pub(crate) mod query_api;
mod system;
