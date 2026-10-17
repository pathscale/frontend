//! Process-wide mode and counters shared by S2 parser wrappers and oracle drivers.
//!
//! Shadow bypass is depth-counted so a full-parser replay cannot recursively enter a fast
//! path. The counter is global because this parser path is synchronous; do not use this API
//! to carry a bypass across concurrently running parser sessions.

use core::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Mode {
    Off = 0,
    Fast = 1,
    Shadow = 2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum Site {
    Path = 0,
    Ty = 1,
    Pat = 2,
    FnParams = 3,
    ExprAtom = 4,
    LetStmt = 5,
}

const SITE_COUNT: usize = 6;
static MODE: AtomicU8 = AtomicU8::new(Mode::Off as u8);
static SHADOW_BYPASS_DEPTH: AtomicUsize = AtomicUsize::new(0);
static ATTEMPTS: [AtomicU64; SITE_COUNT] = [const { AtomicU64::new(0) }; SITE_COUNT];
static HITS: [AtomicU64; SITE_COUNT] = [const { AtomicU64::new(0) }; SITE_COUNT];
static SHADOW_OK: [AtomicU64; SITE_COUNT] = [const { AtomicU64::new(0) }; SITE_COUNT];
static SHADOW_MISMATCH: [AtomicU64; SITE_COUNT] = [const { AtomicU64::new(0) }; SITE_COUNT];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SiteCounts {
    pub attempts: u64,
    pub hits: u64,
    pub shadow_ok: u64,
    pub shadow_mismatches: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub path: SiteCounts,
    pub ty: SiteCounts,
    pub pat: SiteCounts,
    pub fn_params: SiteCounts,
    pub expr_atom: SiteCounts,
    pub let_stmt: SiteCounts,
}

#[inline]
pub fn mode() -> Mode {
    decode_mode(MODE.load(Ordering::Acquire))
}

/// Set the process-wide mode and return the previous mode.
#[inline]
pub fn set_mode(mode: Mode) -> Mode {
    decode_mode(MODE.swap(mode as u8, Ordering::AcqRel))
}

#[inline]
fn decode_mode(value: u8) -> Mode {
    match value {
        0 => Mode::Off,
        2 => Mode::Shadow,
        _ => Mode::Fast,
    }
}

#[inline]
pub fn in_shadow_bypass() -> bool {
    SHADOW_BYPASS_DEPTH.load(Ordering::Acquire) != 0
}

struct ShadowBypassGuard;

impl Drop for ShadowBypassGuard {
    fn drop(&mut self) {
        SHADOW_BYPASS_DEPTH.fetch_sub(1, Ordering::Release);
    }
}

/// Run the full-parser arm with all nested S2 wrappers bypassed.
pub fn with_shadow_bypass<R>(f: impl FnOnce() -> R) -> R {
    SHADOW_BYPASS_DEPTH.fetch_add(1, Ordering::AcqRel);
    let _guard = ShadowBypassGuard;
    f()
}

#[inline]
pub fn record_attempt(site: Site) {
    ATTEMPTS[site as usize].fetch_add(1, Ordering::Relaxed);
}

#[inline]
pub fn record_hit(site: Site) {
    HITS[site as usize].fetch_add(1, Ordering::Relaxed);
}

#[inline]
pub fn record_shadow_ok(site: Site) {
    SHADOW_OK[site as usize].fetch_add(1, Ordering::Relaxed);
}

#[inline]
pub fn record_shadow_mismatch(site: Site) {
    SHADOW_MISMATCH[site as usize].fetch_add(1, Ordering::Relaxed);
}

#[inline]
pub fn reset_counts() {
    SHADOW_FAILURES.lock().clear();
    for counter in
        ATTEMPTS.iter().chain(HITS.iter()).chain(SHADOW_OK.iter()).chain(SHADOW_MISMATCH.iter())
    {
        counter.store(0, Ordering::Relaxed);
    }
}

#[inline]
fn site_counts(site: Site) -> SiteCounts {
    let i = site as usize;
    SiteCounts {
        attempts: ATTEMPTS[i].load(Ordering::Relaxed),
        hits: HITS[i].load(Ordering::Relaxed),
        shadow_ok: SHADOW_OK[i].load(Ordering::Relaxed),
        shadow_mismatches: SHADOW_MISMATCH[i].load(Ordering::Relaxed),
    }
}

#[inline]
pub fn snapshot_counts() -> Counts {
    Counts {
        path: site_counts(Site::Path),
        ty: site_counts(Site::Ty),
        pat: site_counts(Site::Pat),
        fn_params: site_counts(Site::FnParams),
        expr_atom: site_counts(Site::ExprAtom),
        let_stmt: site_counts(Site::LetStmt),
    }
}

/// Compatibility aliases for early staged wrappers.
#[inline]
pub fn attempt(site: Site) {
    record_attempt(site);
}
#[inline]
pub fn hit(site: Site) {
    record_hit(site);
}
#[inline]
pub fn shadow_match(site: Site) {
    record_shadow_ok(site);
}
#[inline]
pub fn counts() -> Counts {
    snapshot_counts()
}

/// Restore the old mode even if the operation unwinds.
pub struct ModeGuard(Mode);

impl ModeGuard {
    pub fn set(mode: Mode) -> Self {
        Self(set_mode(mode))
    }
}

impl Drop for ModeGuard {
    fn drop(&mut self) {
        set_mode(self.0);
    }
}

/// Semantic continuation at a grammar boundary. Allocation scratch is deliberately excluded.
pub(crate) fn continuation(parser: &super::Parser<'_>) -> alloc::string::String {
    let expectations: alloc::vec::Vec<_> = parser.expected_token_types.iter().collect();
    alloc::format!(
        "token={:?} prev={:?} spacing={:?} cursor={:?}/{:?}/{} bumps={} split={} expectations={:?}/{} restrictions={:?} angles={}/{} generics={} unexpected={:?} closure={:?} recovery={:?} body={}/{} subparser={:?} capture_cfg={} capture={:?}",
        parser.token,
        parser.prev_token,
        parser.token_spacing,
        parser.token_cursor.next_index(),
        parser.token_cursor.eof_index(),
        parser.token_cursor.depth(),
        parser.num_bump_calls,
        parser.break_last_token,
        expectations,
        parser.expected_token_types.is_deferred(),
        parser.restrictions,
        parser.unmatched_angle_bracket_count,
        parser.angle_bracket_nesting,
        parser.parsing_generics,
        parser.last_unexpected_token_span,
        parser.current_closure,
        parser.recovery,
        parser.in_fn_body,
        parser.fn_body_missing_semi_guar.is_some(),
        parser.subparser_name,
        parser.capture_cfg,
        parser.capture_state,
    )
}

static SHADOW_FAILURES: parking_lot::Mutex<alloc::vec::Vec<alloc::string::String>> =
    parking_lot::Mutex::new(alloc::vec::Vec::new());

pub fn shadow_failures() -> alloc::vec::Vec<alloc::string::String> {
    SHADOW_FAILURES.lock().clone()
}

pub(super) fn same_continuation(
    site: Site,
    fast: &super::Parser<'_>,
    full: &super::Parser<'_>,
) -> bool {
    let fast = continuation(fast);
    let full = continuation(full);
    if fast == full {
        return true;
    }
    let detail = alloc::format!("{site:?}: fast={fast}; full={full}");
    let mut failures = SHADOW_FAILURES.lock();
    if failures.len() < 24 && !failures.contains(&detail) {
        failures.push(detail);
    }
    false
}
