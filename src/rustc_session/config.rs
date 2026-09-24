//! Contains the semantic configuration used to build compiler sessions.

// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use alloc::collections::btree_map::{
    Iter as BTreeMapIter, Keys as BTreeMapKeysIter, Values as BTreeMapValuesIter,
};
use alloc::collections::{BTreeMap, BTreeSet};
use core::hash::Hash;
use core::num::NonZero;
use eko::path::{Path, PathBuf};
use core::str::{self, FromStr};
use core::{iter};

use crate::rustc_data_structures::fx::FxIndexMap;
use crate::rustc_data_structures::stable_hash::{StableHasher, StableOrd};
use crate::rustc_errors::emitter::HumanReadableErrorType;
use crate::rustc_errors::{ColorConfig, DiagCtxtFlags};
use crate::rustc_feature::UnstableFeatures;
use crate::rustc_hashes::Hash64;
use rustc_macros::{BlobDecodable, Decodable, Encodable, StableHash};
use crate::rustc_span::edition::DEFAULT_EDITION;
use crate::rustc_span::source_map::FilePathMapping;
use crate::rustc_span::{
    FileName, RealFileName, RemapPathScopeComponents, SourceFileHashAlgorithm, Symbol, sym,
};
use crate::rustc_structures::CrateType;
use crate::rustc_target::spec::{
    LinkSelfContainedComponents, LinkerFeatures, SplitDebuginfo, Target, TargetTuple,
};
use tracing::debug;

pub use crate::rustc_session::config::cfg::{Cfg, CheckCfg, ExpectedValues};
use crate::rustc_session::diagnostics::FileWriteFail;
pub use crate::rustc_session::options::*;
use crate::rustc_session::utils::CanonicalizedPath;
use crate::rustc_session::{EarlyDiagCtxt, Session, filesearch};

mod cfg;
pub mod sigpipe;

/// Special CPU name requesting the CPU of the current host.
pub const NATIVE_CPU: &str = "native";

/// The different settings that the `-C strip` flag can have.
#[derive(Clone, Copy, PartialEq, Hash, Debug)]
pub enum Strip {
    /// Do not strip at all.
    None,

    /// Strip debuginfo.
    Debuginfo,

    /// Strip all symbols.
    Symbols,
}

/// The different settings that the `-C control-flow-guard` flag can have.
#[derive(Clone, Copy, PartialEq, Hash, Debug)]
pub enum CFGuard {
    /// Do not emit Control Flow Guard metadata or checks.
    Disabled,

    /// Emit Control Flow Guard metadata but no checks.
    NoChecks,

    /// Emit Control Flow Guard metadata and checks.
    Checks,
}

/// The different settings that the `-Z cf-protection` flag can have.
#[derive(Clone, Copy, PartialEq, Hash, Debug)]
pub enum CFProtection {
    /// Do not enable control-flow protection
    None,

    /// Emit control-flow protection for branches (enables indirect branch tracking).
    Branch,

    /// Emit control-flow protection for returns.
    Return,

    /// Emit control-flow protection for both branches and returns.
    Full,
}

#[derive(Clone, Copy, Debug, PartialEq, Hash, StableHash, Encodable, Decodable)]
pub enum OptLevel {
    /// `-Copt-level=0`
    No,
    /// `-Copt-level=1`
    Less,
    /// `-Copt-level=2`
    More,
    /// `-Copt-level=3` / `-O`
    Aggressive,
    /// `-Copt-level=s`
    Size,
    /// `-Copt-level=z`
    SizeMin,
}

/// This is what the `LtoCli` values get mapped to after resolving defaults and
/// and taking other command line options into account.
///
/// Note that linker plugin-based LTO is a different mechanism entirely.
#[derive(Clone, PartialEq, Encodable, Decodable)]
pub enum Lto {
    /// Don't do any LTO whatsoever.
    No,

    /// Do a full-crate-graph (inter-crate) LTO with ThinLTO.
    Thin,

    /// Do a local ThinLTO (intra-crate, over the CodeGen Units of the local crate only). This is
    /// only relevant if multiple CGUs are used.
    ThinLocal,

    /// Do a full-crate-graph (inter-crate) LTO with "fat" LTO.
    Fat,
}

/// The different settings that the `-C lto` flag can have.
#[derive(Clone, Copy, PartialEq, Hash, Debug)]
pub enum LtoCli {
    /// `-C lto=no`
    No,
    /// `-C lto=yes`
    Yes,
    /// `-C lto`
    NoParam,
    /// `-C lto=thin`
    Thin,
    /// `-C lto=fat`
    Fat,
    /// No `-C lto` flag passed
    Unspecified,
}

/// The different settings that the `-C instrument-coverage` flag can have.
#[derive(Clone, Copy, PartialEq, Hash, Debug)]
pub enum InstrumentCoverage {
    /// `-C instrument-coverage=no` (or `off`, `false` etc.)
    No,
    /// `-C instrument-coverage` or `-C instrument-coverage=yes`
    Yes,
}

/// Individual flag values controlled by `-Zcoverage-options`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct CoverageOptions {
    pub level: CoverageLevel,

    /// **(internal test-only flag)**
    /// `-Zcoverage-options=discard-all-spans-in-codegen`: During codegen,
    /// discard all coverage spans as though they were invalid. Needed by
    /// regression tests for #133606, because we don't have an easy way to
    /// reproduce it from actual source code.
    pub discard_all_spans_in_codegen: bool,
}

/// Controls whether branch coverage is enabled.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub enum CoverageLevel {
    /// Instrument for coverage at the MIR block level.
    #[default]
    Block,
    /// Also instrument branch points (includes block coverage).
    Branch,
    /// Same as branch coverage, but also adds branch instrumentation for
    /// certain boolean expressions that are not directly used for branching.
    ///
    /// For example, in the following code, `b` does not directly participate
    /// in a branch, but condition coverage will instrument it as its own
    /// artificial branch:
    /// ```
    /// # let (a, b) = (false, true);
    /// let x = a && b;
    /// //           ^ last operand
    /// ```
    ///
    /// This level is mainly intended to be a stepping-stone towards full MC/DC
    /// instrumentation, so it might be removed in the future when MC/DC is
    /// sufficiently complete, or if it is making MC/DC changes difficult.
    Condition,
}

// The different settings that the `-Z offload` flag can have.
#[derive(Clone, PartialEq, Hash, Debug, Encodable, Decodable)]
pub enum Offload {
    /// Second step in the offload pipeline, enables kernel compilation for a gpu device
    /// Reads a manifest of required generic kernel instantiations
    /// produced by a previous `HostMetadata` pass. An empty manifest
    /// means there are no generic kernels at all, or that generic kernels are only
    /// called from non-generic device entry points and never from the host, so we
    /// don't need to track their instantiations.
    Device(String),
    /// Third step in the offload pipeline, generates the host code to call kernels.
    Host(String),
    /// Test is similar to Host, but allows testing without a device artifact.
    Test,
    /// First step in the offload pipeline: compile for the host but only emit a manifest of
    /// kernel instantiations required by the host code.
    HostMetadata(String),
}

/// The different settings that the `-Z codegen-emit-retag` flag can have.
#[derive(Copy, Clone, Debug, Default, PartialEq, Hash, Encodable, Decodable)]
pub struct CodegenRetagOptions {
    /// Track interior mutable data on the level of references, instead of on the byte level.
    pub no_precise_im: bool,
    /// Track `UnsafePinned` data on the level of references, instead of on the byte level.
    pub no_precise_pin: bool,
}

/// The different settings that the `-Z autodiff` flag can have.
#[derive(Clone, PartialEq, Hash, Debug, Encodable, Decodable)]
pub enum AutoDiff {
    /// Enable the autodiff opt pipeline
    Enable,

    /// Print TypeAnalysis information
    PrintTA,
    /// Print TypeAnalysis information for a specific function
    PrintTAFn(String),
    /// Print ActivityAnalysis Information
    PrintAA,
    /// Print Performance Warnings from Enzyme
    PrintPerf,
    /// Print intermediate IR generation steps
    PrintSteps,
    /// Print the module, before running autodiff.
    PrintModBefore,
    /// Print the module after running autodiff.
    PrintModAfter,
    /// Print the module after running autodiff and optimizations.
    PrintModFinal,

    /// Print all passes scheduled by LLVM
    PrintPasses,
    /// Disable extra opt run after running autodiff
    NoPostopt,
    /// Enzyme's loose type debug helper (can cause incorrect gradients!!)
    /// Usable in cases where Enzyme errors with `can not deduce type of X`.
    LooseTypes,
    /// Runs Enzyme's aggressive inlining
    Inline,
    /// Disable Type Tree
    NoTT,
}

/// The different settings that the `-Z annotate-moves` flag can have.
#[derive(Clone, Copy, PartialEq, Hash, Debug)]
pub enum AnnotateMoves {
    /// `-Z annotate-moves=no` (or `off`, `false` etc.)
    Disabled,
    /// `-Z annotate-moves` or `-Z annotate-moves=yes` (use default size limit)
    /// `-Z annotate-moves=SIZE` (use specified size limit)
    Enabled(Option<u64>),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct InstrumentMcountOpts {
    // Insert a nop which could be replaced by an mcount call.
    pub no_call: bool,
    // Record the location of the call instrument in a special linker section.
    pub record: bool,
}

/// The different settings that the `-Z Instrument-mcount` flag can have.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum InstrumentMcount {
    /// `-Z instrument-mcount=no`
    Disabled,
    /// `-Z instrument-mcount=yes`
    Mcount(InstrumentMcountOpts),
    /// `-Z instrument-mcount=fentry`
    Fentry(InstrumentMcountOpts),
}

/// Settings for `-Z instrument-xray` flag.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct InstrumentXRay {
    /// `-Z instrument-xray=always`, force instrumentation
    pub always: bool,
    /// `-Z instrument-xray=never`, disable instrumentation
    pub never: bool,
    /// `-Z instrument-xray=ignore-loops`, ignore presence of loops,
    /// instrument functions based only on instruction count
    pub ignore_loops: bool,
    /// `-Z instrument-xray=instruction-threshold=N`, explicitly set instruction threshold
    /// for instrumentation, or `None` to use compiler's default
    pub instruction_threshold: Option<usize>,
    /// `-Z instrument-xray=skip-entry`, do not instrument function entry
    pub skip_entry: bool,
    /// `-Z instrument-xray=skip-exit`, do not instrument function exit
    pub skip_exit: bool,
}

#[derive(Clone, PartialEq, Hash, Debug)]
pub enum LinkerPluginLto {
    LinkerPlugin(PathBuf),
    LinkerPluginAuto,
    Disabled,
}

impl LinkerPluginLto {
    pub fn enabled(&self) -> bool {
        match *self {
            LinkerPluginLto::LinkerPlugin(_) | LinkerPluginLto::LinkerPluginAuto => true,
            LinkerPluginLto::Disabled => false,
        }
    }
}

/// The different values `-C link-self-contained` can take: a list of individually enabled or
/// disabled components used during linking, coming from the rustc distribution, instead of being
/// found somewhere on the host system.
///
/// They can be set in bulk via `-C link-self-contained=yes|y|on` or `-C
/// link-self-contained=no|n|off`, and those boolean values are the historical defaults.
///
/// But each component is fine-grained, and can be unstably targeted, to use:
/// - some CRT objects
/// - the libc static library
/// - libgcc/libunwind libraries
/// - a linker we distribute
/// - some sanitizer runtime libraries
/// - all other MinGW libraries and Windows import libs
///
#[derive(Default, Clone, PartialEq, Debug)]
pub struct LinkSelfContained {
    /// Whether the user explicitly set `-C link-self-contained` on or off, the historical values.
    /// Used for compatibility with the existing opt-in and target inference.
    pub explicitly_set: Option<bool>,

    /// The components that are enabled on the CLI, using the `+component` syntax or one of the
    /// `true` shortcuts.
    enabled_components: LinkSelfContainedComponents,

    /// The components that are disabled on the CLI, using the `-component` syntax or one of the
    /// `false` shortcuts.
    disabled_components: LinkSelfContainedComponents,
}

impl LinkSelfContained {
    /// Turns all components on or off and records that this was done explicitly for compatibility
    /// purposes.
    pub(crate) fn set_all_explicitly(&mut self, enabled: bool) {
        self.explicitly_set = Some(enabled);

        if enabled {
            self.enabled_components = LinkSelfContainedComponents::all();
            self.disabled_components = LinkSelfContainedComponents::empty();
        } else {
            self.enabled_components = LinkSelfContainedComponents::empty();
            self.disabled_components = LinkSelfContainedComponents::all();
        }
    }

    /// Helper creating a fully enabled `LinkSelfContained` instance. Used in tests.
    pub fn on() -> Self {
        let mut on = LinkSelfContained::default();
        on.set_all_explicitly(true);
        on
    }

    /// Returns whether the self-contained linker component was enabled on the CLI, using the
    /// `-C link-self-contained=+linker` syntax, or one of the `true` shortcuts.
    pub fn is_linker_enabled(&self) -> bool {
        self.enabled_components.contains(LinkSelfContainedComponents::LINKER)
    }

    /// Returns whether the self-contained linker component was disabled on the CLI, using the
    /// `-C link-self-contained=-linker` syntax, or one of the `false` shortcuts.
    pub fn is_linker_disabled(&self) -> bool {
        self.disabled_components.contains(LinkSelfContainedComponents::LINKER)
    }

}

/// The different values that `-C linker-features` can take on the CLI: a list of individually
/// enabled or disabled features used during linking.
///
/// There is no need to enable or disable them in bulk. Each feature is fine-grained, and can be
/// used to turn `LinkerFeatures` on or off, without needing to change the linker flavor:
/// - using the system lld, or the self-contained `rust-lld` linker
/// - using a C/C++ compiler to drive the linker (not yet exposed on the CLI)
/// - etc.
#[derive(Default, Copy, Clone, PartialEq, Debug)]
pub struct LinkerFeaturesCli {
    /// The linker features that are enabled on the CLI, using the `+feature` syntax.
    pub enabled: LinkerFeatures,

    /// The linker features that are disabled on the CLI, using the `-feature` syntax.
    pub disabled: LinkerFeatures,
}

/// Used with `-Z assert-incr-state`.
#[derive(Clone, Copy, PartialEq, Hash, Debug)]
pub enum IncrementalStateAssertion {
    /// Found and loaded an existing session directory.
    ///
    /// Note that this says nothing about whether any particular query
    /// will be found to be red or green.
    Loaded,
    /// Did not load an existing session directory.
    NotLoaded,
}

/// The different settings that can be enabled via the `-Z location-detail` flag.
#[derive(Copy, Clone, PartialEq, Hash, Debug)]
pub struct LocationDetail {
    pub file: bool,
    pub line: bool,
    pub column: bool,
}

impl LocationDetail {
    pub(crate) fn all() -> Self {
        Self { file: true, line: true, column: true }
    }
}

/// Values for the `-Z fmt-debug` flag.
#[derive(Copy, Clone, PartialEq, Hash, Debug)]
pub enum FmtDebug {
    /// Derive fully-featured implementation
    Full,
    /// Print only type name, without fields
    Shallow,
    /// `#[derive(Debug)]` and `{:?}` are no-ops
    None,
}

impl FmtDebug {
    pub(crate) fn all() -> [Symbol; 3] {
        [sym::full, sym::none, sym::shallow]
    }
}

#[derive(Clone, PartialEq, Hash, Debug, Encodable, Decodable)]
pub enum SwitchWithOptPath {
    Enabled(Option<PathBuf>),
    Disabled,
}

impl SwitchWithOptPath {
    pub fn enabled(&self) -> bool {
        match *self {
            SwitchWithOptPath::Enabled(_) => true,
            SwitchWithOptPath::Disabled => false,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, StableHash)]
#[derive(Encodable, BlobDecodable)]
pub enum SymbolManglingVersion {
    Legacy,
    V0,
    Hashed,
}

#[derive(Clone, Copy, Debug, PartialEq, Hash)]
pub enum DebugInfo {
    None,
    LineDirectivesOnly,
    LineTablesOnly,
    Limited,
    Full,
}

#[derive(Clone, Copy, Debug, PartialEq, Hash)]
pub enum DebugInfoCompression {
    None,
    Zlib,
    Zstd,
}

#[derive(Clone, Copy, Debug, PartialEq, Hash)]
pub enum MirStripDebugInfo {
    None,
    LocalsInTinyFunctions,
    AllLocals,
}

/// Split debug-information is enabled by `-C split-debuginfo`, this enum is only used if split
/// debug-information is enabled (in either `Packed` or `Unpacked` modes), and the platform
/// uses DWARF for debug-information.
///
/// Some debug-information requires link-time relocation and some does not. LLVM can partition
/// the debuginfo into sections depending on whether or not it requires link-time relocation. Split
/// DWARF provides a mechanism which allows the linker to skip the sections which don't require
/// link-time relocation - either by putting those sections in DWARF object files, or by keeping
/// them in the object file in such a way that the linker will skip them.
#[derive(Clone, Copy, Debug, PartialEq, Hash, Encodable, Decodable)]
pub enum SplitDwarfKind {
    /// Sections which do not require relocation are written into object file but ignored by the
    /// linker.
    Single,
    /// Sections which do not require relocation are written into a DWARF object (`.dwo`) file
    /// which is ignored by the linker.
    Split,
}

impl FromStr for SplitDwarfKind {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, ()> {
        Ok(match s {
            "single" => SplitDwarfKind::Single,
            "split" => SplitDwarfKind::Split,
            _ => return Err(()),
        })
    }
}

macro_rules! define_output_types {
    (
        $(
            $(#[doc = $doc:expr])*
            $Variant:ident => {
                shorthand: $shorthand:expr,
                extension: $extension:expr,
                description: $description:expr,
                default_filename: $default_filename:expr,
                is_text: $is_text:expr,
                compatible_with_cgus_and_single_output: $compatible:expr
            }
        ),* $(,)?
    ) => {
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord, StableHash)]
        #[derive(Encodable, Decodable)]
        pub enum OutputType {
            $(
                $(#[doc = $doc])*
                $Variant,
            )*
        }

        impl StableOrd for OutputType {
            const CAN_USE_UNSTABLE_SORT: bool = true;

            // Trivial C-Style enums have a stable sort order across compilation sessions.
            const THIS_IMPLEMENTATION_HAS_BEEN_TRIPLE_CHECKED: () = ();
        }

        impl OutputType {
            pub fn iter_all() -> impl Iterator<Item = OutputType> {
                static ALL_VARIANTS: &[OutputType] = &[
                    $(
                        OutputType::$Variant,
                    )*
                ];
                ALL_VARIANTS.iter().copied()
            }

            pub fn shorthand(&self) -> &'static str {
                match *self {
                    $(
                        OutputType::$Variant => $shorthand,
                    )*
                }
            }

            pub fn extension(&self) -> &'static str {
                match *self {
                    $(
                        OutputType::$Variant => $extension,
                    )*
                }
            }

            pub fn is_text_output(&self) -> bool {
                match *self {
                    $(
                        OutputType::$Variant => $is_text,
                    )*
                }
            }

            pub fn description(&self) -> &'static str {
                match *self {
                    $(
                        OutputType::$Variant => $description,
                    )*
                }
            }

            pub fn default_filename(&self) -> &'static str {
                match *self {
                    $(
                        OutputType::$Variant => $default_filename,
                    )*
                }
            }


        }
    }
}

define_output_types! {
    Assembly => {
        shorthand: "asm",
        extension: "s",
        description: "Generates a file with the crate's assembly code",
        default_filename: "CRATE_NAME.s",
        is_text: true,
        compatible_with_cgus_and_single_output: false
    },
    #[doc = "This is the optimized bitcode, which could be either pre-LTO or non-LTO bitcode,"]
    #[doc = "depending on the specific request type."]
    Bitcode => {
        shorthand: "llvm-bc",
        extension: "bc",
        description: "Generates a binary file containing the LLVM bitcode",
        default_filename: "CRATE_NAME.bc",
        is_text: false,
        compatible_with_cgus_and_single_output: false
    },
    DepInfo => {
        shorthand: "dep-info",
        extension: "d",
        description: "Generates a file with Makefile syntax that indicates all the source files that were loaded to generate the crate",
        default_filename: "CRATE_NAME.d",
        is_text: true,
        compatible_with_cgus_and_single_output: true
    },
    Exe => {
        shorthand: "link",
        extension: "",
        description: "Generates the crates specified by --crate-type. This is the default if --emit is not specified",
        default_filename: "(platform and crate-type dependent)",
        is_text: false,
        compatible_with_cgus_and_single_output: true
    },
    LlvmAssembly => {
        shorthand: "llvm-ir",
        extension: "ll",
        description: "Generates a file containing LLVM IR",
        default_filename: "CRATE_NAME.ll",
        is_text: true,
        compatible_with_cgus_and_single_output: false
    },
    Metadata => {
        shorthand: "metadata",
        extension: "rmeta",
        description: "Generates a file containing metadata about the crate",
        default_filename: "libCRATE_NAME.rmeta",
        is_text: false,
        compatible_with_cgus_and_single_output: true
    },
    Mir => {
        shorthand: "mir",
        extension: "mir",
        description: "Generates a file containing rustc's mid-level intermediate representation",
        default_filename: "CRATE_NAME.mir",
        is_text: true,
        compatible_with_cgus_and_single_output: false
    },
    Object => {
        shorthand: "obj",
        extension: "o",
        description: "Generates a native object file",
        default_filename: "CRATE_NAME.o",
        is_text: false,
        compatible_with_cgus_and_single_output: false
    },
    #[doc = "This is the summary or index data part of the ThinLTO bitcode."]
    ThinLinkBitcode => {
        shorthand: "thin-link-bitcode",
        extension: "indexing.o",
        description: "Generates the ThinLTO summary as bitcode",
        default_filename: "CRATE_NAME.indexing.o",
        is_text: false,
        compatible_with_cgus_and_single_output: false
    },
}

/// The type of diagnostics output to generate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorOutputType {
    /// Output meant for the consumption of humans.
    HumanReadable { kind: HumanReadableErrorType, color_config: ColorConfig },
    /// Output that's consumed by other tools such as `rustfix` or the `RLS`.
    Json {
        /// Render the JSON in a human readable way (with indents and newlines).
        pretty: bool,
        /// The JSON output includes a `rendered` field that includes the rendered
        /// human output.
        json_rendered: HumanReadableErrorType,
        color_config: ColorConfig,
    },
}

// Written by hand because stable `#[derive(Default)]` accepts only a unit variant as the
// default, and stable Rust has no field default values to fill `HumanReadable` from.
impl Default for ErrorOutputType {
    fn default() -> Self {
        ErrorOutputType::HumanReadable {
            kind: HumanReadableErrorType { short: false, unicode: false },
            color_config: ColorConfig::Auto,
        }
    }
}

#[derive(Clone, Hash, Debug)]
pub enum ResolveDocLinks {
    /// Do not resolve doc links.
    None,
    /// Resolve doc links on exported items only for crate types that have metadata.
    ExportedMetadata,
    /// Resolve doc links on exported items.
    Exported,
    /// Resolve doc links on all items.
    All,
}

/// Use tree-based collections to cheaply get a deterministic `Hash` implementation.
/// *Do not* switch `BTreeMap` out for an unsorted container type! That would break
/// dependency tracking for command-line arguments. Also only hash keys, since tracking
/// should only depend on the output types, not the paths they're written to.
#[derive(Clone, Debug, Hash, StableHash, Encodable, Decodable)]
pub struct OutputTypes(BTreeMap<OutputType, Option<OutFileName>>);

impl OutputTypes {
    pub fn new(entries: &[(OutputType, Option<OutFileName>)]) -> OutputTypes {
        OutputTypes(BTreeMap::from_iter(entries.iter().map(|&(k, ref v)| (k, v.clone()))))
    }

    pub(crate) fn get(&self, key: &OutputType) -> Option<&Option<OutFileName>> {
        self.0.get(key)
    }

    pub fn contains_key(&self, key: &OutputType) -> bool {
        self.0.contains_key(key)
    }

    /// Returns `true` if user specified a name and not just produced type
    pub fn contains_explicit_name(&self, key: &OutputType) -> bool {
        matches!(self.0.get(key), Some(Some(..)))
    }

    pub fn iter(&self) -> BTreeMapIter<'_, OutputType, Option<OutFileName>> {
        self.0.iter()
    }

    pub fn keys(&self) -> BTreeMapKeysIter<'_, OutputType, Option<OutFileName>> {
        self.0.keys()
    }

    pub fn values(&self) -> BTreeMapValuesIter<'_, OutputType, Option<OutFileName>> {
        self.0.values()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns `true` if any of the output types require codegen or linking.
    pub fn should_codegen(&self) -> bool {
        self.0.keys().any(|k| match *k {
            OutputType::Bitcode
            | OutputType::ThinLinkBitcode
            | OutputType::Assembly
            | OutputType::LlvmAssembly
            | OutputType::Mir
            | OutputType::Object
            | OutputType::Exe => true,
            OutputType::Metadata | OutputType::DepInfo => false,
        })
    }

    /// Returns `true` if any of the output types require linking.
    pub fn should_link(&self) -> bool {
        self.0.keys().any(|k| match *k {
            OutputType::Bitcode
            | OutputType::ThinLinkBitcode
            | OutputType::Assembly
            | OutputType::LlvmAssembly
            | OutputType::Mir
            | OutputType::Metadata
            | OutputType::Object
            | OutputType::DepInfo => false,
            OutputType::Exe => true,
        })
    }
}

/// Use tree-based collections to cheaply get a deterministic `Hash` implementation.
/// *Do not* switch `BTreeMap` or `BTreeSet` out for an unsorted container type! That
/// would break dependency tracking for command-line arguments.
#[derive(Clone)]
pub struct Externs(BTreeMap<String, ExternEntry>);

#[derive(Clone, Debug)]
pub struct ExternEntry {
    pub location: ExternLocation,
    /// Indicates this is a "private" dependency for the
    /// `exported_private_dependencies` lint.
    ///
    /// This can be set with the `priv` option like
    /// `--extern priv:name=foo.rlib`.
    pub is_private_dep: bool,
    /// Add the extern entry to the extern prelude.
    ///
    /// This can be disabled with the `noprelude` option like
    /// `--extern noprelude:name`.
    pub add_prelude: bool,
    /// The extern entry shouldn't be considered for unused dependency warnings.
    ///
    /// `--extern nounused:std=/path/to/lib/libstd.rlib`. This is used to
    /// suppress `unused-crate-dependencies` warnings.
    pub nounused_dep: bool,
    /// If the extern entry is not referenced in the crate, force it to be resolved anyway.
    ///
    /// Allows a dependency satisfying, for instance, a missing panic handler to be injected
    /// without modifying source:
    /// `--extern force:extras=/path/to/lib/libstd.rlib`
    pub force: bool,
}

#[derive(Clone, Debug)]
pub enum ExternLocation {
    /// Indicates to look for the library in the search paths.
    ///
    /// Added via `--extern name`.
    FoundInLibrarySearchDirectories,
    /// The locations where this extern entry must be found.
    ///
    /// The `CrateLoader` is responsible for loading these and figuring out
    /// which one to use.
    ///
    /// Added via `--extern prelude_name=some_file.rlib`
    ExactPaths(BTreeSet<CanonicalizedPath>),
}

impl Externs {
    /// Used for testing.
    pub fn new(data: BTreeMap<String, ExternEntry>) -> Externs {
        Externs(data)
    }

    pub fn get(&self, key: &str) -> Option<&ExternEntry> {
        self.0.get(key)
    }

    pub fn iter(&self) -> BTreeMapIter<'_, String, ExternEntry> {
        self.0.iter()
    }
}

impl ExternEntry {
    pub fn files(&self) -> Option<impl Iterator<Item = &CanonicalizedPath>> {
        match &self.location {
            ExternLocation::ExactPaths(set) => Some(set.iter()),
            _ => None,
        }
    }
}

#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq)]
pub struct NextSolverConfig {
    /// Whether the new trait solver should be enabled in coherence.
    pub coherence: bool,
    /// Whether the new trait solver should be enabled everywhere.
    /// This is only `true` if `coherence` is also enabled.
    pub globally: bool,
}

// FIXME(#160895): Using -Znext-solver as default on nightly
// See https://github.com/rust-lang/compiler-team/issues/1014
impl Default for NextSolverConfig {
    fn default() -> Self {
        if option_env!("CFG_DEFAULT_NEXT_SOLVER_GLOBALLY").is_some() {
            Self { coherence: true, globally: true }
        } else {
            Self { coherence: true, globally: false }
        }
    }
}

#[derive(Clone)]
pub enum Input {
    /// Load source code from a file.
    File(PathBuf),
    /// Load source code from a string.
    Str {
        /// A string that is shown in place of a filename.
        name: FileName,
        /// An anonymous string containing the source code. Shared with the `SourceFile` it
        /// becomes, so the text is not copied between the caller and the source map.
        input: alloc::sync::Arc<String>,
    },
}

impl Input {
    pub fn filestem(&self) -> &str {
        if let Input::File(ifile) = self {
            // If for some reason getting the file stem as a UTF-8 string fails,
            // then fallback to a fixed name.
            if let Some(name) = ifile.file_stem().and_then(Path::to_str) {
                return name;
            }
        }
        "rust_out"
    }

    pub fn file_name(&self, session: &Session) -> FileName {
        match *self {
            Input::File(ref ifile) => FileName::Real(
                session
                    .psess
                    .source_map()
                    .path_mapping()
                    .to_real_filename(session.psess.source_map().working_dir(), ifile.as_path()),
            ),
            Input::Str { ref name, .. } => name.clone(),
        }
    }

    pub fn opt_path(&self) -> Option<&Path> {
        match self {
            Input::File(file) => Some(file),
            Input::Str { name, .. } => match name {
                FileName::Real(real) => real.local_path(),
                FileName::CfgSpec(_) => None,
                FileName::Anon(_) => None,
                FileName::MacroExpansion(_) => None,
                FileName::ProcMacroSourceCode(_) => None,
                FileName::CliCrateAttr(_) => None,
                FileName::Custom(_) => None,
                FileName::DocTest(path, _) => Some(path),
                FileName::InlineAsm(_) => None,
            },
        }
    }
}

#[derive(Clone, Hash, Debug, StableHash, PartialEq, Eq, Encodable, Decodable)]
pub enum OutFileName {
    Real(PathBuf),
    Stdout,
}

impl OutFileName {
    pub fn parent(&self) -> Option<&Path> {
        match *self {
            OutFileName::Real(ref path) => path.parent(),
            OutFileName::Stdout => None,
        }
    }

    pub fn filestem(&self) -> Option<&Path> {
        match *self {
            OutFileName::Real(ref path) => path.file_stem(),
            OutFileName::Stdout => Some(Path::new("stdout")),
        }
    }

    pub fn is_stdout(&self) -> bool {
        match *self {
            OutFileName::Real(_) => false,
            OutFileName::Stdout => true,
        }
    }

    pub fn is_tty(&self) -> bool {
        match *self {
            OutFileName::Real(_) => false,
            OutFileName::Stdout => eko::file::stdout().is_terminal(),
        }
    }

    pub fn as_path(&self) -> &Path {
        match *self {
            OutFileName::Real(ref path) => path.as_ref(),
            OutFileName::Stdout => Path::new("stdout"),
        }
    }

    /// For a given output filename, return the actual name of the file that
    /// can be used to write codegen data of type `flavor`. For real-path
    /// output filenames, this would be trivial as we can just use the path.
    /// Otherwise for stdout, return a temporary path so that the codegen data
    /// may be later copied to stdout.
    pub fn file_for_writing(
        &self,
        outputs: &OutputFilenames,
        flavor: OutputType,
        codegen_unit_name: &str,
    ) -> PathBuf {
        match *self {
            OutFileName::Real(ref path) => path.clone(),
            OutFileName::Stdout => outputs.temp_path_for_cgu(flavor, codegen_unit_name),
        }
    }

    pub fn overwrite(&self, content: &str, sess: &Session) {
        match self {
            OutFileName::Stdout => eko::print!("{content}"),
            OutFileName::Real(path) => {
                if let Err(e) = eko::file::write(path, content.as_bytes()) {
                    sess.dcx().emit_fatal(FileWriteFail { path, err: e.to_string() });
                }
            }
        }
    }
}

#[derive(Clone, Hash, Debug, StableHash, Encodable, Decodable)]
pub struct OutputFilenames {
    pub(crate) out_directory: PathBuf,
    /// Crate name. Never contains '-'.
    crate_stem: String,
    /// Typically based on `.rs` input file name. Any '-' is preserved.
    filestem: String,
    pub single_output_file: Option<OutFileName>,
    temps_directory: Option<PathBuf>,

    /// A random string generated per invocation of rustc.
    ///
    /// This is prepended to all temporary files so that they do not collide
    /// during concurrent invocations of rustc, or past invocations that were
    /// preserved with a flag like `-C save-temps`, since these files may be
    /// hard linked.
    // This does not affect incr comp outputs, only where temp files are stored.
    #[stable_hash(ignore)]
    invocation_temp: Option<String>,

    explicit_dwo_out_directory: Option<PathBuf>,
    pub outputs: OutputTypes,
}

pub const RLINK_EXT: &str = "rlink";
pub const RUST_CGU_EXT: &str = "rcgu";
pub const DWARF_OBJECT_EXT: &str = "dwo";
pub const MAX_FILENAME_LENGTH: usize = 143; // ecryptfs limits filenames to 143 bytes see #49914

/// Ensure the filename is not too long, as some filesystems have a limit.
/// If the filename is too long, hash part of it and append the hash to the filename.
/// This is a workaround for long crate names generating overly long filenames.
fn maybe_strip_file_name(mut path: PathBuf) -> PathBuf {
    if path.file_name().map_or(0, |name| name.len()) > MAX_FILENAME_LENGTH {
        let filename = path.file_name().unwrap().to_string_lossy();
        let hash_len = 64 / 4; // Hash64 is 64 bits encoded in hex
        let hyphen_len = 1; // the '-' we insert between hash and suffix

        // number of bytes of suffix we can keep so that "hash-<suffix>" fits
        let allowed_suffix = MAX_FILENAME_LENGTH.saturating_sub(hash_len + hyphen_len);

        // number of bytes to remove from the start
        let stripped_bytes = filename.len().saturating_sub(allowed_suffix);

        // ensure we don't cut in a middle of a char
        let split_at = filename.ceil_char_boundary(stripped_bytes);

        let mut hasher = StableHasher::new();
        filename[..split_at].hash(&mut hasher);
        let hash = hasher.finish::<Hash64>();

        path.set_file_name(format!("{:x}-{}", hash, &filename[split_at..]));
    }
    path
}
impl OutputFilenames {
    pub fn new(
        out_directory: PathBuf,
        out_crate_name: String,
        out_filestem: String,
        single_output_file: Option<OutFileName>,
        temps_directory: Option<PathBuf>,
        invocation_temp: Option<String>,
        explicit_dwo_out_directory: Option<PathBuf>,
        extra: String,
        outputs: OutputTypes,
    ) -> Self {
        OutputFilenames {
            out_directory,
            single_output_file,
            temps_directory,
            invocation_temp,
            explicit_dwo_out_directory,
            outputs,
            crate_stem: format!("{out_crate_name}{extra}"),
            filestem: format!("{out_filestem}{extra}"),
        }
    }

    pub fn path(&self, flavor: OutputType) -> OutFileName {
        self.outputs
            .get(&flavor)
            .and_then(|p| p.to_owned())
            .or_else(|| self.single_output_file.clone())
            .unwrap_or_else(|| OutFileName::Real(self.output_path(flavor)))
    }

    pub fn interface_path(&self) -> PathBuf {
        debug!("using crate_name={} for interface_path", self.crate_stem);
        self.out_directory.join(format!("lib{}.rs", self.crate_stem))
    }

    /// Gets the output path where a compilation artifact of the given type
    /// should be placed on disk.
    fn output_path(&self, flavor: OutputType) -> PathBuf {
        let extension = flavor.extension();
        match flavor {
            OutputType::Metadata => {
                debug!("using crate_name={} for {extension}", self.crate_stem);
                self.out_directory.join(format!("lib{}.{}", self.crate_stem, extension))
            }
            _ => self.with_directory_and_extension(&self.out_directory, extension),
        }
    }

    /// Gets the path where a compilation artifact of the given type for the
    /// given codegen unit should be placed on disk. If codegen_unit_name is
    /// None, a path distinct from those of any codegen unit will be generated.
    pub fn temp_path_for_cgu(&self, flavor: OutputType, codegen_unit_name: &str) -> PathBuf {
        let extension = flavor.extension();
        self.temp_path_ext_for_cgu(extension, codegen_unit_name)
    }

    /// Like `temp_path`, but specifically for dwarf objects.
    pub fn temp_path_dwo_for_cgu(&self, codegen_unit_name: &str) -> PathBuf {
        let p = self.temp_path_ext_for_cgu(DWARF_OBJECT_EXT, codegen_unit_name);
        if let Some(dwo_out) = &self.explicit_dwo_out_directory {
            let mut o = dwo_out.clone();
            o.push(p.file_name().unwrap());
            o
        } else {
            p
        }
    }

    /// Like `temp_path`, but also supports things where there is no corresponding
    /// OutputType, like noopt-bitcode or lto-bitcode.
    pub fn temp_path_ext_for_cgu(&self, ext: &str, codegen_unit_name: &str) -> PathBuf {
        let mut extension = codegen_unit_name.to_string();

        // Append `.{invocation_temp}` to ensure temporary files are unique.
        if let Some(rng) = &self.invocation_temp {
            extension.push('.');
            extension.push_str(rng);
        }

        // FIXME: This is sketchy that we're not appending `.rcgu` when the ext is empty.
        // Append `.rcgu.{ext}`.
        if !ext.is_empty() {
            extension.push('.');
            extension.push_str(RUST_CGU_EXT);
            extension.push('.');
            extension.push_str(ext);
        }

        let temps_directory = self.temps_directory.as_ref().unwrap_or(&self.out_directory);
        maybe_strip_file_name(self.with_directory_and_extension(temps_directory, &extension))
    }

    pub fn temp_path_for_diagnostic(&self, ext: &str) -> PathBuf {
        let temps_directory = self.temps_directory.as_ref().unwrap_or(&self.out_directory);
        self.with_directory_and_extension(temps_directory, &ext)
    }

    pub fn with_extension(&self, extension: &str) -> PathBuf {
        self.with_directory_and_extension(&self.out_directory, extension)
    }

    pub fn with_directory_and_extension(&self, directory: &Path, extension: &str) -> PathBuf {
        debug!("using filestem={} for {extension}", self.filestem);
        let mut path = directory.join(&self.filestem);
        path.set_extension(extension);
        path
    }

    /// Returns the path for the Split DWARF file - this can differ depending on which Split DWARF
    /// mode is being used, which is the logic that this function is intended to encapsulate.
    pub fn split_dwarf_path(
        &self,
        split_debuginfo_kind: SplitDebuginfo,
        split_dwarf_kind: SplitDwarfKind,
        cgu_name: &str,
    ) -> Option<PathBuf> {
        let obj_out = self.temp_path_for_cgu(OutputType::Object, cgu_name);
        let dwo_out = self.temp_path_dwo_for_cgu(cgu_name);
        match (split_debuginfo_kind, split_dwarf_kind) {
            (SplitDebuginfo::Off, SplitDwarfKind::Single | SplitDwarfKind::Split) => None,
            // Single mode doesn't change how DWARF is emitted, but does add Split DWARF attributes
            // (pointing at the path which is being determined here). Use the path to the current
            // object file.
            (SplitDebuginfo::Packed | SplitDebuginfo::Unpacked, SplitDwarfKind::Single) => {
                Some(obj_out)
            }
            // Split mode emits the DWARF into a different file, use that path.
            (SplitDebuginfo::Packed | SplitDebuginfo::Unpacked, SplitDwarfKind::Split) => {
                Some(dwo_out)
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct Sysroot {
    pub explicit: Option<PathBuf>,
    pub default: PathBuf,
}

impl Sysroot {
    pub fn new(explicit: Option<PathBuf>) -> Sysroot {
        Sysroot { explicit, default: filesearch::default_sysroot() }
    }

    /// Return explicit sysroot if it was passed with `--sysroot`, or default sysroot otherwise.
    pub fn path(&self) -> &Path {
        self.explicit.as_deref().unwrap_or(&self.default)
    }

    /// Returns both explicit sysroot if it was passed with `--sysroot` and the default sysroot.
    pub fn all_paths(&self) -> impl Iterator<Item = &Path> {
        self.explicit.as_deref().into_iter().chain(iter::once(&*self.default))
    }
}

pub fn host_tuple() -> &'static str {
    // Get the host triple out of the build environment. This ensures that our
    // idea of the host triple is the same as for the set of libraries we've
    // actually built. We can't just take LLVM's host triple because they
    // normalize all ix86 architectures to i386.
    //
    // Instead of grabbing the host triple (for the current host), we grab (at
    // compile time) the target triple that this rustc is built with and
    // calling that (at runtime) the host triple.
    (option_env!("CFG_COMPILER_HOST_TRIPLE")).expect("CFG_COMPILER_HOST_TRIPLE")
}

fn file_path_mapping(
    remap_path_prefix: Vec<(PathBuf, PathBuf)>,
    remap_cwd_prefix: Option<&Path>,
    remap_path_scope: RemapPathScopeComponents,
) -> FilePathMapping {
    // Apply `-Zremap-cwd-prefix` here rather than in `parse_remap_path_prefix`, so the
    // absolute cwd is never stored in the tracked `remap_path_prefix` option (#132132).
    let cwd_remap = if let Some(to) = remap_cwd_prefix
        && let Some(cwd) = eko::env::current_dir().map(PathBuf::from_bytes)
    {
        Some((cwd, to.to_path_buf()))
    } else {
        None
    };
    // The cwd remapping is appended last: `map_prefix` tries entries in reverse order, so this
    // keeps `-Zremap-cwd-prefix` taking precedence over `--remap-path-prefix`, as documented.
    FilePathMapping::new(remap_path_prefix.into_iter().chain(cwd_remap).collect(), remap_path_scope)
}

impl Default for Options {
    fn default() -> Options {
        let unstable_opts = UnstableOptions::default();

        // FIXME(Urgau): This is a hack that ideally shouldn't exist, but rustdoc
        // currently uses this `Default` implementation, so we have no choice but
        // to create a default working directory.
        let working_dir = {
            let working_dir = PathBuf::from_bytes(
                eko::env::current_dir().expect("the process has a working directory"),
            );
            let file_mapping =
                file_path_mapping(Vec::new(), None, RemapPathScopeComponents::empty());
            file_mapping.to_real_filename(&RealFileName::empty(), &working_dir)
        };

        Options {
            crate_types: Vec::new(),
            optimize: OptLevel::No,
            debuginfo: DebugInfo::None,
            lint_opts: Vec::new(),
            lint_cap: None,
            describe_lints: false,
            output_types: OutputTypes(BTreeMap::new()),
            search_paths: vec![],
            sysroot: Sysroot::new(None),
            target_triple: TargetTuple::from_tuple(host_tuple()),
            test: false,
            incremental: None,
            unstable_opts,
            cg: Default::default(),
            error_format: ErrorOutputType::default(),
            diagnostic_width: None,
            externs: Externs(BTreeMap::new()),
            crate_name: None,
            libs: Vec::new(),
            unstable_features: UnstableFeatures::Disallow,
            debug_assertions: true,
            actually_rustdoc: false,
            resolve_doc_links: ResolveDocLinks::None,
            trimmed_def_paths: false,
            cli_forced_codegen_units: None,
            cli_forced_local_thinlto_off: false,
            remap_path_prefix: Vec::new(),
            remap_path_scope: RemapPathScopeComponents::all(),
            real_rust_source_base_dir: None,
            real_rustc_dev_source_base_dir: None,
            edition: DEFAULT_EDITION,
            json_artifact_notifications: false,
            json_timings: false,
            json_unused_externs: JsonUnusedExterns::No,
            json_future_incompat: false,
            pretty: None,
            working_dir,
            color: ColorConfig::Auto,
            logical_env: FxIndexMap::default(),
            verbose: false,
            target_modifiers: BTreeMap::default(),
            mitigation_coverage_map: Default::default(),
            jobs: Jobs { frontend: None, backend: None, linker: LinkerJobs::Default },
        }
    }
}

impl Options {
    /// Returns `true` if there is a reason to build the dep graph.
    pub fn build_dep_graph(&self) -> bool {
        self.incremental.is_some()
            || self.unstable_opts.dump_dep_graph
            || self.unstable_opts.query_dep_graph
    }

    pub fn file_path_mapping(&self) -> FilePathMapping {
        file_path_mapping(
            self.remap_path_prefix.clone(),
            self.unstable_opts.remap_cwd_prefix.as_deref(),
            self.remap_path_scope,
        )
    }

    /// Returns `true` if there will be an output file generated.
    pub fn will_create_output_file(&self) -> bool {
        !self.unstable_opts.parse_crate_root_only && // The file is just being parsed
            self.unstable_opts.ls.is_empty() // The file is just being queried
    }

    #[inline]
    pub fn share_generics(&self) -> bool {
        match self.unstable_opts.share_generics {
            Some(setting) => setting,
            None => match self.optimize {
                OptLevel::No | OptLevel::Less | OptLevel::Size | OptLevel::SizeMin => true,
                OptLevel::More | OptLevel::Aggressive => false,
            },
        }
    }

    pub fn get_symbol_mangling_version(&self) -> SymbolManglingVersion {
        self.cg.symbol_mangling_version.unwrap_or(SymbolManglingVersion::V0)
    }

    #[inline]
    pub fn autodiff_enabled(&self) -> bool {
        self.unstable_opts.autodiff.contains(&AutoDiff::Enable)
    }
}

impl UnstableOptions {
    pub fn dcx_flags(&self, can_emit_warnings: bool) -> DiagCtxtFlags {
        DiagCtxtFlags {
            can_emit_warnings,
            treat_err_as_bug: self.treat_err_as_bug,
            eagerly_emit_delayed_bugs: self.eagerly_emit_delayed_bugs,
            macro_backtrace: self.macro_backtrace,
            deduplicate_diagnostics: self.deduplicate_diagnostics,
            track_diagnostics: self.track_diagnostics,
        }
    }

    pub fn src_hash_algorithm(&self, target: &Target) -> SourceFileHashAlgorithm {
        self.src_hash_algorithm.unwrap_or_else(|| {
            if target.is_like_msvc {
                SourceFileHashAlgorithm::Sha256
            } else {
                SourceFileHashAlgorithm::Md5
            }
        })
    }

    pub fn checksum_hash_algorithm(&self) -> Option<SourceFileHashAlgorithm> {
        self.checksum_hash_algorithm
    }
}

// The type of entry function, so users can have their own entry functions
#[derive(Copy, Clone, PartialEq, Hash, Debug, StableHash)]
pub enum EntryFnType {
    Main {
        /// Specifies what to do with `SIGPIPE` before calling `fn main()`.
        ///
        /// What values that are valid and what they mean must be in sync
        /// across rustc and libstd, but we don't want it public in libstd,
        /// so we take a bit of an unusual approach with simple constants
        /// and an `include!()`.
        sigpipe: u8,
    },
}

#[derive(Clone, Hash, Debug, PartialEq, Eq, Encodable, Decodable)]
pub enum Passes {
    Some(Vec<String>),
    All,
}

#[derive(Clone, Copy, Hash, Debug, PartialEq)]
pub enum PAuthKey {
    A,
    B,
}

#[derive(Clone, Copy, Hash, Debug, PartialEq)]
pub struct PacRet {
    pub leaf: bool,
    pub pc: bool,
    pub key: PAuthKey,
}

#[derive(Clone, Copy, Hash, Debug, PartialEq, Default)]
pub struct BranchProtection {
    pub bti: bool,
    pub pac_ret: Option<PacRet>,
    pub gcs: bool,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialOrd, PartialEq)]
pub enum PointerAuthOption {
    // See <compiler/rustc_session/src/options.rs> and Clang's command line reference:
    // <https://clang.llvm.org/docs/ClangCommandLineReference.html#cmdoption-clang-fptrauth-auth-traps>
    // for the origin and meaning of the enum values.
    // tidy-alphabetical-start
    Aarch64JumpTableHardening,
    AuthTraps,
    Calls,
    ElfGot,
    FunctionPointerTypeDiscrimination,
    IndirectGotos,
    InitFini,
    InitFiniAddressDiscrimination,
    Intrinsics,
    ReturnAddresses,
    TypeInfoVTPtrDisc,
    VTPtrAddrDisc,
    VTPtrTypeDisc,
    // tidy-alphabetical-end
}
impl PointerAuthOption {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "aarch64-jump-table-hardening" => Some(Self::Aarch64JumpTableHardening),
            "auth-traps" => Some(Self::AuthTraps),
            "calls" => Some(Self::Calls),
            "elf-got" => Some(Self::ElfGot),
            "function-pointer-type-discrimination" => Some(Self::FunctionPointerTypeDiscrimination),
            "indirect-gotos" => Some(Self::IndirectGotos),
            "init-fini" => Some(Self::InitFini),
            "init-fini-address-discrimination" => Some(Self::InitFiniAddressDiscrimination),
            "intrinsics" => Some(Self::Intrinsics),
            "return-addresses" => Some(Self::ReturnAddresses),
            "typeinfo-vt-ptr-discrimination" => Some(Self::TypeInfoVTPtrDisc),
            "vt-ptr-addr-discrimination" => Some(Self::VTPtrAddrDisc),
            "vt-ptr-type-discrimination" => Some(Self::VTPtrTypeDisc),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
pub enum LinkerJobs {
    /// Do not pass anything to the linker, use it's default behavior.
    Default,
    /// Pass some specific number of jobs to use to the linker.
    Explicit(NonZero<usize>),
}

impl LinkerJobs {
    pub fn limit(self) -> Option<NonZero<usize>> {
        match self {
            LinkerJobs::Default => None,
            LinkerJobs::Explicit(n) => Some(n),
        }
    }
}

/// `None` for frontend and backend means everything is single-threaded
/// and synchronization can be disabled.
#[derive(Clone, Copy)]
pub struct Jobs {
    pub frontend: Option<NonZero<usize>>,
    pub backend: Option<NonZero<usize>>,
    pub linker: LinkerJobs,
}


pub fn build_configuration(sess: &Session, mut user_cfg: Cfg) -> Cfg {
    // First disallow some configuration given on the command line
    cfg::disallow_cfgs(sess, &user_cfg);

    // Then combine the configuration requested by the session (command line) with
    // some default and generated configuration items.
    user_cfg.extend(cfg::default_configuration(sess));
    user_cfg
}

pub fn build_target_config(
    early_dcx: &EarlyDiagCtxt,
    target: &TargetTuple,
    sysroot: &Path,
    unstable_options: bool,
) -> Target {
    match Target::search(target, sysroot, unstable_options) {
        Ok((target, warnings)) => {
            for warning in warnings.warning_messages() {
                early_dcx.early_warn(warning)
            }

            if !matches!(target.pointer_width, 16 | 32 | 64) {
                early_dcx.early_fatal(format!(
                    "target specification was invalid: unrecognized target-pointer-width {}",
                    target.pointer_width
                ))
            }
            target
        }
        Err(e) => {
            let mut err =
                early_dcx.early_struct_fatal(format!("error loading target specification: {e}"));
            err.help("run `rustc --print target-list` for a list of built-in targets");
            let typed = target.tuple();
            let limit = typed.len() / 3 + 1;
            if let Some(suggestion) = crate::rustc_target::spec::TARGETS
                .iter()
                .filter_map(|&t| {
                    crate::rustc_span::edit_distance::edit_distance_with_substrings(typed, t, limit)
                        .map(|d| (d, t))
                })
                .min_by_key(|(d, _)| *d)
                .map(|(_, t)| t)
            {
                err.help(format!("did you mean `{suggestion}`?"));
            }
            err.emit()
        }
    }
}

/// Report unused externs in event stream
#[derive(Copy, Clone)]
pub enum JsonUnusedExterns {
    /// Do not
    No,
    /// Report, but do not exit with failure status for deny/forbid
    Silent,
    /// Report, and also exit with failure status for deny/forbid
    Loud,
}

impl JsonUnusedExterns {
    pub fn is_enabled(&self) -> bool {
        match self {
            JsonUnusedExterns::No => false,
            JsonUnusedExterns::Loud | JsonUnusedExterns::Silent => true,
        }
    }

    pub fn is_loud(&self) -> bool {
        match self {
            JsonUnusedExterns::No | JsonUnusedExterns::Silent => false,
            JsonUnusedExterns::Loud => true,
        }
    }
}


#[derive(Copy, Clone, PartialEq, Debug)]
pub enum PpSourceMode {
    /// `-Zunpretty=normal`
    Normal,
    /// `-Zunpretty=expanded`
    Expanded,
    /// `-Zunpretty=expanded,identified`
    ExpandedIdentified,
    /// `-Zunpretty=expanded,hygiene`
    ExpandedHygiene,
}

#[derive(Copy, Clone, PartialEq, Debug)]
pub enum PpHirMode {
    /// `-Zunpretty=hir`
    Normal,
    /// `-Zunpretty=hir,identified`
    Identified,
    /// `-Zunpretty=hir,typed`
    Typed,
}

#[derive(Copy, Clone, PartialEq, Debug)]
/// Pretty print mode
pub enum PpMode {
    /// Options that print the source code, i.e.
    /// `-Zunpretty=normal` and `-Zunpretty=expanded`
    Source(PpSourceMode),
    /// `-Zunpretty=ast-tree`
    AstTree,
    /// `-Zunpretty=ast-tree,expanded`
    AstTreeExpanded,
    /// Options that print the HIR, i.e. `-Zunpretty=hir`
    Hir(PpHirMode),
    /// `-Zunpretty=hir-tree`
    HirTree,
    /// `-Zunpretty=thir-tree`
    ThirTree,
    /// `-Zunpretty=thir-flat`
    ThirFlat,
    /// `-Zunpretty=mir`
    Mir,
    /// `-Zunpretty=mir-cfg`
    MirCFG,
    /// `-Zunpretty=stable-mir`
    StableMir,
}

impl PpMode {
    pub fn needs_ast_map(&self) -> bool {
        use PpMode::*;
        use PpSourceMode::*;
        match *self {
            Source(Normal) | AstTree => false,

            Source(Expanded | ExpandedIdentified | ExpandedHygiene)
            | AstTreeExpanded
            | Hir(_)
            | HirTree
            | ThirTree
            | ThirFlat
            | Mir
            | MirCFG
            | StableMir => true,
        }
    }

    pub fn needs_analysis(&self) -> bool {
        use PpMode::*;
        matches!(*self, Hir(PpHirMode::Typed) | Mir | StableMir | MirCFG | ThirTree | ThirFlat)
    }
}

#[derive(Clone, Hash, PartialEq, Eq, Debug)]
pub enum WasiExecModel {
    Command,
    Reactor,
}

/// Command-line arguments passed to the compiler have to be incorporated with
/// the dependency tracking system for incremental compilation. This module
/// provides some utilities to make this more convenient.
///
/// The values of all command-line arguments that are relevant for dependency
/// tracking are hashed into a single value that determines whether the
/// incremental compilation cache can be re-used or not. This hashing is done
/// via the `DepTrackingHash` trait defined below, since the standard `Hash`
/// implementation might not be suitable (e.g., arguments are stored in a `Vec`,
/// the hash of which is order dependent, but we might not want the order of
/// arguments to make a difference for the hash).
///
/// However, since the value provided by `Hash::hash` often *is* suitable,
/// especially for primitive types, there is the
/// `impl_dep_tracking_hash_via_hash!()` macro that allows to simply reuse the
/// `Hash` implementation for `DepTrackingHash`. It's important though that
/// we have an opt-in scheme here, so one is hopefully forced to think about
/// how the hash should be calculated when adding a new command-line argument.
pub(crate) mod dep_tracking {
    use alloc::string::String;
    use alloc::vec::Vec;
    use alloc::collections::BTreeMap;
    use core::hash::Hash;
    use core::num::NonZero;
    use eko::path::PathBuf;

    use crate::rustc_abi::Align;
    use crate::rustc_ast::attr::version::RustcVersion;
    use crate::rustc_data_structures::fx::FxIndexMap;
    use crate::rustc_data_structures::stable_hash::StableHasher;
    use crate::rustc_feature::UnstableFeatures;
    use crate::rustc_hashes::Hash64;
    use crate::rustc_span::edition::Edition;
    use crate::rustc_span::{RealFileName, RemapPathScopeComponents};
    use crate::rustc_structures::CollapseMacroDebuginfo;
    use crate::rustc_target::spec::{
        CodeModel, FramePointer, MergeFunctions, OnBrokenPipe, PanicStrategy, RelocModel,
        RelroLevel, SanitizerSet, SplitDebuginfo, StackProtector, SymbolVisibility, TargetTuple,
        TlsModel,
    };

    use super::{
        AnnotateMoves, AutoDiff, BranchProtection, CFGuard, CFProtection, CodegenRetagOptions,
        CoverageOptions, CrateType, DebugInfo, DebugInfoCompression, ErrorOutputType, FmtDebug,
        FunctionReturn, InliningThreshold, InstrumentCoverage, InstrumentMcount,
        InstrumentMcountOpts, InstrumentXRay, LinkerPluginLto, LocationDetail, LtoCli,
        MirStripDebugInfo, NextSolverConfig, Offload, OptLevel, OutFileName, OutputType,
        OutputTypes, PatchableFunctionEntry, PointerAuthOption, Polonius, ResolveDocLinks,
        SourceFileHashAlgorithm, SplitDwarfKind, SwitchWithOptPath, SymbolManglingVersion,
        WasiExecModel,
    };
    use crate::rustc_session::lint;
    use crate::rustc_session::utils::NativeLib;

    pub(crate) trait DepTrackingHash {
        fn hash(
            &self,
            hasher: &mut StableHasher,
            error_format: ErrorOutputType,
            for_crate_hash: bool,
        );
    }

    macro_rules! impl_dep_tracking_hash_via_hash {
        ($($t:ty),+ $(,)?) => {$(
            impl DepTrackingHash for $t {
                fn hash(&self, hasher: &mut StableHasher, _: ErrorOutputType, _for_crate_hash: bool) {
                    Hash::hash(self, hasher);
                }
            }
        )+};
    }

    impl<T: DepTrackingHash> DepTrackingHash for Option<T> {
        fn hash(
            &self,
            hasher: &mut StableHasher,
            error_format: ErrorOutputType,
            for_crate_hash: bool,
        ) {
            match self {
                Some(x) => {
                    Hash::hash(&1, hasher);
                    DepTrackingHash::hash(x, hasher, error_format, for_crate_hash);
                }
                None => Hash::hash(&0, hasher),
            }
        }
    }

    impl_dep_tracking_hash_via_hash!(
        (),
        AnnotateMoves,
        AutoDiff,
        Offload,
        bool,
        usize,
        NonZero<usize>,
        u64,
        Hash64,
        String,
        PathBuf,
        lint::Level,
        WasiExecModel,
        u32,
        FramePointer,
        RelocModel,
        CodeModel,
        TlsModel,
        InstrumentCoverage,
        CoverageOptions,
        InstrumentMcount,
        InstrumentMcountOpts,
        InstrumentXRay,
        CrateType,
        MergeFunctions,
        OnBrokenPipe,
        PanicStrategy,
        RelroLevel,
        OptLevel,
        LtoCli,
        DebugInfo,
        DebugInfoCompression,
        MirStripDebugInfo,
        CollapseMacroDebuginfo,
        UnstableFeatures,
        NativeLib,
        SanitizerSet,
        CFGuard,
        CFProtection,
        TargetTuple,
        Edition,
        LinkerPluginLto,
        ResolveDocLinks,
        SplitDebuginfo,
        SplitDwarfKind,
        StackProtector,
        SwitchWithOptPath,
        SymbolManglingVersion,
        SymbolVisibility,
        RemapPathScopeComponents,
        SourceFileHashAlgorithm,
        OutFileName,
        OutputType,
        RealFileName,
        LocationDetail,
        FmtDebug,
        BranchProtection,
        NextSolverConfig,
        PatchableFunctionEntry,
        Polonius,
        InliningThreshold,
        FunctionReturn,
        Align,
        CodegenRetagOptions,
        RustcVersion,
        PointerAuthOption,
    );

    impl<T1, T2> DepTrackingHash for (T1, T2)
    where
        T1: DepTrackingHash,
        T2: DepTrackingHash,
    {
        fn hash(
            &self,
            hasher: &mut StableHasher,
            error_format: ErrorOutputType,
            for_crate_hash: bool,
        ) {
            Hash::hash(&0, hasher);
            DepTrackingHash::hash(&self.0, hasher, error_format, for_crate_hash);
            Hash::hash(&1, hasher);
            DepTrackingHash::hash(&self.1, hasher, error_format, for_crate_hash);
        }
    }

    impl<T1, T2, T3> DepTrackingHash for (T1, T2, T3)
    where
        T1: DepTrackingHash,
        T2: DepTrackingHash,
        T3: DepTrackingHash,
    {
        fn hash(
            &self,
            hasher: &mut StableHasher,
            error_format: ErrorOutputType,
            for_crate_hash: bool,
        ) {
            Hash::hash(&0, hasher);
            DepTrackingHash::hash(&self.0, hasher, error_format, for_crate_hash);
            Hash::hash(&1, hasher);
            DepTrackingHash::hash(&self.1, hasher, error_format, for_crate_hash);
            Hash::hash(&2, hasher);
            DepTrackingHash::hash(&self.2, hasher, error_format, for_crate_hash);
        }
    }

    impl<T: DepTrackingHash> DepTrackingHash for Vec<T> {
        fn hash(
            &self,
            hasher: &mut StableHasher,
            error_format: ErrorOutputType,
            for_crate_hash: bool,
        ) {
            Hash::hash(&self.len(), hasher);
            for (index, elem) in self.iter().enumerate() {
                Hash::hash(&index, hasher);
                DepTrackingHash::hash(elem, hasher, error_format, for_crate_hash);
            }
        }
    }

    impl<T: DepTrackingHash, V: DepTrackingHash> DepTrackingHash for FxIndexMap<T, V> {
        fn hash(
            &self,
            hasher: &mut StableHasher,
            error_format: ErrorOutputType,
            for_crate_hash: bool,
        ) {
            Hash::hash(&self.len(), hasher);
            for (key, value) in self.iter() {
                DepTrackingHash::hash(key, hasher, error_format, for_crate_hash);
                DepTrackingHash::hash(value, hasher, error_format, for_crate_hash);
            }
        }
    }

    impl DepTrackingHash for OutputTypes {
        fn hash(
            &self,
            hasher: &mut StableHasher,
            error_format: ErrorOutputType,
            for_crate_hash: bool,
        ) {
            Hash::hash(&self.0.len(), hasher);
            for (key, val) in &self.0 {
                DepTrackingHash::hash(key, hasher, error_format, for_crate_hash);
                if !for_crate_hash {
                    DepTrackingHash::hash(val, hasher, error_format, for_crate_hash);
                }
            }
        }
    }

    // This is a stable hash because BTreeMap is a sorted container
    pub(crate) fn stable_hash(
        sub_hashes: BTreeMap<&'static str, &dyn DepTrackingHash>,
        hasher: &mut StableHasher,
        error_format: ErrorOutputType,
        for_crate_hash: bool,
    ) {
        for (key, sub_hash) in sub_hashes {
            // Using Hash::hash() instead of DepTrackingHash::hash() is fine for
            // the keys, as they are just plain strings
            Hash::hash(&key.len(), hasher);
            Hash::hash(key, hasher);
            sub_hash.hash(hasher, error_format, for_crate_hash);
        }
    }
}

/// How to run proc-macro code when building this crate
#[derive(Clone, Copy, PartialEq, Hash, Debug)]
pub enum ProcMacroExecutionStrategy {
    /// Run the proc-macro code on the same thread as the server.
    SameThread,

    /// Run the proc-macro code on a different thread.
    CrossThread,
}

/// `-Z patchable-function-entry` representation - how many nops to put before and after function
/// entry.
#[derive(Clone, PartialEq, Hash, Debug, Default)]
pub struct PatchableFunctionEntry {
    /// Nops before the entry
    prefix: u8,
    /// Nops after the entry
    entry: u8,
    /// An optional section name to record the entry location
    section: Option<String>,
}

impl PatchableFunctionEntry {
    pub fn from_parts(
        total_nops: u8,
        prefix_nops: u8,
        section: Option<String>,
    ) -> Option<PatchableFunctionEntry> {
        if total_nops < prefix_nops {
            None
        // Section name cannot contain null characters.
        } else if section.as_ref().map(|x| x.contains('\0') || x.is_empty()).unwrap_or(false) {
            None
        } else {
            Some(Self { prefix: prefix_nops, entry: total_nops - prefix_nops, section })
        }
    }
    pub fn prefix(&self) -> u8 {
        self.prefix
    }
    pub fn entry(&self) -> u8 {
        self.entry
    }
    pub fn section(&self) -> Option<&str> {
        self.section.as_ref().map(|x| x.as_str())
    }
}

/// `-Zpolonius` values, enabling the borrow checker polonius analysis, and which version: legacy,
/// or future prototype.
#[derive(Clone, Copy, PartialEq, Hash, Debug)]
pub enum Polonius {
    /// Polonius is disabled, only use NLL.
    Off,

    /// Legacy version, using datalog and the `polonius-engine` crate. Historical value for `-Zpolonius`.
    Legacy,

    /// In-tree prototype, extending the NLL infrastructure.
    Next,
}

impl Default for Polonius {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl Polonius {
    pub(crate) const DEFAULT: Self =
        if option_env!("CFG_DEFAULT_POLONIUS_NEXT").is_some() { Self::Next } else { Self::Off };

    /// Returns whether the legacy version of polonius is enabled
    pub fn is_legacy_enabled(&self) -> bool {
        matches!(self, Polonius::Legacy)
    }

    /// Returns whether the "next" version of polonius is enabled
    pub fn is_next_enabled(&self) -> bool {
        matches!(self, Polonius::Next)
    }
}

#[derive(Clone, Copy, PartialEq, Hash, Debug)]
pub enum InliningThreshold {
    Always,
    Sometimes(usize),
    Never,
}

impl Default for InliningThreshold {
    fn default() -> Self {
        Self::Sometimes(100)
    }
}

/// The different settings that the `-Zfunction-return` flag can have.
#[derive(Clone, Copy, PartialEq, Hash, Debug, Default)]
pub enum FunctionReturn {
    /// Keep the function return unmodified.
    #[default]
    Keep,

    /// Replace returns with jumps to thunk, without emitting the thunk.
    ThunkExtern,
}

/// Whether extra span comments are included when dumping MIR, via the `-Z mir-include-spans` flag.
/// By default, only enabled in the NLL MIR dumps, and disabled in all other passes.
#[derive(Clone, Copy, Default, PartialEq, Debug)]
pub enum MirIncludeSpans {
    Off,
    On,
    /// Default: include extra comments in NLL MIR dumps only. Can be ignored and considered as
    /// `Off` in all other cases.
    #[default]
    Nll,
}

impl MirIncludeSpans {
    /// Unless opting into extra comments for all passes, they can be considered disabled.
    /// The cases where a distinction between on/off and a per-pass value can exist will be handled
    /// in the passes themselves: i.e. the `Nll` value is considered off for all intents and
    /// purposes, except for the NLL MIR dump pass.
    pub fn is_enabled(self) -> bool {
        self == MirIncludeSpans::On
    }
}
