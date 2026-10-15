//! Owned parse identities, with synchronization checked before they can be shared.

use alloc::sync::Arc;

use super::source_map::SourceMap;
use super::{Edition, SessionGlobals, with_parse_globals};
use crate::rustc_data_structures::sync::enter_session_width;

/// A checked owner of parse symbol, span and hygiene identities.
///
/// Conditional locks remain confined to SessionGlobals. This handle exposes no field or mutable
/// reference, and can only own globals whose conditional locks were built synchronized.
#[derive(Clone)]
pub struct OwnedParseGlobals(Arc<SessionGlobals>);

// SAFETY: constructors check every conditional lock that is shared: the interner's write lock,
// span interner and hygiene data. Their mode never changes. Metavariable spans use FreezeLock,
// source maps use real RwLocks, and the interner's read tables use atomic publication. The
// interner owns its string arena, serialized by its write lock; references into it stay valid
// while this Arc owns it. No stack, compiler arena, emitter or ImplicitCtxt is stored here.
unsafe impl Send for OwnedParseGlobals {}
// SAFETY: the same checked synchronization and immutable ownership as above.
unsafe impl Sync for OwnedParseGlobals {}

impl OwnedParseGlobals {
    /// Share an existing parse context only when its conditional locks are synchronized.
    #[expect(
        clippy::arc_with_non_send_sync,
        reason = "the checked wrapper provides the Send and Sync contract for shared parse identities"
    )]
    pub fn share(globals: &SessionGlobals) -> Option<Self> {
        globals.can_share_for_parse().then(|| Self(Arc::new(globals.share_for_parse())))
    }

    /// Create independent file identities that remain movable even when parsing serially.
    pub fn for_file(edition: Edition, source_map: Arc<SourceMap>, source_len: usize) -> Self {
        // Only construction needs this latch. It restores the caller's selected width before
        // parsing, so a serial parse still creates no pool task.
        let _mode = enter_session_width(2);
        let mut globals =
            SessionGlobals::new_with_symbol_source_len(edition, &[], None, source_len);
        globals.source_map = Some(source_map);
        Self::share(&globals).expect("file parse globals were constructed synchronized")
    }

    /// Enter these identities on the current thread for parsing or result inspection.
    /// The installed reference is borrowed only for this call and is never handed to a task.
    pub fn enter<R>(&self, f: impl FnOnce() -> R) -> R {
        with_parse_globals(&self.0, f)
    }
}
