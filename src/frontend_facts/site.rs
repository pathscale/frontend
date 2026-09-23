//! What is at a cursor, and what is in scope there, read from syntax alone.
//!
//! An editor asks two questions at a byte offset before it can offer completion or hover: what
//! syntactic position the cursor is in, and which names a bare identifier typed there could
//! refer to. [`site_at`] answers both from one parse of the text it is handed.
//!
//! **Syntax only, on purpose.** The source goes through `rustc_parse` into a `rustc_ast::Crate`
//! and nothing further: no macro expansion, no name resolution, no type checking, no sysroot.
//! That is what makes the answer cheap enough to ask on every keystroke and available for code
//! that does not yet compile, which is most code at a cursor. The cost is that every answer here
//! is lexical. A name brought in by a glob import, a prelude, or a macro expansion is not listed,
//! and a pattern identifier such as `None` in `match x { None => .. }` is reported as a binding
//! because only resolution can tell it is a unit variant.
//!
//! The walk is one pass that descends only into nodes whose span contains the offset. The
//! enclosing scopes are collected on the way down, and nothing beside that path is visited.

// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them.
use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::rustc_ast::visit::{self, AssocCtxt, Visitor};
use crate::rustc_ast::{
    Arm, AssocItem, AssocItemKind, BinOpKind, Block, Crate, Expr, ExprKind, Fn, FnDecl, FnRetTy,
    ForeignItemKind, GenericParamKind, Generics, Item, ItemKind, LocalKind, ModKind, Mutability,
    Pat, PatKind, SelfKind, Stmt, StmtKind, Ty, TyKind, UseTree, UseTreeKind,
};
use crate::rustc_errors::DiagCtxt;
use crate::rustc_errors::plain_emitter::PlainEmitter;
use crate::rustc_parse::lexer::StripTokens;
use crate::rustc_parse::new_parser_from_source_str;
use crate::rustc_session::parse::ParseSess;
use crate::rustc_span::edition::Edition;
use crate::rustc_span::fatal_error::catch_fatal_errors;
use crate::rustc_span::source_map::{FilePathMapping, SourceMap};
use crate::rustc_span::{
    BytePos, FileName, Ident, SourceFile, Span, create_session_if_not_set_then, kw,
};
use serde::{Deserialize, Serialize};

/// A byte range in the source [`site_at`] was handed, `start..end`.
///
/// Offsets are into the caller's text as given, not rustc's normalized copy, so a source with a
/// byte order mark or CRLF line endings still slices correctly.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TextRange {
    pub start: u32,
    pub end: u32,
}

/// The syntactic position an offset sits in: the innermost construct that gives the cursor a
/// role. Deliberately coarse, so a caller can branch on it without matching every AST node.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Position {
    /// Between items at module level: the crate root or an inline `mod { .. }` body.
    Item,
    /// Between the items of an `impl` body.
    ImplItem,
    /// Between the items of a `trait` body.
    TraitItem,
    /// At statement level inside a block: between statements, or on a statement that is a bare
    /// name. Usually a fn body; a block in a const initializer reports the same.
    FnBody,
    /// In the initializer of a `let`, after the `=`, and not inside anything more specific.
    LetInit,
    /// In the argument list of a call or method call, directly in an argument slot.
    CallArg,
    /// Inside some other expression.
    Expr,
    /// Anywhere else: a signature, a pattern, a type, an attribute's item header, the tokens of
    /// a macro call, which are not parsed until expansion.
    Other,
}

/// The innermost item around the offset.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EnclosingItem {
    /// Names from the crate root inward, joined with `::`: `outer::Thing::method`. An `impl`
    /// contributes its self type as written, since an impl has no name of its own.
    pub path: String,
    pub span: TextRange,
}

/// How a method takes `self`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Receiver {
    /// `self`
    Value,
    /// `mut self`
    MutValue,
    /// `&self`, `&'a self`
    Ref,
    /// `&mut self`
    RefMut,
    /// `&pin const self`
    PinRef,
    /// `&pin mut self`
    PinMut,
    /// `self: Box<Self>` and the like; `ty` is the type as written.
    Typed { ty: String, mutable: bool },
}

/// One non-receiver parameter of a fn, as source text.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FnParam {
    pub pattern: String,
    pub ty: String,
}

/// The signature of the innermost fn around the offset, as source text.
///
/// Text rather than resolved types, because nothing here has resolved a path: the text is the
/// only thing this layer can state truthfully.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FnSignature {
    pub name: String,
    /// `<T: Copy, 'a>` with the brackets, or `None` when the fn has no generic parameters.
    pub generics: Option<String>,
    /// `where T: Copy`, or `None`.
    pub where_clause: Option<String>,
    /// Every parameter except the receiver, which is in `receiver`.
    pub params: Vec<FnParam>,
    /// The written return type, or `None` for unit.
    pub ret: Option<String>,
    pub receiver: Option<Receiver>,
    /// The full signature, `fn name(..) -> T`, as written.
    pub text: String,
    pub span: TextRange,
}

/// What kind of item a name in scope refers to.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemCategory {
    Fn,
    Struct,
    Enum,
    Union,
    Const,
    Static,
    /// A trait or a trait alias.
    Trait,
    TypeAlias,
    Mod,
    /// A `macro_rules!` or `macro` definition.
    Macro,
}

/// How a name in scope was introduced.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BindingKind {
    /// A parameter of the enclosing fn, `self` included.
    Param,
    /// A `let` binding, or a binding from a `match` arm, `if let`, `while let` or `for` pattern.
    Local,
    /// A parameter of an enclosing closure.
    ClosureParam,
    Item(ItemCategory),
    /// A name bound by `use` or `extern crate`, after any `as` rename.
    Import,
    /// A type, lifetime or const parameter of an enclosing item. Lifetimes keep their `'`.
    GenericParam,
}

/// One name visible at the offset.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Binding {
    pub name: String,
    pub kind: BindingKind,
    /// The span of the name where it is declared.
    pub span: TextRange,
    /// The type when the source writes one for this name: a param's type, an annotated `let`,
    /// a const or static's type, a const generic's type. `None` when it would take inference.
    pub ty: Option<String>,
}

/// The answer [`site_at`] gives for one offset.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Site {
    pub position: Position,
    pub enclosing_item: Option<EnclosingItem>,
    pub enclosing_fn: Option<FnSignature>,
    /// Names a bare identifier at the offset could refer to, innermost scope first and in name
    /// order within a scope. A shadowed name appears once, as its innermost declaration.
    pub scope: Vec<Binding>,
    /// Syntax errors the parser recovered from. The answer still stands, because code at a
    /// cursor is usually incomplete; these say how much the parser had to guess.
    pub syntax_errors: Vec<String>,
}

/// Say what is at `offset` in `source` and which names are in scope there.
///
/// `offset` is a byte offset into `source` and may equal its length. `Err` carries the parser's
/// diagnostics when it could not produce a crate at all, for instance on an unclosed delimiter,
/// or one message when `offset` is out of range or not on a character boundary.
///
/// Parsing only, so it needs no sysroot and runs as-is on code that does not type check; see
/// the module header for what that leaves out.
///
/// The parser still raises rustc's fatal error on some inputs, by unwinding, so this wraps the
/// parse in [`crate::rustc_span::fatal_error::catch_fatal_errors`] to turn that into `Err`
/// rather than a dead process. That wrap needs `panic = "unwind"` and a catcher installed
/// through [`crate::unwind_janky::install_catcher`], as [`super::analyze_source`] does.
pub fn site_at(source: &str, offset: u32) -> Result<Site, Vec<String>> {
    assert!(
        crate::unwind_janky::unwinding_is_enabled(),
        "site_at needs panic=unwind and a catcher installed through unwind_janky::install_catcher"
    );
    let captured = Arc::new(eko::thread::Mutex::new(String::new()));
    match catch_fatal_errors(|| locate(source, offset, &captured)) {
        Ok(answer) => answer,
        Err(_) => {
            let mut errors = error_entries(&captured.lock());
            if errors.is_empty() {
                errors.push("error: the parser stopped on a fatal error".to_string());
            }
            Err(errors)
        }
    }
}

/// [`site_at`] for each `(source, offset)` of `inputs`, in order, with the session setup shared.
///
/// One result per input, in input order, each exactly what [`site_at`] returns for that pair on
/// its own: every offset in a [`Site`] is into that input's own source, because each call still
/// parses into a source map of its own. The calls run on the caller's thread under one set of
/// session globals, rebuilt every [`super::session::RECYCLE_EVERY`] calls. Spawns nothing. See
/// [`super::session`] for why sharing the globals cannot change an answer.
///
/// Many offsets into one source are many parses of it; this shares the setup, not the parse.
///
/// Needs the same catcher as [`site_at`], asserted up front.
pub fn site_at_many<'a, I>(inputs: I) -> Vec<Result<Site, Vec<String>>>
where
    I: IntoIterator<Item = (&'a str, u32)>,
{
    assert!(
        crate::unwind_janky::unwinding_is_enabled(),
        "site_at_many needs panic=unwind and a catcher installed through unwind_janky::install_catcher"
    );
    super::session::run_batch(inputs, super::session::RECYCLE_EVERY, |(source, offset)| {
        site_at(source, offset)
    })
}

/// [`site_at`] without the fatal-error catch. Diagnostics go into `captured`, so that a caller
/// that catches an unwind out of here can still say what the parser objected to.
fn locate(
    source: &str,
    offset: u32,
    captured: &Arc<eko::thread::Mutex<String>>,
) -> Result<Site, Vec<String>> {
    if offset as usize > source.len() || !source.is_char_boundary(offset as usize) {
        return Err(alloc::vec![format!(
            "error: offset {offset} is not a character boundary in a source of {} bytes",
            source.len()
        )]);
    }
    // Reuses the caller's session globals when there are some, so this can be asked from
    // inside another compiler session on the same thread without tripping the one-per-thread
    // assertion in `create_session_globals_then`.
    create_session_if_not_set_then(Edition::Edition2024, |_| {
        let found = {
            let sm = Arc::new(SourceMap::new(FilePathMapping::empty()));
            let emitter = PlainEmitter::new()
                .sm(Some(Arc::clone(&sm)))
                .short_message(true)
                .dst(Box::new(Capture(Arc::clone(captured))));
            let mut psess = ParseSess::with_dcx(DiagCtxt::new(Box::new(emitter)), sm);
            psess.edition = Edition::Edition2024;
            let parsed = match new_parser_from_source_str(
                &psess,
                FileName::anon_source_code(source),
                source.to_string(),
                StripTokens::ShebangAndFrontmatter,
            ) {
                Ok(mut parser) => match parser.parse_crate_mod() {
                    Ok(krate) => Some(krate),
                    Err(diag) => {
                        diag.emit();
                        None
                    }
                },
                Err(diags) => {
                    for diag in diags {
                        diag.emit();
                    }
                    None
                }
            };
            let found = parsed.map(|krate| {
                let file = Arc::clone(&psess.source_map().files()[0]);
                let mut finder = Finder::new(source, &file, offset);
                finder.crate_root(&krate);
                finder.finish()
            });
            // Anything the parser stashed for later is emitted now, so it is in `captured`
            // before that is read, rather than on the session's drop after.
            psess.dcx().emit_stashed_diagnostics();
            found
        };
        let errors = error_entries(&captured.lock());
        match found {
            Some(mut site) => {
                site.syntax_errors = errors;
                Ok(site)
            }
            None if errors.is_empty() => {
                Err(alloc::vec!["error: the parser produced no crate".to_string()])
            }
            None => Err(errors),
        }
    })
}

/// A `fmt::Write` into a buffer the caller keeps a handle on, so the emitter can own one end.
struct Capture(Arc<eko::thread::Mutex<String>>);

impl core::fmt::Write for Capture {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.0.lock().push_str(s);
        Ok(())
    }
}

/// Split emitter output into one string per error. A diagnostic starts on a line with no
/// leading space and its location lines follow, indented; warnings are dropped.
fn error_entries(text: &str) -> Vec<String> {
    let mut entries: Vec<String> = Vec::new();
    for line in text.lines() {
        if line.starts_with(' ') {
            if let Some(entry) = entries.last_mut() {
                entry.push('\n');
                entry.push_str(line);
            }
        } else {
            entries.push(line.to_string());
        }
    }
    entries.retain(|entry| entry.starts_with("error"));
    entries
}

/// One lexical scope on the way down: a module, an item's generics, a fn's params, a block.
/// A map so that a later declaration of a name in the same scope replaces the earlier one,
/// which is `let` shadowing.
type Frame = BTreeMap<String, Binding>;

/// The single pass. Every `visit_*` here returns at once for a node that does not contain the
/// offset, so the work is the depth of the path to the offset plus the items of the scopes it
/// crosses, not the size of the file.
struct Finder<'s> {
    source: &'s str,
    file: &'s SourceFile,
    offset: u32,
    position: Position,
    path: Vec<String>,
    enclosing_item: Option<EnclosingItem>,
    enclosing_fn: Option<FnSignature>,
    frames: Vec<Frame>,
}

impl<'s> Finder<'s> {
    fn new(source: &'s str, file: &'s SourceFile, offset: u32) -> Self {
        Finder {
            source,
            file,
            offset,
            position: Position::Other,
            path: Vec::new(),
            enclosing_item: None,
            enclosing_fn: None,
            frames: Vec::new(),
        }
    }

    fn finish(self) -> Site {
        let mut seen = BTreeSet::new();
        let mut scope = Vec::new();
        for frame in self.frames.into_iter().rev() {
            for (name, binding) in frame {
                if seen.insert(name) {
                    scope.push(binding);
                }
            }
        }
        Site {
            position: self.position,
            enclosing_item: self.enclosing_item,
            enclosing_fn: self.enclosing_fn,
            scope,
            syntax_errors: Vec::new(),
        }
    }

    // ---- spans, in the caller's offsets ----------------------------------------------------

    fn at(&self, pos: BytePos) -> u32 {
        // A recovered node can carry a dummy span; clamp it rather than underflow.
        if pos < self.file.start_pos {
            return 0;
        }
        self.file.original_relative_byte_pos(pos).0
    }

    fn range(&self, span: Span) -> TextRange {
        TextRange { start: self.at(span.lo()), end: self.at(span.hi()) }
    }

    fn text(&self, span: Span) -> String {
        let r = self.range(span);
        self.source.get(r.start as usize..r.end as usize).unwrap_or("").to_string()
    }

    /// The offset is strictly between the span's ends: for a delimited body, inside the braces.
    fn inside(&self, span: Span) -> bool {
        let r = self.range(span);
        r.start < self.offset && self.offset < r.end
    }

    /// The offset is on the span, ends included, except just past a closing delimiter, where the
    /// cursor has left the construct: after `f(x)` is not in the call.
    fn touches(&self, span: Span) -> bool {
        let r = self.range(span);
        if self.offset < r.start || self.offset > r.end {
            return false;
        }
        if self.offset == r.end && r.end > r.start {
            let last = self.source.as_bytes().get(r.end as usize - 1).copied();
            return !matches!(last, Some(b')' | b']' | b'}'));
        }
        true
    }

    /// The first `byte` in `from..to`, for the one token the AST keeps no span for: a body's
    /// `{`, a `let`'s `=`, a method call's `(`. Bounded by the node's own header, so linear.
    fn find_byte(&self, from: u32, to: u32, byte: u8) -> Option<u32> {
        let bytes = self.source.as_bytes();
        let to = (to as usize).min(bytes.len());
        (from as usize..to).find(|&i| bytes[i] == byte).map(|i| i as u32)
    }

    /// The offset is past the `{` that opens a body somewhere in `header_end..span.hi`.
    fn in_body_after(&self, header_end: Span, span: Span) -> bool {
        let from = self.at(header_end.hi());
        let to = self.at(span.hi());
        self.find_byte(from, to, b'{').is_some_and(|open| open < self.offset && self.offset < to)
    }

    // ---- scope -----------------------------------------------------------------------------

    fn push_frame(&mut self) {
        self.frames.push(Frame::new());
    }

    fn bind(&mut self, ident: Ident, kind: BindingKind, ty: Option<String>) {
        let name = ident.name.as_str();
        if name.is_empty() || name == "_" {
            return;
        }
        let binding = Binding { name: name.to_string(), kind, span: self.range(ident.span), ty };
        if self.frames.is_empty() {
            self.push_frame();
        }
        self.frames.last_mut().expect("a frame was just pushed").insert(binding.name.clone(), binding);
    }

    /// Crossing into a module: nothing outside it is visible by bare name, except
    /// `macro_rules!` definitions, whose scope is textual and runs on into child modules.
    fn enter_module_scope(&mut self) {
        for frame in &mut self.frames {
            frame.retain(|_, b| matches!(b.kind, BindingKind::Item(ItemCategory::Macro)));
        }
        self.frames.retain(|frame| !frame.is_empty());
    }

    /// Crossing into a nested item: an item cannot see the locals, params or generic params of
    /// the fn it sits in, only the items and imports of the blocks and module around it.
    fn enter_item_scope(&mut self) {
        for frame in &mut self.frames {
            frame.retain(|_, b| matches!(b.kind, BindingKind::Item(_) | BindingKind::Import));
        }
        self.frames.retain(|frame| !frame.is_empty());
    }

    fn written_ty(&self, ty: &Ty) -> Option<String> {
        match ty.kind {
            TyKind::Infer | TyKind::ImplicitSelf | TyKind::Err(_) | TyKind::Dummy => None,
            _ => Some(self.text(ty.span)),
        }
    }

    /// Bind every identifier a pattern introduces. A bare `name` pattern takes `ty` as its
    /// written type; a destructuring pattern gives its parts none, since splitting the type
    /// text would be guessing.
    fn bind_pat(&mut self, pat: &Pat, ty: Option<&Ty>, kind: BindingKind) {
        if let PatKind::Ident(_, ident, None) = pat.kind {
            let ty = ty.and_then(|ty| self.written_ty(ty));
            self.bind(ident, kind, ty);
            return;
        }
        let mut idents = Vec::new();
        PatIdents(&mut idents).visit_pat(pat);
        for ident in idents {
            self.bind(ident, kind, None);
        }
    }

    fn bind_params(&mut self, decl: &FnDecl, kind: BindingKind) {
        for param in &decl.inputs {
            if let Some(eself) = param.to_self() {
                let ty = match eself.node {
                    SelfKind::Explicit(ty, _) => Some(self.text(ty.span)),
                    _ => None,
                };
                if let PatKind::Ident(_, ident, _) = param.pat.kind {
                    self.bind(ident, kind, ty);
                }
                continue;
            }
            self.bind_pat(&param.pat, Some(&param.ty), kind);
        }
    }

    fn bind_generics(&mut self, generics: &Generics) {
        for param in &generics.params {
            let ty = match &param.kind {
                GenericParamKind::Const { ty, .. } => self.written_ty(ty),
                _ => None,
            };
            self.bind(param.ident, BindingKind::GenericParam, ty);
        }
    }

    /// Bind the names a list of items declares, all at once, because an item is visible
    /// throughout its module or block regardless of order. The exception is a macro, which is
    /// visible only after its definition.
    fn bind_items<'i>(&mut self, items: impl Iterator<Item = &'i Item>) {
        for item in items {
            let category = match &item.kind {
                ItemKind::Use(tree) => {
                    self.bind_use(tree, None);
                    continue;
                }
                ItemKind::ExternCrate(_, ident) => {
                    self.bind(*ident, BindingKind::Import, None);
                    continue;
                }
                ItemKind::ForeignMod(foreign) => {
                    for fi in &foreign.items {
                        let (ident, category, ty) = match &fi.kind {
                            ForeignItemKind::Fn(f) => (f.ident, ItemCategory::Fn, None),
                            ForeignItemKind::Static(s) => {
                                (s.ident, ItemCategory::Static, self.written_ty(&s.ty))
                            }
                            ForeignItemKind::TyAlias(t) => (t.ident, ItemCategory::TypeAlias, None),
                            ForeignItemKind::MacCall(_) => continue,
                        };
                        self.bind(ident, BindingKind::Item(category), ty);
                    }
                    continue;
                }
                ItemKind::MacroDef(..) if self.at(item.span.hi()) > self.offset => continue,
                ItemKind::Fn(_) => ItemCategory::Fn,
                ItemKind::Struct(..) => ItemCategory::Struct,
                ItemKind::Enum(..) => ItemCategory::Enum,
                ItemKind::Union(..) => ItemCategory::Union,
                ItemKind::Const(_) => ItemCategory::Const,
                ItemKind::Static(_) => ItemCategory::Static,
                ItemKind::Trait(_) | ItemKind::TraitAlias(_) => ItemCategory::Trait,
                ItemKind::TyAlias(_) => ItemCategory::TypeAlias,
                ItemKind::Mod(..) => ItemCategory::Mod,
                ItemKind::MacroDef(..) => ItemCategory::Macro,
                _ => continue,
            };
            let ty = match &item.kind {
                ItemKind::Const(c) => self.written_ty(&c.ty),
                ItemKind::Static(s) => self.written_ty(&s.ty),
                _ => None,
            };
            if let Some(ident) = item.kind.ident() {
                self.bind(ident, BindingKind::Item(category), ty);
            }
        }
    }

    /// Bind the leaf names of a `use` tree. `parent` is the last segment above a nested group,
    /// which is the name `self` in `use a::{self}` stands for.
    fn bind_use(&mut self, tree: &UseTree, parent: Option<Ident>) {
        match &tree.kind {
            UseTreeKind::Simple(rename) => {
                let last = tree.prefix.segments.last().map(|s| s.ident);
                let ident = match (rename, last) {
                    (Some(rename), _) => *rename,
                    (None, Some(last)) if last.name == kw::SelfLower => match parent {
                        Some(parent) => Ident::new(parent.name, last.span),
                        None => return,
                    },
                    (None, Some(last)) => last,
                    (None, None) => return,
                };
                if ident.name == kw::Underscore || ident.name == kw::PathRoot {
                    return;
                }
                self.bind(ident, BindingKind::Import, None);
            }
            UseTreeKind::Nested { items, .. } => {
                let parent = tree.prefix.segments.last().map(|s| s.ident).or(parent);
                for (nested, _) in items {
                    self.bind_use(nested, parent);
                }
            }
            // A glob names nothing until resolution says what it covers.
            UseTreeKind::Glob(_) => {}
        }
    }

    /// Bind what `if let` / `while let` conditions introduce, through `&&` chains.
    fn bind_let_chain(&mut self, cond: &Expr) {
        match &cond.kind {
            ExprKind::Let(pat, ..) => self.bind_pat(pat, None, BindingKind::Local),
            ExprKind::Binary(op, lhs, rhs) if op.node == BinOpKind::And => {
                self.bind_let_chain(lhs);
                self.bind_let_chain(rhs);
            }
            ExprKind::Paren(inner) => self.bind_let_chain(inner),
            _ => {}
        }
    }

    // ---- items -----------------------------------------------------------------------------

    fn set_enclosing(&mut self, name: String, span: Span) {
        self.path.push(name);
        self.enclosing_item = Some(EnclosingItem { path: self.path.join("::"), span: self.range(span) });
    }

    fn crate_root(&mut self, krate: &Crate) {
        self.module_body(&krate.items);
    }

    fn module_body(&mut self, items: &[Box<Item>]) {
        self.enter_module_scope();
        self.push_frame();
        self.bind_items(items.iter().map(|item| &**item));
        self.position = Position::Item;
        if let Some(item) = items.iter().find(|item| self.inside(item.span)) {
            self.item(item);
        }
    }

    fn item(&mut self, item: &Item) {
        self.enter_item_scope();
        self.position = Position::Other;
        match &item.kind {
            ItemKind::Mod(_, ident, ModKind::Loaded(items, ..)) => {
                self.set_enclosing(ident.name.as_str().to_string(), item.span);
                if self.in_body_after(ident.span, item.span) {
                    self.module_body(items);
                }
            }
            ItemKind::Fn(f) => {
                self.set_enclosing(f.ident.name.as_str().to_string(), item.span);
                self.push_frame();
                self.function(f);
            }
            ItemKind::Impl(imp) => {
                self.set_enclosing(self.text(imp.self_ty.span), item.span);
                self.push_frame();
                self.bind_generics(&imp.generics);
                if self.in_body_after(imp.self_ty.span, item.span) {
                    self.assoc_body(&imp.items, Position::ImplItem, AssocCtxt::Impl {
                        of_trait: imp.of_trait.is_some(),
                    });
                }
            }
            ItemKind::Trait(t) => {
                self.set_enclosing(t.ident.name.as_str().to_string(), item.span);
                self.push_frame();
                self.bind_generics(&t.generics);
                if self.in_body_after(t.ident.span, item.span) {
                    self.assoc_body(&t.items, Position::TraitItem, AssocCtxt::Trait);
                }
            }
            _ => {
                if let Some(ident) = item.kind.ident() {
                    self.set_enclosing(ident.name.as_str().to_string(), item.span);
                }
                if let Some(generics) = item.kind.generics() {
                    self.push_frame();
                    self.bind_generics(generics);
                }
                // A const or static initializer, an enum discriminant, an array length in a
                // field type: the expression walk below reaches whichever holds the offset.
                visit::walk_item(self, item);
            }
        }
    }

    fn assoc_body(&mut self, items: &[Box<AssocItem>], position: Position, ctxt: AssocCtxt) {
        self.position = position;
        if let Some(item) = items.iter().find(|item| self.inside(item.span)) {
            self.assoc_item(item, ctxt);
        }
    }

    fn assoc_item(&mut self, item: &AssocItem, ctxt: AssocCtxt) {
        // No `enter_item_scope`: an associated item does see the generics of its impl or trait.
        self.position = Position::Other;
        match &item.kind {
            AssocItemKind::Fn(f) => {
                self.set_enclosing(f.ident.name.as_str().to_string(), item.span);
                self.push_frame();
                self.function(f);
            }
            kind => {
                if let Some(ident) = kind.ident() {
                    self.set_enclosing(ident.name.as_str().to_string(), item.span);
                }
                if let Some(generics) = item.opt_generics() {
                    self.push_frame();
                    self.bind_generics(generics);
                }
                visit::walk_assoc_item(self, item, ctxt);
            }
        }
    }

    fn function(&mut self, f: &Fn) {
        self.enclosing_fn = Some(self.signature(f));
        self.bind_generics(&f.generics);
        match &f.body {
            Some(body) if self.inside(body.span) => {
                self.push_frame();
                self.bind_params(&f.sig.decl, BindingKind::Param);
                self.visit_block(body);
            }
            _ => self.position = Position::Other,
        }
    }

    fn signature(&self, f: &Fn) -> FnSignature {
        let mut receiver = None;
        let mut params = Vec::new();
        for param in &f.sig.decl.inputs {
            if let Some(eself) = param.to_self() {
                receiver = Some(match eself.node {
                    SelfKind::Value(Mutability::Not) => Receiver::Value,
                    SelfKind::Value(Mutability::Mut) => Receiver::MutValue,
                    SelfKind::Region(_, Mutability::Not) => Receiver::Ref,
                    SelfKind::Region(_, Mutability::Mut) => Receiver::RefMut,
                    SelfKind::Pinned(_, Mutability::Not) => Receiver::PinRef,
                    SelfKind::Pinned(_, Mutability::Mut) => Receiver::PinMut,
                    SelfKind::Explicit(ty, mutbl) => Receiver::Typed {
                        ty: self.text(ty.span),
                        mutable: mutbl == Mutability::Mut,
                    },
                });
                continue;
            }
            params.push(FnParam { pattern: self.text(param.pat.span), ty: self.text(param.ty.span) });
        }
        let generics = &f.generics;
        FnSignature {
            name: f.ident.name.as_str().to_string(),
            generics: (!generics.params.is_empty()).then(|| self.text(generics.span)),
            where_clause: (!generics.where_clause.is_empty())
                .then(|| self.text(generics.where_clause.span)),
            params,
            ret: match &f.sig.decl.output {
                FnRetTy::Default(_) => None,
                FnRetTy::Ty(ty) => Some(self.text(ty.span)),
            },
            receiver,
            text: self.text(f.sig.span),
            span: self.range(f.sig.span),
        }
    }

    // ---- statements and expressions --------------------------------------------------------

    fn statement(&mut self, stmt: &Stmt) {
        match &stmt.kind {
            StmtKind::Let(local) => {
                // The AST keeps no span for `=`, so find it after the pattern and type.
                let head = local.ty.as_ref().map_or(local.pat.span, |ty| ty.span);
                let eq = self.find_byte(self.at(head.hi()), self.at(stmt.span.hi()), b'=');
                match &local.kind {
                    LocalKind::InitElse(_, els) if self.inside(els.span) => self.visit_block(els),
                    LocalKind::Init(init) | LocalKind::InitElse(init, _)
                        if eq.is_some_and(|eq| eq < self.offset)
                            && self.offset <= self.at(init.span.hi()) =>
                    {
                        self.position = Position::LetInit;
                        self.visit_expr(init);
                    }
                    _ => self.position = Position::Other,
                }
            }
            StmtKind::Item(item) => {
                if self.inside(item.span) {
                    self.item(item);
                }
            }
            StmtKind::Expr(expr) | StmtKind::Semi(expr) => self.visit_expr(expr),
            // Macro arguments are tokens until expansion, and this does not expand.
            StmtKind::MacCall(_) => self.position = Position::Other,
            StmtKind::Empty => {}
        }
    }

    fn call_args(&mut self, open_from: Span, call: &Expr, args: &[Box<Expr>]) {
        let from = self.at(open_from.hi());
        let end = self.at(call.span.hi());
        let open = self.find_byte(from, end, b'(');
        if open.is_some_and(|open| open < self.offset && self.offset < end) {
            self.position = Position::CallArg;
            if let Some(arg) = args.iter().find(|arg| self.touches(arg.span)) {
                self.visit_expr(arg);
            }
        }
    }

    fn arm(&mut self, arm: &Arm) {
        if self.offset <= self.at(arm.pat.span.hi()) {
            self.position = Position::Other;
            return;
        }
        self.push_frame();
        self.bind_pat(&arm.pat, None, BindingKind::Local);
        self.position = Position::Expr;
        if let Some(guard) = &arm.guard {
            if self.touches(guard.cond.span) {
                self.visit_expr(&guard.cond);
                return;
            }
        }
        if let Some(body) = &arm.body {
            self.visit_expr(body);
        }
    }
}

impl<'a, 's> Visitor<'a> for Finder<'s> {
    type Result = ();

    fn visit_item(&mut self, item: &'a Item) {
        if self.inside(item.span) {
            self.item(item);
        }
    }

    fn visit_assoc_item(&mut self, item: &'a AssocItem, ctxt: AssocCtxt) {
        if self.inside(item.span) {
            self.assoc_item(item, ctxt);
        }
    }

    fn visit_block(&mut self, block: &'a Block) {
        if !self.inside(block.span) {
            return;
        }
        self.position = Position::FnBody;
        self.push_frame();
        self.bind_items(block.stmts.iter().filter_map(|stmt| match &stmt.kind {
            StmtKind::Item(item) => Some(&**item),
            _ => None,
        }));
        for stmt in &block.stmts {
            let r = self.range(stmt.span);
            // A statement without a trailing `;` or `}` still holds a cursor at its very end,
            // which is where one sits while typing a name.
            let open_end = matches!(stmt.kind, StmtKind::Expr(_)) && self.touches(stmt.span);
            if r.start < self.offset && (self.offset < r.end || open_end) {
                self.statement(stmt);
                return;
            }
            if r.end > self.offset {
                return;
            }
            // Wholly before the offset: its `let` bindings are in scope from here on.
            if let StmtKind::Let(local) = &stmt.kind {
                self.bind_pat(&local.pat, local.ty.as_deref(), BindingKind::Local);
            }
        }
    }

    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        if self.touches(stmt.span) {
            self.statement(stmt);
        }
    }

    fn visit_arm(&mut self, arm: &'a Arm) {
        if self.touches(arm.span) {
            self.arm(arm);
        }
    }

    fn visit_expr(&mut self, expr: &'a Expr) {
        if !self.touches(expr.span) {
            return;
        }
        let strictly = self.at(expr.span.lo()) < self.offset;
        match &expr.kind {
            // A leaf leaves the position to its parent: a name in an argument slot is still a
            // call argument.
            ExprKind::Path(..)
            | ExprKind::Lit(_)
            | ExprKind::Underscore
            | ExprKind::Err(_)
            | ExprKind::Dummy => {}
            ExprKind::MacCall(_) => {
                if strictly {
                    self.position = Position::Other;
                }
            }
            ExprKind::Call(callee, args) => {
                if self.touches(callee.span) {
                    if strictly {
                        self.position = Position::Expr;
                    }
                    self.visit_expr(callee);
                } else {
                    self.call_args(callee.span, expr, args);
                }
            }
            ExprKind::MethodCall(call) => {
                if self.touches(call.receiver.span) {
                    if strictly {
                        self.position = Position::Expr;
                    }
                    self.visit_expr(&call.receiver);
                } else {
                    self.position = Position::Expr;
                    self.call_args(call.seg.ident.span, expr, &call.args);
                }
            }
            ExprKind::Closure(closure) => {
                if self.offset >= self.at(closure.fn_decl_span.hi()) {
                    self.push_frame();
                    self.bind_params(&closure.fn_decl, BindingKind::ClosureParam);
                    self.position = Position::Expr;
                    self.visit_expr(&closure.body);
                } else {
                    self.position = Position::Other;
                }
            }
            ExprKind::If(cond, then, els) => {
                self.position = Position::Expr;
                if self.touches(cond.span) {
                    self.visit_expr(cond);
                } else if self.inside(then.span) {
                    self.push_frame();
                    self.bind_let_chain(cond);
                    self.visit_block(then);
                } else if let Some(els) = els {
                    self.visit_expr(els);
                }
            }
            ExprKind::While(cond, body, _) => {
                self.position = Position::Expr;
                if self.touches(cond.span) {
                    self.visit_expr(cond);
                } else if self.inside(body.span) {
                    self.push_frame();
                    self.bind_let_chain(cond);
                    self.visit_block(body);
                }
            }
            ExprKind::ForLoop(for_loop) => {
                self.position = Position::Expr;
                if self.touches(for_loop.iter.span) {
                    self.visit_expr(&for_loop.iter);
                } else if self.inside(for_loop.body.span) {
                    self.push_frame();
                    self.bind_pat(&for_loop.pat, None, BindingKind::Local);
                    self.visit_block(&for_loop.body);
                } else {
                    self.position = Position::Other;
                }
            }
            ExprKind::Match(scrutinee, arms, _) => {
                self.position = Position::Expr;
                if self.touches(scrutinee.span) {
                    self.visit_expr(scrutinee);
                } else if let Some(arm) = arms.iter().find(|arm| self.touches(arm.span)) {
                    self.arm(arm);
                } else {
                    // Between arms, where only a pattern can start.
                    self.position = Position::Other;
                }
            }
            _ => {
                if strictly {
                    self.position = Position::Expr;
                }
                visit::walk_expr(self, expr);
            }
        }
    }
}

/// Collects the identifiers a pattern binds, sub-patterns of `name @ pat` included.
struct PatIdents<'v>(&'v mut Vec<Ident>);

impl<'a, 'v> Visitor<'a> for PatIdents<'v> {
    type Result = ();

    fn visit_pat(&mut self, pat: &'a Pat) {
        if let PatKind::Ident(_, ident, _) = pat.kind {
            self.0.push(ident);
        }
        visit::walk_pat(self, pat);
    }

    // A pattern can hold an expression (a range bound, a const block); nothing in one binds.
    fn visit_expr(&mut self, _: &'a Expr) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    // These call `locate`, not `site_at`: the catcher `site_at` asserts on can only be built
    // from `std::panic::catch_unwind`, and `std` cannot be named in this crate, test builds
    // included. None of these sources makes the parser raise a fatal error, so nothing needs
    // catching.
    fn at_marker(source: &str) -> Site {
        let offset = source.find("/*HERE*/").expect("the source has a marker") as u32;
        let captured = Arc::new(eko::thread::Mutex::new(String::new()));
        locate(source, offset, &captured).expect("the source parses")
    }

    fn find<'s>(site: &'s Site, name: &str) -> Option<&'s Binding> {
        site.scope.iter().find(|b| b.name == name)
    }

    #[test]
    fn params_locals_and_items_at_statement_level() {
        let site = at_marker(
            "fn is_vowel(c: char) -> bool { true }\nfn count(s: &str) -> usize { let n = 0; /*HERE*/ n }",
        );
        assert_eq!(site.position, Position::FnBody);
        let s = find(&site, "s").expect("s is in scope");
        assert_eq!(s.kind, BindingKind::Param);
        assert_eq!(s.ty.as_deref(), Some("&str"));
        assert_eq!(find(&site, "n").map(|b| b.kind), Some(BindingKind::Local));
        assert_eq!(find(&site, "is_vowel").map(|b| b.kind), Some(BindingKind::Item(ItemCategory::Fn)));
        assert_eq!(find(&site, "count").map(|b| b.kind), Some(BindingKind::Item(ItemCategory::Fn)));
        assert!(find(&site, "c").is_none(), "another fn's param is not in scope");

        let sig = site.enclosing_fn.expect("inside a fn");
        assert_eq!(sig.name, "count");
        assert_eq!(sig.ret.as_deref(), Some("usize"));
        assert_eq!(sig.params, alloc::vec![FnParam { pattern: "s".into(), ty: "&str".into() }]);
        assert_eq!(sig.receiver, None);
        assert_eq!(site.enclosing_item.map(|i| i.path).as_deref(), Some("count"));
        assert!(site.syntax_errors.is_empty(), "{:?}", site.syntax_errors);
    }

    #[test]
    fn a_let_in_a_closed_block_is_gone_and_a_later_let_is_not_yet_here() {
        let site = at_marker("fn f() { { let hidden = 1; } let seen = 2; /*HERE*/ let later = 3; }");
        assert!(find(&site, "hidden").is_none());
        assert!(find(&site, "seen").is_some());
        assert!(find(&site, "later").is_none());

        let inner = at_marker("fn f() { { let hidden = 1; /*HERE*/ } }");
        assert!(find(&inner, "hidden").is_some());
    }

    #[test]
    fn imports_bind_their_leaf_names() {
        let site = at_marker(
            "use core::fmt::Write as W;\nuse a::{b, c::{self, d as e}, g::*};\nfn f() { /*HERE*/ }",
        );
        for name in ["W", "b", "c", "e"] {
            assert_eq!(find(&site, name).map(|b| b.kind), Some(BindingKind::Import), "{name}");
        }
        assert!(find(&site, "d").is_none(), "a renamed import binds only its new name");
        assert!(find(&site, "Write").is_none());
    }

    #[test]
    fn method_in_an_impl_in_a_module() {
        let site = at_marker(
            "mod outer { struct Thing; impl<T> Thing { fn method(&self, x: u8) -> Self { /*HERE*/ loop {} } } }",
        );
        assert_eq!(site.enclosing_item.as_ref().map(|i| i.path.as_str()), Some("outer::Thing::method"));
        let sig = site.enclosing_fn.as_ref().expect("inside a method");
        assert_eq!(sig.receiver, Some(Receiver::Ref));
        assert_eq!(sig.params, alloc::vec![FnParam { pattern: "x".into(), ty: "u8".into() }]);
        assert_eq!(sig.ret.as_deref(), Some("Self"));
        assert_eq!(find(&site, "T").map(|b| b.kind), Some(BindingKind::GenericParam));
        assert_eq!(find(&site, "x").and_then(|b| b.ty.as_deref()), Some("u8"));
        assert_eq!(find(&site, "self").map(|b| b.kind), Some(BindingKind::Param));
        assert_eq!(find(&site, "Thing").map(|b| b.kind), Some(BindingKind::Item(ItemCategory::Struct)));
    }

    #[test]
    fn a_module_does_not_see_its_parent() {
        let site = at_marker("fn top() {}\nmod m { fn inner() { /*HERE*/ } }");
        assert!(find(&site, "inner").is_some());
        assert!(find(&site, "top").is_none());
    }

    #[test]
    fn positions() {
        assert_eq!(at_marker("fn a() {}\n/*HERE*/\nfn b() {}").position, Position::Item);
        assert_eq!(at_marker("fn f() { let x = /*HERE*/1; }").position, Position::LetInit);
        assert_eq!(at_marker("fn g(a: u8) {}\nfn f() { let x = g(/*HERE*/); }").position, Position::CallArg);
        assert_eq!(at_marker("fn f(v: u8) { v.max(/*HERE*/); }").position, Position::CallArg);
        assert_eq!(at_marker("struct S; impl S { /*HERE*/ }").position, Position::ImplItem);
        assert_eq!(at_marker("trait T { /*HERE*/ }").position, Position::TraitItem);
        assert_eq!(at_marker("fn f() { let y = 1 + /*HERE*/2; }").position, Position::Expr);
        assert_eq!(at_marker("fn f(/*HERE*/) {}").position, Position::Other);
    }

    #[test]
    fn closure_and_arm_bindings() {
        let site = at_marker("fn f() { let add = |k: u8| /*HERE*/ k; }");
        let k = find(&site, "k").expect("closure param in scope");
        assert_eq!(k.kind, BindingKind::ClosureParam);
        assert_eq!(k.ty.as_deref(), Some("u8"));
        assert!(find(&site, "add").is_none(), "a let is not in scope in its own initializer");

        let site = at_marker("fn f(o: Option<u8>) { match o { Some(v) => { /*HERE*/ } None => {} } }");
        assert_eq!(find(&site, "v").map(|b| b.kind), Some(BindingKind::Local));
    }

    #[test]
    fn unparseable_source_and_bad_offsets_are_errors() {
        let captured = Arc::new(eko::thread::Mutex::new(String::new()));
        assert!(locate("fn f( {", 3, &captured).is_err());
        let captured = Arc::new(eko::thread::Mutex::new(String::new()));
        assert!(locate("fn f() {}", 100, &captured).is_err());
    }
}
