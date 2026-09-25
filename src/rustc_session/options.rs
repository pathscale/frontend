// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use alloc::collections::BTreeMap;
use core::num::NonZero;
use eko::path::PathBuf;
use core::str;

use crate::rustc_abi::Align;
use crate::rustc_ast::attr::version::RustcVersion;
use crate::rustc_data_structures::fx::FxIndexMap;
use crate::rustc_data_structures::profiling::TimePassesFormat;
use crate::rustc_data_structures::stable_hash::StableHasher;
use crate::rustc_errors::{ColorConfig, TerminalUrl};
use crate::rustc_feature::UnstableFeatures;
use crate::rustc_hashes::Hash64;
use rustc_macros::{BlobDecodable, Encodable};
use crate::rustc_span::edition::Edition;
use crate::rustc_span::{RealFileName, RemapPathScopeComponents, SourceFileHashAlgorithm};
use crate::rustc_structures::{CollapseMacroDebuginfo, CrateType};
use crate::rustc_target::spec::{
    CodeModel, FramePointer, LinkerFlavorCli, MergeFunctions, OnBrokenPipe, PanicStrategy,
    RelocModel, RelroLevel, SanitizerSet, SplitDebuginfo, StackProtector, SymbolVisibility,
    TargetTuple, TlsModel,
};

use crate::rustc_session::config::*;
use crate::rustc_session::search_paths::SearchPath;
use crate::rustc_session::utils::NativeLib;
use crate::rustc_session::{Session, lint};

macro_rules! insert {
    ($opt_name:ident, $opt_expr:expr, $sub_hashes:expr) => {
        if $sub_hashes
            .insert(stringify!($opt_name), $opt_expr as &dyn dep_tracking::DepTrackingHash)
            .is_some()
        {
            panic!("duplicate key in CLI DepTrackingHash: {}", stringify!($opt_name))
        }
    };
}

macro_rules! hash_opt {
    ($opt_name:ident, $opt_expr:expr, $sub_hashes:expr, $_for_crate_hash: ident, [UNTRACKED]) => {{}};
    ($opt_name:ident, $opt_expr:expr, $sub_hashes:expr, $_for_crate_hash: ident, [TRACKED]) => {{ insert!($opt_name, $opt_expr, $sub_hashes) }};
    ($opt_name:ident, $opt_expr:expr, $sub_hashes:expr, $for_crate_hash: ident, [TRACKED_NO_CRATE_HASH]) => {{
        if !$for_crate_hash {
            insert!($opt_name, $opt_expr, $sub_hashes)
        }
    }};
    ($opt_name:ident, $opt_expr:expr, $sub_hashes:expr, $_for_crate_hash: ident, [SUBSTRUCT]) => {{}};
}

macro_rules! hash_substruct {
    ($opt_name:ident, $opt_expr:expr, $error_format:expr, $for_crate_hash:expr, $hasher:expr, [UNTRACKED]) => {{}};
    ($opt_name:ident, $opt_expr:expr, $error_format:expr, $for_crate_hash:expr, $hasher:expr, [TRACKED]) => {{}};
    ($opt_name:ident, $opt_expr:expr, $error_format:expr, $for_crate_hash:expr, $hasher:expr, [TRACKED_NO_CRATE_HASH]) => {{}};
    ($opt_name:ident, $opt_expr:expr, $error_format:expr, $for_crate_hash:expr, $hasher:expr, [SUBSTRUCT]) => {{
        use crate::rustc_session::config::dep_tracking::DepTrackingHash;
        $opt_expr.dep_tracking_hash($for_crate_hash, $error_format).hash(
            $hasher,
            $error_format,
            $for_crate_hash,
        );
    }};
}

/// Extended target modifier info.
/// For example, when external target modifier is '-Zregparm=2':
/// Target modifier enum value + user value ('2') from external crate
/// is converted into description: prefix ('Z'), name ('regparm'), tech value ('Some(2)').
pub struct ExtendedTargetModifierInfo {
    /// Flag prefix (usually, 'C' for codegen flags or 'Z' for unstable flags)
    pub prefix: String,
    /// Flag name
    pub name: String,
    /// Flag parsed technical value
    pub tech_value: String,
}

/// A recorded -Zopt_name=opt_value (or -Copt_name=opt_value)
/// which alter the ABI or effectiveness of exploit mitigations.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Encodable, BlobDecodable)]
pub struct TargetModifier {
    /// Option enum value
    pub opt: OptionsTargetModifiers,
    /// User-provided option value (before parsing)
    pub value_name: String,
}

pub mod mitigation_coverage;

mod target_modifier_consistency_check {
    use super::*;
    pub(super) fn sanitizer(
        sess: &Session,
        l: &TargetModifier,
        r: Option<&TargetModifier>,
    ) -> bool {
        let mut lparsed: SanitizerSet = sess.target.options.default_sanitizers;
        let lval = if l.value_name.is_empty() { None } else { Some(l.value_name.as_str()) };
        parse::parse_sanitizers(&mut lparsed, lval);

        let mut rparsed: SanitizerSet = sess.target.options.default_sanitizers;
        let rval = r.filter(|v| !v.value_name.is_empty()).map(|v| v.value_name.as_str());
        parse::parse_sanitizers(&mut rparsed, rval);

        // Some sanitizers need to be target modifiers, and some do not.
        // For now, we should mark all sanitizers as target modifiers except for these:
        // AddressSanitizer, LeakSanitizer
        let tmod_sanitizers = SanitizerSet::MEMORY
            | SanitizerSet::THREAD
            | SanitizerSet::HWADDRESS
            | SanitizerSet::CFI
            | SanitizerSet::MEMTAG
            | SanitizerSet::SHADOWCALLSTACK
            | SanitizerSet::KCFI
            | SanitizerSet::KERNELADDRESS
            | SanitizerSet::KERNELHWADDRESS
            | SanitizerSet::SAFESTACK
            | SanitizerSet::DATAFLOW;

        lparsed & tmod_sanitizers == rparsed & tmod_sanitizers
    }
    pub(super) fn sanitizer_cfi_normalize_integers(
        sess: &Session,
        l: &TargetModifier,
        r: Option<&TargetModifier>,
    ) -> bool {
        // For kCFI, the helper flag -Zsanitizer-cfi-normalize-integers should also be a target modifier
        if sess.sanitizers().contains(SanitizerSet::KCFI) {
            if let Some(r) = r {
                return l.extend().tech_value == r.extend().tech_value;
            } else {
                return false;
            }
        }
        true
    }
    pub(super) fn target_cpu(
        sess: &Session,
        l: &TargetModifier,
        r: Option<&TargetModifier>,
    ) -> bool {
        if !sess.target.requires_consistent_cpu {
            return true;
        }
        let l_tech_value = l.extend().tech_value;
        let r_tech_value = match r {
            Some(r) => r.extend().tech_value,
            // If only one of the two compared crates specifies the CPU
            // explicitly we compare against the target's default CPU.
            None => {
                // We reuse the same parsing logic.
                CodegenOptionsTargetModifiers::TargetCpu
                    .reparse(sess.target.cpu.as_ref())
                    .tech_value
            }
        };
        l_tech_value == r_tech_value
    }
}

impl TargetModifier {
    pub fn extend(&self) -> ExtendedTargetModifierInfo {
        self.opt.reparse(&self.value_name)
    }
    // Custom consistency check for target modifiers (or default `l.tech_value == r.tech_value`)
    // When other is None, consistency with default value is checked
    pub fn consistent(&self, sess: &Session, other: Option<&TargetModifier>) -> bool {
        assert!(other.is_none() || self.opt == other.unwrap().opt);
        match self.opt {
            OptionsTargetModifiers::UnstableOptions(unstable) => match unstable {
                UnstableOptionsTargetModifiers::Sanitizer => {
                    return target_modifier_consistency_check::sanitizer(sess, self, other);
                }
                UnstableOptionsTargetModifiers::SanitizerCfiNormalizeIntegers => {
                    return target_modifier_consistency_check::sanitizer_cfi_normalize_integers(
                        sess, self, other,
                    );
                }
                _ => {}
            },
            OptionsTargetModifiers::CodegenOptions(codegen) => match codegen {
                CodegenOptionsTargetModifiers::TargetCpu => {
                    return target_modifier_consistency_check::target_cpu(sess, self, other);
                }
            },
        };
        match other {
            Some(other) => self.extend().tech_value == other.extend().tech_value,
            None => false,
        }
    }
}

fn tmod_push_impl(
    opt: OptionsTargetModifiers,
    tmod_vals: &BTreeMap<OptionsTargetModifiers, String>,
    tmods: &mut Vec<TargetModifier>,
) {
    if let Some(v) = tmod_vals.get(&opt) {
        tmods.push(TargetModifier { opt, value_name: v.clone() })
    }
}

macro_rules! top_level_options {
    (
        $(#[$top_level_attr:meta])*
        pub struct Options {
            $(
                $(#[$attr:meta])*
                $opt:ident : $t:ty
                [$dep_tracking_marker:ident]
                $( { TARGET_MODIFIER: $tmod_variant:ident($tmod_enum:ident) } )?
                ,
            )*
        }
    ) => {
        #[derive(PartialEq, Eq, PartialOrd, Ord, Debug, Copy, Clone, Encodable, BlobDecodable)]
        pub enum OptionsTargetModifiers {
            $(
                $(
                    $tmod_variant($tmod_enum),
                )?
            )*
        }

        impl OptionsTargetModifiers {
            pub fn reparse(&self, user_value: &str) -> ExtendedTargetModifierInfo {
                match self {
                    $(
                        $(
                            Self::$tmod_variant(v) => v.reparse(user_value),
                        )?
                    )*
                    #[allow(unreachable_patterns)]
                    _ => panic!("unknown target modifier option: {self:?}"),
                }
            }

            pub fn is_target_modifier(flag_name: &str) -> bool {
                $(
                    $(
                        if $tmod_enum::is_target_modifier(flag_name) {
                            return true
                        }
                    )?
                )*
                false
            }
        }

        #[derive(Clone)]
        $(#[$top_level_attr])*
        pub struct Options {
            $(
                $(#[$attr])*
                pub $opt: $t,
            )*
            pub target_modifiers: BTreeMap<OptionsTargetModifiers, String>,
            pub mitigation_coverage_map: mitigation_coverage::MitigationCoverageMap,
        }

        impl Options {
            pub fn dep_tracking_hash(&self, for_crate_hash: bool) -> Hash64 {
                let mut sub_hashes = BTreeMap::new();
                $(
                    hash_opt!(
                        $opt,
                        &self.$opt,
                        &mut sub_hashes,
                        for_crate_hash,
                        [$dep_tracking_marker]
                    );
                )*
                let mut hasher = StableHasher::new();
                dep_tracking::stable_hash(
                    sub_hashes,
                    &mut hasher,
                    self.error_format,
                    for_crate_hash,
                );
                $(
                    hash_substruct!(
                        $opt,
                        &self.$opt,
                        self.error_format,
                        for_crate_hash,
                        &mut hasher,
                        [$dep_tracking_marker]
                    );
                )*
                hasher.finish()
            }

            pub fn gather_target_modifiers(&self) -> Vec<TargetModifier> {
                let mut mods = Vec::<TargetModifier>::new();
                $(
                    $(
                        // Only expand for flags that have `TARGET_MODIFIER`. The
                        // `metavar_ignore!` call drives this `$(...)?` on `$tmod_enum`.
                        $crate::metavar_ignore!([$tmod_enum]);
                        self.$opt.gather_target_modifiers(&mut mods, &self.target_modifiers);
                    )?
                )*
                mods.sort_by(|a, b| a.opt.cmp(&b.opt));
                mods
            }
        }
    }
}

top_level_options!(
    /// The top-level command-line options struct.
    ///
    /// For each option, one has to specify how it behaves with regard to the
    /// dependency tracking system of incremental compilation. This is done via the
    /// square-bracketed directive after the field type. The options are:
    ///
    /// - `[TRACKED]`
    /// A change in the given field will cause the compiler to completely clear the
    /// incremental compilation cache before proceeding.
    ///
    /// - `[TRACKED_NO_CRATE_HASH]`
    /// Same as `[TRACKED]`, but will not affect the crate hash. This is useful for options that
    /// only affect the incremental cache.
    ///
    /// - `[UNTRACKED]`
    /// Incremental compilation is not influenced by this option.
    ///
    /// - `[SUBSTRUCT]`
    /// Second-level sub-structs containing more options.
    ///
    /// If you add a new option to this struct or one of the sub-structs like
    /// `CodegenOptions`, think about how it influences incremental compilation. If in
    /// doubt, specify `[TRACKED]`, which is always "correct" but might lead to
    /// unnecessary re-compilation.
    pub struct Options {
        /// The crate config requested for the session, which may be combined
        /// with additional crate configurations during the compile process.
        crate_types: Vec<CrateType> [TRACKED],
        optimize: OptLevel [TRACKED],
        /// Include the `debug_assertions` flag in dependency tracking, since it
        /// can influence whether overflow checks are done or not.
        debug_assertions: bool [TRACKED],
        debuginfo: DebugInfo [TRACKED],
        lint_opts: Vec<(String, lint::Level)> [TRACKED_NO_CRATE_HASH],
        lint_cap: Option<lint::Level> [TRACKED_NO_CRATE_HASH],
        describe_lints: bool [UNTRACKED],
        output_types: OutputTypes [TRACKED],
        search_paths: Vec<SearchPath> [UNTRACKED],
        libs: Vec<NativeLib> [TRACKED],
        sysroot: Sysroot [UNTRACKED],

        target_triple: TargetTuple [TRACKED],

        /// Effective logical environment used by `env!`/`option_env!` macros
        logical_env: FxIndexMap<String, String> [TRACKED],

        test: bool [TRACKED],
        error_format: ErrorOutputType [UNTRACKED],
        diagnostic_width: Option<usize> [UNTRACKED],

        /// If `Some`, enable incremental compilation, using the given
        /// directory to store intermediate results.
        incremental: Option<PathBuf> [UNTRACKED],

        unstable_opts: UnstableOptions [SUBSTRUCT] { TARGET_MODIFIER: UnstableOptions(UnstableOptionsTargetModifiers) },
        cg: CodegenOptions [SUBSTRUCT] { TARGET_MODIFIER: CodegenOptions(CodegenOptionsTargetModifiers) },
        externs: Externs [UNTRACKED],
        /// Per loaded proc-macro crate that was read from source, the host dylib a compiler built
        /// from that source: `(metadata file the crate is loaded from, dylib)`. Such a crate's
        /// macros run through that dylib; a proc-macro crate with none declares its macros and
        /// refuses every expansion. frontend's own, with no rustc flag behind it.
        proc_macro_dylibs: Vec<(PathBuf, PathBuf)> [UNTRACKED],
        crate_name: Option<String> [TRACKED],
        /// Indicates how the compiler should treat unstable features.
        unstable_features: UnstableFeatures [TRACKED],

        /// Indicates whether this run of the compiler is actually rustdoc. This
        /// is currently just a hack and will be removed eventually, so please
        /// try to not rely on this too much.
        actually_rustdoc: bool [TRACKED],
        /// Whether name resolver should resolve documentation links.
        resolve_doc_links: ResolveDocLinks [TRACKED],

        /// Control path trimming.
        trimmed_def_paths: bool [TRACKED],

        /// Specifications of codegen units / ThinLTO which are forced as a
        /// result of parsing command line options. These are not necessarily
        /// what rustc was invoked with, but massaged a bit to agree with
        /// commands like `--emit llvm-ir` which they're often incompatible with
        /// if we otherwise use the defaults of rustc.
        cli_forced_codegen_units: Option<usize> [UNTRACKED],
        cli_forced_local_thinlto_off: bool [UNTRACKED],

        /// Remap source path prefixes in all output (messages, object files, debug, etc.).
        remap_path_prefix: Vec<(PathBuf, PathBuf)> [TRACKED_NO_CRATE_HASH],
        /// Defines which scopes of paths should be remapped by `--remap-path-prefix`.
        remap_path_scope: RemapPathScopeComponents [TRACKED_NO_CRATE_HASH],

        /// Base directory containing the `library/` directory for the Rust standard library.
        /// Right now it's always `$sysroot/lib/rustlib/src/rust`
        /// (i.e. the `rustup` `rust-src` component).
        ///
        /// This directory is what the virtual `/rustc/$hash` is translated back to,
        /// if Rust was built with path remapping to `/rustc/$hash` enabled
        /// (the `rust.remap-debuginfo` option in `bootstrap.toml`).
        real_rust_source_base_dir: Option<PathBuf> [TRACKED_NO_CRATE_HASH],

        /// Base directory containing the `compiler/` directory for the rustc sources.
        /// Right now it's always `$sysroot/lib/rustlib/rustc-src/rust`
        /// (i.e. the `rustup` `rustc-dev` component).
        ///
        /// This directory is what the virtual `/rustc-dev/$hash` is translated back to,
        /// if Rust was built with path remapping to `/rustc/$hash` enabled
        /// (the `rust.remap-debuginfo` option in `bootstrap.toml`).
        real_rustc_dev_source_base_dir: Option<PathBuf> [TRACKED_NO_CRATE_HASH],

        edition: Edition [TRACKED],

        /// `true` if we're emitting JSON blobs about each artifact produced
        /// by the compiler.
        json_artifact_notifications: bool [TRACKED],

        /// `true` if we're emitting JSON timings with the start and end of
        /// high-level compilation sections
        json_timings: bool [UNTRACKED],

        /// `true` if we're emitting a JSON blob containing the unused externs
        json_unused_externs: JsonUnusedExterns [UNTRACKED],

        /// `true` if we're emitting a JSON job containing a future-incompat report for lints
        json_future_incompat: bool [TRACKED],

        pretty: Option<PpMode> [UNTRACKED],

        /// The (potentially remapped) working directory
        working_dir: RealFileName [TRACKED],
        color: ColorConfig [UNTRACKED],
        verbose: bool [TRACKED_NO_CRATE_HASH],
        jobs: Jobs [UNTRACKED],
    }
);

/// Defines the `CodegenOptions` and `UnstableOptions` data models, defaults, stable hashing, and
/// target-modifier metadata used across sessions.
macro_rules! options {
    (
        $struct_name:ident,
        $tmod_enum:ident,
        $stat:ident,
        $optmod:ident,
        $prefix:expr,
        $outputname:expr,

        $(
            $(#[$attr:meta])*
            $opt:ident : $t:ty = (
                $init:expr,
                $parse:ident,
                [$dep_tracking_marker:ident]
                $( { TARGET_MODIFIER: $tmod_variant:ident } )?
                $( { MITIGATION: $mitigation_variant:ident } )?
                ,
                $desc:expr
                $(, removed: $removed:ident )?
            ),
        )*
    ) => {
        #[derive(Clone)]
        pub struct $struct_name {
            $(
                $(#[$attr])*
                pub $opt: $t,
            )*
        }

        #[derive(PartialEq, Eq, PartialOrd, Ord, Debug, Copy, Clone, Encodable, BlobDecodable)]
        pub enum $tmod_enum {
            $(
                $( $tmod_variant, )?
            )*
        }

        impl $tmod_enum {
            pub fn reparse(&self, _user_value: &str) -> ExtendedTargetModifierInfo {
                match self {
                    $(
                        $(
                            Self::$tmod_variant => {
                                let mut parsed: $t = Default::default();
                                let val = if _user_value.is_empty() { None } else { Some(_user_value) };
                                parse::$parse(&mut parsed, val);
                                ExtendedTargetModifierInfo {
                                    prefix: $prefix.to_string(),
                                    name: stringify!($opt).to_string().replace('_', "-"),
                                    tech_value: format!("{:?}", parsed),
                                }
                            }
                        )?
                    )*

                    #[allow(unreachable_patterns)]
                    _ => panic!("unknown target modifier option: {:?}", *self)
                }
            }

            pub fn is_target_modifier(flag_name: &str) -> bool {
                match flag_name.replace('-', "_").as_str() {
                    $(
                        $(
                            // Only expand for flags that have `TARGET_MODIFIER`. The pattern
                            // goes through `metavar_ignore!` so this `$(...)?` is driven by
                            // `$tmod_variant` without emitting it.
                            $crate::metavar_ignore!([$tmod_variant] stringify!($opt)) => true,
                        )?
                    )*
                    _ => false,
                }
            }
        }

        impl Default for $struct_name {
            fn default() -> $struct_name {
                $struct_name {
                    $(
                        $opt: $init,
                    )*
                }
            }
        }

        impl $struct_name {
            fn dep_tracking_hash(
                &self,
                for_crate_hash: bool,
                error_format: ErrorOutputType,
            ) -> Hash64 {
                let mut sub_hashes = BTreeMap::new();
                $(
                    hash_opt!(
                        $opt,
                        &self.$opt,
                        &mut sub_hashes,
                        for_crate_hash,
                        [$dep_tracking_marker]
                    );
                )*
                let mut hasher = StableHasher::new();
                dep_tracking::stable_hash(
                    sub_hashes,
                    &mut hasher,
                    error_format,
                    for_crate_hash,
                );
                hasher.finish()
            }

            pub fn gather_target_modifiers(
                &self,
                _mods: &mut Vec<TargetModifier>,
                _tmod_vals: &BTreeMap<OptionsTargetModifiers, String>,
            ) {
                $(
                    $(
                        if self.$opt != $init {
                            tmod_push_impl(
                                OptionsTargetModifiers::$struct_name($tmod_enum::$tmod_variant),
                                _tmod_vals,
                                _mods,
                            );
                        }
                    )?
                )*
            }
        }
    }
}

impl CodegenOptions {
    // JUSTIFICATION: defn of the suggested wrapper fn
    pub fn instrument_coverage(&self) -> InstrumentCoverage {
        self.instrument_coverage
    }
}

pub mod parse {
    use alloc::string::String;
    use alloc::vec::Vec;
    use alloc::string::ToString;
    use core::str::FromStr;

    pub(crate) use super::*;

    pub(crate) fn parse_bool(slot: &mut bool, v: Option<&str>) -> bool {
        match v {
            Some("y") | Some("yes") | Some("on") | Some("true") | None => {
                *slot = true;
                true
            }
            Some("n") | Some("no") | Some("off") | Some("false") => {
                *slot = false;
                true
            }
            _ => false,
        }
    }

    pub(crate) fn parse_opt_bool(slot: &mut Option<bool>, v: Option<&str>) -> bool {
        let mut parsed = false;
        if parse_bool(&mut parsed, v) {
            *slot = Some(parsed);
            true
        } else {
            false
        }
    }

    pub(crate) fn parse_opt_string(slot: &mut Option<String>, v: Option<&str>) -> bool {
        match v {
            Some(value) => {
                *slot = Some(value.to_string());
                true
            }
            None => false,
        }
    }

    pub(crate) fn parse_opt_number<T: Copy + FromStr>(
        slot: &mut Option<T>,
        v: Option<&str>,
    ) -> bool {
        match v {
            Some(value) => {
                *slot = value.parse().ok();
                slot.is_some()
            }
            None => false,
        }
    }

    pub(crate) fn parse_sanitizers(slot: &mut SanitizerSet, v: Option<&str>) -> bool {
        let Some(value) = v else {
            return false;
        };
        for sanitizer in value.split(',') {
            *slot |= match sanitizer {
                "address" => SanitizerSet::ADDRESS,
                "cfi" => SanitizerSet::CFI,
                "dataflow" => SanitizerSet::DATAFLOW,
                "kcfi" => SanitizerSet::KCFI,
                "kernel-address" => SanitizerSet::KERNELADDRESS,
                "kernel-hwaddress" => SanitizerSet::KERNELHWADDRESS,
                "leak" => SanitizerSet::LEAK,
                "memory" => SanitizerSet::MEMORY,
                "memtag" => SanitizerSet::MEMTAG,
                "shadow-call-stack" => SanitizerSet::SHADOWCALLSTACK,
                "thread" => SanitizerSet::THREAD,
                "hwaddress" => SanitizerSet::HWADDRESS,
                "safestack" => SanitizerSet::SAFESTACK,
                "realtime" => SanitizerSet::REALTIME,
                _ => return false,
            };
        }
        true
    }

    pub(crate) fn parse_target_feature(slot: &mut String, v: Option<&str>) -> bool {
        match v {
            Some(value) => {
                if !slot.is_empty() {
                    slot.push(',');
                }
                slot.push_str(value);
                true
            }
            None => false,
        }
    }

    pub(crate) fn parse_pointer_authentication_list_with_polarity(
        slot: &mut Vec<(PointerAuthOption, bool)>,
        v: Option<&str>,
    ) -> bool {
        let Some(value) = v else {
            return false;
        };
        let mut options = BTreeMap::new();
        for item in value.split(',') {
            let Some(name) = item.strip_prefix(&['+', '-'][..]) else {
                return false;
            };
            let Some(option) = PointerAuthOption::parse(name) else {
                return false;
            };
            options.insert(option, item.starts_with('+'));
        }
        slot.clear();
        slot.extend(options);
        true
    }

    pub(crate) fn parse_branch_protection(
        slot: &mut Option<BranchProtection>,
        v: Option<&str>,
    ) -> bool {
        let Some(value) = v else {
            return false;
        };
        let parsed = slot.get_or_insert_default();
        for option in value.split(',') {
            match option {
                "bti" => parsed.bti = true,
                "pac-ret" if parsed.pac_ret.is_none() => {
                    parsed.pac_ret = Some(PacRet {
                        leaf: false,
                        pc: false,
                        key: PAuthKey::A,
                    });
                }
                "leaf" => match parsed.pac_ret.as_mut() {
                    Some(pac) => pac.leaf = true,
                    None => return false,
                },
                "b-key" => match parsed.pac_ret.as_mut() {
                    Some(pac) => pac.key = PAuthKey::B,
                    None => return false,
                },
                "pc" => match parsed.pac_ret.as_mut() {
                    Some(pac) => pac.pc = true,
                    None => return false,
                },
                "gcs" => parsed.gcs = true,
                _ => return false,
            }
        }
        true
    }
}

options! {
    CodegenOptions, CodegenOptionsTargetModifiers, CG_OPTIONS, cgopts, "C", "codegen",

    // If you add a new option, please update:
    // - compiler/rustc_interface/src/tests.rs
    // - src/doc/rustc/src/codegen-options/index.md

    // tidy-alphabetical-start
    ar: () = ((), parse_ignore, [UNTRACKED],
        "this option has been removed",
        removed: Err),
    code_model: Option<CodeModel> = (None, parse_code_model, [TRACKED],
        "choose the code model to use (`rustc --print code-models` for details)"),
    codegen_units: Option<usize> = (None, parse_opt_number, [UNTRACKED],
        "divide crate into N units to optimize in parallel"),
    collapse_macro_debuginfo: CollapseMacroDebuginfo = (CollapseMacroDebuginfo::Unspecified,
        parse_collapse_macro_debuginfo, [TRACKED],
        "set option to collapse debuginfo for macros"),
    control_flow_guard: CFGuard = (CFGuard::Disabled, parse_cfguard, [TRACKED] { MITIGATION: ControlFlowGuard },
        "use Windows Control Flow Guard (default: no)"),
    debug_assertions: Option<bool> = (None, parse_opt_bool, [TRACKED],
        "explicitly enable the `cfg(debug_assertions)` directive"),
    debuginfo: DebugInfo = (DebugInfo::None, parse_debuginfo, [TRACKED],
        "debug info emission level (0-2, none, line-directives-only, \
        line-tables-only, limited, or full; default: 0)"),
    default_linker_libraries: bool = (false, parse_bool, [UNTRACKED],
        "allow the linker to link its default libraries (default: no)"),
    dlltool: Option<PathBuf> = (None, parse_opt_pathbuf, [UNTRACKED],
        "import library generation tool (ignored except when targeting windows-gnu)"),
    dwarf_version: Option<u32> = (None, parse_opt_number, [TRACKED],
        "version of DWARF debug information to emit (default: 2 or 4, depending on platform)"),
    embed_bitcode: bool = (true, parse_bool, [TRACKED],
        "emit bitcode in rlibs (default: yes)"),
    extra_filename: String = (String::new(), parse_string, [UNTRACKED],
        "extra data to put in each output filename"),
    force_frame_pointers: FramePointer = (FramePointer::MayOmit, parse_frame_pointer, [TRACKED],
        "force use of the frame pointers"),
    force_unwind_tables: Option<bool> = (None, parse_opt_bool, [TRACKED],
        "force use of unwind tables"),
    help: bool = (false, parse_no_value, [UNTRACKED], "Print codegen options"),
    incremental: Option<String> = (None, parse_opt_string, [UNTRACKED],
        "enable incremental compilation"),
    inline_threshold: () = ((), parse_ignore, [UNTRACKED],
        "this option has been removed \
        (consider using `-Cllvm-args=--inline-threshold=...`)",
        removed: Err),
    instrument_coverage: InstrumentCoverage = (InstrumentCoverage::No, parse_instrument_coverage, [TRACKED],
        "instrument the generated code to support LLVM source-based code coverage reports \
        (note, the compiler build config must include `profiler = true`); \
        implies `-C symbol-mangling-version=v0`"),
    jump_tables: bool = (true, parse_bool, [TRACKED],
        "allow jump table and lookup table generation from switch case lowering (default: yes)"),
    link_arg: (/* redirected to link_args */) = ((), parse_string_push, [UNTRACKED],
        "a single extra argument to append to the linker invocation (can be used several times)"),
    link_args: Vec<String> = (Vec::new(), parse_list, [UNTRACKED],
        "extra arguments to append to the linker invocation (space separated)"),
    link_dead_code: Option<bool> = (None, parse_opt_bool, [TRACKED],
        "try to generate and link dead code (default: no)"),
    link_self_contained: LinkSelfContained = (LinkSelfContained::default(), parse_link_self_contained, [UNTRACKED],
        "control whether to link Rust provided C objects/libraries or rely \
        on a C toolchain or linker installed in the system"),
    linker: Option<PathBuf> = (None, parse_opt_pathbuf, [UNTRACKED],
        "system linker to link outputs with"),
    linker_features: LinkerFeaturesCli = (LinkerFeaturesCli::default(), parse_linker_features, [UNTRACKED],
        "a comma-separated list of linker features to enable (+) or disable (-): `lld`"),
    linker_flavor: Option<LinkerFlavorCli> = (None, parse_linker_flavor, [UNTRACKED],
        "linker flavor"),
    linker_plugin_lto: LinkerPluginLto = (LinkerPluginLto::Disabled,
        parse_linker_plugin_lto, [TRACKED],
        "generate build artifacts that are compatible with linker-based LTO"),
    llvm_args: Vec<String> = (Vec::new(), parse_list, [TRACKED],
        "a list of arguments to pass to LLVM (space separated)"),
    lto: LtoCli = (LtoCli::Unspecified, parse_lto, [TRACKED],
        "perform LLVM link-time optimizations"),
    metadata: Vec<String> = (Vec::new(), parse_list, [TRACKED],
        "metadata to mangle symbol names with"),
    no_prepopulate_passes: bool = (false, parse_no_value, [TRACKED],
        "give an empty list of passes to the pass manager"),
    no_redzone: Option<bool> = (None, parse_opt_bool, [TRACKED],
        "disable the use of the redzone"),
    no_stack_check: () = ((), parse_ignore, [UNTRACKED],
        "this option has been removed",
        removed: Err),
    no_vectorize_loops: bool = (false, parse_no_value, [TRACKED],
        "disable loop vectorization optimization passes"),
    no_vectorize_slp: bool = (false, parse_no_value, [TRACKED],
        "disable LLVM's SLP vectorization pass"),
    opt_level: String = ("0".to_string(), parse_string, [TRACKED],
        "optimization level (0-3, s, or z; default: 0)"),
    overflow_checks: Option<bool> = (None, parse_opt_bool, [TRACKED],
        "use overflow checks for integer arithmetic"),
    panic: Option<PanicStrategy> = (None, parse_opt_panic_strategy, [TRACKED],
        "panic strategy to compile crate with"),
    passes: Vec<String> = (Vec::new(), parse_list, [TRACKED],
        "a list of extra LLVM passes to run (space separated)"),
    prefer_dynamic: bool = (false, parse_bool, [TRACKED],
        "prefer dynamic linking to static linking (default: no)"),
    profile_generate: SwitchWithOptPath = (SwitchWithOptPath::Disabled,
        parse_switch_with_opt_path, [TRACKED],
        "compile the program with profiling instrumentation"),
    profile_sample_use: Option<PathBuf> = (None, parse_opt_pathbuf, [TRACKED],
        "use the given `.prof` file for sample-based profile-guided optimization"),
    profile_use: Option<PathBuf> = (None, parse_opt_pathbuf, [TRACKED],
        "use the given `.profdata` file for profile-guided optimization"),
    relocation_model: Option<RelocModel> = (None, parse_relocation_model, [TRACKED],
        "control generation of position-independent code (PIC) \
        (`rustc --print relocation-models` for details)"),
    relro_level: Option<RelroLevel> = (None, parse_relro_level, [TRACKED],
        "choose which RELRO level to use"),
    remark: Passes = (Passes::Some(Vec::new()), parse_passes, [UNTRACKED],
        "output remarks for these optimization passes (space separated, or \"all\")"),
    rpath: bool = (false, parse_bool, [UNTRACKED],
        "set rpath values in libs/exes (default: no)"),
    save_temps: bool = (false, parse_bool, [UNTRACKED],
        "save all temporary output files during compilation (default: no)"),
    soft_float: () = ((), parse_ignore, [UNTRACKED],
        "this option has been removed \
        (use a corresponding *eabi target instead)",
        removed: Err),
    split_debuginfo: Option<SplitDebuginfo> = (None, parse_split_debuginfo, [TRACKED],
        "how to handle split-debuginfo, a platform-specific option"),
    strip: Strip = (Strip::None, parse_strip, [UNTRACKED],
        "tell the linker which information to strip (`none` (default), `debuginfo` or `symbols`)"),
    symbol_mangling_version: Option<SymbolManglingVersion> = (None,
        parse_symbol_mangling_version, [TRACKED],
        "which mangling version to use for symbol names ('legacy', 'v0' (default), or 'hashed')"),
    target_cpu: Option<String> = (None, parse_opt_string, [TRACKED] { TARGET_MODIFIER: TargetCpu },
        "select target processor (`rustc --print target-cpus` for details) \
        The resulting binary must only be executed on CPUs that have all the features \
        of the given CPU."),
    target_feature: String = (String::new(), parse_target_feature, [TRACKED],
        "target-specific attributes (`rustc --print target-features` for details). \
        The resulting binary must only be executed on CPUs that have all the given features."),
    unsafe_allow_abi_mismatch: Vec<String> = (Vec::new(), parse_comma_list, [UNTRACKED],
        "Allow incompatible target modifiers in dependency crates (comma separated list)"),
    // tidy-alphabetical-end

    // If you add a new option, please update:
    // - compiler/rustc_interface/src/tests.rs
    // - src/doc/rustc/src/codegen-options/index.md
}

options! {
    UnstableOptions, UnstableOptionsTargetModifiers, Z_OPTIONS, dbopts, "Z", "unstable",

    // If you add a new option, please update:
    // - compiler/rustc_interface/src/tests.rs
    // - src/doc/unstable-book/src/compiler-flags

    // tidy-alphabetical-start
    allow_features: Option<Vec<String>> = (None, parse_opt_comma_list, [TRACKED],
        "only allow the listed language features to be enabled in code (comma separated)"),
    // the real parser is at the `setter_for` macro, to allow `-Z` and `-C` options to
    // work together.
    allow_partial_mitigations: () = ((), parse_allow_partial_mitigations, [UNTRACKED],
        "Allow mitigations not enabled for all dependency crates (comma separated list)"),
    always_encode_mir: bool = (false, parse_bool, [TRACKED],
        "encode MIR of all functions into the crate metadata (default: no)"),
    annotate_moves: AnnotateMoves = (AnnotateMoves::Disabled, parse_annotate_moves, [TRACKED],
        "emit debug info for compiler-generated move and copy operations \
        to make them visible in profilers. Can be a boolean or a size limit in bytes (default: disabled)"),
    assert_incr_state: Option<IncrementalStateAssertion> = (None, parse_assert_incr_state, [UNTRACKED],
        "assert that the incremental cache is in given state: \
         either `loaded` or `not-loaded`."),
    assume_incomplete_release: bool = (false, parse_bool, [TRACKED],
        "make cfg(version) treat the current version as incomplete (default: no)"),
    assumptions_on_binders: bool = (false, parse_bool, [TRACKED],
        "allow deducing higher-ranked outlives assumptions from all binders (`for<'a>`); \
         implies `-Znext-solver=globally`"),
    autodiff: Vec<crate::rustc_session::config::AutoDiff> = (Vec::new(), parse_autodiff, [TRACKED],
        "a list of autodiff flags to enable
        Mandatory setting:
        `=Enable`
        Optional extra settings:
        `=PrintTA`
        `=PrintAA`
        `=PrintPerf`
        `=PrintSteps`
        `=PrintModBefore`
        `=PrintModAfter`
        `=PrintModFinal`
        `=PrintPasses`,
        `=NoPostopt`
        `=LooseTypes`
        `=Inline`
        Multiple options can be combined with commas."),
    autodiff_post_passes: Option<String> = (None, parse_opt_string, [TRACKED],
        "set llvm passes to run after enzyme (no passes run when it is empty)"),
    binary_dep_depinfo: bool = (false, parse_bool, [TRACKED],
        "include artifacts (sysroot, crate dependencies) used during compilation in dep-info \
        (default: no)"),
    box_noalias: bool = (true, parse_bool, [TRACKED],
        "emit noalias metadata for box (default: yes)"),
    branch_protection: Option<BranchProtection> = (None, parse_branch_protection, [TRACKED] { TARGET_MODIFIER: BranchProtection },
        "set options for branch target identification and pointer authentication on AArch64"),
    build_sdylib_interface: bool = (false, parse_bool, [UNTRACKED],
        "whether the stable interface is being built"),
    cache_proc_macros: bool = (false, parse_bool, [TRACKED],
        "cache the results of derive proc macro invocations (potentially unsound!) (default: no"),
    cf_protection: CFProtection = (CFProtection::None, parse_cfprotection, [TRACKED],
        "instrument control-flow architecture protection"),
    check_cfg_all_expected: bool = (false, parse_bool, [UNTRACKED],
        "show all expected values in check-cfg diagnostics (default: no)"),
    checksum_hash_algorithm: Option<SourceFileHashAlgorithm> = (None, parse_cargo_src_file_hash, [TRACKED],
        "hash algorithm of source files used to check freshness in cargo (`blake3` or `sha256`)"),
    codegen_backend: Option<String> = (None, parse_opt_string, [TRACKED],
        "the backend to use"),
    codegen_emit_retag: Option<CodegenRetagOptions> = (None, parse_codegen_retag_options, [TRACKED],
        "emit retag function calls in generated code"),
    codegen_source_order: bool = (false, parse_bool, [UNTRACKED],
        "emit mono items in the order of spans in source files (default: no)"),
    contract_checks: Option<bool> = (None, parse_opt_bool, [TRACKED],
        "emit runtime checks for contract pre- and post-conditions (default: no)"),
    coverage_options: CoverageOptions = (CoverageOptions::default(), parse_coverage_options, [TRACKED],
        "control details of coverage instrumentation"),
    crate_attr: Vec<String> = (Vec::new(), parse_string_push, [TRACKED],
        "inject the given attribute in the crate"),
    cross_crate_inline_threshold: InliningThreshold = (InliningThreshold::Sometimes(100), parse_inlining_threshold, [TRACKED],
        "threshold to allow cross crate inlining of functions"),
    debug_info_type_line_numbers: bool = (false, parse_bool, [TRACKED],
        "emit type and line information for additional data types (default: no)"),
    debuginfo_compression: DebugInfoCompression = (DebugInfoCompression::None, parse_debuginfo_compression, [TRACKED],
        "compress debug info sections (none, zlib, zstd, default: none)"),
    debuginfo_for_profiling: bool = (false, parse_bool, [TRACKED],
        "emit extra debug info to make sample profile more accurate"),
    deduplicate_diagnostics: bool = (true, parse_bool, [UNTRACKED],
        "deduplicate identical diagnostics (default: yes)"),
    default_visibility: Option<SymbolVisibility> = (None, parse_opt_symbol_visibility, [TRACKED],
        "overrides the `default_visibility` setting of the target"),
    deny_partial_mitigations: () = ((), parse_deny_partial_mitigations, [UNTRACKED],
        "Deny mitigations not enabled for all dependency crates (comma separated list)"),
    dep_info_omit_d_target: bool = (false, parse_bool, [TRACKED],
        "in dep-info output, omit targets for tracking dependencies of the dep-info files \
        themselves (default: no)"),
    direct_access_external_data: Option<bool> = (None, parse_opt_bool, [TRACKED],
        "Direct or use GOT indirect to reference external data symbols"),
    disable_fast_paths: bool = (false, parse_bool, [TRACKED],
        "disable various performance optimizations in trait solving"),
    disable_incr_comp_backend_caching: bool = (false, parse_bool, [TRACKED],
        "disable caching of compiled objects by the codegen backend during incremental compilation"),
    disable_param_env_normalization_hack: bool = (false, parse_bool, [TRACKED],
        "do not treat all aliases in the environment as rigid with `-Znext-solver`"),
    dual_proc_macros: bool = (false, parse_bool, [TRACKED],
        "load proc macros for both target and host, but only link to the target (default: no)"),
    dump_dep_graph: bool = (false, parse_bool, [UNTRACKED],
        "dump the dependency graph to `$RUST_DEP_GRAPH` as both a text file and a GraphViz dot file (default: ./dep_graph.{dot, txt}) \
        (default: no)"),
    dump_mir: Option<String> = (None, parse_opt_string, [UNTRACKED],
        "dump MIR state to file.
        `val` is used to select which passes and functions to dump. For example:
        `all` matches all passes and functions,
        `foo` matches all passes for functions whose name contains 'foo',
        `foo & ConstProp` only the 'ConstProp' pass for function names containing 'foo',
        `foo | bar` all passes for function names containing 'foo' or 'bar'."),
    dump_mir_dataflow: bool = (false, parse_bool, [UNTRACKED],
        "in addition to `.mir` files, create graphviz `.dot` files with dataflow results \
        (default: no)"),
    dump_mir_dir: String = ("mir_dump".to_string(), parse_string, [UNTRACKED],
        "the directory the MIR is dumped into (default: `mir_dump`)"),
    dump_mir_exclude_alloc_bytes: bool = (false, parse_bool, [UNTRACKED],
        "exclude the raw bytes of allocations when dumping MIR (used in tests) (default: no)"),
    dump_mir_exclude_pass_number: bool = (false, parse_bool, [UNTRACKED],
        "exclude the pass number when dumping MIR (used in tests) (default: no)"),
    dump_mir_graphviz: bool = (false, parse_bool, [UNTRACKED],
        "in addition to `.mir` files, create graphviz `.dot` files (default: no)"),
    dwarf_version: Option<u32> = (None, parse_opt_number, [TRACKED],
        "version of DWARF debug information to emit (default: 2 or 4, depending on platform)"),
    dylib_lto: bool = (false, parse_bool, [UNTRACKED],
        "enables LTO for dylib crate type"),
    eagerly_emit_delayed_bugs: bool = (false, parse_bool, [UNTRACKED],
        "emit delayed bugs eagerly as errors instead of stashing them and emitting \
        them only if an error has not been emitted"),
    ehcont_guard: bool = (false, parse_bool, [TRACKED],
        "generate Windows EHCont Guard tables"),
    embed_metadata: bool = (true, parse_bool, [TRACKED],
        "embed metadata in rlibs and dylibs (default: yes)"),
    embed_source: bool = (false, parse_bool, [TRACKED],
        "embed source text in DWARF debug sections (default: no)"),
    emit_stack_sizes: bool = (false, parse_bool, [UNTRACKED],
        "emit a section containing stack size metadata (default: no)"),
    enforce_type_length_limit: bool = (false, parse_bool, [TRACKED],
        "enforce the type length limit when monomorphizing instances in codegen"),
    experimental_default_bounds: bool = (false, parse_bool, [TRACKED],
        "enable default bounds for experimental group of auto traits"),
    export_executable_symbols: bool = (false, parse_bool, [TRACKED],
        "export symbols from executables, as if they were dynamic libraries"),
    external_clangrt: bool = (false, parse_bool, [UNTRACKED],
        "rely on user specified linker commands to find clangrt"),
    extra_const_ub_checks: bool = (false, parse_bool, [TRACKED],
        "turns on more checks to detect const UB, which can be slow (default: no)"),
    fewer_names: Option<bool> = (None, parse_opt_bool, [TRACKED],
        "reduce memory use by retaining fewer names within compilation artifacts (LLVM-IR) \
        (default: no)"),
    fixed_x18: bool = (false, parse_bool, [TRACKED] { TARGET_MODIFIER: FixedX18 },
        "make the x18 register reserved on AArch64 (default: no)"),
    flatten_format_args: bool = (true, parse_bool, [TRACKED],
        "flatten nested format_args!() and literals into a simplified format_args!() call \
        (default: yes)"),
    fmt_debug: FmtDebug = (FmtDebug::Full, parse_fmt_debug, [TRACKED],
        "how detailed `#[derive(Debug)]` should be. `full` prints types recursively, \
        `shallow` prints only type names, `none` prints nothing and disables `{:?}`. (default: `full`)"),
    force_intrinsic_fallback: bool = (false, parse_bool, [TRACKED],
        "always use the fallback body of an intrinsic, if it has one, instead of lowering \
        the intrinsic in the codegen backend (default: no)."),
    force_unstable_if_unmarked: bool = (false, parse_bool, [TRACKED],
        "force all crates to be `rustc_private` unstable (default: no)"),
    function_return: FunctionReturn = (FunctionReturn::default(), parse_function_return, [TRACKED],
        "replace returns with jumps to `__x86_return_thunk` (default: `keep`)"),
    function_sections: Option<bool> = (None, parse_opt_bool, [TRACKED],
        "whether each function should go in its own section"),
    future_incompat_test: bool = (false, parse_bool, [UNTRACKED],
        "forces all lints to be future incompatible, used for internal testing (default: no)"),
    graphviz_dark_mode: bool = (false, parse_bool, [UNTRACKED],
        "use dark-themed colors in graphviz output (default: no)"),
    graphviz_font: String = ("Courier, monospace".to_string(), parse_string, [UNTRACKED],
        "use the given `fontname` in graphviz output; can be overridden by setting \
        environment variable `RUSTC_GRAPHVIZ_FONT` (default: `Courier, monospace`)"),
    has_thread_local: Option<bool> = (None, parse_opt_bool, [TRACKED],
        "explicitly enable the `cfg(target_thread_local)` directive"),
    help: bool = (false, parse_no_value, [UNTRACKED], "Print unstable compiler options"),
    higher_ranked_assumptions: bool = (false, parse_bool, [TRACKED],
        "allow deducing higher-ranked outlives assumptions from coroutines when proving auto traits"),
    hint_mostly_unused: bool = (false, parse_bool, [TRACKED],
        "hint that most of this crate will go unused, to minimize work for uncalled functions"),
    hint_msrv: Option<RustcVersion> = (None, parse_rust_version, [TRACKED],
        "control the minimum rust version for lints"),
    human_readable_cgu_names: bool = (false, parse_bool, [TRACKED],
        "generate human-readable, predictable names for codegen units (default: no)"),
    identify_regions: bool = (false, parse_bool, [UNTRACKED],
        "display unnamed regions as `'<id>`, using a non-ident unique id (default: no)"),
    ignore_directory_in_diagnostics_source_blocks: Vec<String> = (Vec::new(), parse_string_push, [UNTRACKED],
        "do not display the source code block in diagnostics for files in the directory"),
    implicit_sysroot_deps: bool = (true, parse_bool, [TRACKED],
        "allows rust to search sysroot for a crate's dependencies (default: yes)"),
    incremental_ignore_spans: bool = (false, parse_bool, [TRACKED],
        "ignore spans during ICH computation -- used for testing (default: no)"),
    incremental_info: bool = (false, parse_bool, [UNTRACKED],
        "print high-level information about incremental reuse (or the lack thereof) \
        (default: no)"),
    incremental_verify_ich: bool = (false, parse_bool, [UNTRACKED],
        "verify extended properties for incr. comp. (default: no):
        - hashes of green query instances
        - hash collisions of query keys
        - hash collisions when creating dep-nodes"),
    indirect_branch_cs_prefix: bool = (false, parse_bool, [TRACKED] { TARGET_MODIFIER: IndirectBranchCsPrefix },
        "add `cs` prefix to `call` and `jmp` to indirect thunks (default: no)"),
    inline_llvm: bool = (true, parse_bool, [TRACKED],
        "enable LLVM inlining (default: yes)"),
    inline_mir: Option<bool> = (None, parse_opt_bool, [TRACKED],
        "enable MIR inlining (default: no)"),
    inline_mir_forwarder_threshold: Option<usize> = (None, parse_opt_number, [TRACKED],
        "inlining threshold when the caller is a simple forwarding function (default: 30)"),
    inline_mir_hint_threshold: Option<usize> = (None, parse_opt_number, [TRACKED],
        "inlining threshold for functions with inline hint (default: 100)"),
    inline_mir_preserve_debug: Option<bool> = (None, parse_opt_bool, [TRACKED],
        "when MIR inlining, whether to preserve debug info for callee variables \
        (default: preserve for debuginfo != None, otherwise remove)"),
    inline_mir_threshold: Option<usize> = (None, parse_opt_number, [TRACKED],
        "a default MIR inlining threshold (default: 50)"),
    input_stats: bool = (false, parse_bool, [UNTRACKED],
        "print some statistics about AST and HIR (default: no)"),
    instrument_mcount: InstrumentMcount = (InstrumentMcount::Disabled, parse_instrument_mcount, [TRACKED],
        "insert function instrument code for mcount-based tracing (default: no)"),
    instrument_xray: Option<InstrumentXRay> = (None, parse_instrument_xray, [TRACKED],
        "insert function instrument code for XRay-based tracing (default: no)
         Optional extra settings:
         `=always`
         `=never`
         `=ignore-loops`
         `=instruction-threshold=N`
         `=skip-entry`
         `=skip-exit`
         Multiple options can be combined with commas."),
    // The only purpose of this flag is to act as a deterrent:
    // Marking a feature that's only meant for testing as internal might not preclude somebody from
    // trying to use it in `core` or `std` as both enable various internal features and utilize them
    // throughout. This is intentionally a flag not another internal feature as having to modify the
    // build cfg in `bootstrap` is arguably scarier than just needing to add `#![feature]`. It also
    // stands out a lot more during code review making it easier to get caught.
    internal_testing_features: bool = (false, parse_bool, [TRACKED],
        "allow certain internal language features to be enabled that help exercise & test the compiler"),
    large_data_threshold: Option<u64> = (None, parse_opt_number, [TRACKED],
        "set the threshold for objects to be stored in a \"large data\" section \
         (only effective with -Ccode-model=medium, default: 65536)"),
    layout_seed: Option<u64> = (None, parse_opt_number, [TRACKED],
        "seed layout randomization"),
    library_read: bool = (false, parse_bool, [UNTRACKED],
        "read the crate as a library already compiled by its own compiler: extract its facts and \
        write its metadata, and run no pass whose only job is to reject the source (default: no)"),
    link_directives: bool = (true, parse_bool, [TRACKED],
        "honor #[link] directives in the compiled crate (default: yes)"),
    link_native_libraries: bool = (true, parse_bool, [UNTRACKED],
        "link native libraries in the linker invocation (default: yes)"),
    link_only: bool = (false, parse_bool, [TRACKED],
        "link the `.rlink` file generated by `-Z no-link` (default: no)"),
    lint_llvm_ir: bool = (false, parse_bool, [TRACKED],
        "lint LLVM IR (default: no)"),
    lint_mir: bool = (false, parse_bool, [UNTRACKED],
        "lint MIR before and after each transformation"),
    llvm_module_flag: Vec<(String, u32, String)> = (Vec::new(), parse_llvm_module_flag, [TRACKED],
        "a list of module flags to pass to LLVM (space separated)"),
    llvm_plugins: Vec<String> = (Vec::new(), parse_list, [TRACKED],
        "a list LLVM plugins to enable (space separated)"),
    llvm_target_feature: String = (String::new(), parse_target_feature, [TRACKED] { TARGET_MODIFIER: LlvmTargetFeature },
        "enable/disable LLVM-level target features. \
        This feature is unsafe and can cause ABI issues and compiler crashes, \
        because LLVM does not support all target feature combinations."),
    llvm_writable: bool = (false, parse_bool, [TRACKED],
        "emit the LLVM writable attribute for mutable reference arguments (default: no)"),
    location_detail: LocationDetail = (LocationDetail::all(), parse_location_detail, [TRACKED],
        "what location details should be tracked when using caller_location, either \
        `none`, or a comma separated list of location details, for which \
        valid options are `file`, `line`, and `column` (default: `file,line,column`)"),
    ls: Vec<String> = (Vec::new(), parse_list, [UNTRACKED],
        "decode and print various parts of the crate metadata for a library crate \
        (space separated)"),
    macro_backtrace: bool = (false, parse_bool, [UNTRACKED],
        "show macro backtraces (default: no)"),
    macro_stats: bool = (false, parse_bool, [UNTRACKED],
        "print some statistics about macro expansions (default: no)"),
    maximal_hir_to_mir_coverage: bool = (false, parse_bool, [TRACKED],
        "save as much information as possible about the correspondence between MIR and HIR \
        as source scopes (default: no)"),
    merge_functions: Option<MergeFunctions> = (None, parse_merge_functions, [TRACKED],
        "control the operation of the MergeFunctions LLVM pass, taking \
        the same values as the target option of the same name"),
    meta_stats: bool = (false, parse_bool, [UNTRACKED],
        "gather metadata statistics (default: no)"),
    metrics_dir: Option<PathBuf> = (None, parse_opt_pathbuf, [UNTRACKED],
        "the directory metrics emitted by rustc are dumped into (implicitly enables default set of metrics)"),
    min_function_alignment: Option<Align> = (None, parse_align, [TRACKED],
        "align all functions to at least this many bytes. Must be a power of 2"),
    min_recursion_limit: Option<usize> = (None, parse_opt_number, [TRACKED],
        "set a minimum recursion limit (final limit = max(this, recursion_limit_from_crate))"),
    mir_enable_passes: Vec<(String, bool)> = (Vec::new(), parse_list_with_polarity, [TRACKED],
        "use like `-Zmir-enable-passes=+DestinationPropagation,-InstSimplify`. Forces the \
        specified passes to be enabled, overriding all other checks. In particular, this will \
        enable unsound (known-buggy and hence usually disabled) passes without further warning! \
        Passes that are not specified are enabled or disabled by other flags as usual."),
    mir_include_spans: MirIncludeSpans = (MirIncludeSpans::default(), parse_mir_include_spans, [UNTRACKED],
        "include extra comments in mir pretty printing, like line numbers and statement indices, \
         details about types, etc. (boolean for all passes, 'nll' to enable in NLL MIR only, default: 'nll')"),
    mir_opt_bisect_limit: Option<usize> = (None, parse_opt_number, [TRACKED],
        "limit the number of MIR optimization pass executions (global across all bodies). \
        Pass executions after this limit are skipped and reported. (default: no limit)"),
    mir_opt_level: Option<usize> = (None, parse_opt_number, [TRACKED],
        "MIR optimization level (0-4; default: 1 in non optimized builds and 2 in optimized builds)"),
    mir_preserve_ub: bool = (false, parse_bool, [TRACKED],
        "keep place mention statements and reads in trivial SwitchInt terminators, which are interpreted \
        e.g., by miri; implies -Zmir-opt-level=0 (default: no)"),
    mir_strip_debuginfo: MirStripDebugInfo = (MirStripDebugInfo::None, parse_mir_strip_debuginfo, [TRACKED],
        "Whether to remove some of the MIR debug info from methods.  Default: None"),
    move_size_limit: Option<usize> = (None, parse_opt_number, [TRACKED],
        "the size at which the `large_assignments` lint starts to be emitted"),
    namespaced_crates: bool = (false, parse_bool, [TRACKED],
        "allow crates to be namespaced by other crates (default: no)"),
    next_solver: NextSolverConfig = (NextSolverConfig::default(), parse_next_solver_config, [TRACKED],
        "enable and configure the next generation trait solver used by rustc"),
    no_analysis: bool = (false, parse_no_value, [UNTRACKED],
        "parse and expand the source, but run no analysis"),
    no_codegen: bool = (false, parse_no_value, [TRACKED_NO_CRATE_HASH],
        "run all passes except codegen; no output"),
    no_generate_arange_section: bool = (false, parse_no_value, [TRACKED],
        "omit DWARF address ranges that give faster lookups"),
    no_implied_bounds_compat: bool = (false, parse_bool, [TRACKED],
        "disable the compatibility version of the `implied_bounds_ty` query"),
    no_leak_check: bool = (false, parse_no_value, [UNTRACKED],
        "disable the 'leak check' for subtyping; unsound, but useful for tests"),
    no_link: bool = (false, parse_no_value, [TRACKED],
        "compile without linking"),
    no_parallel_backend: bool = (false, parse_no_value, [UNTRACKED],
        "use `--jobs-backend=1` instead"),
    no_profiler_runtime: bool = (false, parse_no_value, [TRACKED],
        "prevent automatic injection of the profiler_builtins crate"),
    no_steal_thir: bool = (false, parse_bool, [UNTRACKED],
        "don't steal the THIR when we're done with it; useful for rustc drivers (default: no)"),
    no_trait_vptr: bool = (false, parse_no_value, [TRACKED],
        "disable generation of trait vptr in vtable for upcasting"),
    no_unique_section_names: bool = (false, parse_bool, [TRACKED],
        "do not use unique names for text and data sections when -Z function-sections is used"),
    normalize_docs: bool = (false, parse_bool, [TRACKED],
        "normalize associated items in rustdoc when generating documentation"),
    offload: Vec<crate::rustc_session::config::Offload> = (Vec::new(), parse_offload, [TRACKED],
        "a list of offload flags to enable
        Mandatory setting:
        `=Enable`
        Currently the only option available"),
    on_broken_pipe: OnBrokenPipe = (OnBrokenPipe::Default, parse_on_broken_pipe, [TRACKED],
        "behavior of std::io::ErrorKind::BrokenPipe (SIGPIPE)"),
    osx_rpath_install_name: bool = (false, parse_bool, [TRACKED],
        "pass `-install_name @rpath/...` to the macOS linker (default: no)"),
    packed_bundled_libs: bool = (false, parse_bool, [TRACKED],
        "change rlib format to store native libraries as archives"),
    packed_stack: bool = (false, parse_bool, [TRACKED],
        "use packed stack frames (s390x only) (default: no)"),
    panic_abort_tests: bool = (false, parse_bool, [TRACKED],
        "support compiling tests with panic=abort (default: no)"),
    panic_in_drop: PanicStrategy = (PanicStrategy::Unwind, parse_panic_strategy, [TRACKED],
        "panic strategy for panics in drops"),
    parse_crate_root_only: bool = (false, parse_bool, [UNTRACKED],
        "parse the crate root file only; do not parse other files, compile, assemble, or link \
        (default: no)"),
    patchable_function_entry: PatchableFunctionEntry = (PatchableFunctionEntry::default(), parse_patchable_function_entry, [TRACKED],
        "nop padding at function entry"),
    plt: Option<bool> = (None, parse_opt_bool, [TRACKED],
        "whether to use the PLT when calling into shared libraries;
        only has effect for PIC code on systems with ELF binaries
        (default: PLT is disabled if full relro is enabled on x86_64)"),
    pointer_authentication: Vec<(PointerAuthOption, bool)> = (
        Vec::new(),
        parse_pointer_authentication_list_with_polarity,
        [TRACKED]
        { TARGET_MODIFIER: PointerAuthentication },
        "A comma-separated list of pointer authentication options, each prefixed with `+` (enable) or `-` (disable). Available options:
        `aarch64-jump-table-hardening` - enable hardened lowering for jump-table dispatch
        `auth-traps` - trap immediately on pointer authentication failure
        `calls` - enable signing and authentication of all indirect calls
        `elf-got` - enable authentication of pointers from GOT (ELF only)
        `function-pointer-type-discrimination` - enable type discrimination on C function pointers
        `indirect-gotos` - enable signing and authentication of indirect goto targets
        `init-fini` - enable signing of function pointers in init/fini arrays
        `init-fini-address-discrimination` - enable address discrimination in init/fini arrays
        `intrinsics` - pointer authentication intrinsics
        `return-addresses` - enable signing and authentication of return addresses
        `typeinfo-vt-ptr-discrimination - incorporate type and address discrimination in authenticated vtable pointers for std::type_info
        `vt-ptr-addr-discrimination - incorporate address discrimination in authenticated vtable pointers
        `vt-ptr-type-discrimination - incorporate type discrimination in authenticated vtable pointers
        Example: `-Zpointer-authentication=+calls,-init-fini`."),
    polonius: Polonius = (Polonius::default(), parse_polonius, [TRACKED],
        "select the polonius-based borrow checker"),
    pre_link_arg: (/* redirected to pre_link_args */) = ((), parse_string_push, [UNTRACKED],
        "a single extra argument to prepend the linker invocation (can be used several times)"),
    pre_link_args: Vec<String> = (Vec::new(), parse_list, [UNTRACKED],
        "extra arguments to prepend to the linker invocation (space separated)"),
    precise_enum_drop_elaboration: bool = (true, parse_bool, [TRACKED],
        "use a more precise version of drop elaboration for matches on enums (default: yes). \
        This results in better codegen, but has caused miscompilations on some tier 2 platforms. \
        See #77382 and #74551."),
    proc_macro_backtrace: bool = (false, parse_bool, [UNTRACKED],
         "show backtraces for panics during proc-macro execution (default: no)"),
    proc_macro_execution_strategy: ProcMacroExecutionStrategy = (ProcMacroExecutionStrategy::SameThread,
        parse_proc_macro_execution_strategy, [UNTRACKED],
        "how to run proc-macro code (default: same-thread)"),
    profile_closures: bool = (false, parse_no_value, [UNTRACKED],
        "profile size of closures"),
    profiler_runtime: String = (String::from("profiler_builtins"), parse_string, [TRACKED],
        "name of the profiler runtime crate to automatically inject (default: `profiler_builtins`)"),
    query_dep_graph: bool = (false, parse_bool, [UNTRACKED],
        "enable queries of the dependency graph for regression testing (default: no)"),
    randomize_layout: bool = (false, parse_bool, [TRACKED],
        "randomize the layout of types (default: no)"),
    reg_struct_return: bool = (false, parse_bool, [TRACKED] { TARGET_MODIFIER: RegStructReturn },
        "On x86-32 targets, it overrides the default ABI to return small structs in registers.
        It is UNSOUND to link together crates that use different values for this flag!"),
    regparm: Option<u32> = (None, parse_opt_number, [TRACKED] { TARGET_MODIFIER: Regparm },
        "On x86-32 targets, setting this to N causes the compiler to pass N arguments \
        in registers EAX, EDX, and ECX instead of on the stack for\
        \"C\", \"cdecl\", and \"stdcall\" fn.\
        It is UNSOUND to link together crates that use different values for this flag!"),
    relax_elf_relocations: Option<bool> = (None, parse_opt_bool, [TRACKED],
        "whether ELF relocations can be relaxed"),
    remap_cwd_prefix: Option<PathBuf> = (None, parse_opt_pathbuf, [TRACKED],
        "remap paths under the current working directory to this path prefix"),
    remark_dir: Option<PathBuf> = (None, parse_opt_pathbuf, [UNTRACKED],
        "directory into which to write optimization remarks (if not specified, they will be \
written to standard error output)"),
    renormalize_rigid_aliases: bool = (false, parse_bool, [TRACKED],
        "do not skip rigid aliases in normalization for internal debugging"),
    retpoline: bool = (false, parse_bool, [TRACKED] { TARGET_MODIFIER: Retpoline },
        "enables retpoline-indirect-branches and retpoline-indirect-calls target features (default: no)"),
    retpoline_external_thunk: bool = (false, parse_bool, [TRACKED] { TARGET_MODIFIER: RetpolineExternalThunk },
        "enables retpoline-external-thunk, retpoline-indirect-branches and retpoline-indirect-calls \
        target features (default: no)"),
    sanitizer: SanitizerSet = (SanitizerSet::empty(), parse_sanitizers, [TRACKED] { TARGET_MODIFIER: Sanitizer },
        "use a sanitizer"),
    sanitizer_cfi_canonical_jump_tables: Option<bool> = (Some(true), parse_opt_bool, [TRACKED],
        "enable canonical jump tables (default: yes)"),
    sanitizer_cfi_generalize_pointers: Option<bool> = (None, parse_opt_bool, [TRACKED],
        "enable generalizing pointer types (default: no)"),
    sanitizer_cfi_normalize_integers: Option<bool> = (None, parse_opt_bool, [TRACKED] { TARGET_MODIFIER: SanitizerCfiNormalizeIntegers },
        "enable normalizing integer types (default: no)"),
    sanitizer_cfi_diag: Option<bool> = (None, parse_opt_bool, [TRACKED],
        "enable CFI diagnostics (default: no)"),
    sanitizer_cfi_recover: Option<bool> = (None, parse_opt_bool, [TRACKED],
        "enable CFI recovery (default: no)"),
    sanitizer_dataflow_abilist: Vec<String> = (Vec::new(), parse_comma_list, [TRACKED],
        "additional ABI list files that control how shadow parameters are passed (comma separated)"),
    sanitizer_kcfi_arity: Option<bool> = (None, parse_opt_bool, [TRACKED],
        "enable KCFI arity indicator (default: no)"),
    sanitizer_memory_track_origins: usize = (0, parse_sanitizer_memory_track_origins, [TRACKED],
        "enable origins tracking in MemorySanitizer"),
    sanitizer_recover: SanitizerSet = (SanitizerSet::empty(), parse_sanitizers, [TRACKED],
        "enable recovery for selected sanitizers"),
    saturating_float_casts: Option<bool> = (None, parse_opt_bool, [TRACKED],
        "make float->int casts UB-free: numbers outside the integer type's range are clipped to \
        the max/min integer respectively, and NaN is mapped to 0 (default: yes)"),
    self_profile: SwitchWithOptPath = (SwitchWithOptPath::Disabled,
        parse_switch_with_opt_path, [UNTRACKED],
        "run the self profiler and output the raw event data"),
    self_profile_counter: String = ("wall-time".to_string(), parse_string, [UNTRACKED],
        "counter used by the self profiler (default: `wall-time`), one of:
        `wall-time` (monotonic clock, i.e. `std::time::Instant`)
        `instructions:u` (retired instructions, userspace-only)
        `instructions-minus-irqs:u` (subtracting hardware interrupt counts for extra accuracy)"
    ),
    /// keep this in sync with the event filter names in librustc_data_structures/profiling.rs
    self_profile_events: Option<Vec<String>> = (None, parse_opt_comma_list, [UNTRACKED],
        "specify the events recorded by the self profiler;
        for example: `-Z self-profile-events=default,query-keys`
        all options: none, all, default, generic-activity, query-provider, query-cache-hit
                     query-blocked, incr-cache-load, incr-result-hashing, query-keys, function-args, args, llvm, artifact-sizes"),
    share_generics: Option<bool> = (None, parse_opt_bool, [TRACKED],
        "make the current crate share its generic instantiations"),
    shell_argfiles: bool = (false, parse_bool, [UNTRACKED],
        "allow argument files to be specified with POSIX \"shell-style\" argument quoting"),
    simulate_remapped_rust_src_base: Option<PathBuf> = (None, parse_opt_pathbuf, [TRACKED],
        "simulate the effect of remap-debuginfo = true at bootstrapping by remapping path \
        to rust's source base directory. only meant for testing purposes"),
    small_data_threshold: Option<usize> = (None, parse_opt_number, [TRACKED],
        "Set the threshold for objects to be stored in a \"small data\" section"),
    span_debug: bool = (false, parse_bool, [UNTRACKED],
        "forward proc_macro::Span's `Debug` impl to `Span`"),
    /// o/w tests have closure@path
    span_free_formats: bool = (false, parse_bool, [UNTRACKED],
        "exclude spans when debug-printing compiler state (default: no)"),
    split_dwarf_inlining: bool = (false, parse_bool, [TRACKED],
        "provide minimal debug info in the object/executable to facilitate online \
         symbolication/stack traces in the absence of .dwo/.dwp files when using Split DWARF"),
    split_dwarf_kind: SplitDwarfKind = (SplitDwarfKind::Split, parse_split_dwarf_kind, [TRACKED],
        "split dwarf variant (only if -Csplit-debuginfo is enabled and on relevant platform)
        (default: `split`)

        `split`: sections which do not require relocation are written into a DWARF object (`.dwo`)
                 file which is ignored by the linker
        `single`: sections which do not require relocation are written into object file but ignored
                  by the linker"),
    split_dwarf_out_dir : Option<PathBuf> = (None, parse_opt_pathbuf, [TRACKED],
        "location for writing split DWARF objects (`.dwo`) if enabled"),
    split_lto_unit: Option<bool> = (None, parse_opt_bool, [TRACKED],
        "enable LTO unit splitting (default: no)"),
    src_hash_algorithm: Option<SourceFileHashAlgorithm> = (None, parse_src_file_hash, [TRACKED],
        "hash algorithm of source files in debug info (`md5`, `sha1`, or `sha256`)"),
    stack_protector: StackProtector = (StackProtector::None, parse_stack_protector, [TRACKED] { MITIGATION: StackProtector },
        "control stack smash protection strategy (`rustc --print stack-protector-strategies` for details)"),
    staticlib_allow_rdylib_deps: bool = (false, parse_bool, [TRACKED],
        "allow staticlibs to have rust dylib dependencies"),
    staticlib_hide_internal_symbols: bool = (false, parse_bool, [TRACKED],
        "hide non-exported Rust symbols when building staticlibs by setting STV_HIDDEN"),
    staticlib_rename_internal_symbols: bool = (false, parse_bool, [TRACKED],
        "rename non-exported Rust symbols when building staticlibs to avoid conflicts"),
    staticlib_prefer_dynamic: bool = (false, parse_bool, [TRACKED],
        "prefer dynamic linking to static linking for staticlibs (default: no)"),
    strict_init_checks: bool = (false, parse_bool, [TRACKED],
        "control if mem::uninitialized and mem::zeroed panic on more UB"),
    teach: bool = (false, parse_bool, [TRACKED],
        "show extended diagnostic help (default: no)"),
    temps_dir: Option<String> = (None, parse_opt_string, [UNTRACKED],
        "the directory the intermediate files are written to"),
    terminal_urls: TerminalUrl = (TerminalUrl::No, parse_terminal_url, [UNTRACKED],
        "use the OSC 8 hyperlink terminal specification to print hyperlinks in the compiler output"),
    thinlto: Option<bool> = (None, parse_opt_bool, [TRACKED],
        "enable ThinLTO when possible"),
    threads: Option<String> = (None, parse_opt_string, [UNTRACKED],
        "use `--jobs-frontend` instead"),
    time_passes: bool = (false, parse_bool, [UNTRACKED],
        "measure time of each rustc pass (default: no)"),
    time_passes_format: TimePassesFormat = (TimePassesFormat::Text, parse_time_passes_format, [UNTRACKED],
        "the format to use for -Z time-passes (`text` (default) or `json`)"),
    tiny_const_eval_limit: bool = (false, parse_bool, [TRACKED],
        "sets a tiny, non-configurable limit for const eval; useful for compiler tests"),
    tls_model: Option<TlsModel> = (None, parse_tls_model, [TRACKED],
        "choose the TLS model to use (`rustc --print tls-models` for details)"),
    trace_macros: bool = (false, parse_bool, [UNTRACKED],
        "for every macro invocation, print its name and arguments (default: no)"),
    track_diagnostics: bool = (false, parse_bool, [UNTRACKED],
        "tracks where in rustc a diagnostic was emitted"),
    translate_remapped_path_to_local_path: bool = (true, parse_bool, [TRACKED],
        "translate remapped paths into local paths when possible (default: yes)"),
    trap_unreachable: Option<bool> = (None, parse_opt_bool, [TRACKED],
        "generate trap instructions for unreachable intrinsics (default: use target setting, usually yes)"),
    treat_err_as_bug: Option<NonZero<usize>> = (None, parse_treat_err_as_bug, [TRACKED],
        "treat the `val`th error that occurs as bug (default if not specified: 0 - don't treat errors as bugs. \
        default if specified without a value: 1 - treat the first error as bug)"),
    trim_diagnostic_paths: bool = (true, parse_bool, [UNTRACKED],
        "in diagnostics, use heuristics to shorten paths referring to items"),
    tune_cpu: Option<String> = (None, parse_opt_string, [TRACKED],
        "select processor to schedule for (`rustc --print target-cpus` for details)"),
    typing_mode_post_typeck_until_borrowck: bool = (false, parse_bool, [TRACKED],
        "enable `TypingMode::PostTypeckUntilBorrowck`, changing the way opaque types are handled during MIR borrowck"),
    ub_checks: Option<bool> = (None, parse_opt_bool, [TRACKED],
        "emit runtime checks for Undefined Behavior (default: -Cdebug-assertions)"),
    ui_testing: bool = (false, parse_bool, [UNTRACKED],
        "emit compiler diagnostics in a form suitable for UI testing (default: no)"),
    uninit_const_chunk_threshold: usize = (16, parse_number, [TRACKED],
        "allow generating const initializers with mixed init/uninit chunks, \
        and set the maximum number of chunks for which this is allowed (default: 16)"),
    unleash_the_miri_inside_of_you: bool = (false, parse_bool, [TRACKED],
        "take the brakes off const evaluation. NOTE: this is unsound (default: no)"),
    unpretty: Option<String> = (None, parse_unpretty, [UNTRACKED],
        "present the input source, unstable (and less-pretty) variants;
        `normal`,
        `expanded`, `expanded,identified`,
        `expanded,hygiene` (with internal representations),
        `ast-tree` (raw AST before expansion),
        `ast-tree,expanded` (raw AST after expansion),
        `hir` (the HIR), `hir,identified`,
        `hir,typed` (HIR with types for each node),
        `hir-tree` (dump the raw HIR),
        `thir-tree`, `thir-flat`,
        `mir` (the MIR), or `mir-cfg` (graphviz formatted MIR)"),
    unsound_mir_opts: bool = (false, parse_bool, [TRACKED],
        "enable unsound and buggy MIR optimizations (default: no)"),
    /// This name is kind of confusing: Most unstable options enable something themselves, while
    /// this just allows "normal" options to be feature-gated.
    ///
    /// The main check for `-Zunstable-options` takes place separately from the
    /// usual parsing of `-Z` options (see [`crate::rustc_session::config::nightly_options`]),
    /// so this boolean value is mostly used for enabling unstable _values_ of
    /// stable options. That separate check doesn't handle boolean values, so
    /// to avoid an inconsistent state we also forbid them here.
    unstable_options: bool = (false, parse_no_value, [UNTRACKED],
        "adds unstable command line options to rustc interface (default: no)"),
    use_ctors_section: Option<bool> = (None, parse_opt_bool, [TRACKED],
        "use legacy .ctors section for initializers rather than .init_array"),
    use_sync_unwind: Option<bool> = (None, parse_opt_bool, [TRACKED],
        "Generate sync unwind tables instead of async unwind tables (default: no)"),
    validate_mir: bool = (false, parse_bool, [UNTRACKED],
        "validate MIR after each transformation"),
    verbose_asm: bool = (false, parse_bool, [TRACKED],
        "add descriptive comments from LLVM to the assembly (may change behavior) (default: no)"),
    verbose_internals: bool = (false, parse_bool, [TRACKED_NO_CRATE_HASH],
        "in general, enable more debug printouts (default: no)"),
    virtual_function_elimination: bool = (false, parse_bool, [TRACKED],
        "enables dead virtual function elimination optimization. \
        Requires `-Clto[=[fat,yes]]`"),
    wasi_exec_model: Option<WasiExecModel> = (None, parse_wasi_exec_model, [TRACKED],
        "whether to build a wasi command or reactor"),
    // This option only still exists to provide a more gradual transition path for people who need
    // the spec-complaint C ABI to be used.
    // FIXME remove this after a couple releases
    wasm_c_abi: () = ((), parse_wasm_c_abi, [TRACKED],
        "use spec-compliant C ABI for `wasm32-unknown-unknown` (deprecated, always enabled)"),
    wasm_proc_macros: bool = (false, parse_bool, [TRACKED],
        "enable support for compiling and loading wasm proc macros"),
    write_long_types_to_disk: bool = (true, parse_bool, [UNTRACKED],
        "whether long type names should be written to files instead of being printed in errors"),
    // tidy-alphabetical-end

    // If you add a new option, please update:
    // - compiler/rustc_interface/src/tests.rs
    // - src/doc/unstable-book/src/compiler-flags
}
