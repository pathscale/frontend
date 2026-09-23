//! Name and scope diagnostics, read off the parsed tree alone.
//!
//! A port of the checks in rust-analyzer's `ide-diagnostics` handlers (commit `e8f7e90aa3`) that
//! can be decided from syntax and lexical scope: no macro expansion, no name resolution pass, no
//! type checking and no standard library. Each diagnostic keeps rust-analyzer's code string,
//! severity and message wording, so an editor or a tool that already understands rust-analyzer's
//! output can take these unchanged.
//!
//! | rust-analyzer handler | code | here |
//! | --- | --- | --- |
//! | `unresolved_ident` | E0425 | single-segment lowercase value paths, when the scope is complete |
//! | `mismatched_arg_count` | E0061, E0057 | in-file free fns, tuple structs and variants, `Type::assoc` paths, closures bound by `let`, and method calls whose receiver type is written out |
//! | `missing_fields` | E0063 | struct literals and patterns of in-file structs and variants |
//! | `no_such_field` | E0560, E0559 | the same literals and patterns |
//! | `missing_match_arms` | E0004 | matches over an in-file enum whose arms are variant paths |
//! | `trait_impl_missing_assoc_item` | E0046 | impls of in-file traits |
//! | `trait_impl_redundant_assoc_item` | E0407 | impls of in-file traits |
//! | `unresolved_import` | E0432 | `crate::`, `self::`, `super::` and local-module paths |
//! | `unresolved_module` | E0583 | not ported, see below |
//!
//! **Zero false positives is the contract.** A parsed tree is not the program: macros have not
//! run, `#[cfg]` has not been evaluated, there is no prelude, and nothing outside the file is
//! known. Every check below therefore reports only when the answer cannot change once those
//! things happen, and silently skips otherwise. The conditions, stated once:
//!
//! - **A scope is complete** when nothing unseen can add a name to it: no glob import, no macro
//!   invocation in item or statement position, no item carrying an attribute that could be a
//!   macro (anything outside the builtin inert set, see [`classify`]), and no `#[derive]` of
//!   anything but the nine derives `core` ships. A name missing from a complete scope is missing.
//!   A name missing from an incomplete one is unknown, and unknown never reports.
//! - **A definition is exact** when neither it nor any enclosing item carries `#[cfg]`, or an
//!   attribute that could rewrite it. Only exact definitions are used to decide an arity, a field
//!   list, a variant list or a trait's items. A `#[cfg]`'d definition still counts as present
//!   for name lookups, because it may well be compiled in.
//! - **Inside `#[cfg]`'d or macro-attributed code nothing is reported**, since that code may never
//!   be compiled or may be rewritten before it is.
//! - **Only single-segment paths are resolved**, plus `Type::Name` where `Type` is an in-file
//!   struct or enum (or `Self` inside an inherent or trait impl of one). Anything qualified through
//!   a module, a crate or a `<T as Trait>` is skipped.
//!
//! **Why `unresolved-module` is not here.** `mod foo;` names a file. This analysis is handed one
//! source string and never sees a file system, so every out-of-line module would be reported.
//! The absence of the file is not a property of the source, and rust-analyzer only decides it by
//! looking at the VFS. Out-of-line modules are indexed as present with unknown contents instead.
//!
//! **Cost.** One visitor pass builds the index into `FxHashMap`s keyed by `Symbol`: item scopes
//! for every module and block, plus imports, definitions, impls and `use` trees. A second pass
//! walks bodies with lexical locals, then each check is a handful of hash lookups. Locals are one
//! map from name to a shadowing stack, with an undo log per scope, so pushing, popping and looking
//! up a name are all constant time; nothing rescans a list per name. A check whose code is not
//! enabled in `cx.opts` costs nothing, and the parts of the index only it needs are not built.
//! A diagnostic's range costs one span lookup.

// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them.
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::rustc_ast::ast;
use crate::rustc_ast::ast::{
    AngleBracketedArg, AssocItem, AssocItemKind, AttrKind, AttrVec, Attribute, BorrowKind, ByRef,
    Crate, Defaultness, EnumDef, Expr, ExprKind, ForeignItem, ForeignItemKind, GenericArg,
    GenericArgs, GenericParamKind, Generics, ImplPolarity, Item, ItemKind, LocalKind, ModKind,
    Mutability, NodeId, Param, Pat, PatField, PatFieldsRest, PatKind, Path, PathSegment, Recovered,
    Stmt, StmtKind, StructRest, Ty, TyKind, UseTree, UseTreeKind, VariantData,
};
use crate::rustc_ast::visit::{self, AssocCtxt, FnCtxt, FnKind, Visitor};
use crate::rustc_data_structures::fx::{FxHashMap, FxHashSet};
use crate::rustc_span::{Ident, Span, Symbol, kw};

use super::{Cx, Diagnostic, Severity};

// ---------------------------------------------------------------------------------------------
// Entry point and code gating
// ---------------------------------------------------------------------------------------------

/// Which checks `cx.opts` asks for. Each is keyed by rust-analyzer's code string (`E0425`) and
/// accepts rust-analyzer's diagnostic name (`unresolved-ident`) as an alias, since callers coming
/// from rust-analyzer's configuration know the checks by that name.
#[derive(Clone, Copy)]
struct Enabled {
    ident: bool,
    arg_fn: bool,
    arg_closure: bool,
    missing_fields: bool,
    field_struct: bool,
    field_variant: bool,
    match_arms: bool,
    trait_missing: bool,
    trait_redundant: bool,
    import: bool,
}

/// Whether `code` is enabled. Every diagnostic in this file is `Severity::Error`, the highest
/// level, so `min_severity` can never filter one and is not consulted.
fn enabled(cx: &Cx<'_>, code: &str, names: &[&str]) -> bool {
    match &cx.opts.codes {
        None => true,
        Some(codes) => codes.iter().any(|c| c.as_str() == code || names.contains(&c.as_str())),
    }
}

impl Enabled {
    fn new(cx: &Cx<'_>) -> Enabled {
        const ARG: &[&str] = &["mismatched-arg-count"];
        const FIELD: &[&str] = &["no-such-field"];
        Enabled {
            ident: enabled(cx, "E0425", &["unresolved-ident"]),
            arg_fn: enabled(cx, "E0061", ARG),
            arg_closure: enabled(cx, "E0057", ARG),
            missing_fields: enabled(cx, "E0063", &["missing-fields"]),
            field_struct: enabled(cx, "E0560", FIELD),
            field_variant: enabled(cx, "E0559", FIELD),
            match_arms: enabled(cx, "E0004", &["missing-match-arm", "missing-match-arms"]),
            trait_missing: enabled(
                cx,
                "E0046",
                &["trait-impl-missing-assoc_item", "trait-impl-missing-assoc-item"],
            ),
            trait_redundant: enabled(
                cx,
                "E0407",
                &["trait-impl-redundant-assoc_item", "trait-impl-redundant-assoc-item"],
            ),
            import: enabled(cx, "E0432", &["unresolved-import"]),
        }
    }

    fn arg_count(&self) -> bool {
        self.arg_fn || self.arg_closure
    }

    fn fields(&self) -> bool {
        self.missing_fields || self.field_struct || self.field_variant
    }

    fn traits(&self) -> bool {
        self.trait_missing || self.trait_redundant
    }

    /// The checks that need the second pass over bodies.
    fn bodies(&self) -> bool {
        self.ident || self.arg_count() || self.fields() || self.match_arms
    }
}

pub(super) fn check(cx: &Cx<'_>, krate: &Crate, out: &mut Vec<Diagnostic>) {
    let en = Enabled::new(cx);
    if !(en.bodies() || en.traits() || en.import) {
        return;
    }

    // A crate under `#![cfg]`, or under an inner attribute that could be a macro, is unsettled
    // from the root down.
    let root_attrs = classify(&krate.attrs);
    if root_attrs.cfg || root_attrs.macro_like {
        return;
    }
    let mut indexer = Indexer { ix: Index::default(), cur: 0, exact: true, keep_uses: en.import };
    indexer.ix.scopes.push(Scope::new(None, 0, None));
    if root_attrs.foreign_derive {
        indexer.ix.scopes[0].incomplete = true;
    }
    visit::walk_crate(&mut indexer, krate);
    let mut ix = indexer.ix;

    if en.match_arms || en.arg_count() || en.fields() || en.import {
        ix.build_variants();
    }
    if en.fields() {
        ix.build_fields();
    }
    if en.arg_count() {
        ix.build_methods();
    }

    if en.traits() {
        check_trait_impls(cx, &ix, en, out);
    }
    if en.import {
        check_imports(cx, &ix, out);
    }
    if en.bodies() {
        let mut checker = Checker {
            cx,
            ix: &ix,
            en,
            out,
            cur: 0,
            locals: FxHashMap::default(),
            log: Vec::new(),
            marks: Vec::new(),
            wild: 0,
            fn_id: 0,
            next_fn_id: 0,
            suppress: 0,
            in_pattern: 0,
            assignee: 0,
            self_ty: None,
            self_value: false,
            self_param: None,
        };
        visit::walk_crate(&mut checker, krate);
    }
}

fn push(cx: &Cx<'_>, out: &mut Vec<Diagnostic>, code: &str, message: String, span: Span) {
    let (start, end) = (cx.span)(span);
    out.push(Diagnostic { code: code.to_string(), severity: Severity::Error, message, start, end });
}

// ---------------------------------------------------------------------------------------------
// Attributes
// ---------------------------------------------------------------------------------------------

/// What a list of attributes can do to the thing it is on.
#[derive(Clone, Copy, Default)]
struct AttrFacts {
    /// `#[cfg]`: the thing may not exist.
    cfg: bool,
    /// An attribute outside the builtin inert set, so possibly an attribute macro that rewrites
    /// the thing or adds items beside it. `cfg_attr` counts unless every attribute it can apply
    /// is itself inert.
    macro_like: bool,
    /// A `#[derive]` of something `core` does not ship. It cannot change the item, but a derive
    /// macro can emit arbitrary items next to it.
    foreign_derive: bool,
}

/// Builtin attributes that neither remove, rewrite nor add items. Anything else could be an
/// attribute macro. `rustc_*` attributes are deliberately absent: some change semantics this
/// module relies on (`rustc_legacy_const_generics` changes a function's arity).
fn is_inert(name: &str) -> bool {
    matches!(
        name,
        "doc"
            | "allow"
            | "warn"
            | "deny"
            | "forbid"
            | "expect"
            | "deprecated"
            | "must_use"
            | "inline"
            | "cold"
            | "track_caller"
            | "repr"
            | "non_exhaustive"
            | "default"
            | "automatically_derived"
            | "no_mangle"
            | "export_name"
            | "link_section"
            | "link"
            | "link_name"
            | "link_ordinal"
            | "used"
            | "path"
            | "macro_use"
            | "macro_export"
            | "test"
            | "ignore"
            | "should_panic"
            | "bench"
            | "no_std"
            | "no_core"
            | "no_main"
            | "no_implicit_prelude"
            | "recursion_limit"
            | "type_length_limit"
            | "crate_name"
            | "crate_type"
            | "windows_subsystem"
            | "feature"
            | "target_feature"
            | "naked"
            | "optimize"
            | "coverage"
            | "instruction_set"
            | "no_builtins"
            | "global_allocator"
            | "panic_handler"
            | "thread_local"
            | "collapse_debuginfo"
            | "debugger_visualizer"
            | "may_dangle"
            | "must_not_suspend"
    )
}

/// Tool namespaces whose attributes (`#[rustfmt::skip]`) are inert.
fn is_tool(name: &str) -> bool {
    matches!(name, "rustfmt" | "clippy" | "rust_analyzer" | "rustdoc" | "diagnostic" | "miri")
}

/// The derives `core` ships. They add trait impls only.
fn is_core_derive(name: &str) -> bool {
    matches!(
        name,
        "Debug" | "Clone" | "Copy" | "PartialEq" | "Eq" | "PartialOrd" | "Ord" | "Hash" | "Default"
    )
}

fn classify(attrs: &[Attribute]) -> AttrFacts {
    let mut facts = AttrFacts::default();
    for attr in attrs {
        let normal = match &attr.kind {
            AttrKind::Normal(normal) => normal,
            AttrKind::DocComment(..) => continue,
            _ => {
                facts.macro_like = true;
                continue;
            }
        };
        let segments = &normal.item.path.segments;
        let Some(first) = segments.first() else {
            facts.macro_like = true;
            continue;
        };
        if segments.len() > 1 {
            if !is_tool(first.ident.name.as_str()) {
                facts.macro_like = true;
            }
            continue;
        }
        match first.ident.name.as_str() {
            "cfg" => facts.cfg = true,
            "cfg_attr" => {
                // `cfg_attr(predicate, attr, ...)`: harmless when everything it can apply is.
                let inert = attr.meta_item_list().is_some_and(|list| {
                    list.len() >= 2
                        && list[1..]
                            .iter()
                            .all(|m| m.name().is_some_and(|n| is_inert(n.as_str())))
                });
                if !inert {
                    facts.macro_like = true;
                }
            }
            "derive" => {
                let core_only = attr.meta_item_list().is_some_and(|list| {
                    list.iter().all(|m| m.name().is_some_and(|n| is_core_derive(n.as_str())))
                });
                if !core_only {
                    facts.foreign_derive = true;
                }
            }
            name if is_inert(name) => {}
            _ => facts.macro_like = true,
        }
    }
    facts
}

/// `#[cfg]`, or anything that might be a macro: the node may be absent or rewritten.
fn is_unsettled(attrs: &[Attribute]) -> bool {
    let facts = classify(attrs);
    facts.cfg || facts.macro_like
}

// ---------------------------------------------------------------------------------------------
// The index
// ---------------------------------------------------------------------------------------------

type ScopeId = usize;
type DefIx = usize;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Ns {
    Value,
    Type,
}

/// What a name in a scope is bound to.
#[derive(Clone, Copy)]
enum Binding {
    Def(DefIx),
    /// A `use`. Its target is not followed; it only proves the name exists.
    Import,
    /// Bound twice in one scope, which is an error of its own or a `#[cfg]` pair.
    Ambiguous,
}

fn bind(map: &mut FxHashMap<Symbol, Binding>, name: Symbol, binding: Binding) {
    map.entry(name).and_modify(|b| *b = Binding::Ambiguous).or_insert(binding);
}

/// The items of one module or one block.
struct Scope {
    /// The enclosing block or module whose items this scope also sees. `None` for a module:
    /// a module does not see its parent's items.
    parent: Option<ScopeId>,
    /// The nearest enclosing module, which is what `self::` names.
    module: ScopeId,
    /// For a module declared directly in another module, that module. `super::` follows it.
    module_parent: Option<ScopeId>,
    values: FxHashMap<Symbol, Binding>,
    types: FxHashMap<Symbol, Binding>,
    incomplete: bool,
}

impl Scope {
    fn new(parent: Option<ScopeId>, module: ScopeId, module_parent: Option<ScopeId>) -> Scope {
        Scope {
            parent,
            module,
            module_parent,
            values: FxHashMap::default(),
            types: FxHashMap::default(),
            incomplete: false,
        }
    }
}

#[derive(Clone, Copy)]
enum DefKind<'a> {
    Fn(&'a ast::FnDecl),
    Struct(&'a VariantData, &'a Generics),
    Union,
    Enum(&'a EnumDef, &'a Generics),
    Trait(&'a ast::Trait),
    Mod(ScopeId),
    Other,
}

struct Def<'a> {
    ident: Ident,
    kind: DefKind<'a>,
    /// The scope the definition is written in, where the names in its signature resolve.
    scope: ScopeId,
    exact: bool,
}

struct ImplRec<'a> {
    imp: &'a ast::Impl,
    scope: ScopeId,
    exact: bool,
}

struct UseRec<'a> {
    tree: &'a UseTree,
    scope: ScopeId,
    exact: bool,
}

/// A struct's or variant's fields, by name. Tuple fields are named `0`, `1`, ...
struct FieldTable {
    names: FxHashMap<Symbol, usize>,
    list: Vec<(Symbol, bool)>,
    has_defaults: bool,
}

/// An inherent associated function, or proof that the name is not one function.
enum Assoc<'a> {
    One(&'a ast::Fn),
    Many,
}

/// A type's inherent associated items. `poisoned` when some inherent impl of it could hold
/// items this file cannot see: a macro in the impl, a macro-like attribute, a `#[cfg]`, or a
/// self type with concrete generic arguments, which only applies to some instantiations.
#[derive(Default)]
struct Methods<'a> {
    poisoned: bool,
    fns: FxHashMap<Symbol, Assoc<'a>>,
}

/// Field-table key for a struct itself, as opposed to one of an enum's variants.
const STRUCT: usize = usize::MAX;

enum Lookup {
    Found(Binding),
    Missing,
    Unknown,
}

#[derive(Default)]
struct Index<'a> {
    scopes: Vec<Scope>,
    defs: Vec<Def<'a>>,
    impls: Vec<ImplRec<'a>>,
    uses: Vec<UseRec<'a>>,
    macros: FxHashSet<Symbol>,
    blocks: FxHashMap<*const ast::Block, ScopeId>,
    mods: FxHashMap<*const Item, ScopeId>,
    /// Per enum, variant name to index; `None` when a variant is `#[cfg]`'d or attributed.
    variants: FxHashMap<DefIx, Option<FxHashMap<Symbol, usize>>>,
    /// Per struct (`STRUCT`) or enum variant, the field table; `None` when not exact.
    fields: FxHashMap<(DefIx, usize), Option<FieldTable>>,
    methods: FxHashMap<DefIx, Methods<'a>>,
}

impl<'a> Index<'a> {
    /// Look `name` up from `scope` outwards. Stops at the first scope that binds it, and at the
    /// first incomplete scope that does not, since that scope may bind it once expanded.
    fn lookup(&self, mut scope: ScopeId, name: Symbol, ns: Ns) -> Lookup {
        loop {
            let s = &self.scopes[scope];
            let map = if ns == Ns::Value { &s.values } else { &s.types };
            if let Some(b) = map.get(&name) {
                return Lookup::Found(*b);
            }
            if s.incomplete {
                return Lookup::Unknown;
            }
            match s.parent {
                Some(p) => scope = p,
                None => return Lookup::Missing,
            }
        }
    }

    fn exact_def(&self, scope: ScopeId, name: Symbol, ns: Ns) -> Option<DefIx> {
        match self.lookup(scope, name, ns) {
            Lookup::Found(Binding::Def(d)) if self.defs[d].exact => Some(d),
            _ => None,
        }
    }

    fn is_adt(&self, d: DefIx) -> bool {
        matches!(self.defs[d].kind, DefKind::Struct(..) | DefKind::Enum(..) | DefKind::Union)
    }

    /// The in-file struct, enum or union an impl is for, and whether its generic arguments are
    /// exactly the impl's own parameters (so the impl covers every instantiation).
    fn impl_self(&self, scope: ScopeId, imp: &ast::Impl) -> Option<(DefIx, bool)> {
        let TyKind::Path(None, path) = &imp.self_ty.kind else { return None };
        let [seg] = &path.segments[..] else { return None };
        let is_param = |name: Symbol| imp.generics.params.iter().any(|p| p.ident.name == name);
        if is_param(seg.ident.name) {
            return None;
        }
        let d = self.exact_def(scope, seg.ident.name, Ns::Type)?;
        if !self.is_adt(d) {
            return None;
        }
        let generic = match seg.args.as_deref() {
            None => true,
            Some(GenericArgs::AngleBracketed(args)) => {
                let mut seen = FxHashSet::default();
                args.args.iter().all(|arg| match arg {
                    AngleBracketedArg::Arg(GenericArg::Lifetime(_)) => true,
                    AngleBracketedArg::Arg(GenericArg::Type(ty)) => match &ty.kind {
                        TyKind::Path(None, p) => match &p.segments[..] {
                            [s] if s.args.is_none() => {
                                is_param(s.ident.name) && seen.insert(s.ident.name)
                            }
                            _ => false,
                        },
                        _ => false,
                    },
                    _ => false,
                })
            }
            Some(_) => false,
        };
        Some((d, generic))
    }

    fn build_variants(&mut self) {
        for (d, def) in self.defs.iter().enumerate() {
            let DefKind::Enum(e, _) = def.kind else { continue };
            let table = if e.variants.iter().any(|v| is_unsettled(&v.attrs)) {
                None
            } else {
                let mut map = FxHashMap::default();
                for (i, v) in e.variants.iter().enumerate() {
                    map.insert(v.ident.name, i);
                }
                Some(map)
            };
            self.variants.insert(d, table);
        }
    }

    fn build_fields(&mut self) {
        for (d, def) in self.defs.iter().enumerate() {
            match def.kind {
                DefKind::Struct(data, _) => {
                    self.fields.insert((d, STRUCT), field_table(data));
                }
                DefKind::Enum(e, _) => {
                    for (i, v) in e.variants.iter().enumerate() {
                        self.fields.insert((d, i), field_table(&v.data));
                    }
                }
                _ => {}
            }
        }
    }

    fn build_methods(&mut self) {
        let mut methods: FxHashMap<DefIx, Methods<'a>> = FxHashMap::default();
        for rec in &self.impls {
            if rec.imp.of_trait.is_some() {
                continue;
            }
            // An impl whose self type this file cannot resolve is ignored. It can only add
            // methods under the same names as an impl we do see, which is E0592 on its own.
            let Some((d, generic)) = self.impl_self(rec.scope, rec.imp) else { continue };
            let entry = methods.entry(d).or_default();
            if !rec.exact || !generic {
                entry.poisoned = true;
                continue;
            }
            for item in rec.imp.items.iter() {
                let unsettled = is_unsettled(&item.attrs);
                match &item.kind {
                    AssocItemKind::Fn(f) => {
                        let one = if unsettled { Assoc::Many } else { Assoc::One(&**f) };
                        entry
                            .fns
                            .entry(f.ident.name)
                            .and_modify(|a| *a = Assoc::Many)
                            .or_insert(one);
                    }
                    AssocItemKind::Const(c) => {
                        entry.fns.insert(c.ident.name, Assoc::Many);
                    }
                    AssocItemKind::Type(_) => {}
                    AssocItemKind::MacCall(_)
                    | AssocItemKind::Delegation(_)
                    | AssocItemKind::DelegationMac(_) => entry.poisoned = true,
                }
            }
        }
        self.methods = methods;
    }

    fn inherent(&self, d: DefIx, name: Symbol) -> Option<&'a ast::Fn> {
        let m = self.methods.get(&d)?;
        if m.poisoned {
            return None;
        }
        match m.fns.get(&name)? {
            Assoc::One(f) => Some(*f),
            Assoc::Many => None,
        }
    }

    fn variant(&self, d: DefIx, name: Symbol) -> Option<usize> {
        self.variants.get(&d)?.as_ref()?.get(&name).copied()
    }
}

fn field_table(data: &VariantData) -> Option<FieldTable> {
    let fields: &[ast::FieldDef] = match data {
        VariantData::Struct { fields, recovered } => {
            if !matches!(recovered, Recovered::No) {
                return None;
            }
            fields
        }
        VariantData::Tuple(fields, _) => fields,
        VariantData::Unit(_) => &[],
    };
    let mut table = FieldTable {
        names: FxHashMap::default(),
        list: Vec::with_capacity(fields.len()),
        has_defaults: false,
    };
    for (i, f) in fields.iter().enumerate() {
        if is_unsettled(&f.attrs) {
            return None;
        }
        let name = match f.ident {
            Some(ident) => ident.name,
            None => Symbol::intern(&format!("{i}")),
        };
        let has_default = f.extras.as_ref().is_some_and(|x| x.default.is_some());
        table.has_defaults |= has_default;
        table.names.insert(name, i);
        table.list.push((name, has_default));
    }
    Some(table)
}

// ---------------------------------------------------------------------------------------------
// Pass one: the indexer
// ---------------------------------------------------------------------------------------------

struct Indexer<'a> {
    ix: Index<'a>,
    cur: ScopeId,
    /// No enclosing item is `#[cfg]`'d or macro-attributed.
    exact: bool,
    keep_uses: bool,
}

impl<'a> Indexer<'a> {
    fn new_scope(&mut self, parent: Option<ScopeId>, module: Option<ScopeId>) -> ScopeId {
        let id = self.ix.scopes.len();
        let module_parent = if module.is_none() && self.ix.scopes[self.cur].module == self.cur {
            // A module declared directly in a module. One declared in a block gets no `super`
            // here, because which module it names there is not worth guessing at.
            Some(self.cur)
        } else {
            None
        };
        self.ix.scopes.push(Scope::new(parent, module.unwrap_or(id), module_parent));
        id
    }

    fn def(&mut self, ident: Ident, kind: DefKind<'a>, exact: bool, value: bool, ty: bool) {
        let d = self.ix.defs.len();
        self.ix.defs.push(Def { ident, kind, scope: self.cur, exact });
        let scope = &mut self.ix.scopes[self.cur];
        if value {
            bind(&mut scope.values, ident.name, Binding::Def(d));
        }
        if ty {
            bind(&mut scope.types, ident.name, Binding::Def(d));
        }
    }

    fn incomplete(&mut self) {
        self.ix.scopes[self.cur].incomplete = true;
    }

    /// Bind the names a `use` tree introduces. `parent_last` is the enclosing tree's last
    /// segment, which is what a `{self}` leaf imports.
    fn use_names(&mut self, tree: &UseTree, parent_last: Option<Ident>) {
        let last = tree.prefix.segments.last().map(|s| s.ident);
        match &tree.kind {
            UseTreeKind::Simple(rename) => {
                let name = match (rename, last) {
                    (Some(r), _) => Some(*r),
                    (None, Some(l)) if l.name == kw::SelfLower => parent_last,
                    (None, l) => l,
                };
                if let Some(n) = name
                    && n.name != kw::Underscore
                {
                    let scope = &mut self.ix.scopes[self.cur];
                    bind(&mut scope.values, n.name, Binding::Import);
                    bind(&mut scope.types, n.name, Binding::Import);
                }
            }
            UseTreeKind::Glob(_) => self.incomplete(),
            UseTreeKind::Nested { items, .. } => {
                let last = last.or(parent_last);
                for (t, _) in items.iter() {
                    self.use_names(t, last);
                }
            }
        }
    }
}

impl<'a> Visitor<'a> for Indexer<'a> {
    type Result = ();

    fn visit_item(&mut self, item: &'a Item) {
        let attrs = classify(&item.attrs);
        if attrs.macro_like || attrs.foreign_derive {
            self.incomplete();
        }
        let exact = self.exact && !attrs.cfg && !attrs.macro_like;
        match &item.kind {
            ItemKind::Fn(f) => self.def(f.ident, DefKind::Fn(&f.sig.decl), exact, true, false),
            ItemKind::Struct(ident, generics, data) => {
                let ctor = !matches!(data, VariantData::Struct { .. });
                self.def(*ident, DefKind::Struct(data, generics), exact, ctor, true)
            }
            ItemKind::Union(ident, ..) => self.def(*ident, DefKind::Union, exact, false, true),
            ItemKind::Enum(ident, generics, e) => {
                self.def(*ident, DefKind::Enum(e, generics), exact, false, true)
            }
            ItemKind::Trait(t) => self.def(t.ident, DefKind::Trait(t), exact, false, true),
            ItemKind::TraitAlias(t) => self.def(t.ident, DefKind::Other, exact, false, true),
            ItemKind::TyAlias(t) => self.def(t.ident, DefKind::Other, exact, false, true),
            ItemKind::ExternCrate(_, ident) => self.def(*ident, DefKind::Other, exact, false, true),
            ItemKind::Const(c) => self.def(c.ident, DefKind::Other, exact, true, false),
            ItemKind::Static(s) => self.def(s.ident, DefKind::Other, exact, true, false),
            ItemKind::Delegation(d) => {
                self.def(d.rename.unwrap_or(d.ident), DefKind::Other, exact, true, false)
            }
            ItemKind::Use(tree) => {
                self.use_names(tree, None);
                // A `use` in a block resolves `self::` against the enclosing module, and that
                // case is left alone rather than decided here.
                if self.keep_uses && self.ix.scopes[self.cur].module == self.cur {
                    self.ix.uses.push(UseRec { tree, scope: self.cur, exact });
                }
            }
            ItemKind::Impl(imp) => self.ix.impls.push(ImplRec { imp, scope: self.cur, exact }),
            ItemKind::MacroDef(ident, _) => {
                self.ix.macros.insert(ident.name);
            }
            ItemKind::Mod(_, ident, ModKind::Loaded(..)) => {
                let s = self.new_scope(None, None);
                self.def(*ident, DefKind::Mod(s), exact, false, true);
                self.ix.mods.insert(item as *const Item, s);
                let (cur, outer) = (self.cur, self.exact);
                self.cur = s;
                self.exact = exact;
                visit::walk_item(self, item);
                self.cur = cur;
                self.exact = outer;
                return;
            }
            // `mod foo;` is a file this analysis never sees: present, contents unknown.
            ItemKind::Mod(_, ident, ModKind::Unloaded) => {
                self.def(*ident, DefKind::Other, false, false, true)
            }
            ItemKind::MacCall(_)
            | ItemKind::DelegationMac(_)
            | ItemKind::TestBinderConstraints(_) => self.incomplete(),
            ItemKind::ForeignMod(_) | ItemKind::GlobalAsm(_) | ItemKind::ConstBlock(_) => {}
        }
        let outer = self.exact;
        self.exact = exact;
        visit::walk_item(self, item);
        self.exact = outer;
    }

    fn visit_foreign_item(&mut self, item: &'a ForeignItem) {
        let attrs = classify(&item.attrs);
        if attrs.macro_like || attrs.foreign_derive {
            self.incomplete();
        }
        let exact = self.exact && !attrs.cfg && !attrs.macro_like;
        match &item.kind {
            ForeignItemKind::Fn(f) => {
                self.def(f.ident, DefKind::Fn(&f.sig.decl), exact, true, false)
            }
            ForeignItemKind::Static(s) => self.def(s.ident, DefKind::Other, exact, true, false),
            ForeignItemKind::TyAlias(t) => self.def(t.ident, DefKind::Other, exact, false, true),
            ForeignItemKind::MacCall(_) => self.incomplete(),
        }
        let outer = self.exact;
        self.exact = exact;
        visit::walk_item(self, item);
        self.exact = outer;
    }

    fn visit_assoc_item(&mut self, item: &'a AssocItem, ctxt: AssocCtxt) {
        let outer = self.exact;
        self.exact = outer && !is_unsettled(&item.attrs);
        visit::walk_assoc_item(self, item, ctxt);
        self.exact = outer;
    }

    fn visit_block(&mut self, b: &'a ast::Block) {
        let parent = self.cur;
        let module = self.ix.scopes[parent].module;
        let s = self.new_scope(Some(parent), Some(module));
        self.ix.blocks.insert(b as *const ast::Block, s);
        self.cur = s;
        visit::walk_block(self, b);
        self.cur = parent;
    }

    fn visit_stmt(&mut self, s: &'a Stmt) {
        // A statement macro can define items, or `let` a name it was handed.
        if let StmtKind::MacCall(_) = s.kind {
            self.incomplete();
        }
        visit::walk_stmt(self, s);
    }
}

// ---------------------------------------------------------------------------------------------
// The prelude
// ---------------------------------------------------------------------------------------------

/// Names the standard preludes put in scope, which this module cannot see because nothing is
/// expanded or loaded: the `core` and `std` preludes for editions 2015 through 2024, and the
/// primitive types. Only lowercase value paths are ever reported, so the list only has to be
/// complete for those; the rest is here so the list can be checked against the prelude modules
/// themselves. Over-inclusion is harmless: it can only hide a report.
fn in_prelude(name: &str) -> bool {
    matches!(
        name,
        // Values and functions.
        "drop"
            | "size_of"
            | "size_of_val"
            | "align_of"
            | "align_of_val"
            | "Some"
            | "None"
            | "Ok"
            | "Err"
            // Types and traits.
            | "Copy"
            | "Send"
            | "Sized"
            | "Sync"
            | "Unpin"
            | "Drop"
            | "Fn"
            | "FnMut"
            | "FnOnce"
            | "AsyncFn"
            | "AsyncFnMut"
            | "AsyncFnOnce"
            | "Box"
            | "ToOwned"
            | "Clone"
            | "PartialEq"
            | "PartialOrd"
            | "Eq"
            | "Ord"
            | "AsRef"
            | "AsMut"
            | "Into"
            | "From"
            | "Default"
            | "Iterator"
            | "Extend"
            | "IntoIterator"
            | "DoubleEndedIterator"
            | "ExactSizeIterator"
            | "Option"
            | "Result"
            | "String"
            | "ToString"
            | "Vec"
            | "TryFrom"
            | "TryInto"
            | "FromIterator"
            | "Future"
            | "IntoFuture"
            // Primitive types.
            | "bool"
            | "char"
            | "str"
            | "i8"
            | "i16"
            | "i32"
            | "i64"
            | "i128"
            | "isize"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "u128"
            | "usize"
            | "f16"
            | "f32"
            | "f64"
            | "f128"
            // The crates every edition can name without `extern crate`. Naming one as a value
            // is E0423, not E0425, so it must not be reported under this code.
            | "std"
            | "core"
            | "alloc"
            | "proc_macro"
            | "test"
    )
}

fn starts_lowercase(name: &str) -> bool {
    name.chars().next().is_some_and(|c| !c.is_uppercase())
}

// ---------------------------------------------------------------------------------------------
// Pass two: bodies
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Recv {
    Value,
    Ref,
    RefMut,
}

impl Recv {
    fn prefix(self) -> &'static str {
        match self {
            Recv::Value => "",
            Recv::Ref => "&",
            Recv::RefMut => "&mut ",
        }
    }
}

/// What is known about a local binding.
#[derive(Clone, Copy)]
struct LocalInfo {
    /// Which function body bound it. A nested `fn` cannot see an outer function's locals, so a
    /// local from another body is never used to decide anything precise.
    fn_id: u32,
    /// Bound by `let f = |..| ..` with no annotation: the closure's arity.
    closure_arity: Option<usize>,
    /// Its type, when written out as an in-file enum or one reference to one.
    shape: Option<(DefIx, Recv)>,
}

/// The receiver a method declares, when it is `self`, `&self` or `&mut self`.
fn declared_recv(param: &Param) -> Option<Recv> {
    let PatKind::Ident(ast::BindingMode(ByRef::No, _), ident, None) = &param.pat.kind else {
        return None;
    };
    if ident.name != kw::SelfLower {
        return None;
    }
    match &param.ty.kind {
        TyKind::ImplicitSelf => Some(Recv::Value),
        TyKind::Ref(_, mt) if mt.ty.kind.is_implicit_self() => Some(match mt.mutbl {
            Mutability::Not => Recv::Ref,
            Mutability::Mut => Recv::RefMut,
        }),
        _ => None,
    }
}

/// A function's parameter count and whether it is C-variadic, unless a parameter carries an
/// attribute that could remove it.
fn arity(decl: &ast::FnDecl) -> Option<(usize, bool)> {
    if decl.inputs.iter().any(|p| is_unsettled(&p.attrs)) {
        return None;
    }
    let variadic = decl.c_variadic();
    Some((decl.inputs.len() - usize::from(variadic), variadic))
}

fn closure_arity(e: &Expr) -> Option<usize> {
    match &e.kind {
        ExprKind::Closure(c) if c.fn_decl.inputs.iter().all(|p| p.attrs.is_empty()) => {
            Some(c.fn_decl.inputs.len())
        }
        ExprKind::Paren(inner) => closure_arity(inner),
        _ => None,
    }
}

/// An argument list whose length is not what it looks like: a `#[cfg]`'d argument, or a macro
/// that may expand to nothing.
fn args_unsettled(args: &[alloc::boxed::Box<Expr>]) -> bool {
    args.iter().any(|a| !a.attrs.is_empty() || matches!(a.kind, ExprKind::MacCall(_)))
}

enum Cover {
    Covered,
    /// A refutable pattern on this variant; whether it covers the variant is not decided.
    Partial(usize),
    /// A wildcard or a binding: the match is exhaustive.
    CatchAll,
    /// A pattern this module does not decide.
    Bail,
}

struct Checker<'a, 'b, 'c> {
    cx: &'b Cx<'c>,
    ix: &'b Index<'a>,
    en: Enabled,
    out: &'b mut Vec<Diagnostic>,
    cur: ScopeId,
    locals: FxHashMap<Symbol, Vec<LocalInfo>>,
    /// Undo log for `locals`. `None` records a binding this module cannot name (a pattern
    /// macro), which makes every miss unknown while it is in scope.
    log: Vec<Option<Symbol>>,
    marks: Vec<usize>,
    wild: u32,
    fn_id: u32,
    next_fn_id: u32,
    /// Depth inside `#[cfg]`'d or macro-attributed code, where nothing is reported.
    suppress: u32,
    in_pattern: u32,
    /// Inside the left-hand side of an assignment, where `S { a, .. }` is a pattern.
    assignee: u32,
    /// The in-file type `Self` names, when it is one.
    self_ty: Option<DefIx>,
    /// The innermost function has a `self` parameter.
    self_value: bool,
    self_param: Option<Recv>,
}

impl<'a, 'b, 'c> Checker<'a, 'b, 'c> {
    fn emit(&mut self, code: &str, message: String, span: Span) {
        if self.suppress == 0 {
            push(self.cx, self.out, code, message, span);
        }
    }

    // --- locals -------------------------------------------------------------------------

    fn push_scope(&mut self) {
        self.marks.push(self.log.len());
    }

    fn pop_scope(&mut self) {
        let mark = self.marks.pop().unwrap_or(0);
        while self.log.len() > mark {
            match self.log.pop() {
                Some(Some(name)) => {
                    if let Some(stack) = self.locals.get_mut(&name) {
                        stack.pop();
                    }
                }
                Some(None) => self.wild -= 1,
                None => break,
            }
        }
    }

    fn plain(&self) -> LocalInfo {
        LocalInfo { fn_id: self.fn_id, closure_arity: None, shape: None }
    }

    fn add_local(&mut self, name: Symbol, info: LocalInfo) {
        self.locals.entry(name).or_default().push(info);
        self.log.push(Some(name));
    }

    fn local(&self, name: Symbol) -> Option<LocalInfo> {
        self.locals.get(&name).and_then(|stack| stack.last().copied())
    }

    /// A local of the current function body, usable to decide something precise.
    fn own_local(&self, name: Symbol) -> Option<LocalInfo> {
        self.local(name).filter(|l| l.fn_id == self.fn_id)
    }

    /// Bind every name `pat` introduces. `top` describes the binding when `pat` is a plain
    /// by-value identifier.
    fn bind_pat(&mut self, pat: &Pat, top: LocalInfo) {
        let plain = self.plain();
        match &pat.kind {
            PatKind::Ident(mode, ident, sub) => {
                let simple = sub.is_none() && matches!(mode.0, ByRef::No);
                if ident.name != kw::SelfLower {
                    self.add_local(ident.name, if simple { top } else { plain });
                }
                if let Some(sub) = sub {
                    self.bind_pat(sub, plain);
                }
            }
            PatKind::Struct(_, _, fields, _) => {
                for f in fields.iter() {
                    self.bind_pat(&f.pat, plain);
                }
            }
            PatKind::TupleStruct(_, _, pats)
            | PatKind::Tuple(pats)
            | PatKind::Slice(pats)
            | PatKind::Or(pats) => {
                for p in pats.iter() {
                    self.bind_pat(p, plain);
                }
            }
            PatKind::Deref(p) | PatKind::Ref(p, ..) | PatKind::Paren(p) | PatKind::Guard(p, _) => {
                self.bind_pat(p, plain)
            }
            PatKind::MacCall(_) => {
                self.log.push(None);
                self.wild += 1;
            }
            _ => {}
        }
    }

    // --- resolution ---------------------------------------------------------------------

    fn lookup(&self, name: Symbol, ns: Ns) -> Lookup {
        self.ix.lookup(self.cur, name, ns)
    }

    /// An exact in-file definition for a single-segment path. `None` when a local or a generic
    /// parameter could shadow it, or when anything is unknown.
    fn resolve(&self, name: Symbol, ns: Ns) -> Option<DefIx> {
        if self.wild > 0 || self.local(name).is_some() {
            return None;
        }
        self.ix.exact_def(self.cur, name, ns)
    }

    /// The in-file struct, enum or union a type-position segment names, `Self` included.
    fn type_seg(&self, seg: &PathSegment) -> Option<DefIx> {
        let d = if seg.ident.name == kw::SelfUpper {
            self.self_ty?
        } else {
            self.resolve(seg.ident.name, Ns::Type)?
        };
        self.ix.is_adt(d).then_some(d)
    }

    fn enum_def(&self, d: DefIx) -> Option<(&'a EnumDef, &'a Generics)> {
        match self.ix.defs[d].kind {
            DefKind::Enum(e, g) => Some((e, g)),
            _ => None,
        }
    }

    /// `Enum::Variant` or `Self::Variant` naming an in-file enum's variant.
    fn variant_path(&self, path: &Path) -> Option<(DefIx, usize)> {
        let [e, v] = &path.segments[..] else { return None };
        let d = self.type_seg(e)?;
        self.enum_def(d)?;
        Some((d, self.ix.variant(d, v.ident.name)?))
    }

    /// The struct or variant a struct literal or struct pattern names.
    fn struct_target(&self, path: &Path) -> Option<(DefIx, usize)> {
        match &path.segments[..] {
            [s] => {
                let d = self.type_seg(s)?;
                matches!(self.ix.defs[d].kind, DefKind::Struct(..)).then_some((d, STRUCT))
            }
            [_, _] => self.variant_path(path),
            _ => None,
        }
    }

    /// The in-file enum a type names directly or through one reference.
    fn ty_shape(&self, ty: &Ty) -> Option<(DefIx, Recv)> {
        match &ty.kind {
            TyKind::Paren(inner) => self.ty_shape(inner),
            TyKind::Ref(_, mt) => match self.ty_shape(&mt.ty)? {
                (d, Recv::Value) => Some((
                    d,
                    if mt.mutbl == Mutability::Not { Recv::Ref } else { Recv::RefMut },
                )),
                _ => None,
            },
            TyKind::ImplicitSelf => None,
            TyKind::Path(None, path) => {
                let [seg] = &path.segments[..] else { return None };
                let d = self.type_seg(seg)?;
                self.enum_def(d)?;
                Some((d, Recv::Value))
            }
            _ => None,
        }
    }

    /// The in-file enum an expression evidently has as its type, directly or behind one `&`.
    fn expr_shape(&self, e: &Expr) -> Option<(DefIx, Recv)> {
        match &e.kind {
            ExprKind::Paren(inner) => self.expr_shape(inner),
            ExprKind::AddrOf(BorrowKind::Ref, m, inner) => match self.expr_shape(inner)? {
                (d, Recv::Value) => {
                    Some((d, if *m == Mutability::Not { Recv::Ref } else { Recv::RefMut }))
                }
                _ => None,
            },
            ExprKind::Path(None, path) => match &path.segments[..] {
                [s] if s.ident.name == kw::SelfLower => {
                    let d = self.self_ty?;
                    self.enum_def(d)?;
                    if !self.self_value {
                        return None;
                    }
                    Some((d, self.self_param?))
                }
                [s] => self.own_local(s.ident.name)?.shape,
                [_, _] => {
                    let (d, v) = self.variant_path(path)?;
                    let (e, _) = self.enum_def(d)?;
                    matches!(e.variants[v].data, VariantData::Unit(_)).then_some((d, Recv::Value))
                }
                _ => None,
            },
            ExprKind::Call(callee, _) => {
                let ExprKind::Path(None, path) = &callee.kind else { return None };
                let (d, v) = self.variant_path(path)?;
                let (e, _) = self.enum_def(d)?;
                matches!(e.variants[v].data, VariantData::Tuple(..)).then_some((d, Recv::Value))
            }
            ExprKind::Struct(se) if se.qself.is_none() => {
                let (d, _) = self.variant_path(&se.path)?;
                Some((d, Recv::Value))
            }
            _ => None,
        }
    }

    // --- unresolved-ident ---------------------------------------------------------------

    fn check_ident(&mut self, e: &Expr, path: &Path) {
        if !self.en.ident || self.in_pattern > 0 || self.suppress > 0 {
            return;
        }
        let [seg] = &path.segments[..] else { return };
        if seg.args.is_some() {
            return;
        }
        let name = seg.ident.name;
        if name == kw::SelfLower {
            if !self.self_value {
                self.emit("E0425", "no such value in this scope".to_string(), e.span);
            }
            return;
        }
        if name == kw::SelfUpper
            || name == kw::Super
            || name == kw::Crate
            || name == kw::PathRoot
            || name == kw::Underscore
        {
            return;
        }
        if !starts_lowercase(name.as_str()) || in_prelude(name.as_str()) {
            return;
        }
        if self.wild > 0 || self.local(name).is_some() {
            return;
        }
        if !matches!(self.lookup(name, Ns::Value), Lookup::Missing)
            || !matches!(self.lookup(name, Ns::Type), Lookup::Missing)
        {
            return;
        }
        self.emit("E0425", "no such value in this scope".to_string(), e.span);
    }

    // --- mismatched-arg-count -----------------------------------------------------------

    /// Report an arity mismatch with rust-analyzer's range: the argument list when either count
    /// is zero, the closing parenthesis when arguments are missing, and the surplus arguments
    /// through the closing parenthesis when there are too many.
    fn emit_arg_count(
        &mut self,
        code: &str,
        expected: usize,
        found: usize,
        call: Span,
        before_args: Span,
        args: &[alloc::boxed::Box<Expr>],
    ) {
        if self.suppress > 0 {
            return;
        }
        let s = if expected == 1 { "" } else { "s" };
        let message = format!("expected {expected} argument{s}, found {found}");
        let list = call.with_lo(before_args.hi());
        let surplus = expected < found && expected > 0;
        let span = if surplus { call.with_lo(args[expected].span.lo()) } else { list };
        let (mut start, end) = (self.cx.span)(span);
        let bytes = self.cx.source.as_bytes();
        if found < expected && found > 0 {
            if end > 0 && bytes.get(end as usize - 1) == Some(&b')') {
                start = end - 1;
            }
        } else if !surplus {
            // The span starts where the callee ends; the list starts at its parenthesis.
            let mut at = start;
            while at < end && bytes.get(at as usize).is_some_and(|b| *b != b'(') {
                at += 1;
            }
            if at < end {
                start = at;
            }
        }
        self.out.push(Diagnostic {
            code: code.to_string(),
            severity: Severity::Error,
            message,
            start,
            end,
        });
    }

    fn check_fn_arity(
        &mut self,
        decl: &ast::FnDecl,
        call: &Expr,
        callee: &Expr,
        args: &[alloc::boxed::Box<Expr>],
    ) {
        let Some((fixed, variadic)) = arity(decl) else { return };
        let found = args.len();
        if found < fixed || (found > fixed && !variadic) {
            self.emit_arg_count("E0061", fixed, found, call.span, callee.span, args);
        }
    }

    fn check_tuple_arity(
        &mut self,
        data: &VariantData,
        call: &Expr,
        callee: &Expr,
        args: &[alloc::boxed::Box<Expr>],
    ) {
        let VariantData::Tuple(fields, _) = data else { return };
        if fields.iter().any(|f| is_unsettled(&f.attrs)) {
            return;
        }
        if fields.len() != args.len() {
            self.emit_arg_count("E0061", fields.len(), args.len(), call.span, callee.span, args);
        }
    }

    fn check_call(&mut self, call: &Expr, callee: &Expr, args: &[alloc::boxed::Box<Expr>]) {
        if !self.en.arg_count() || self.suppress > 0 || args_unsettled(args) {
            return;
        }
        let ExprKind::Path(None, path) = &callee.kind else { return };
        match &path.segments[..] {
            [seg] => {
                let name = seg.ident.name;
                if name == kw::SelfUpper {
                    if !self.en.arg_fn {
                        return;
                    }
                    let Some(d) = self.self_ty else { return };
                    if let DefKind::Struct(data, _) = self.ix.defs[d].kind {
                        self.check_tuple_arity(data, call, callee, args);
                    }
                    return;
                }
                if self.wild > 0 {
                    return;
                }
                if let Some(local) = self.local(name) {
                    if local.fn_id == self.fn_id
                        && let Some(expected) = local.closure_arity
                        && expected != args.len()
                        && self.en.arg_closure
                    {
                        self.emit_arg_count(
                            "E0057",
                            expected,
                            args.len(),
                            call.span,
                            callee.span,
                            args,
                        );
                    }
                    return;
                }
                if !self.en.arg_fn {
                    return;
                }
                let Some(d) = self.resolve(name, Ns::Value) else { return };
                match self.ix.defs[d].kind {
                    DefKind::Fn(decl) => self.check_fn_arity(decl, call, callee, args),
                    DefKind::Struct(data, _) => self.check_tuple_arity(data, call, callee, args),
                    _ => {}
                }
            }
            [ty, item] => {
                if !self.en.arg_fn {
                    return;
                }
                let Some(d) = self.type_seg(ty) else { return };
                // An enum's variants shadow its inherent associated items.
                if let Some((e, _)) = self.enum_def(d) {
                    match self.ix.variants.get(&d) {
                        Some(Some(map)) => {
                            if let Some(&v) = map.get(&item.ident.name) {
                                self.check_tuple_arity(&e.variants[v].data, call, callee, args);
                                return;
                            }
                        }
                        _ => return,
                    }
                }
                // Inherent associated items shadow trait ones in a type-relative path.
                if let Some(f) = self.ix.inherent(d, item.ident.name) {
                    self.check_fn_arity(&f.sig.decl, call, callee, args);
                }
            }
            _ => {}
        }
    }

    /// The type of a method receiver, when it is written out: `self` in an impl, a struct
    /// literal, a unit struct, or one `&` of those.
    fn receiver(&self, e: &Expr) -> Option<(DefIx, Recv)> {
        match &e.kind {
            ExprKind::Paren(inner) => self.receiver(inner),
            ExprKind::AddrOf(BorrowKind::Ref, m, inner) => match self.receiver(inner)? {
                (d, Recv::Value) => {
                    Some((d, if *m == Mutability::Not { Recv::Ref } else { Recv::RefMut }))
                }
                _ => None,
            },
            ExprKind::Path(None, path) => {
                let [seg] = &path.segments[..] else { return None };
                if seg.ident.name == kw::SelfLower {
                    if !self.self_value {
                        return None;
                    }
                    return Some((self.self_ty?, self.self_param?));
                }
                let d = self.resolve(seg.ident.name, Ns::Value)?;
                match self.ix.defs[d].kind {
                    DefKind::Struct(VariantData::Unit(_), _) => Some((d, Recv::Value)),
                    _ => None,
                }
            }
            ExprKind::Struct(se) if se.qself.is_none() => {
                let [seg] = &se.path.segments[..] else { return None };
                let d = self.type_seg(seg)?;
                matches!(self.ix.defs[d].kind, DefKind::Struct(..)).then_some((d, Recv::Value))
            }
            _ => None,
        }
    }

    /// `x.m(..)` is checked only when the receiver's type is written out and `m` is that type's
    /// only inherent function of that name, declared with exactly the receiver's own form
    /// (`self`, `&self` or `&mut self`). Method probing then picks it at the first step, by
    /// value, before autoref and before any trait method: inherent methods win a tie at a
    /// step. Any other receiver form could let a trait method in scope win first, and this
    /// module cannot see which traits are in scope, so it is skipped.
    fn check_method(&mut self, call: &Expr, mc: &ast::MethodCall) {
        if !self.en.arg_fn || self.suppress > 0 || args_unsettled(&mc.args) {
            return;
        }
        let Some((d, recv)) = self.receiver(&mc.receiver) else { return };
        let Some(f) = self.ix.inherent(d, mc.seg.ident.name) else { return };
        let Some(first) = f.sig.decl.inputs.first() else { return };
        if declared_recv(first) != Some(recv) {
            return;
        }
        let Some((fixed, _)) = arity(&f.sig.decl) else { return };
        let expected = fixed - 1;
        let found = mc.args.len();
        if expected != found {
            let before = mc.seg.args.as_ref().map_or(mc.seg.ident.span, |a| a.span());
            self.emit_arg_count("E0061", expected, found, call.span, before, &mc.args);
        }
    }

    // --- missing-fields and no-such-field -----------------------------------------------

    /// `written` is every field the literal or pattern names. `required` is `None` when the
    /// rest syntax excuses missing fields, `Some(false)` when every field is required, and
    /// `Some(true)` when only fields without a default value are.
    fn check_fields(
        &mut self,
        target: (DefIx, usize),
        path: &Path,
        written: &[(Ident, Span)],
        required: Option<bool>,
    ) {
        let ix = self.ix;
        let Some(Some(table)) = ix.fields.get(&target) else { return };
        let variant = target.1 != STRUCT;
        let (code, on) = if variant {
            ("E0559", self.en.field_variant)
        } else {
            ("E0560", self.en.field_struct)
        };
        let mut seen = vec![false; table.list.len()];
        let mut reports: Vec<(&str, String, Span)> = Vec::new();
        for (ident, span) in written {
            match table.names.get(&ident.name) {
                Some(&i) => seen[i] = true,
                None if on => reports.push((code, "no such field".to_string(), *span)),
                None => {}
            }
        }
        if self.en.missing_fields
            && let Some(defaults_excused) = required
            && !(defaults_excused && !table.has_defaults)
        {
            let mut message = String::from("missing structure fields:\n");
            let mut any = false;
            for (i, (name, has_default)) in table.list.iter().enumerate() {
                if !seen[i] && !(defaults_excused && *has_default) {
                    message.push_str(&format!("- {name}\n"));
                    any = true;
                }
            }
            if any {
                reports.push(("E0063", message, path.span));
            }
        }
        for (code, message, span) in reports {
            self.emit(code, message, span);
        }
    }

    fn check_struct_expr(&mut self, se: &ast::StructExpr) {
        if !self.en.fields() || self.suppress > 0 || se.qself.is_some() {
            return;
        }
        if se.fields.iter().any(|f| !f.attrs.is_empty()) {
            return;
        }
        let Some(target) = self.struct_target(&se.path) else { return };
        let required = match &se.rest {
            StructRest::Base(_) => None,
            // In an assignee, `..` is a rest pattern; elsewhere it asks for default values.
            StructRest::Rest(_) => (self.assignee == 0).then_some(true),
            StructRest::None => Some(false),
            StructRest::NoneWithError(_) => return,
        };
        let written: Vec<(Ident, Span)> = se.fields.iter().map(|f| (f.ident, f.span)).collect();
        self.check_fields(target, &se.path, &written, required);
    }

    fn check_struct_pat(&mut self, path: &Path, fields: &[PatField], rest: &PatFieldsRest) {
        if !self.en.fields() || self.suppress > 0 {
            return;
        }
        if fields.iter().any(|f| !f.attrs.is_empty()) {
            return;
        }
        let Some(target) = self.struct_target(path) else { return };
        let required = match rest {
            PatFieldsRest::Rest(_) => None,
            PatFieldsRest::None => Some(false),
            PatFieldsRest::Recovered(_) => return,
        };
        let written: Vec<(Ident, Span)> = fields.iter().map(|f| (f.ident, f.span)).collect();
        self.check_fields(target, path, &written, required);
    }

    // --- missing-match-arms -------------------------------------------------------------

    /// A sub-pattern that matches everything of its type.
    fn irrefutable(&self, p: &Pat) -> bool {
        match &p.kind {
            PatKind::Wild | PatKind::Rest => true,
            // A lowercase name with nothing to resolve to is a binding, not a constant.
            PatKind::Ident(_, ident, None) => {
                starts_lowercase(ident.name.as_str())
                    && matches!(self.lookup(ident.name, Ns::Value), Lookup::Missing)
            }
            PatKind::Ident(_, _, Some(sub)) => self.irrefutable(sub),
            PatKind::Paren(inner) | PatKind::Ref(inner, ..) => self.irrefutable(inner),
            PatKind::Tuple(pats) => pats.iter().all(|p| self.irrefutable(p)),
            _ => false,
        }
    }

    fn arm_cover(&self, pat: &Pat, d: DefIx, deref: bool, covered: &mut [bool]) -> Cover {
        let Some((e, _)) = self.enum_def(d) else { return Cover::Bail };
        match &pat.kind {
            PatKind::Paren(inner) => self.arm_cover(inner, d, deref, covered),
            PatKind::Ref(inner, ..) if deref => self.arm_cover(inner, d, false, covered),
            PatKind::Or(pats) => {
                let mut partial = None;
                for p in pats.iter() {
                    match self.arm_cover(p, d, deref, covered) {
                        Cover::Covered => {}
                        Cover::Partial(v) => partial = Some(v),
                        other => return other,
                    }
                }
                partial.map_or(Cover::Covered, Cover::Partial)
            }
            PatKind::Wild | PatKind::Ident(_, _, None) => Cover::CatchAll,
            PatKind::Ident(_, _, Some(sub)) => self.arm_cover(sub, d, deref, covered),
            PatKind::Path(None, path) => match self.variant_path(path) {
                Some((pd, v)) if pd == d && matches!(e.variants[v].data, VariantData::Unit(_)) => {
                    covered[v] = true;
                    Cover::Covered
                }
                _ => Cover::Bail,
            },
            PatKind::TupleStruct(None, path, subs) => {
                let Some((pd, v)) = self.variant_path(path) else { return Cover::Bail };
                if pd != d {
                    return Cover::Bail;
                }
                let VariantData::Tuple(fields, _) = &e.variants[v].data else {
                    return Cover::Bail;
                };
                let rest = subs.iter().any(|p| matches!(p.kind, PatKind::Rest));
                let named = subs.iter().filter(|p| !matches!(p.kind, PatKind::Rest)).count();
                if (rest && named > fields.len()) || (!rest && named != fields.len()) {
                    return Cover::Bail;
                }
                if subs.iter().all(|p| self.irrefutable(p)) {
                    covered[v] = true;
                    Cover::Covered
                } else {
                    Cover::Partial(v)
                }
            }
            PatKind::Struct(None, path, fields, rest) => {
                let Some((pd, v)) = self.variant_path(path) else { return Cover::Bail };
                if pd != d
                    || !matches!(e.variants[v].data, VariantData::Struct { .. })
                    || matches!(rest, PatFieldsRest::Recovered(_))
                {
                    return Cover::Bail;
                }
                if fields.iter().all(|f| f.attrs.is_empty() && self.irrefutable(&f.pat)) {
                    covered[v] = true;
                    Cover::Covered
                } else {
                    Cover::Partial(v)
                }
            }
            _ => Cover::Bail,
        }
    }

    /// Whether a type certainly has a value. Exhaustiveness does not ask for variants whose
    /// fields are visibly uninhabited, so a missing variant is only reported when every field
    /// is known to be inhabited. Unknown means no.
    fn inhabited(&self, ty: &Ty, generics: &Generics, scope: ScopeId, depth: u32) -> bool {
        if depth > 4 {
            return false;
        }
        match &ty.kind {
            TyKind::Ref(..) | TyKind::PinnedRef(..) | TyKind::Ptr(_) | TyKind::FnPtr(_) => true,
            TyKind::Tup(tys) => tys.iter().all(|t| self.inhabited(t, generics, scope, depth)),
            TyKind::Paren(inner) => self.inhabited(inner, generics, scope, depth),
            TyKind::Path(None, path) => {
                let [seg] = &path.segments[..] else { return false };
                let name = seg.ident.name;
                if generics.params.iter().any(|p| p.ident.name == name) {
                    return false;
                }
                match self.ix.lookup(scope, name, Ns::Type) {
                    Lookup::Found(Binding::Def(d)) if self.ix.defs[d].exact => {
                        let inner = self.ix.defs[d].scope;
                        match self.ix.defs[d].kind {
                            DefKind::Struct(data, g) => data
                                .fields()
                                .iter()
                                .all(|f| self.inhabited(&f.ty, g, inner, depth + 1)),
                            DefKind::Enum(e, g) => e.variants.iter().any(|v| {
                                v.data
                                    .fields()
                                    .iter()
                                    .all(|f| self.inhabited(&f.ty, g, inner, depth + 1))
                            }),
                            _ => false,
                        }
                    }
                    // Prelude types a pattern cannot see into: private fields, or `None`.
                    Lookup::Missing => matches!(
                        name.as_str(),
                        "bool"
                            | "char"
                            | "i8"
                            | "i16"
                            | "i32"
                            | "i64"
                            | "i128"
                            | "isize"
                            | "u8"
                            | "u16"
                            | "u32"
                            | "u64"
                            | "u128"
                            | "usize"
                            | "f16"
                            | "f32"
                            | "f64"
                            | "f128"
                            | "String"
                            | "Vec"
                            | "Option"
                    ),
                    _ => false,
                }
            }
            _ => false,
        }
    }

    /// rust-analyzer's witness text for a variant no arm touches.
    fn witness(v: &ast::Variant) -> String {
        match &v.data {
            VariantData::Unit(_) => format!("{}", v.ident),
            VariantData::Tuple(fields, _) if fields.is_empty() => format!("{}", v.ident),
            VariantData::Tuple(fields, _) => {
                let wildcards = vec!["_"; fields.len()].join(", ");
                format!("{}({wildcards})", v.ident)
            }
            VariantData::Struct { fields, .. } if fields.is_empty() => {
                format!("{} {{  }}", v.ident)
            }
            VariantData::Struct { .. } => format!("{} {{ .. }}", v.ident),
        }
    }

    fn check_match(&mut self, scrutinee: &Expr, arms: &[ast::Arm]) {
        if !self.en.match_arms || self.suppress > 0 {
            return;
        }
        let Some((d, recv)) = self.expr_shape(scrutinee) else { return };
        let Some((e, generics)) = self.enum_def(d) else { return };
        if !matches!(self.ix.variants.get(&d), Some(Some(_))) {
            return;
        }
        let n = e.variants.len();
        let mut covered = vec![false; n];
        let mut partial = vec![false; n];
        for arm in arms {
            if !arm.attrs.is_empty() {
                return;
            }
            // A guarded arm covers nothing.
            if arm.guard.is_some() {
                continue;
            }
            match self.arm_cover(&arm.pat, d, recv != Recv::Value, &mut covered) {
                Cover::Covered => {}
                Cover::Partial(v) => partial[v] = true,
                Cover::CatchAll | Cover::Bail => return,
            }
        }
        let missing: Vec<usize> = (0..n).filter(|&v| !covered[v]).collect();
        if missing.is_empty() || missing.iter().any(|&v| partial[v]) {
            return;
        }
        let scope = self.ix.defs[d].scope;
        for &v in &missing {
            let fields = e.variants[v].data.fields();
            if !fields.iter().all(|f| self.inhabited(&f.ty, generics, scope, 0)) {
                return;
            }
        }
        let witnesses: Vec<String> = missing
            .iter()
            .map(|&v| format!("{}{}", recv.prefix(), Self::witness(&e.variants[v])))
            .collect();
        const LIMIT: usize = 3;
        let uncovered = match witnesses.len() {
            1 => format!("`{}` not covered", witnesses[0]),
            k if k - 1 < LIMIT => {
                let head = witnesses[..k - 1].join("`, `");
                format!("`{head}` and `{}` not covered", witnesses[k - 1])
            }
            k => {
                let head = witnesses[..LIMIT].join("`, `");
                format!("`{head}` and {} more not covered", k - LIMIT)
            }
        };
        self.emit("E0004", format!("missing match arm: {uncovered}"), scrutinee.span);
    }

    // --- walking ------------------------------------------------------------------------

    fn visit_let(&mut self, local: &'a ast::Local) {
        let hush = is_unsettled(&local.attrs);
        if hush {
            self.suppress += 1;
        }
        if let Some(ty) = &local.ty {
            self.visit_ty(ty);
        }
        let mut shape = local.ty.as_ref().and_then(|t| self.ty_shape(t));
        let mut closure = None;
        match &local.kind {
            LocalKind::Decl => {}
            LocalKind::Init(init) => {
                self.visit_expr(init);
                if local.ty.is_none() {
                    shape = self.expr_shape(init);
                    closure = closure_arity(init);
                }
            }
            LocalKind::InitElse(init, els) => {
                self.visit_expr(init);
                self.visit_block(els);
                shape = None;
            }
        }
        self.visit_pat(&local.pat);
        let info = if hush {
            self.plain()
        } else {
            LocalInfo { fn_id: self.fn_id, closure_arity: closure, shape }
        };
        self.bind_pat(&local.pat, info);
        if hush {
            self.suppress -= 1;
        }
    }

    fn visit_fn_param(&mut self, p: &'a Param) {
        let hush = is_unsettled(&p.attrs);
        if hush {
            self.suppress += 1;
        }
        self.visit_ty(&p.ty);
        self.visit_pat(&p.pat);
        let info = LocalInfo { shape: self.ty_shape(&p.ty), ..self.plain() };
        self.bind_pat(&p.pat, info);
        if hush {
            self.suppress -= 1;
        }
    }
}

impl<'a, 'b, 'c> Visitor<'a> for Checker<'a, 'b, 'c> {
    type Result = ();

    fn visit_item(&mut self, item: &'a Item) {
        let hush = is_unsettled(&item.attrs);
        if hush {
            self.suppress += 1;
        }
        // Nothing nested in an item sees the enclosing function's `self` or `Self`.
        let saved = (self.self_ty, self.self_value, self.self_param, self.cur);
        self.self_ty = None;
        self.self_value = false;
        self.self_param = None;
        self.push_scope();
        let mut lost = false;
        match &item.kind {
            ItemKind::Mod(_, _, ModKind::Loaded(..)) => {
                match self.ix.mods.get(&(item as *const Item)) {
                    Some(&s) => self.cur = s,
                    None => lost = true,
                }
            }
            ItemKind::Impl(imp) => {
                self.self_ty = self.ix.impl_self(self.cur, imp).map(|(d, _)| d);
            }
            _ => {}
        }
        if lost {
            self.suppress += 1;
        }
        visit::walk_item(self, item);
        if lost {
            self.suppress -= 1;
        }
        self.pop_scope();
        (self.self_ty, self.self_value, self.self_param, self.cur) = saved;
        if hush {
            self.suppress -= 1;
        }
    }

    fn visit_assoc_item(&mut self, item: &'a AssocItem, ctxt: AssocCtxt) {
        let hush = is_unsettled(&item.attrs);
        if hush {
            self.suppress += 1;
        }
        let saved = (self.self_value, self.self_param);
        self.self_value = false;
        self.self_param = None;
        visit::walk_assoc_item(self, item, ctxt);
        (self.self_value, self.self_param) = saved;
        if hush {
            self.suppress -= 1;
        }
    }

    fn visit_foreign_item(&mut self, item: &'a ForeignItem) {
        let hush = is_unsettled(&item.attrs);
        if hush {
            self.suppress += 1;
        }
        visit::walk_item(self, item);
        if hush {
            self.suppress -= 1;
        }
    }

    fn visit_fn(&mut self, fk: FnKind<'a>, _attrs: &AttrVec, _span: Span, _id: NodeId) {
        match fk {
            FnKind::Fn(ctxt, _, f) => {
                let saved = (self.fn_id, self.self_ty, self.self_value, self.self_param);
                self.next_fn_id += 1;
                self.fn_id = self.next_fn_id;
                let decl = &f.sig.decl;
                if let FnCtxt::Assoc(_) = ctxt {
                    self.self_value = decl.has_self();
                    self.self_param = decl.inputs.first().and_then(declared_recv);
                } else {
                    self.self_ty = None;
                    self.self_value = false;
                    self.self_param = None;
                }
                self.push_scope();
                self.visit_generics(&f.generics);
                for p in decl.inputs.iter() {
                    self.visit_fn_param(p);
                }
                visit::walk_fn_ret_ty(self, &decl.output);
                if let Some(body) = &f.body {
                    self.visit_block(body);
                }
                self.pop_scope();
                (self.fn_id, self.self_ty, self.self_value, self.self_param) = saved;
            }
            FnKind::Closure(_, _, decl, body) => {
                self.push_scope();
                for p in decl.inputs.iter() {
                    self.visit_fn_param(p);
                }
                visit::walk_fn_ret_ty(self, &decl.output);
                self.visit_expr(body);
                self.pop_scope();
            }
        }
    }

    fn visit_generics(&mut self, g: &'a Generics) {
        let plain = self.plain();
        for p in g.params.iter() {
            if !matches!(p.kind, GenericParamKind::Lifetime) {
                self.add_local(p.ident.name, plain);
            }
        }
        visit::walk_generics(self, g);
    }

    fn visit_block(&mut self, b: &'a ast::Block) {
        let saved = self.cur;
        let lost = match self.ix.blocks.get(&(b as *const ast::Block)) {
            Some(&s) => {
                self.cur = s;
                false
            }
            None => true,
        };
        if lost {
            self.suppress += 1;
        }
        self.push_scope();
        for s in b.stmts.iter() {
            self.visit_stmt(s);
        }
        self.pop_scope();
        if lost {
            self.suppress -= 1;
        }
        self.cur = saved;
    }

    fn visit_stmt(&mut self, s: &'a Stmt) {
        match &s.kind {
            StmtKind::Let(local) => self.visit_let(local),
            // Tokens only; the block is already incomplete.
            StmtKind::MacCall(_) => {}
            _ => visit::walk_stmt(self, s),
        }
    }

    fn visit_arm(&mut self, arm: &'a ast::Arm) {
        let hush = is_unsettled(&arm.attrs);
        if hush {
            self.suppress += 1;
        }
        self.push_scope();
        self.visit_pat(&arm.pat);
        let plain = self.plain();
        self.bind_pat(&arm.pat, plain);
        if let Some(guard) = &arm.guard {
            self.visit_expr(&guard.cond);
        }
        if let Some(body) = &arm.body {
            self.visit_expr(body);
        }
        self.pop_scope();
        if hush {
            self.suppress -= 1;
        }
    }

    fn visit_pat(&mut self, p: &'a Pat) {
        self.in_pattern += 1;
        if let PatKind::Struct(None, path, fields, rest) = &p.kind {
            self.check_struct_pat(path, fields, rest);
        }
        visit::walk_pat(self, p);
        self.in_pattern -= 1;
    }

    fn visit_expr(&mut self, e: &'a Expr) {
        let hush = is_unsettled(&e.attrs);
        if hush {
            self.suppress += 1;
        }
        match &e.kind {
            ExprKind::Path(None, path) => {
                self.check_ident(e, path);
                visit::walk_expr(self, e);
            }
            ExprKind::Call(callee, args) => {
                self.check_call(e, callee, args);
                visit::walk_expr(self, e);
            }
            ExprKind::MethodCall(mc) => {
                self.check_method(e, mc);
                visit::walk_expr(self, e);
            }
            ExprKind::Struct(se) => {
                self.check_struct_expr(se);
                visit::walk_expr(self, e);
            }
            ExprKind::Match(scrutinee, arms, _) => {
                self.visit_expr(scrutinee);
                self.check_match(scrutinee, arms);
                for arm in arms.iter() {
                    self.visit_arm(arm);
                }
            }
            // `if let` and `while let` bindings live in the condition and the body only.
            ExprKind::If(cond, then, els) => {
                self.push_scope();
                self.visit_expr(cond);
                self.visit_block(then);
                self.pop_scope();
                if let Some(els) = els {
                    self.visit_expr(els);
                }
            }
            ExprKind::While(cond, body, _) => {
                self.push_scope();
                self.visit_expr(cond);
                self.visit_block(body);
                self.pop_scope();
            }
            ExprKind::Let(pat, scrutinee, ..) => {
                self.visit_expr(scrutinee);
                self.visit_pat(pat);
                let plain = self.plain();
                self.bind_pat(pat, plain);
            }
            ExprKind::ForLoop(fl) => {
                self.visit_expr(&fl.iter);
                self.push_scope();
                self.visit_pat(&fl.pat);
                let plain = self.plain();
                self.bind_pat(&fl.pat, plain);
                self.visit_block(&fl.body);
                self.pop_scope();
            }
            ExprKind::Assign(lhs, rhs, _) => {
                self.visit_expr(rhs);
                self.assignee += 1;
                self.visit_expr(lhs);
                self.assignee -= 1;
            }
            // Tokens only: nothing inside a macro call is visited.
            ExprKind::MacCall(_) => {}
            _ => visit::walk_expr(self, e),
        }
        if hush {
            self.suppress -= 1;
        }
    }

    fn visit_mac_call(&mut self, _mac: &'a ast::MacCall) {}

    // `#[attr = expr]` holds an expression, which is attribute input, not code in scope.
    fn visit_attribute(&mut self, _attr: &'a Attribute) {}
}

// ---------------------------------------------------------------------------------------------
// trait-impl-missing-assoc-item and trait-impl-redundant-assoc-item
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum AssocKind {
    Fn,
    Const,
    Type,
}

impl AssocKind {
    fn keyword(self) -> &'static str {
        match self {
            AssocKind::Fn => "fn",
            AssocKind::Const => "const",
            AssocKind::Type => "type",
        }
    }

    fn is_type(self) -> bool {
        self == AssocKind::Type
    }
}

struct AssocFacts {
    ident: Ident,
    kind: AssocKind,
    has_default: bool,
    /// Carries a `where` clause, which can make it optional (`where Self: Sized` on an unsized
    /// self type). Deciding that needs types, so a missing bounded item is never reported.
    bounded: bool,
    specializable: bool,
}

/// `None` for an item this module cannot account for: a macro, a delegation, a `#[cfg]`.
fn assoc_facts(item: &AssocItem) -> Option<AssocFacts> {
    if is_unsettled(&item.attrs) {
        return None;
    }
    let facts = match &item.kind {
        AssocItemKind::Fn(f) => AssocFacts {
            ident: f.ident,
            kind: AssocKind::Fn,
            has_default: f.body.is_some(),
            bounded: !f.generics.where_clause.predicates.is_empty(),
            specializable: matches!(f.defaultness, Defaultness::Default(_)),
        },
        AssocItemKind::Const(c) => AssocFacts {
            ident: c.ident,
            kind: AssocKind::Const,
            has_default: c.body.is_some(),
            bounded: !c.generics.where_clause.predicates.is_empty(),
            specializable: matches!(c.defaultness, Defaultness::Default(_)),
        },
        AssocItemKind::Type(t) => AssocFacts {
            ident: t.ident,
            kind: AssocKind::Type,
            has_default: t.ty.is_some(),
            bounded: !t.generics.where_clause.predicates.is_empty()
                || !t.after_where_clause.predicates.is_empty(),
            specializable: matches!(t.defaultness, Defaultness::Default(_)),
        },
        AssocItemKind::MacCall(_)
        | AssocItemKind::Delegation(_)
        | AssocItemKind::DelegationMac(_) => return None,
    };
    Some(facts)
}

/// A trait's items, keyed by name and namespace.
struct TraitTable {
    items: Vec<AssocFacts>,
    by_name: FxHashMap<(Symbol, bool), usize>,
}

fn trait_table(tr: &ast::Trait) -> Option<TraitTable> {
    let mut table = TraitTable { items: Vec::new(), by_name: FxHashMap::default() };
    for item in tr.items.iter() {
        let facts = assoc_facts(item)?;
        table.by_name.insert((facts.ident.name, facts.kind.is_type()), table.items.len());
        table.items.push(facts);
    }
    Some(table)
}

fn impl_trait<'a>(ix: &Index<'a>, rec: &ImplRec<'a>) -> Option<(DefIx, &'a ast::Trait)> {
    let header = rec.imp.of_trait.as_deref()?;
    let [seg] = &header.trait_ref.path.segments[..] else { return None };
    let d = ix.exact_def(rec.scope, seg.ident.name, Ns::Type)?;
    match ix.defs[d].kind {
        DefKind::Trait(t) => Some((d, t)),
        _ => None,
    }
}

fn check_trait_impls(cx: &Cx<'_>, ix: &Index<'_>, en: Enabled, out: &mut Vec<Diagnostic>) {
    // Specialization lets one impl supply another's items, so a trait with any `default` item
    // or `default impl` anywhere is not checked for missing items at all.
    let mut resolved = Vec::new();
    let mut specialized: FxHashSet<DefIx> = FxHashSet::default();
    for (i, rec) in ix.impls.iter().enumerate() {
        let Some((t, _)) = impl_trait(ix, rec) else { continue };
        let header = rec.imp.of_trait.as_deref();
        let default_impl = header.is_some_and(|h| matches!(h.defaultness, Defaultness::Default(_)));
        let default_item = rec
            .imp
            .items
            .iter()
            .any(|it| assoc_facts(it).is_none_or(|f| f.specializable));
        if default_impl || default_item {
            specialized.insert(t);
        }
        resolved.push((i, t));
    }

    let mut tables: FxHashMap<DefIx, Option<TraitTable>> = FxHashMap::default();
    for (i, t) in resolved {
        let rec = &ix.impls[i];
        let Some(header) = rec.imp.of_trait.as_deref() else { continue };
        // Negative impls provide no items, and are not asked for any.
        if !rec.exact || matches!(header.polarity, ImplPolarity::Negative(_)) {
            continue;
        }
        let DefKind::Trait(tr) = ix.defs[t].kind else { continue };
        let Some(table) = tables.entry(t).or_insert_with(|| trait_table(tr)).as_ref() else {
            continue;
        };
        let Some(items) = rec.imp.items.iter().map(|it| assoc_facts(it)).collect::<Option<Vec<_>>>()
        else {
            continue;
        };

        let mut present = vec![false; table.items.len()];
        let mut redundant = Vec::new();
        let mut clash = false;
        for (item, facts) in rec.imp.items.iter().zip(&items) {
            match table.by_name.get(&(facts.ident.name, facts.kind.is_type())) {
                Some(&ti) if table.items[ti].kind == facts.kind => present[ti] = true,
                // Same name, other kind: rustc says E0323 or E0324, not this.
                Some(_) => clash = true,
                None => {
                    if table.by_name.contains_key(&(facts.ident.name, !facts.kind.is_type())) {
                        clash = true;
                    } else {
                        redundant.push((item.span, facts));
                    }
                }
            }
        }
        if clash {
            continue;
        }

        if en.trait_redundant {
            let trait_name = ix.defs[t].ident;
            for (span, facts) in redundant {
                let message = format!(
                    "`{} {}` is not a member of trait `{trait_name}`",
                    facts.kind.keyword(),
                    facts.ident
                );
                push(cx, out, "E0407", message, span);
            }
        }

        if en.trait_missing && !specialized.contains(&t) {
            let missing: Vec<&AssocFacts> = table
                .items
                .iter()
                .zip(&present)
                .filter(|(f, p)| !**p && !f.has_default)
                .map(|(f, _)| f)
                .collect();
            if !missing.is_empty() && missing.iter().all(|f| !f.bounded) {
                let list: Vec<String> = missing
                    .iter()
                    .map(|f| format!("`{} {}`", f.kind.keyword(), f.ident))
                    .collect();
                let message =
                    format!("not all trait items implemented, missing: {}", list.join(", "));
                push(cx, out, "E0046", message, header.trait_ref.path.span);
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// unresolved-import
// ---------------------------------------------------------------------------------------------

fn check_imports(cx: &Cx<'_>, ix: &Index<'_>, out: &mut Vec<Diagnostic>) {
    let mut path = Vec::new();
    for rec in &ix.uses {
        if rec.exact {
            walk_use(cx, ix, rec.scope, rec.tree, &mut path, out);
        }
    }
}

fn walk_use(
    cx: &Cx<'_>,
    ix: &Index<'_>,
    module: ScopeId,
    tree: &UseTree,
    path: &mut Vec<Ident>,
    out: &mut Vec<Diagnostic>,
) {
    let base = path.len();
    path.extend(tree.prefix.segments.iter().map(|s| s.ident));
    match &tree.kind {
        UseTreeKind::Simple(rename) => {
            if import_unresolved(ix, module, path) {
                let span = match rename {
                    Some(r) => tree.prefix.span.to(r.span),
                    None => tree.prefix.span,
                };
                push(cx, out, "E0432", "unresolved import".to_string(), span);
            }
        }
        UseTreeKind::Nested { items, .. } => {
            for (t, _) in items.iter() {
                walk_use(cx, ix, module, t, path, out);
            }
        }
        UseTreeKind::Glob(_) => {}
    }
    path.truncate(base);
}

/// True only when the path certainly names nothing: it starts at `crate`, `self`, `super` or a
/// module declared in this module, every step is an exact inline module (or finally an enum),
/// and the last name is absent from a complete module in every namespace. A path that leaves
/// the file, or passes through anything else, is not decided.
fn import_unresolved(ix: &Index<'_>, module: ScopeId, path: &[Ident]) -> bool {
    let Some(first) = path.first() else { return false };
    let mut m;
    let mut i = 1;
    if first.name == kw::Crate {
        m = 0;
    } else if first.name == kw::SelfLower {
        m = module;
    } else if first.name == kw::Super {
        let Some(p) = ix.scopes[module].module_parent else { return false };
        m = p;
        while i < path.len() && path[i].name == kw::Super {
            let Some(p) = ix.scopes[m].module_parent else { return false };
            m = p;
            i += 1;
        }
    } else if first.name == kw::PathRoot {
        return false;
    } else {
        // A leading plain name is a module of this one, or else possibly a crate.
        match ix.scopes[module].types.get(&first.name) {
            Some(Binding::Def(d)) if ix.defs[*d].exact => match ix.defs[*d].kind {
                DefKind::Mod(s) => m = s,
                _ => return false,
            },
            _ => return false,
        }
    }
    if i >= path.len() {
        return false;
    }
    while i < path.len() {
        let seg = path[i];
        let s = &ix.scopes[m];
        if i + 1 == path.len() {
            if seg.name == kw::SelfLower {
                return false;
            }
            let present = s.values.contains_key(&seg.name)
                || s.types.contains_key(&seg.name)
                || ix.macros.contains(&seg.name);
            return !present && !s.incomplete;
        }
        if seg.name == kw::SelfLower
            || seg.name == kw::Super
            || seg.name == kw::Crate
            || seg.name == kw::PathRoot
        {
            return false;
        }
        match s.types.get(&seg.name) {
            Some(Binding::Def(d)) if ix.defs[*d].exact => match ix.defs[*d].kind {
                DefKind::Mod(inner) => m = inner,
                DefKind::Enum(..) if i + 2 == path.len() => {
                    let v = path[i + 1];
                    if v.name == kw::SelfLower {
                        return false;
                    }
                    return match ix.variants.get(d) {
                        Some(Some(map)) => !map.contains_key(&v.name),
                        _ => false,
                    };
                }
                _ => return false,
            },
            Some(_) => return false,
            None => {
                let present = s.values.contains_key(&seg.name) || ix.macros.contains(&seg.name);
                return !present && !s.incomplete;
            }
        }
        i += 1;
    }
    false
}

// ---------------------------------------------------------------------------------------------
// Tests, ported from rust-analyzer's handler tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;

    use crate::rustc_parse::lexer::StripTokens;
    use crate::rustc_parse::{new_parser_from_source_str, unwrap_or_emit_fatal};
    use crate::rustc_session::parse::ParseSess;
    use crate::rustc_span::{FileName, Span, create_default_session_globals_then};

    /// Parse `src` and run the checks, returning `(code, message, covered text)` in source
    /// order.
    ///
    /// This duplicates the parse that `diagnostics::diagnose` does in `mod.rs`, because these
    /// tests were written before that entry point existed. It should be folded into it, and the
    /// tests pointed at `diagnose`, once both are in the tree.
    fn run(src: &str) -> Vec<(String, String, String)> {
        run_with(src, None)
    }

    fn run_with(src: &str, codes: Option<Vec<String>>) -> Vec<(String, String, String)> {
        create_default_session_globals_then(|| {
            let psess = ParseSess::new();
            let mut parser = unwrap_or_emit_fatal(new_parser_from_source_str(
                &psess,
                FileName::Custom("names.rs".to_string()),
                src.to_string(),
                StripTokens::Nothing,
            ));
            let krate = match parser.parse_crate_mod() {
                Ok(krate) => krate,
                Err(err) => {
                    err.emit();
                    panic!("fixture does not parse");
                }
            };
            assert!(psess.dcx().has_errors().is_none(), "fixture does not parse cleanly");
            let sm = psess.source_map();
            let span = |sp: Span| {
                (sm.lookup_byte_offset(sp.lo()).pos.0, sm.lookup_byte_offset(sp.hi()).pos.0)
            };
            let opts =
                super::super::Options { codes, min_severity: super::super::Severity::WeakWarning };
            let cx = super::super::Cx { source: src, span: &span, opts: &opts };
            let mut out = Vec::new();
            super::check(&cx, &krate, &mut out);
            out.sort_by_key(|d| (d.start, d.end));
            out.into_iter()
                .map(|d| (d.code, d.message, src[d.start as usize..d.end as usize].to_string()))
                .collect()
        })
    }

    #[track_caller]
    fn expect(src: &str, want: &[(&str, &str, &str)]) {
        let got = run(src);
        let want: Vec<(String, String, String)> = want
            .iter()
            .map(|(c, m, t)| (c.to_string(), m.to_string(), t.to_string()))
            .collect();
        assert_eq!(got, want);
    }

    #[track_caller]
    fn clean(src: &str) {
        expect(src, &[]);
    }

    const NO_VALUE: &str = "no such value in this scope";

    // --- unresolved_ident.rs --------------------------------------------------------------

    #[test]
    fn ident_missing() {
        expect("fn main() {\n    let _ = x;\n}\n", &[("E0425", NO_VALUE, "x")]);
    }

    #[test]
    fn ident_present() {
        clean("fn main() {\n    let x = 5;\n    let _ = x;\n}\n");
    }

    #[test]
    fn ident_unresolved_self_val() {
        // rust-analyzer's fixture continues with `let self: self = self;`, which rustc's parser
        // refuses outright, so only the first half is ported.
        expect("fn main() {\n    self.a;\n}\n", &[("E0425", NO_VALUE, "self")]);
    }

    // rust-analyzer's `feature` test checks `{unresolved}` inside `format_args!`. Nothing here
    // looks inside a macro call, so it is not ported.

    #[test]
    fn ident_prelude_and_items_resolve() {
        clean(
            "fn helper() {}\nconst limit: u8 = 1;\nfn main() {\n    helper();\n    drop(limit);\n    let _ = size_of::<u8>();\n    later();\n    fn later() {}\n}\n",
        );
    }

    #[test]
    fn ident_scopes() {
        expect(
            "fn main() {\n    if let Some(a) = f() { a; } else { a; }\n    for b in f() { b; }\n    b;\n    match f() { c => c, }\n    let g = |d| d;\n    d;\n}\nfn f() {}\n",
            &[("E0425", NO_VALUE, "a"), ("E0425", NO_VALUE, "b"), ("E0425", NO_VALUE, "d")],
        );
    }

    #[test]
    fn ident_modules_do_not_see_parent_items() {
        expect(
            "fn top() {}\nmod m {\n    fn f() { top(); }\n}\n",
            &[("E0425", NO_VALUE, "top")],
        );
    }

    #[test]
    fn ident_incomplete_scopes_stay_silent() {
        // A glob, an item macro, a statement macro, a pattern macro, an attribute macro and a
        // `#[cfg]`: in every case the name could exist once the program is expanded.
        clean("use other::*;\nfn main() { let _ = x; }\n");
        clean("some_macro! {}\nfn main() { let _ = x; }\n");
        clean("fn main() { make_x!(x); let _ = x; }\n");
        clean("fn main() { let pat!(x) = 1; let _ = x; }\n");
        clean("#[my_attribute]\nfn main() { let _ = x; }\n");
        clean("#[cfg(any())]\nfn main() { let _ = x; }\n");
        clean("#[derive(Serialize)]\nstruct S;\nfn main() { let _ = x; }\n");
    }

    #[test]
    fn ident_uppercase_and_qualified_are_skipped() {
        clean("fn main() { let _ = X; let _ = a::b; let _ = <u8>::MAX; }\n");
    }

    // --- mismatched_arg_count.rs ----------------------------------------------------------

    #[test]
    fn simple_free_fn_zero() {
        expect(
            "fn zero() {}\nfn f() { zero(1); }\n",
            &[("E0061", "expected 0 arguments, found 1", "(1)")],
        );
        clean("fn zero() {}\nfn f() { zero(); }\n");
    }

    #[test]
    fn simple_free_fn_one() {
        expect(
            "fn one(_arg: u8) {}\nfn f() { one(); }\n",
            &[("E0061", "expected 1 argument, found 0", "()")],
        );
        clean("fn one(_arg: u8) {}\nfn f() { one(1); }\n");
    }

    #[test]
    fn method_as_fn() {
        expect(
            "struct S;\nimpl S { fn method(&self) {} }\n\nfn f() {\n    S::method();\n}\n",
            &[("E0061", "expected 1 argument, found 0", "()")],
        );
        clean(
            "struct S;\nimpl S { fn method(&self) {} }\n\nfn f() {\n    S::method(&S);\n    S.method();\n}\n",
        );
    }

    #[test]
    fn method_with_arg() {
        // rust-analyzer also reports `S.method()`, where `S` autorefs to `&S`. The autoref step
        // comes after a by-value step at which a trait method in scope could win, and this
        // module cannot see the traits in scope, so that call is skipped. The receiver written
        // as `&S` is decided.
        clean("struct S;\nimpl S { fn method(&self, _arg: u8) {} }\n\nfn f() {\n    S.method();\n}\n");
        expect(
            "struct S;\nimpl S { fn method(&self, _arg: u8) {} }\n\nfn f() {\n    (&S).method();\n}\n",
            &[("E0061", "expected 1 argument, found 0", "()")],
        );
        clean(
            "struct S;\nimpl S { fn method(&self, _arg: u8) {} }\n\nfn f() {\n    S::method(&S, 0);\n    (&S).method(1);\n}\n",
        );
    }

    #[test]
    fn method_on_self() {
        expect(
            "struct S;\nimpl S {\n    fn one(&self, _a: u8) {}\n    fn go(&self) { self.one(1, 2); }\n}\n",
            &[("E0061", "expected 1 argument, found 2", "2)")],
        );
    }

    #[test]
    fn method_unknown_receiver() {
        clean("trait Foo { fn method(&self, _arg: usize) {} }\n\nfn f() {\n    let x;\n    x.method();\n}\n");
    }

    #[test]
    fn tuple_struct() {
        expect(
            "struct Tup(u8, u16);\nfn f() {\n    Tup(0);\n}\n",
            &[("E0061", "expected 2 arguments, found 1", ")")],
        );
    }

    #[test]
    fn enum_variant() {
        expect(
            "enum En { Variant(u8, u16), }\nfn f() {\n    En::Variant(0);\n}\n",
            &[("E0061", "expected 2 arguments, found 1", ")")],
        );
    }

    #[test]
    fn enum_variant_type_macro() {
        expect(
            "macro_rules! Type {\n    () => { u32 };\n}\nenum Foo {\n    Bar(Type![])\n}\nimpl Foo {\n    fn new() {\n        Foo::Bar(0);\n        Foo::Bar(0, 1);\n        Foo::Bar();\n    }\n}\n",
            &[
                ("E0061", "expected 1 argument, found 2", "1)"),
                ("E0061", "expected 1 argument, found 0", "()"),
            ],
        );
    }

    #[test]
    fn varargs() {
        expect(
            "extern \"C\" {\n    fn fixed(fixed: u8);\n    fn varargs(fixed: u8, ...);\n    fn varargs2(...);\n}\n\nfn f() {\n    unsafe {\n        fixed(0);\n        fixed(0, 1);\n        varargs(0);\n        varargs(0, 1);\n        varargs2();\n        varargs2(0);\n        varargs2(0, 1);\n    }\n}\n",
            &[("E0061", "expected 1 argument, found 2", "1)")],
        );
    }

    #[test]
    fn arg_count_lambda() {
        expect(
            "fn main() {\n    let f = |()| ();\n    f();\n    f(());\n    f((), ());\n}\n",
            &[
                ("E0057", "expected 1 argument, found 0", "()"),
                ("E0057", "expected 1 argument, found 2", "())"),
            ],
        );
    }

    #[test]
    fn arg_count_multi_arg_closure() {
        expect(
            "fn main() {\n    let f = |_a: u8, _b: u8| ();\n    f();\n    f(1, 2);\n    f(1, 2, 3);\n}\n",
            &[
                ("E0057", "expected 2 arguments, found 0", "()"),
                ("E0057", "expected 2 arguments, found 3", "3)"),
            ],
        );
    }

    #[test]
    fn cfgd_out_call_arguments() {
        clean(
            "struct C(#[cfg(FALSE)] ());\nimpl C {\n    fn new() -> Self {\n        Self(\n            #[cfg(FALSE)]\n            (),\n        )\n    }\n\n    fn method(&self) {}\n}\n\nfn main() {\n    C::new().method(#[cfg(FALSE)] 0);\n}\n",
        );
    }

    #[test]
    fn cfgd_out_fn_params() {
        clean(
            "fn foo(#[cfg(NEVER)] x: ()) {}\n\nstruct S;\n\nimpl S {\n    fn method(#[cfg(NEVER)] self) {}\n    fn method2(#[cfg(NEVER)] self, _arg: u8) {}\n    fn method3(self, #[cfg(NEVER)] _arg: u8) {}\n}\n\nextern \"C\" {\n    fn fixed(fixed: u8, #[cfg(NEVER)] ...);\n    fn varargs(#[cfg(not(NEVER))] ...);\n}\n\nfn main() {\n    foo();\n    S::method();\n    S::method2(0);\n    S::method3(S);\n    S.method3();\n    unsafe {\n        fixed(0);\n        varargs(1, 2, 3);\n    }\n}\n",
        );
    }

    #[test]
    fn legacy_const_generics() {
        // The attribute changes the arity, and is not in the inert set, so the fn is skipped.
        clean(
            "#[rustc_legacy_const_generics(1, 3)]\nfn b<const N1: u8, const N2: u8>(\n    _a: u8,\n    _b: u8,\n) {}\n\nfn g() {\n    b(0, 1, 2);\n}\n",
        );
    }

    #[test]
    fn no_type_mismatches_when_arg_count_mismatch() {
        expect(
            "fn foo((): (), (): ()) {\n    foo(1, 2, 3);\n    foo(1);\n}\n",
            &[
                ("E0061", "expected 2 arguments, found 3", "3)"),
                ("E0061", "expected 2 arguments, found 1", ")"),
            ],
        );
    }

    #[test]
    fn regression_17233() {
        clean(
            "pub trait A {\n    type X: B;\n}\npub trait B: A {\n    fn confused_name(self, _: i32);\n}\n\npub struct Foo;\nimpl Foo {\n    pub fn confused_name(&self) {}\n}\n\npub fn repro<T: A>() {\n    Foo.confused_name();\n}\n",
        );
    }

    #[test]
    fn cfg_inside_macro_inside_arg() {
        clean(
            "fn foo() {}\n\nmacro_rules! make_X {\n    () => { #[cfg(false)] X };\n}\n\nfn main() {\n    foo(make_X!());\n}\n",
        );
    }

    #[test]
    fn local_shadows_fn() {
        clean("fn f(_a: u8) {}\nfn main() {\n    let f = |a: u8, b: u8| a + b;\n    f(1, 2);\n}\n");
    }

    // --- missing_fields.rs ----------------------------------------------------------------

    const MISSING_BAR: &str = "missing structure fields:\n- bar\n";

    #[test]
    fn missing_record_pat_field_diagnostic() {
        expect(
            "struct S { foo: i32, bar: () }\nfn baz(s: S) {\n    let S { foo: _ } = s;\n}\n",
            &[("E0063", MISSING_BAR, "S")],
        );
    }

    #[test]
    fn missing_record_pat_field_no_diagnostic_if_not_exhaustive() {
        clean("struct S { foo: i32, bar: () }\nfn baz(s: S) -> i32 {\n    match s {\n        S { foo, .. } => foo,\n    }\n}\n");
    }

    #[test]
    fn missing_record_pat_field_ref() {
        clean("struct S { s: u32 }\nfn x(a: S) {\n    let S { ref s } = a;\n    _ = s;\n}\n");
    }

    #[test]
    fn missing_record_expr_in_assignee_expr() {
        clean(
            "struct S { s: usize, t: usize }\nstruct S2 { s: S, t: () }\nstruct T(S);\nfn regular(a: S) {\n    let s;\n    S { s, .. } = a;\n    _ = s;\n}\nfn nested(a: S2) {\n    let s;\n    S2 { s: S { s, .. }, .. } = a;\n    _ = s;\n}\nfn in_tuple(a: (S,)) {\n    let s;\n    (S { s, .. },) = a;\n    _ = s;\n}\nfn in_array(a: [S;1]) {\n    let s;\n    [S { s, .. },] = a;\n    _ = s;\n}\nfn in_tuple_struct(a: T) {\n    let s;\n    T(S { s, .. }) = a;\n    _ = s;\n}\n",
        );
    }

    #[test]
    fn test_fill_struct_fields_no_diagnostic() {
        clean("struct TestStruct { one: i32, two: i64 }\n\nfn test_fn() {\n    let one = 1;\n    let _s = TestStruct{ one, two: 2 };\n}\n");
    }

    #[test]
    fn test_fill_struct_fields_no_diagnostic_on_spread() {
        clean("struct TestStruct { one: i32, two: i64 }\n\nfn test_fn() {\n    let one = 1;\n    let a = TestStruct{ one, two: 2 };\n    let _ = TestStruct{ ..a };\n}\n");
    }

    #[test]
    fn missing_fields_on_literal_and_self() {
        expect(
            "struct TestStruct { one: i32, two: i64 }\nimpl TestStruct {\n    fn new() -> Self { Self { one: 1 } }\n}\nfn f() { let _ = TestStruct { two: 2 }; }\n",
            &[
                ("E0063", "missing structure fields:\n- two\n", "Self"),
                ("E0063", "missing structure fields:\n- one\n", "TestStruct"),
            ],
        );
    }

    #[test]
    fn test_default_field_values_basic() {
        clean("#![feature(default_field_values)]\nstruct Struct {\n    a: usize = 0,\n    b: usize,\n}\n\nfn main() {\n    Struct { b: 1, .. };\n}\n");
    }

    #[test]
    fn test_default_field_values_missing_field_error() {
        expect(
            "#![feature(default_field_values)]\nstruct UserInfo {\n    id: i32,\n    age: f32 = 1.0,\n    email: String,\n}\n\nfn main() {\n    UserInfo { id: 20, .. };\n}\n",
            &[("E0063", "missing structure fields:\n- email\n", "UserInfo")],
        );
    }

    #[test]
    fn test_default_field_values_requires_spread_syntax() {
        expect(
            "#![feature(default_field_values)]\nstruct Point {\n    x: i32 = 0,\n    y: i32 = 0,\n}\n\nfn main() {\n    Point { x: 0 };\n}\n",
            &[("E0063", "missing structure fields:\n- y\n", "Point")],
        );
    }

    #[test]
    fn test_default_field_values_pattern_matching() {
        clean("#![feature(default_field_values)]\nstruct Point {\n    x: i32 = 0,\n    y: i32 = 0,\n    z: i32,\n}\n\nfn main() {\n    let Point { x, .. } = Point { z: 5, .. };\n}\n");
    }

    // --- no_such_field.rs -----------------------------------------------------------------

    #[test]
    fn dont_work_for_field_with_disabled_cfg() {
        clean(
            "struct Test {\n    #[cfg(feature = \"hello\")]\n    test: u32,\n    other: u32\n}\n\nfn main() {\n    let a = Test {\n        #[cfg(feature = \"hello\")]\n        test: 1,\n        other: 1\n    };\n\n    let Test {\n        #[cfg(feature = \"hello\")]\n        test,\n        mut other,\n        ..\n    } = a;\n\n    other += 1;\n}\n",
        );
    }

    #[test]
    fn no_such_field_diagnostics() {
        expect(
            "struct S { foo: i32, bar: () }\nimpl S {\n    fn new(\n        s@S {\n            foo,\n            baz: baz2,\n            qux\n        }: S\n    ) -> S {\n        S {\n            foo,\n            baz: baz2,\n            qux\n        } = s;\n        S {\n            foo: 92,\n            baz: 62,\n            qux\n        }\n    }\n}\n",
            &[
                ("E0063", MISSING_BAR, "S"),
                ("E0560", "no such field", "baz: baz2"),
                ("E0560", "no such field", "qux"),
                ("E0063", MISSING_BAR, "S"),
                ("E0560", "no such field", "baz: baz2"),
                ("E0560", "no such field", "qux"),
                ("E0063", MISSING_BAR, "S"),
                ("E0560", "no such field", "baz: 62"),
                ("E0560", "no such field", "qux"),
            ],
        );
    }

    #[test]
    fn no_such_field_with_feature_flag_diagnostics_on_struct_lit() {
        clean(
            "struct S {\n    #[cfg(feature = \"foo\")]\n    foo: u32,\n    #[cfg(not(feature = \"foo\"))]\n    bar: u32,\n}\n\nimpl S {\n    #[cfg(feature = \"foo\")]\n    fn new(foo: u32) -> Self {\n        Self { foo }\n    }\n    #[cfg(not(feature = \"foo\"))]\n    fn new(bar: u32) -> Self {\n        Self { bar }\n    }\n    fn new2(val: u32) -> Self {\n        Self {\n            #[cfg(feature = \"foo\")]\n            foo: val,\n            #[cfg(not(feature = \"foo\"))]\n            bar: val,\n        }\n    }\n}\n",
        );
    }

    #[test]
    fn no_such_field_with_type_macro() {
        clean("macro_rules! Type { () => { u32 }; }\nstruct Foo { bar: Type![] }\n\nimpl Foo {\n    fn new() -> Self {\n        Foo { bar: 0 }\n    }\n}\n");
    }

    #[test]
    fn no_such_field_on_variant() {
        expect(
            "enum E { V { a: u8 } }\nfn f() { let _ = E::V { a: 1, b: 2 }; }\n",
            &[("E0559", "no such field", "b: 2")],
        );
    }

    #[test]
    fn test_tuple_field_on_record_struct() {
        expect(
            "struct Struct {}\nfn main() {\n    Struct {\n        0: 0\n    };\n}\n",
            &[("E0560", "no such field", "0: 0")],
        );
    }

    // `test_struct_field_private` needs visibility across modules and is not ported: a private
    // field is `field is private`, never `no such field`, and nothing here reports it.

    // --- missing_match_arms.rs ------------------------------------------------------------

    #[test]
    fn enums() {
        expect(
            "enum Either { A, B, }\n\nfn main() {\n    match Either::A { }\n    match Either::B { Either::A => (), }\n\n    match &Either::B {\n        Either::A => (),\n    }\n\n    match Either::B {\n        Either::A => (), Either::B => (),\n    }\n    match &Either::B {\n        Either::A => (), Either::B => (),\n    }\n}\n",
            &[
                ("E0004", "missing match arm: `A` and `B` not covered", "Either::A"),
                ("E0004", "missing match arm: `B` not covered", "Either::B"),
                ("E0004", "missing match arm: `&B` not covered", "&Either::B"),
            ],
        );
    }

    #[test]
    fn enum_containing_bool() {
        // rust-analyzer also reports `A(false)` for an arm on `A(true)`. Deciding which values
        // of a field are left needs the field's type, so a partly covered variant leaves the
        // match undecided here.
        expect(
            "enum Either { A(bool), B }\n\nfn main() {\n    match Either::B { }\n    match Either::B {\n        Either::A(true) => (), Either::B => ()\n    }\n\n    match Either::B {\n        Either::A(true) => (),\n        Either::A(false) => (),\n        Either::B => (),\n    }\n    match Either::B {\n        Either::B => (),\n        _ => (),\n    }\n    match Either::B {\n        Either::A(_) => (),\n        Either::B => (),\n    }\n}\n",
            &[("E0004", "missing match arm: `A(_)` and `B` not covered", "Either::B")],
        );
    }

    #[test]
    fn enum_different_sizes() {
        clean(
            "enum Either { A(bool), B(bool, bool) }\n\nfn main() {\n    match Either::A(false) {\n        Either::A(_) => (),\n        Either::B(false, _) => (),\n    }\n\n    match Either::A(false) {\n        Either::A(_) => (),\n        Either::B(true, _) => (),\n        Either::B(false, _) => (),\n    }\n}\n",
        );
    }

    #[test]
    fn or_pattern_no_diagnostic() {
        clean("enum Either {A, B}\n\nfn main() {\n    match (Either::A, Either::B) {\n        (Either::A | Either::B, _) => (),\n    }\n}\n");
    }

    #[test]
    fn expr_diverges() {
        // `loop {}` has no written type, so none of rust-analyzer's four matches is decided.
        clean(
            "enum Either { A, B }\n\nfn main() {\n    match loop {} {\n        Either::A => (),\n    }\n}\n",
        );
    }

    #[test]
    fn expr_partially_diverges() {
        clean(
            "enum Either<T> { A(T), B }\n\nfn foo() -> Either<!> { Either::B }\nfn main() -> u32 {\n    match foo() {\n        Either::A(val) => val,\n        Either::B => 0,\n    }\n}\n",
        );
    }

    #[test]
    fn enum_record() {
        expect(
            "enum Either { A { foo: bool }, B }\n\nfn main() {\n    let a = Either::A { foo: true };\n    match a { }\n    match a {\n        Either::A { } => (),\n        Either::B => (),\n    }\n    match a {\n        Either::A { } => (),\n    }\n    match a {\n        Either::A { foo: _ } => (),\n        Either::B => (),\n    }\n}\n",
            &[
                ("E0004", "missing match arm: `A { .. }` and `B` not covered", "a"),
                ("E0063", "missing structure fields:\n- foo\n", "Either::A"),
                ("E0004", "missing match arm: `B` not covered", "a"),
                ("E0063", "missing structure fields:\n- foo\n", "Either::A"),
            ],
        );
    }

    #[test]
    fn match_on_typed_param_and_self() {
        expect(
            "enum E { A, B, C, D, F }\nimpl E {\n    fn f(&self) { match self { E::A => {} } }\n}\nfn g(e: E) { match e { E::A => {} E::B => {} } }\n",
            &[
                ("E0004", "missing match arm: `&B`, `&C`, `&D` and 1 more not covered", "self"),
                ("E0004", "missing match arm: `C`, `D` and `F` not covered", "e"),
            ],
        );
    }

    #[test]
    fn match_guard_does_not_cover() {
        expect(
            "enum E { A, B }\nfn g(e: E) { match e { E::A if true => {} E::B => {} } }\n",
            &[("E0004", "missing match arm: `A` not covered", "e")],
        );
    }

    #[test]
    fn match_with_uninhabitable_variant_is_skipped() {
        clean("enum Void {}\nenum E { A(Void), B }\nfn g(e: E) { match e { E::B => {} } }\n");
    }

    #[test]
    fn macro_or_pat() {
        // rust-analyzer expands `m!()` and reports `Type3`. A pattern macro is not decided here.
        clean(
            "macro_rules! m {\n    () => {\n        Enum::Type1 | Enum::Type2\n    };\n}\n\nenum Enum {\n    Type1,\n    Type2,\n    Type3,\n}\n\nfn f(ty: Enum) {\n    match ty {\n        m!() => (),\n    }\n}\n",
        );
    }

    // --- trait_impl_missing_assoc_item.rs -------------------------------------------------

    #[test]
    fn trait_with_default_value() {
        clean("trait Marker {\n    const FLAG: bool = false;\n}\nstruct Foo;\nimpl Marker for Foo {}\n");
    }

    #[test]
    fn missing_simple() {
        expect(
            "trait Trait {\n    const C: ();\n    type T;\n    fn f();\n}\n\nimpl Trait for () {\n    const C: () = ();\n    type T = ();\n    fn f() {}\n}\n\nimpl Trait for () {\n    type T = ();\n    fn f() {}\n}\n\nimpl Trait for () {\n}\n",
            &[
                ("E0046", "not all trait items implemented, missing: `const C`", "Trait"),
                (
                    "E0046",
                    "not all trait items implemented, missing: `const C`, `type T`, `fn f`",
                    "Trait",
                ),
            ],
        );
    }

    #[test]
    fn missing_default() {
        expect(
            "trait Trait {\n    const C: ();\n    type T = ();\n    fn f() {}\n}\n\nimpl Trait for () {\n    const C: () = ();\n    type T = ();\n    fn f() {}\n}\n\nimpl Trait for () {\n    type T = ();\n    fn f() {}\n}\n\nimpl Trait for () {\n     type T = ();\n }\n\nimpl Trait for () {\n}\n",
            &[
                ("E0046", "not all trait items implemented, missing: `const C`", "Trait"),
                ("E0046", "not all trait items implemented, missing: `const C`", "Trait"),
                ("E0046", "not all trait items implemented, missing: `const C`", "Trait"),
            ],
        );
    }

    #[test]
    fn negative_impl() {
        clean("trait Trait {\n    fn item();\n}\n\nimpl !Trait for () {}\n");
    }

    #[test]
    fn impl_sized_for_unsized() {
        // rust-analyzer reports the `Adt<i32>` impl. Whether `where Self: Sized` excuses an item
        // depends on the self type's sizedness, which is a type question, so none is reported.
        clean(
            "trait Trait {\n    type Item\n    where\n        Self: Sized;\n\n    fn item()\n    where\n        Self: Sized;\n}\n\nimpl Trait for str {}\nimpl Trait for Adt<i32> {}\n\nstruct Adt<T>(i32, T);\n",
        );
    }

    #[test]
    fn no_false_positive_on_specialization() {
        clean("#![feature(specialization)]\n\npub trait Foo {\n    fn foo();\n}\n\nimpl<T> Foo for T {\n    default fn foo() {}\n}\nimpl Foo for bool {}\n");
    }

    // --- trait_impl_redundant_assoc_item.rs -----------------------------------------------

    #[test]
    fn redundant_trait_with_default_value() {
        expect(
            "trait Marker {\n    const FLAG: bool = false;\n    fn boo();\n    fn foo () {}\n}\nstruct Foo;\nimpl Marker for Foo {\n    type T = i32;\n\n    const FLAG: bool = true;\n\n    fn bar() {}\n\n    fn boo() {}\n}\n",
            &[
                ("E0407", "`type T` is not a member of trait `Marker`", "type T = i32;"),
                ("E0407", "`fn bar` is not a member of trait `Marker`", "fn bar() {}"),
            ],
        );
    }

    #[test]
    fn dont_work_for_negative_impl() {
        clean("trait Marker {\n    const FLAG: bool = false;\n    fn boo();\n    fn foo () {}\n}\nstruct Foo;\nimpl !Marker for Foo {\n    type T = i32;\n    const FLAG: bool = true;\n    fn bar() {}\n    fn boo() {}\n}\n");
    }

    #[test]
    fn trait_from_elsewhere_is_skipped() {
        clean("struct Foo;\nimpl dep::Marker for Foo {\n    type T = i32;\n}\nimpl Display for Foo {}\n");
    }

    // --- unresolved_import.rs -------------------------------------------------------------

    #[test]
    fn unresolved_import() {
        // rust-analyzer also reports `use does_not_exist;`. With no knowledge of the crate
        // graph, a leading name that is not a local module may be a dependency, so only paths
        // rooted in this file are decided. The decided case is added from another module: an
        // import into the module it resolves in binds its own name there, and telling that
        // binding from another import's would need import provenance, so it is skipped.
        expect(
            "use does_exist;\nuse does_not_exist;\nmod inner {\n    use crate::also_missing;\n}\n\nmod does_exist {}\n",
            &[("E0432", "unresolved import", "crate::also_missing")],
        );
    }

    #[test]
    fn unresolved_import_in_use_tree() {
        // rust-analyzer's fixture also has `use {does_not_exist::*, does_exist};`, which binds
        // `does_exist` a second time beside the module. A doubly bound name is not followed, so
        // that line is left out here to keep the first import decidable.
        expect(
            "use does_exist::{Exists, DoesntExist};\n\nuse does_not_exist::{\n    a,\n    b,\n    c,\n};\n\nmod does_exist {\n    pub struct Exists;\n}\n",
            &[("E0432", "unresolved import", "DoesntExist")],
        );
    }

    #[test]
    fn dedup_unresolved_import_from_unresolved_crate() {
        expect(
            "mod a {\n    extern crate doesnotexist;\n\n    use doesnotexist::{self, bla, *};\n\n    use crate::doesnotexist;\n}\n\nmod m {\n    use super::doesnotexist;\n}\n",
            &[
                ("E0432", "unresolved import", "crate::doesnotexist"),
                ("E0432", "unresolved import", "super::doesnotexist"),
            ],
        );
    }

    #[test]
    fn import_into_incomplete_module_is_skipped() {
        clean("mod m { gen_items!(); }\nuse crate::m::generated;\nmod n { pub use other::*; }\nuse self::n::x;\n");
    }

    #[test]
    fn import_of_variant_and_macro() {
        expect(
            "enum E { A }\nuse crate::E::A;\nuse crate::E::Z;\nmacro_rules! mac { () => {} }\nuse crate::mac;\n",
            &[("E0432", "unresolved import", "crate::E::Z")],
        );
    }

    // --- gating ---------------------------------------------------------------------------

    #[test]
    fn disabled_codes_are_not_run() {
        let src = "fn main() { let _ = x; zero(1); }\nfn zero() {}\n";
        let by_name: Vec<String> = run_with(src, Some(alloc::vec!["mismatched-arg-count".into()]))
            .into_iter()
            .map(|(code, ..)| code)
            .collect();
        assert_eq!(by_name, alloc::vec!["E0061".to_string()]);
        let by_code: Vec<String> =
            run_with(src, Some(alloc::vec!["E0425".into()])).into_iter().map(|(c, ..)| c).collect();
        assert_eq!(by_code, alloc::vec!["E0425".to_string()]);
        assert!(run_with(src, Some(Vec::new())).is_empty());
    }
}
