//! Structural checks: the rust-analyzer diagnostics that the shape of the tree decides.
//!
//! Ported from `crates/ide-diagnostics/src/handlers` at rust-analyzer `e8f7e90aa3`, with the
//! detection logic each handler relies on (`hir-def`'s body lowering, `hir-ty`'s inference and
//! body validation, `hir-ty`'s `decl_check`). Codes, severities and messages are rust-analyzer's.
//!
//! | code | handler | rust-analyzer's own code |
//! | --- | --- | --- |
//! | `break-outside-of-loop` | `break_outside_of_loop.rs` | `E0268` |
//! | `return-outside-of-function` | `return_outside_function.rs` | `E0572` |
//! | `await-outside-of-async` | `await_outside_of_async.rs` | `E0728` |
//! | `undeclared-label` | `undeclared_label.rs` | `undeclared-label` |
//! | `unreachable-label` | `unreachable_label.rs` | `E0767` |
//! | `incorrect-ident-case` | `incorrect_case.rs` | `non_snake_case`, `non_upper_case_globals`, `non_camel_case_types` |
//! | `unnecessary-braces` | `useless_braces.rs` | `unused_braces` |
//! | `remove-trailing-return` | `remove_trailing_return.rs` | `clippy::needless_return` |
//! | `remove-unnecessary-else` | `remove_unnecessary_else.rs` | `remove-unnecessary-else` |
//! | `field-shorthand` | `field_shorthand.rs` | `clippy::redundant_field_names` |
//! | `missing-body` | `missing_body.rs` | `syntax-error` |
//! | `duplicate-field` | `duplicate_field.rs` | `E0062` |
//! | `union-expr-must-have-exactly-one-field` | `union_expr_must_have_exactly_one_field.rs` | `E0784` |
//!
//! Lint-backed checks honour `allow`, `expect`, `warn`, `deny` and `forbid` on the enclosing
//! items, statements, expressions, fields, variants, parameters and arms, with rust-analyzer's
//! lint groups (`nonstandard_style`, `bad_style`, `unused`, `clippy::style`, `warnings`): the
//! innermost node that mentions the lint wins, and within one node the last attribute does.
//! `cfg_attr` is not evaluated, because no cfg set is known here.
//!
//! Where rust-analyzer needs resolution or types, the port keeps only the part the syntax
//! decides, and says so at the check:
//!
//! - `remove-unnecessary-else` fires when the `then` branch ends in `return`, `break`,
//!   `continue` or `become`, or in a block, `if` or `match` that does. A call to a function
//!   returning `!`, `loop {}` and `panic!()` are not recognised.
//! - `duplicate-field` fires on the second use of a name, without checking the name is a field.
//! - `union-expr-must-have-exactly-one-field` fires only for a single-segment path that names a
//!   union defined in the enclosing module or block scope, with nothing else of that name there
//!   and no item macro in between.
//! - `incorrect-ident-case` treats a bare identifier pattern as a path, and does not check it,
//!   when the file defines or imports that name as a unit or tuple item, a constant or a static,
//!   when it is a prelude variant (`None`, `Some`, `Ok`, `Err`), or when the file has a glob
//!   import and the name starts upper case. rust-analyzer resolves the name instead.
//! - `missing-body` also covers functions (rustc's "free function without a body"), which
//!   rust-analyzer leaves as a FIXME in `validate_required_body`.
//!
//! Nothing inside an unexpanded macro call is seen.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::mem;

use crate::rustc_ast::visit::{self, AssocCtxt, Visitor};
use crate::rustc_ast::*;
use crate::rustc_data_structures::fx::{FxHashMap, FxHashSet};
use crate::rustc_span::edition::Edition;
use crate::rustc_span::{Ident, Span, Symbol, kw};

use super::{Cx, Diagnostic, Severity};

const BREAK: &str = "break-outside-of-loop";
const RETURN: &str = "return-outside-of-function";
const AWAIT: &str = "await-outside-of-async";
const UNDECLARED: &str = "undeclared-label";
const UNREACHABLE: &str = "unreachable-label";
const CASE: &str = "incorrect-ident-case";
const BRACES: &str = "unnecessary-braces";
const TRAILING: &str = "remove-trailing-return";
const ELSE: &str = "remove-unnecessary-else";
const SHORTHAND: &str = "field-shorthand";
const MISSING_BODY: &str = "missing-body";
const DUPLICATE: &str = "duplicate-field";
const UNION: &str = "union-expr-must-have-exactly-one-field";

/// A lint as rust-analyzer's `handle_lints` sees it: the names an attribute can use to reach
/// it, whether `warnings` reaches it, and its level when no attribute does.
struct Lint {
    groups: &'static [&'static str],
    in_warnings: bool,
    default: Severity,
}

const NON_SNAKE_CASE: Lint = Lint {
    groups: &["non_snake_case", "nonstandard_style", "bad_style"],
    in_warnings: true,
    default: Severity::Warning,
};
const NON_UPPER_CASE_GLOBALS: Lint = Lint {
    groups: &["non_upper_case_globals", "nonstandard_style", "bad_style"],
    in_warnings: true,
    default: Severity::Warning,
};
const NON_CAMEL_CASE_TYPES: Lint = Lint {
    groups: &["non_camel_case_types", "nonstandard_style", "bad_style"],
    in_warnings: true,
    default: Severity::Warning,
};
const UNUSED_BRACES: Lint =
    Lint { groups: &["unused_braces", "unused"], in_warnings: true, default: Severity::Warning };
// Clippy lints are allow-by-default in rust-analyzer's table, and a diagnostic raised at weak
// severity keeps it, so these report weak until an attribute says otherwise.
const NEEDLESS_RETURN: Lint = Lint {
    groups: &["clippy::needless_return", "clippy::style"],
    in_warnings: false,
    default: Severity::WeakWarning,
};
const REDUNDANT_FIELD_NAMES: Lint = Lint {
    groups: &["clippy::redundant_field_names", "clippy::style"],
    in_warnings: false,
    default: Severity::WeakWarning,
};

/// Names a bare identifier pattern could be referring to rather than binding, so
/// `incorrect-ident-case` leaves them alone.
const PRELUDE_VARIANTS: &[&str] = &["None", "Some", "Ok", "Err"];

/// Which checks run. Worked out once per call, so a skipped check costs one branch.
struct Wants {
    brk: bool,
    ret: bool,
    awt: bool,
    undeclared: bool,
    unreachable: bool,
    case: bool,
    braces: bool,
    trailing: bool,
    else_: bool,
    shorthand: bool,
    missing_body: bool,
    duplicate: bool,
    union: bool,
}

impl Wants {
    fn new(cx: &Cx<'_>) -> Wants {
        // A lint can be raised to an error by `deny`, so its ceiling is `Error` too.
        let up_to_error = |code: &str| cx.wants(code, Severity::Error);
        Wants {
            brk: up_to_error(BREAK),
            ret: up_to_error(RETURN),
            awt: up_to_error(AWAIT),
            undeclared: up_to_error(UNDECLARED),
            unreachable: up_to_error(UNREACHABLE),
            case: up_to_error(CASE),
            braces: up_to_error(BRACES),
            trailing: up_to_error(TRAILING),
            else_: cx.wants(ELSE, Severity::WeakWarning),
            shorthand: up_to_error(SHORTHAND),
            missing_body: up_to_error(MISSING_BODY),
            duplicate: up_to_error(DUPLICATE),
            union: up_to_error(UNION),
        }
    }

    fn any(&self) -> bool {
        self.brk
            || self.ret
            || self.awt
            || self.undeclared
            || self.unreachable
            || self.case
            || self.braces
            || self.trailing
            || self.else_
            || self.shorthand
            || self.missing_body
            || self.duplicate
            || self.union
    }

    /// The loop and label stack is only kept when something reads it.
    fn labels(&self) -> bool {
        self.brk || self.undeclared || self.unreachable
    }
}

pub(super) fn check(cx: &Cx<'_>, krate: &Crate, out: &mut Vec<Diagnostic>) {
    let want = Wants::new(cx);
    if !want.any() {
        return;
    }
    let mut paths = PathNames::default();
    if want.case {
        for item in &krate.items {
            paths.visit_item(item);
        }
    }
    let mut checker = Checker {
        cx,
        out,
        want,
        paths,
        lints: Vec::new(),
        body: Body::NONE,
        frames: Vec::new(),
        trailing: FxHashMap::default(),
        scopes: Vec::new(),
        param_pat: 0,
    };
    checker.with_lints(&krate.attrs, |this| {
        this.with_scope(krate.items.iter().map(|item| &**item), true, |this| {
            for item in &krate.items {
                this.visit_item(item);
            }
        })
    });
}

/// The body the walk is inside, as far as `await`, `return` and binding names care.
#[derive(Clone, Copy)]
struct Body {
    /// `None` where `.await` is allowed; otherwise how rust-analyzer names the place.
    awaitable: Option<&'static str>,
    /// `return` and `become` are allowed.
    returns: bool,
    /// Binding names here are checked. rust-analyzer checks the patterns of function bodies,
    /// closures included, and nowhere else.
    bindings: bool,
}

impl Body {
    const NONE: Body = Body { awaitable: Some("unknown"), returns: false, bindings: false };

    fn constant(location: &'static str) -> Body {
        Body { awaitable: Some(location), returns: false, bindings: false }
    }

    fn function(f: &Fn) -> Body {
        let is_async = matches!(
            &f.sig.header.coroutine_marker,
            Some(m) if matches!(m.kind, CoroutineKind::Async | CoroutineKind::AsyncGen)
        );
        Body {
            awaitable: if is_async { None } else { Some("non-async function") },
            returns: true,
            bindings: true,
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum FrameKind {
    /// `value` is false for `while` and `for`, which cannot break with a value.
    Loop { value: bool },
    /// A labelled block.
    Block,
    /// A closure, a coroutine block or a constant: nothing breaks or names a label across it.
    Border,
}

#[derive(Clone, Copy)]
struct Frame {
    kind: FrameKind,
    label: Option<Symbol>,
}

const BORDER: Frame = Frame { kind: FrameKind::Border, label: None };

#[derive(Clone, Copy)]
enum Level {
    Allow,
    Warn,
    Deny,
}

/// The type-namespace names one module or block scope defines, for the union check.
struct Scope {
    module: bool,
    /// An item macro could define anything, so a lookup does not see past this scope.
    macro_items: bool,
    /// `true` when the name is a union and nothing else.
    types: FxHashMap<Symbol, bool>,
}

struct Checker<'c, 'x> {
    cx: &'c Cx<'x>,
    out: &'c mut Vec<Diagnostic>,
    want: Wants,
    paths: PathNames,
    /// Lint attributes of the enclosing nodes, innermost last. Only nodes that have one push.
    lints: Vec<Vec<(String, Level)>>,
    body: Body,
    frames: Vec<Frame>,
    /// Trailing `return`s found on entering a body, keyed by address, with the span to report.
    /// Reported when the walk reaches them, so the lint attributes in force are theirs.
    trailing: FxHashMap<usize, Span>,
    scopes: Vec<Scope>,
    /// Address of the current parameter's top-level pattern, so its binding is reported as a
    /// parameter rather than a variable.
    param_pat: usize,
}

fn addr<T>(node: &T) -> usize {
    node as *const T as usize
}

impl<'c, 'x> Checker<'c, 'x> {
    // ---- plumbing -------------------------------------------------------------------------

    fn push_range(&mut self, code: &str, severity: Severity, start: u32, end: u32, message: String) {
        self.out.push(Diagnostic { code: code.to_string(), severity, message, start, end });
    }

    fn push(&mut self, code: &str, severity: Severity, span: Span, message: String) {
        let (start, end) = (self.cx.span)(span);
        self.push_range(code, severity, start, end, message);
    }

    fn lint_level(&self, lint: &Lint) -> Option<Severity> {
        for frame in self.lints.iter().rev() {
            for (name, level) in frame.iter().rev() {
                if lint.groups.contains(&name.as_str()) || (lint.in_warnings && name == "warnings") {
                    return match level {
                        Level::Allow => None,
                        Level::Warn => Some(Severity::Warning),
                        Level::Deny => Some(Severity::Error),
                    };
                }
            }
        }
        Some(lint.default)
    }

    fn lint(&mut self, lint: &Lint, code: &str, span: Span, message: impl FnOnce() -> String) {
        if let Some(severity) = self.lint_level(lint)
            && severity.at_least(self.cx.opts.min_severity)
        {
            self.push(code, severity, span, message());
        }
    }

    fn with_lints(&mut self, attrs: &[Attribute], f: impl FnOnce(&mut Self)) {
        match lint_attrs(attrs) {
            Some(frame) => {
                self.lints.push(frame);
                f(self);
                self.lints.pop();
            }
            None => f(self),
        }
    }

    /// Enter an item that owns a body of its own: nothing outside it is a loop or a label here.
    fn with_owner(&mut self, body: Body, f: impl FnOnce(&mut Self)) {
        let saved_body = mem::replace(&mut self.body, body);
        let saved_frames = mem::take(&mut self.frames);
        f(self);
        self.frames = saved_frames;
        self.body = saved_body;
    }

    fn with_body(&mut self, body: Body, f: impl FnOnce(&mut Self)) {
        let saved = mem::replace(&mut self.body, body);
        f(self);
        self.body = saved;
    }

    fn with_frame(&mut self, frame: Frame, f: impl FnOnce(&mut Self)) {
        if !self.want.labels() {
            return f(self);
        }
        self.frames.push(frame);
        f(self);
        self.frames.pop();
    }

    fn with_scope<'i>(
        &mut self,
        items: impl Iterator<Item = &'i Item>,
        module: bool,
        f: impl FnOnce(&mut Self),
    ) {
        if !self.want.union {
            return f(self);
        }
        let mut scope = Scope { module, macro_items: false, types: FxHashMap::default() };
        for item in items {
            scope_item(&mut scope, item);
        }
        self.scopes.push(scope);
        f(self);
        self.scopes.pop();
    }

    // ---- labels and loops: break_outside_of_loop, undeclared_label, unreachable_label -----

    /// `Some(name)` when the label names a frame that can be reached from here.
    fn resolve_label(&mut self, label: &Label) -> Option<Symbol> {
        let name = label.ident.name;
        // `Some(crossed a border)` when a frame carries the label.
        let mut found = None;
        let mut crossed_border = false;
        for frame in self.frames.iter().rev() {
            if frame.label == Some(name) {
                found = Some(crossed_border);
                break;
            }
            if frame.kind == FrameKind::Border {
                crossed_border = true;
            }
        }
        match found {
            Some(false) => return Some(name),
            Some(true) => {
                if self.want.unreachable {
                    self.push(
                        UNREACHABLE,
                        Severity::Error,
                        label.ident.span,
                        format!("use of unreachable label `{name}`"),
                    );
                }
            }
            None => {
                if self.want.undeclared {
                    self.push(
                        UNDECLARED,
                        Severity::Error,
                        label.ident.span,
                        format!("use of undeclared label `{name}`"),
                    );
                }
            }
        }
        None
    }

    /// rust-analyzer's `find_breakable` and `find_continuable`, after its label resolution: a
    /// label that did not resolve is treated as absent, as its lowering does.
    fn check_jump(&mut self, e: &Expr, label: Option<&Label>, has_value: bool, is_break: bool) {
        if !self.want.labels() {
            return;
        }
        let resolved = label.and_then(|label| self.resolve_label(label));
        if !self.want.brk {
            return;
        }
        let mut target = None;
        for frame in self.frames.iter().rev() {
            if frame.kind == FrameKind::Border {
                break;
            }
            let found = match resolved {
                Some(name) => frame.label == Some(name),
                None => matches!(frame.kind, FrameKind::Loop { .. }),
            };
            if found {
                target = Some(frame.kind);
                break;
            }
        }
        let message = if is_break {
            match target {
                None => "break outside of loop",
                Some(FrameKind::Loop { value: false }) if has_value => {
                    "can't break with a value in this position"
                }
                Some(_) => return,
            }
        } else {
            match target {
                None | Some(FrameKind::Block) => "continue outside of loop",
                Some(_) => return,
            }
        };
        self.push(BREAK, Severity::Error, e.span, message.to_string());
    }

    // ---- remove_trailing_return ----------------------------------------------------------

    fn trailing_block(&mut self, block: &Block) {
        let Some(stmt) = block.stmts.last() else { return };
        match &stmt.kind {
            StmtKind::Expr(e) => self.trailing_expr(e, None),
            StmtKind::Semi(e) => self.trailing_expr(e, Some(stmt.span)),
            _ => {}
        }
    }

    /// `stmt` is the statement span when `e` is directly an expression statement: a trailing
    /// `return x;` is reported with its semicolon, as rust-analyzer does.
    fn trailing_expr(&mut self, e: &Expr, stmt: Option<Span>) {
        match &e.kind {
            ExprKind::Block(block, _) => self.trailing_block(block),
            ExprKind::If(_, then, els) => {
                self.trailing_block(then);
                if let Some(els) = els {
                    self.trailing_expr(els, None);
                }
            }
            ExprKind::Match(_, arms, _) => {
                for arm in arms {
                    if let Some(body) = &arm.body {
                        self.trailing_expr(body, None);
                    }
                }
            }
            ExprKind::Paren(inner) => self.trailing_expr(inner, None),
            ExprKind::Ret(_) => {
                self.trailing.insert(addr(e), stmt.unwrap_or(e.span));
            }
            _ => {}
        }
    }

    // ---- remove_unnecessary_else ---------------------------------------------------------

    /// An `if` in statement position, and the `else if`s chained to it. rust-analyzer skips an
    /// `if` whose chain does not start at a statement.
    fn else_chain(&mut self, mut e: &Expr) {
        while let ExprKind::If(_, then, Some(els)) = &e.kind {
            if block_diverges(then) {
                let from = (self.cx.span)(then.span).1;
                let to = (self.cx.span)(els.span).0;
                if let Some((start, end)) = else_keyword(self.cx.source, from, to) {
                    self.push_range(
                        ELSE,
                        Severity::WeakWarning,
                        start,
                        end,
                        "remove unnecessary else block".to_string(),
                    );
                }
            }
            e = &**els;
        }
    }

    // ---- incorrect_case ------------------------------------------------------------------

    fn case(&mut self, ident: Ident, case: Case, what: &str, raw_if_keyword: bool) {
        if !self.want.case || ident.name == kw::Underscore {
            return;
        }
        let name = ident.name.as_str();
        let suggested = match case {
            Case::Snake => to_lower_snake_case(name),
            Case::UpperSnake => to_upper_snake_case(name),
            Case::Camel => to_camel_case(name),
        };
        let Some(mut suggested) = suggested else { return };
        if raw_if_keyword && is_keyword(&suggested) {
            suggested.insert_str(0, "r#");
        }
        let (lint, case_name) = match case {
            Case::Snake => (&NON_SNAKE_CASE, "snake_case"),
            Case::UpperSnake => (&NON_UPPER_CASE_GLOBALS, "UPPER_SNAKE_CASE"),
            Case::Camel => (&NON_CAMEL_CASE_TYPES, "UpperCamelCase"),
        };
        self.lint(lint, CASE, ident.span, || {
            format!("{what} `{name}` should have {case_name} name, e.g. `{suggested}`")
        });
    }

    fn fn_name(&mut self, f: &Fn, attrs: &[Attribute]) {
        // rustc and rust-analyzer excuse `#[no_mangle]` on a function with a foreign ABI.
        let rust_abi = match &f.sig.header.ext {
            Extern::None => true,
            Extern::Implicit(_) => false,
            Extern::Explicit(abi, _) => abi.symbol_unescaped.as_str() == "Rust",
        };
        if rust_abi || !has_attr(attrs, "no_mangle") {
            self.case(f.ident, Case::Snake, "Function", false);
        }
    }

    /// A binding: a parameter at a parameter's top level, a variable anywhere else.
    fn binding(&mut self, pat: &Pat) {
        let PatKind::Ident(mode, ident, sub) = &pat.kind else { return };
        if *mode == BindingMode::NONE && sub.is_none() && self.paths.may_name(ident.name) {
            return;
        }
        let what = if addr(pat) == self.param_pat { "Parameter" } else { "Variable" };
        self.case(*ident, Case::Snake, what, true);
    }

    // ---- missing_body --------------------------------------------------------------------

    fn missing_body(&mut self, span: Span, message: &str) {
        if self.want.missing_body {
            self.push(MISSING_BODY, Severity::Error, span, message.to_string());
        }
    }

    // ---- record literals and patterns: duplicate_field, field_shorthand, union ----------

    fn struct_expr(&mut self, se: &StructExpr, span: Span) {
        if self.want.duplicate && se.fields.len() > 1 {
            let mut seen = FxHashSet::default();
            for field in &se.fields {
                if !seen.insert(field.ident.name) {
                    self.push(
                        DUPLICATE,
                        Severity::Error,
                        field.span,
                        "field specified more than once".to_string(),
                    );
                }
            }
        }
        if self.want.union
            && se.fields.len() != 1
            && se.qself.is_none()
            && let [segment] = &*se.path.segments
            && self.local_union(segment.ident.name)
        {
            self.push(
                UNION,
                Severity::Error,
                span,
                "union expressions should have exactly one field".to_string(),
            );
        }
        if self.want.shorthand {
            for field in &se.fields {
                if field.is_shorthand || is_tuple_index(field.ident) {
                    continue;
                }
                let ExprKind::Path(None, path) = &field.expr.kind else { continue };
                let [segment] = &*path.segments else { continue };
                if segment.args.is_some() || segment.ident.name != field.ident.name {
                    continue;
                }
                // rust-analyzer compares the text, which also rules out `r#` on one side only.
                if self.cx.text(field.expr.span) != self.cx.text(field.ident.span) {
                    continue;
                }
                self.lint(&REDUNDANT_FIELD_NAMES, SHORTHAND, field.span, || {
                    "Shorthand struct initialization".to_string()
                });
            }
        }
    }

    fn struct_pat(&mut self, fields: &[PatField]) {
        if self.want.duplicate && fields.len() > 1 {
            let mut seen = FxHashSet::default();
            for field in fields {
                if !seen.insert(field.ident.name) {
                    self.push(
                        DUPLICATE,
                        Severity::Error,
                        field.span,
                        "field specified more than once".to_string(),
                    );
                }
            }
        }
        if self.want.shorthand {
            for field in fields {
                if field.is_shorthand || is_tuple_index(field.ident) {
                    continue;
                }
                let PatKind::Ident(mode, ident, None) = &field.pat.kind else { continue };
                if *mode != BindingMode::NONE
                    || ident.name != field.ident.name
                    || self.cx.text(field.pat.span) != self.cx.text(field.ident.span)
                {
                    continue;
                }
                self.lint(&REDUNDANT_FIELD_NAMES, SHORTHAND, field.span, || {
                    "Shorthand struct pattern".to_string()
                });
            }
        }
    }

    /// The name is a union defined in scope here, and certainly nothing else.
    fn local_union(&self, name: Symbol) -> bool {
        for scope in self.scopes.iter().rev() {
            if let Some(&is_union) = scope.types.get(&name) {
                return is_union;
            }
            if scope.macro_items || scope.module {
                return false;
            }
        }
        false
    }

    // ---- items ---------------------------------------------------------------------------

    fn item<'a>(&mut self, item: &'a Item) {
        match &item.kind {
            ItemKind::Fn(f) => {
                self.fn_name(f, &item.attrs);
                if f.body.is_none() && !has_attr(&item.attrs, "rustc_intrinsic") {
                    self.missing_body(item.span, "free function without a body");
                }
                self.with_owner(Body::function(f), |this| {
                    if this.want.trailing
                        && let Some(body) = &f.body
                    {
                        this.trailing_block(body);
                    }
                    visit::walk_item(this, item);
                });
            }
            ItemKind::Const(c) => {
                self.case(c.ident, Case::UpperSnake, "Constant", false);
                if c.body.is_none() && c.kind == ConstItemKind::Body {
                    self.missing_body(item.span, "free constant item without body");
                }
                self.with_owner(Body::constant("constant"), |this| visit::walk_item(this, item));
            }
            ItemKind::Static(s) => {
                if !has_attr(&item.attrs, "no_mangle") {
                    self.case(s.ident, Case::UpperSnake, "Static variable", false);
                }
                if s.expr.is_none() {
                    self.missing_body(item.span, "free static item without body");
                }
                self.with_owner(Body::constant("static"), |this| visit::walk_item(this, item));
            }
            ItemKind::ConstBlock(_) => {
                self.with_owner(Body::constant("constant"), |this| visit::walk_item(this, item));
            }
            ItemKind::TyAlias(t) => {
                self.case(t.ident, Case::Camel, "Type alias", false);
                if t.ty.is_none() {
                    self.missing_body(item.span, "free type alias without body");
                }
                self.with_owner(Body::NONE, |this| visit::walk_item(this, item));
            }
            ItemKind::Mod(_, ident, kind) => {
                self.case(*ident, Case::Snake, "Module", false);
                self.with_owner(Body::NONE, |this| match kind {
                    ModKind::Loaded(items, _, _) => {
                        this.with_scope(items.iter().map(|item| &**item), true, |this| {
                            visit::walk_item(this, item)
                        })
                    }
                    ModKind::Unloaded => visit::walk_item(this, item),
                });
            }
            ItemKind::Struct(ident, ..) | ItemKind::Union(ident, ..) | ItemKind::Enum(ident, ..) => {
                // rustc excuses `#[repr(C)]` types, which mostly mirror C names.
                if !repr_c(&item.attrs) {
                    let what = match &item.kind {
                        ItemKind::Struct(..) => "Structure",
                        ItemKind::Union(..) => "Union",
                        _ => "Enum",
                    };
                    self.case(*ident, Case::Camel, what, false);
                }
                self.with_owner(Body::NONE, |this| visit::walk_item(this, item));
            }
            ItemKind::Trait(t) => {
                self.case(t.ident, Case::Camel, "Trait", false);
                self.with_owner(Body::NONE, |this| visit::walk_item(this, item));
            }
            _ => self.with_owner(Body::NONE, |this| visit::walk_item(this, item)),
        }
    }

    fn assoc_item<'a>(&mut self, item: &'a AssocItem, ctxt: AssocCtxt) {
        let in_impl = matches!(ctxt, AssocCtxt::Impl { .. });
        // A trait implementation's names are the trait's, so only its bodies are checked.
        let in_trait_impl = matches!(ctxt, AssocCtxt::Impl { of_trait: true });
        match &item.kind {
            AssocItemKind::Fn(f) => {
                if !in_trait_impl {
                    self.fn_name(f, &item.attrs);
                }
                if in_impl && f.body.is_none() {
                    self.missing_body(item.span, "associated function in `impl` without body");
                }
                self.with_owner(Body::function(f), |this| {
                    if this.want.trailing
                        && let Some(body) = &f.body
                    {
                        this.trailing_block(body);
                    }
                    visit::walk_assoc_item(this, item, ctxt);
                });
            }
            AssocItemKind::Const(c) => {
                if !in_trait_impl {
                    self.case(c.ident, Case::UpperSnake, "Constant", false);
                }
                if in_impl && c.body.is_none() && c.kind == ConstItemKind::Body {
                    self.missing_body(item.span, "associated constant in `impl` without body");
                }
                self.with_owner(Body::constant("constant"), |this| {
                    visit::walk_assoc_item(this, item, ctxt)
                });
            }
            AssocItemKind::Type(t) => {
                if !in_trait_impl {
                    self.case(t.ident, Case::Camel, "Type alias", false);
                }
                if in_impl && t.ty.is_none() {
                    self.missing_body(item.span, "associated type in `impl` without body");
                }
                self.with_owner(Body::NONE, |this| visit::walk_assoc_item(this, item, ctxt));
            }
            _ => self.with_owner(Body::NONE, |this| visit::walk_assoc_item(this, item, ctxt)),
        }
    }

    // ---- expressions ---------------------------------------------------------------------

    fn expr<'a>(&mut self, e: &'a Expr) {
        match &e.kind {
            ExprKind::Closure(closure) => {
                let is_async = matches!(
                    &closure.coroutine_marker,
                    Some(m) if matches!(m.kind, CoroutineKind::Async | CoroutineKind::AsyncGen)
                );
                if self.want.trailing {
                    self.trailing_expr(&closure.body, None);
                }
                let body = Body {
                    awaitable: if is_async { None } else { Some("non-async closure") },
                    returns: true,
                    bindings: self.body.bindings,
                };
                self.with_body(body, |this| {
                    this.with_frame(BORDER, |this| visit::walk_expr(this, e))
                });
                return;
            }
            ExprKind::Gen(_, _, kind, _) => {
                let awaitable = match kind {
                    CoroutineKind::Gen => Some("non-async gen block"),
                    CoroutineKind::Async | CoroutineKind::AsyncGen => None,
                };
                let body = Body { awaitable, ..self.body };
                self.with_body(body, |this| {
                    this.with_frame(BORDER, |this| visit::walk_expr(this, e))
                });
                return;
            }
            ExprKind::ConstBlock(anon) => {
                // rust-analyzer infers an inline `const` inside the enclosing body, so `return`
                // keeps its meaning, but no loop or label is visible through it.
                let body = Body { awaitable: Some("constant block"), ..self.body };
                self.with_body(body, |this| {
                    this.with_frame(BORDER, |this| this.visit_expr(&anon.value))
                });
                return;
            }
            ExprKind::Repeat(element, count) => {
                self.visit_expr(element);
                self.with_frame(BORDER, |this| this.visit_expr(&count.value));
                return;
            }
            ExprKind::Loop(_, label, _) => {
                let frame =
                    Frame { kind: FrameKind::Loop { value: true }, label: label.map(|l| l.ident.name) };
                self.with_frame(frame, |this| visit::walk_expr(this, e));
                return;
            }
            ExprKind::While(_, _, label) => {
                // The label is in scope in the condition too.
                let frame = Frame {
                    kind: FrameKind::Loop { value: false },
                    label: label.map(|l| l.ident.name),
                };
                self.with_frame(frame, |this| visit::walk_expr(this, e));
                return;
            }
            ExprKind::ForLoop(for_loop) => {
                self.visit_pat(&for_loop.pat);
                self.visit_expr(&for_loop.iter);
                let frame = Frame {
                    kind: FrameKind::Loop { value: false },
                    label: for_loop.label.map(|l| l.ident.name),
                };
                self.with_frame(frame, |this| this.visit_block(&for_loop.body));
                return;
            }
            ExprKind::Block(_, Some(label)) => {
                let frame = Frame { kind: FrameKind::Block, label: Some(label.ident.name) };
                self.with_frame(frame, |this| visit::walk_expr(this, e));
                return;
            }
            ExprKind::Break(label, value) => {
                self.check_jump(e, label.as_ref(), value.is_some(), true);
            }
            ExprKind::Continue(label) => self.check_jump(e, label.as_ref(), false, false),
            ExprKind::Ret(_) => {
                if self.want.ret && !self.body.returns {
                    self.push(
                        RETURN,
                        Severity::Error,
                        e.span,
                        "return statement outside of function body".to_string(),
                    );
                }
                if let Some(span) = self.trailing.remove(&addr(e)) {
                    self.lint(&NEEDLESS_RETURN, TRAILING, span, || {
                        "replace return <expr>; with <expr>".to_string()
                    });
                }
            }
            ExprKind::Become(_) => {
                if self.want.ret && !self.body.returns {
                    self.push(
                        RETURN,
                        Severity::Error,
                        e.span,
                        "become statement outside of function body".to_string(),
                    );
                }
            }
            ExprKind::Await(_, await_kw) => {
                if self.want.awt
                    && let Some(location) = self.body.awaitable
                {
                    self.push(
                        AWAIT,
                        Severity::Error,
                        *await_kw,
                        format!("`await` is used inside {location}, which is not an `async` context"),
                    );
                }
            }
            ExprKind::Struct(se) => self.struct_expr(se, e.span),
            _ => {}
        }
        visit::walk_expr(self, e);
    }
}

#[derive(Clone, Copy)]
enum Case {
    Snake,
    UpperSnake,
    Camel,
}

impl<'a, 'c, 'x> Visitor<'a> for Checker<'c, 'x> {
    type Result = ();

    fn visit_item(&mut self, item: &'a Item) {
        self.with_lints(&item.attrs, |this| this.item(item));
    }

    fn visit_assoc_item(&mut self, item: &'a AssocItem, ctxt: AssocCtxt) {
        self.with_lints(&item.attrs, |this| this.assoc_item(item, ctxt));
    }

    fn visit_foreign_item(&mut self, item: &'a ForeignItem) {
        // rust-analyzer skips the names of foreign functions and statics, and their parameters,
        // but not foreign types.
        self.with_lints(&item.attrs, |this| {
            if let ForeignItemKind::TyAlias(t) = &item.kind {
                this.case(t.ident, Case::Camel, "Type alias", false);
            }
            this.with_owner(Body::NONE, |this| visit::walk_item(this, item));
        });
    }

    fn visit_variant(&mut self, variant: &'a Variant) {
        self.with_lints(&variant.attrs, |this| {
            this.case(variant.ident, Case::Camel, "Variant", false);
            this.visit_variant_data(&variant.data);
            if let Some(disr) = &variant.disr_expr {
                this.with_owner(Body::constant("enum variant"), |this| this.visit_expr(&disr.value));
            }
        });
    }

    fn visit_field_def(&mut self, field: &'a FieldDef) {
        self.with_lints(&field.attrs, |this| {
            if let Some(ident) = field.ident {
                this.case(ident, Case::Snake, "Field", false);
            }
            visit::walk_field_def(this, field);
        });
    }

    fn visit_anon_const(&mut self, anon: &'a AnonConst) {
        // Array lengths in types and const generic arguments: bodies of their own.
        self.with_owner(Body::constant("constant"), |this| this.visit_expr(&anon.value));
    }

    fn visit_ty(&mut self, ty: &'a Ty) {
        // A function pointer type's parameter names are not bindings.
        let saved = self.body.bindings;
        self.body.bindings = false;
        visit::walk_ty(self, ty);
        self.body.bindings = saved;
    }

    fn visit_block(&mut self, block: &'a Block) {
        // Only the union check keeps scopes, and only a block with items is a scope of its own.
        let has_items = self.want.union
            && block.stmts.iter().any(|s| matches!(s.kind, StmtKind::Item(_) | StmtKind::MacCall(_)));
        if !has_items {
            return visit::walk_block(self, block);
        }
        let items = block.stmts.iter().filter_map(|s| match &s.kind {
            StmtKind::Item(item) => Some(&**item),
            _ => None,
        });
        // A statement macro can expand to items.
        let macro_stmts = block.stmts.iter().any(|s| matches!(s.kind, StmtKind::MacCall(_)));
        self.with_scope(items, false, |this| {
            if macro_stmts && let Some(scope) = this.scopes.last_mut() {
                scope.macro_items = true;
            }
            visit::walk_block(this, block)
        });
    }

    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        if self.want.else_
            && let StmtKind::Expr(e) | StmtKind::Semi(e) = &stmt.kind
        {
            self.else_chain(e);
        }
        visit::walk_stmt(self, stmt);
    }

    fn visit_local(&mut self, local: &'a Local) {
        self.with_lints(&local.attrs, |this| visit::walk_local(this, local));
    }

    fn visit_arm(&mut self, arm: &'a Arm) {
        self.with_lints(&arm.attrs, |this| visit::walk_arm(this, arm));
    }

    fn visit_param(&mut self, param: &'a Param) {
        self.with_lints(&param.attrs, |this| {
            let saved = mem::replace(&mut this.param_pat, addr(&*param.pat));
            visit::walk_param(this, param);
            this.param_pat = saved;
        });
    }

    fn visit_expr(&mut self, e: &'a Expr) {
        self.with_lints(&e.attrs, |this| this.expr(e));
    }

    fn visit_expr_field(&mut self, field: &'a ExprField) {
        self.with_lints(&field.attrs, |this| visit::walk_expr_field(this, field));
    }

    fn visit_pat(&mut self, pat: &'a Pat) {
        if self.body.bindings {
            self.binding(pat);
        }
        if let PatKind::Struct(_, _, fields, _) = &pat.kind {
            self.struct_pat(fields);
        }
        visit::walk_pat(self, pat);
    }

    fn visit_pat_field(&mut self, field: &'a PatField) {
        // A shorthand field is a use of the field's name, not a declaration.
        if field.is_shorthand {
            return;
        }
        self.with_lints(&field.attrs, |this| visit::walk_pat_field(this, field));
    }

    fn visit_use_tree(&mut self, tree: &'a UseTree) {
        if self.want.braces
            && let UseTreeKind::Nested { items, span } = &tree.kind
            && let [(single, _)] = &**items
            // `{self}` and `{a::self}` need the braces; `{*}` has no path to keep.
            && let Some(last) = single.prefix.segments.last()
            && last.ident.name != kw::SelfLower
        {
            let text = self.cx.text(*span);
            // A comment inside is taken to be a path someone commented out.
            if !text.contains("//") && !text.contains("/*") {
                self.lint(&UNUSED_BRACES, BRACES, *span, || {
                    "Unnecessary braces in use statement".to_string()
                });
            }
        }
        visit::walk_use_tree(self, tree);
    }
}

// ---- helpers ------------------------------------------------------------------------------

/// The lint attributes among `attrs`, in source order, or `None` when there are none.
fn lint_attrs(attrs: &[Attribute]) -> Option<Vec<(String, Level)>> {
    let mut entries = Vec::new();
    for attr in attrs {
        let Some(name) = attr.name() else { continue };
        let level = match name.as_str() {
            "allow" | "expect" => Level::Allow,
            "warn" => Level::Warn,
            "deny" | "forbid" => Level::Deny,
            _ => continue,
        };
        let Some(list) = attr.meta_item_list() else { continue };
        for inner in &list {
            if let Some(meta) = inner.meta_item() {
                let mut path = String::new();
                for (i, segment) in meta.path.segments.iter().enumerate() {
                    if i > 0 {
                        path.push_str("::");
                    }
                    path.push_str(segment.ident.name.as_str());
                }
                entries.push((path, level));
            }
        }
    }
    (!entries.is_empty()).then_some(entries)
}

fn has_attr(attrs: &[Attribute], name: &str) -> bool {
    attrs.iter().any(|attr| attr.name().is_some_and(|n| n.as_str() == name))
}

fn repr_c(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| {
        attr.name().is_some_and(|n| n.as_str() == "repr")
            && attr.meta_item_list().is_some_and(|list| {
                list.iter().any(|inner| inner.name().is_some_and(|n| n.as_str() == "C"))
            })
    })
}

fn is_tuple_index(ident: Ident) -> bool {
    ident.name.as_str().bytes().next().is_some_and(|b| b.is_ascii_digit())
}

fn is_keyword(text: &str) -> bool {
    let symbol = Symbol::intern(text);
    symbol.can_be_raw() && symbol.is_reserved(|| Edition::Edition2024)
}

/// The block's value is `!` by its shape alone. See the module header for what is missed.
fn block_diverges(block: &Block) -> bool {
    match block.stmts.last() {
        Some(Stmt { kind: StmtKind::Expr(e) | StmtKind::Semi(e), .. }) => expr_diverges(e),
        _ => false,
    }
}

fn expr_diverges(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Ret(_) | ExprKind::Break(..) | ExprKind::Continue(_) | ExprKind::Become(_) => {
            true
        }
        ExprKind::Paren(inner) => expr_diverges(inner),
        // A labelled block can be broken out of, so its type is not decided here.
        ExprKind::Block(block, None) => block_diverges(block),
        ExprKind::If(_, then, Some(els)) => block_diverges(then) && expr_diverges(els),
        ExprKind::Match(_, arms, _) => {
            arms.iter().all(|arm| arm.body.as_deref().is_some_and(expr_diverges))
        }
        _ => false,
    }
}

/// Where the `else` keyword sits between the end of a `then` block and its `else` branch.
/// Only whitespace and comments can come between, so this reads at most a few bytes.
fn else_keyword(source: &str, from: u32, to: u32) -> Option<(u32, u32)> {
    let bytes = source.as_bytes();
    let to = (to as usize).min(bytes.len());
    let mut i = from as usize;
    while i < to {
        match bytes[i] {
            b' ' | b'\t' | b'\n' | b'\r' => i += 1,
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < to && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                let mut depth = 0usize;
                while i < to {
                    if bytes[i..].starts_with(b"/*") {
                        depth += 1;
                        i += 2;
                    } else if bytes[i..].starts_with(b"*/") {
                        depth -= 1;
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
            }
            _ => {
                return bytes[i..to].starts_with(b"else").then_some((i as u32, i as u32 + 4));
            }
        }
    }
    None
}

/// Record one item of a scope into its type-namespace table.
fn scope_item(scope: &mut Scope, item: &Item) {
    let types = &mut scope.types;
    let mut add = |name: Symbol, is_union: bool| {
        types.entry(name).and_modify(|u| *u = false).or_insert(is_union);
    };
    match &item.kind {
        ItemKind::Union(ident, ..) => add(ident.name, true),
        ItemKind::Struct(ident, ..)
        | ItemKind::Enum(ident, ..)
        | ItemKind::Mod(_, ident, _)
        | ItemKind::ExternCrate(_, ident) => add(ident.name, false),
        ItemKind::TyAlias(t) => add(t.ident.name, false),
        ItemKind::Trait(t) => add(t.ident.name, false),
        ItemKind::TraitAlias(t) => add(t.ident.name, false),
        ItemKind::Use(tree) => use_bindings(tree, None, &mut |name| add(name, false)),
        ItemKind::ForeignMod(fm) => {
            for foreign in &fm.items {
                match &foreign.kind {
                    ForeignItemKind::TyAlias(t) => add(t.ident.name, false),
                    ForeignItemKind::MacCall(_) => scope.macro_items = true,
                    _ => {}
                }
            }
        }
        ItemKind::MacCall(_) => scope.macro_items = true,
        _ => {}
    }
}

/// The names a `use` tree binds. `parent` is the last segment above, for `{self}`.
fn use_bindings(tree: &UseTree, parent: Option<Symbol>, add: &mut dyn FnMut(Symbol)) {
    let last = match tree.prefix.segments.last() {
        Some(segment) if segment.ident.name == kw::SelfLower => parent,
        Some(segment) => Some(segment.ident.name),
        None => parent,
    };
    match &tree.kind {
        UseTreeKind::Simple(rename) => {
            if let Some(name) = rename.map(|r| r.name).or(last) {
                add(name);
            }
        }
        UseTreeKind::Nested { items, .. } => {
            for (nested, _) in items {
                use_bindings(nested, last, add);
            }
        }
        UseTreeKind::Glob(_) => {}
    }
}

/// Every name in the crate a bare identifier pattern might be naming rather than binding.
/// One walk, and only when `incorrect-ident-case` runs.
#[derive(Default)]
struct PathNames {
    names: FxHashSet<Symbol>,
    glob: bool,
}

impl PathNames {
    fn may_name(&self, name: Symbol) -> bool {
        let text = name.as_str();
        self.names.contains(&name)
            || PRELUDE_VARIANTS.contains(&text)
            || (self.glob && text.chars().next().is_some_and(char::is_uppercase))
    }

    fn use_tree(&mut self, tree: &UseTree) {
        if has_glob(tree) {
            self.glob = true;
        }
        let names = &mut self.names;
        use_bindings(tree, None, &mut |name| {
            names.insert(name);
        });
    }
}

fn has_glob(tree: &UseTree) -> bool {
    match &tree.kind {
        UseTreeKind::Glob(_) => true,
        UseTreeKind::Nested { items, .. } => items.iter().any(|(nested, _)| has_glob(nested)),
        UseTreeKind::Simple(_) => false,
    }
}

impl<'a> Visitor<'a> for PathNames {
    type Result = ();

    fn visit_item(&mut self, item: &'a Item) {
        match &item.kind {
            ItemKind::Enum(_, _, def) => {
                for variant in &def.variants {
                    if !matches!(variant.data, VariantData::Struct { .. }) {
                        self.names.insert(variant.ident.name);
                    }
                }
            }
            ItemKind::Struct(ident, _, data) if !matches!(data, VariantData::Struct { .. }) => {
                self.names.insert(ident.name);
            }
            ItemKind::Const(c) => {
                self.names.insert(c.ident.name);
            }
            ItemKind::Static(s) => {
                self.names.insert(s.ident.name);
            }
            ItemKind::Use(tree) => self.use_tree(tree),
            _ => {}
        }
        visit::walk_item(self, item);
    }

    fn visit_foreign_item(&mut self, item: &'a ForeignItem) {
        if let ForeignItemKind::Static(s) = &item.kind {
            self.names.insert(s.ident.name);
        }
        visit::walk_item(self, item);
    }
}

// ---- case conversion, from rust-analyzer's `hir-ty/src/diagnostics/decl_check/case_conv.rs`
// ---- and `stdx`, which took them from rustc's `nonstandard_style.rs` ------------------------

/// `None` when `ident` is already UpperCamelCase.
fn to_camel_case(ident: &str) -> Option<String> {
    if is_camel_case(ident) {
        return None;
    }
    let mut out = String::new();
    let mut prev: Option<String> = None;
    for component in ident.trim_matches('_').split('_').filter(|c| !c.is_empty()) {
        let mut cased = String::with_capacity(component.len());
        let mut new_word = true;
        let mut prev_is_lower_case = true;
        for c in component.chars() {
            // Keep an uppercase letter after a lowercase one, so `camelCase` becomes `CamelCase`.
            if prev_is_lower_case && c.is_uppercase() {
                new_word = true;
            }
            if new_word {
                cased.extend(c.to_uppercase());
            } else {
                cased.extend(c.to_lowercase());
            }
            prev_is_lower_case = c.is_lowercase();
            new_word = false;
        }
        // Separate two components with an underscore when case cannot mark the boundary.
        let join = prev.as_ref().is_some_and(|prev| {
            match (prev.chars().last(), cased.chars().next()) {
                (Some(l), Some(f)) => !char_has_case(l) && !char_has_case(f),
                _ => false,
            }
        });
        if join {
            out.push('_');
        }
        out.push_str(&cased);
        prev = Some(cased);
    }
    Some(out)
}

/// `None` when `ident` is already lower_snake_case.
fn to_lower_snake_case(ident: &str) -> Option<String> {
    if is_lower_snake_case(ident) {
        return None;
    } else if is_upper_snake_case(ident) {
        return Some(ident.to_lowercase());
    }
    Some(to_snake_case(ident, false))
}

/// `None` when `ident` is already UPPER_SNAKE_CASE.
fn to_upper_snake_case(ident: &str) -> Option<String> {
    if is_upper_snake_case(ident) {
        return None;
    } else if is_lower_snake_case(ident) {
        return Some(ident.to_uppercase());
    }
    Some(to_snake_case(ident, true))
}

fn to_snake_case(s: &str, upper: bool) -> String {
    let mut words: Vec<String> = Vec::new();
    // Leading underscores are kept, one empty word each.
    let trimmed = s.trim_start_matches('_');
    for _ in 0..(s.len() - trimmed.len()) {
        words.push(String::new());
    }
    for part in trimmed.split('_') {
        if part.is_empty() {
            continue;
        }
        let mut last_upper = false;
        let mut buf = String::new();
        for ch in part.chars() {
            if !buf.is_empty() && buf != "'" && ch.is_uppercase() && !last_upper {
                words.push(mem::take(&mut buf));
            }
            last_upper = ch.is_uppercase();
            if upper {
                buf.extend(ch.to_uppercase());
            } else {
                buf.extend(ch.to_lowercase());
            }
        }
        words.push(buf);
    }
    words.join("_")
}

fn char_has_case(c: char) -> bool {
    c.is_lowercase() || c.is_uppercase()
}

fn is_camel_case(name: &str) -> bool {
    let name = name.trim_matches('_');
    if name.is_empty() {
        return true;
    }
    let mut fst: Option<char> = None;
    // Start with a non-lowercase letter rather than a non-uppercase one: some scripts have no
    // case at all.
    name.chars().next().is_none_or(|c| !c.is_lowercase())
        && !name.contains("__")
        && !name.chars().any(|snd| {
            let ret = match fst {
                None => false,
                Some(fst) => char_has_case(fst) && snd == '_' || char_has_case(snd) && fst == '_',
            };
            fst = Some(snd);
            ret
        })
}

fn is_lower_snake_case(ident: &str) -> bool {
    is_snake_case(ident, char::is_uppercase)
}

fn is_upper_snake_case(ident: &str) -> bool {
    is_snake_case(ident, char::is_lowercase)
}

fn is_snake_case(ident: &str, wrong_case: fn(char) -> bool) -> bool {
    if ident.is_empty() {
        return true;
    }
    let ident = ident.trim_matches('_');
    let mut allow_underscore = true;
    ident.chars().all(|c| {
        allow_underscore = match c {
            '_' if !allow_underscore => return false,
            '_' => false,
            // Checking for the wrong case rather than the right one, because some characters
            // have no case.
            c if !wrong_case(c) => true,
            _ => return false,
        };
        true
    })
}

#[cfg(test)]
mod tests {
    //! rust-analyzer's tests for the ported handlers, as close to verbatim as the port allows.
    //! A fixture's `//^^^ severity: message` lines are its expectations, read the way
    //! rust-analyzer's `extract_annotations` reads them. Tests that only exercise a quick fix,
    //! or need several files, crates or a cfg set, are not ported.

    use alloc::collections::BTreeMap;
    use alloc::format;
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;

    use super::*;
    use crate::frontend_facts::diagnostics::{Options, run};

    type Found = Vec<(u32, u32, String)>;

    fn severity(s: Severity) -> &'static str {
        match s {
            Severity::Error => "error",
            Severity::Warning => "warn",
            Severity::WeakWarning => "weak",
        }
    }

    /// Fixture metadata lines (`//- minicore: ...`) mean nothing without rust-analyzer's test
    /// crate graph, so they are dropped before parsing and before reading annotations.
    fn strip_meta(fixture: &str) -> String {
        let mut out = String::new();
        for line in fixture.split_inclusive('\n') {
            if !line.trim_start().starts_with("//-") {
                out.push_str(line);
            }
        }
        out
    }

    fn found(source: &str, disabled: &[&str]) -> Found {
        let opts = Options::default();
        let out = run(source, &opts, |cx, krate, out| check(cx, krate, out))
            .unwrap_or_else(|errors| panic!("fixture does not parse: {errors:?}\n{source}"));
        out.into_iter()
            .filter(|d| !disabled.contains(&d.code.as_str()))
            .map(|d| (d.start, d.end, format!("{}: {}", severity(d.severity), d.message)))
            .collect()
    }

    /// rust-analyzer's `test_utils::extract_annotations`, for `^` markers.
    fn extract_annotations(text: &str) -> Found {
        let mut res = Vec::new();
        // Line length to the start of the last line that long.
        let mut line_start_map: BTreeMap<u32, u32> = BTreeMap::new();
        let mut line_start = 0u32;
        for line in text.split_inclusive('\n') {
            let line_length = if let Some((prefix, suffix)) = line.split_once("//") {
                let annotation_offset = prefix.len() as u32 + 2;
                for (offset, len, content) in line_annotations(suffix.trim_end_matches('\n')) {
                    let start = offset + annotation_offset;
                    let end = start + len;
                    let (_, &base) = line_start_map
                        .range(end..)
                        .next()
                        .expect("an annotation below a line long enough to hold it");
                    res.push((start + base, end + base, content));
                }
                annotation_offset
            } else {
                line.len() as u32
            };
            line_start_map = line_start_map.split_off(&line_length);
            line_start_map.insert(line_length, line_start);
            line_start += line.len() as u32;
        }
        res
    }

    fn line_annotations(mut line: &str) -> Found {
        let mut res = Vec::new();
        let mut offset = 0u32;
        while let Some(idx) = line.find('^') {
            offset += idx as u32;
            line = &line[idx..];
            let len = line.chars().take_while(|&c| c == '^').count();
            let rest = &line[len..];
            let next = rest.find('^').map_or(line.len(), |it| it + len);
            let content = line[len..next].trim_end().trim_start();
            let content = content.trim_start_matches('💡').trim_start();
            res.push((offset, len as u32, content.to_string()));
            line = &line[next..];
            offset += next as u32;
        }
        res
    }

    fn check_diagnostics_with_disabled(fixture: &str, disabled: &[&str]) {
        let source = strip_meta(fixture);
        let mut expected = extract_annotations(&source);
        let mut actual = found(&source, disabled);
        expected.sort();
        actual.sort();
        assert_eq!(actual, expected, "\n{source}");
    }

    fn check_diagnostics(fixture: &str) {
        check_diagnostics_with_disabled(fixture, &[]);
    }

    // ---- break_outside_of_loop.rs ------------------------------------------------------

    #[test]
    fn outside_of_loop() {
        check_diagnostics(
            r#"
fn foo() {
    break;
  //^^^^^ error: break outside of loop
    continue;
  //^^^^^^^^ error: continue outside of loop
}
"#,
        );
    }

    #[test]
    fn break_async_blocks_are_borders() {
        check_diagnostics(
            r#"
fn foo() {
    'a: loop {
        async {
                break;
              //^^^^^ error: break outside of loop
                continue;
              //^^^^^^^^ error: continue outside of loop
        };
    }
}
"#,
        );
    }

    #[test]
    fn break_closures_are_borders() {
        check_diagnostics(
            r#"
fn foo() {
    'a: loop {
        || {
                break;
              //^^^^^ error: break outside of loop
                continue;
              //^^^^^^^^ error: continue outside of loop
        };
    }
}
"#,
        );
    }

    #[test]
    fn break_blocks_pass_through() {
        check_diagnostics(
            r#"
fn foo() {
    'a: loop {
        {
            break;
            continue;
        }
    }
}
"#,
        );
    }

    #[test]
    fn break_try_blocks_pass_through() {
        check_diagnostics(
            r#"
fn foo() {
    'a: loop {
        try {
                break;
                continue;
        };
    }
}
"#,
        );
    }

    #[test]
    fn label_blocks() {
        check_diagnostics(
            r#"
fn foo() {
    'a: {
        break;
      //^^^^^ error: break outside of loop
        continue;
      //^^^^^^^^ error: continue outside of loop
    }
}
"#,
        );
    }

    #[test]
    fn value_break_in_for_loop() {
        check_diagnostics(
            r#"
//- minicore: iterator
fn test() {
    for _ in [()] {
        break 3;
     // ^^^^^^^ error: can't break with a value in this position
    }
}
"#,
        );
    }

    #[test]
    fn try_block_desugaring_inside_closure() {
        check_diagnostics(
            r#"
//- minicore: option, try
fn test() {
    let _: Option<_> = try {
        || {
            let x = Some(2);
            Some(x?)
        };
    };
}
"#,
        );
    }

    // ---- return_outside_function.rs ----------------------------------------------------

    #[test]
    fn return_in_const() {
        check_diagnostics(
            r#"
const _: () = {
    return;
  //^^^^^^ error: return statement outside of function body
};
"#,
        );
    }

    #[test]
    fn return_in_static() {
        check_diagnostics(
            r#"
static _S: i32 = {
    return 0;
  //^^^^^^^^ error: return statement outside of function body
    0
};
"#,
        );
    }

    #[test]
    fn return_in_function_is_correct() {
        check_diagnostics(
            r#"
fn foo() -> i32 {
    if true { return 42; }
    0
}
"#,
        );
    }

    #[test]
    fn become_in_const() {
        check_diagnostics(
            r#"
const _: () = {
    become 0;
  //^^^^^^^^ error: become statement outside of function body
};
"#,
        );
    }

    #[test]
    fn become_in_static() {
        check_diagnostics(
            r#"
static _S: () = {
    become 0;
  //^^^^^^^^ error: become statement outside of function body
    ()
};
"#,
        );
    }

    #[test]
    fn become_in_function_is_correct() {
        check_diagnostics(
            r#"
fn foo() {
    if true { become (); }
}
"#,
        );
    }

    // ---- await_outside_of_async.rs -----------------------------------------------------

    #[test]
    fn await_inside_non_async_fn() {
        check_diagnostics(
            r#"
async fn foo() {}

fn bar() {
    foo().await;
        //^^^^^ error: `await` is used inside non-async function, which is not an `async` context
}
"#,
        );
    }

    #[test]
    fn await_inside_async_fn() {
        check_diagnostics(
            r#"
async fn foo() {}

async fn bar() {
    foo().await;
}
"#,
        );
    }

    #[test]
    fn await_inside_closure() {
        check_diagnostics(
            r#"
//- minicore: future
async fn foo() {}

async fn bar() {
    let _a = || { foo().await };
                      //^^^^^ error: `await` is used inside non-async closure, which is not an `async` context
}
"#,
        );
    }

    #[test]
    fn await_inside_async_block() {
        check_diagnostics(
            r#"
//- minicore: future
async fn foo() {}

fn bar() {
    let _a = async { foo().await };
}
"#,
        );
    }

    #[test]
    fn await_in_complex_context() {
        check_diagnostics(
            r#"
//- minicore: future
async fn foo() {}

fn bar() {
    async fn baz() {
        let a = foo().await;
    }

    let x = || {
        let y = async {
            baz().await;
            let z = || {
                baz().await;
                    //^^^^^ error: `await` is used inside non-async closure, which is not an `async` context
            };
        };
    };
}
"#,
        );
    }

    // ---- undeclared_label.rs -----------------------------------------------------------

    #[test]
    fn undeclared_label_smoke_test() {
        check_diagnostics(
            r#"
fn foo() {
    break 'a;
  //^^^^^^^^ error: break outside of loop
        //^^ error: use of undeclared label `'a`
    continue 'a;
  //^^^^^^^^^^^ error: continue outside of loop
           //^^ error: use of undeclared label `'a`
}
"#,
        );
    }

    #[test]
    fn while_let_loop_with_label_in_condition() {
        check_diagnostics(
            r#"
//- minicore: option

fn foo() {
    let mut optional = Some(0);

    'my_label: while let Some(_) = match optional {
        None => break 'my_label,
        Some(val) => Some(val),
    } {
        optional = None;
        continue 'my_label;
    }
}
"#,
        );
    }

    #[test]
    fn for_loop() {
        check_diagnostics(
            r#"
//- minicore: iterator
fn foo() {
    'xxx: for _ in [] {
        'yyy: for _ in [] {
            break 'xxx;
            continue 'yyy;
            break 'zzz;
                //^^^^ error: use of undeclared label `'zzz`
        }
        continue 'xxx;
        continue 'yyy;
               //^^^^ error: use of undeclared label `'yyy`
        break 'xxx;
        break 'yyy;
            //^^^^ error: use of undeclared label `'yyy`
    }
}
"#,
        );
    }

    #[test]
    fn try_operator_desugar_works() {
        check_diagnostics(
            r#"
//- minicore: option, try
fn foo() -> Option<()> {
    None?;
    None
}
"#,
        );
        check_diagnostics(
            r#"
//- minicore: option, try, future
async fn foo() -> Option<()> {
    None?;
    None
}
"#,
        );
        check_diagnostics(
            r#"
//- minicore: option, try, future, fn
async fn foo() {
    || { None?; Some(()) };
}
"#,
        );
    }

    #[test]
    fn macro_expansion_can_refer_label_defined_before_macro_definition() {
        check_diagnostics(
            r#"
fn foo() {
    'bar: loop {
        macro_rules! m {
            () => { break 'bar };
        }
        m!();
    }
}
"#,
        );
        check_diagnostics(
            r#"
fn foo() {
    'bar: loop {
        macro_rules! m {
            () => { break 'bar };
        }
        'bar: loop {
            m!();
        }
    }
}
"#,
        );
    }

    // ---- unreachable_label.rs ----------------------------------------------------------

    #[test]
    fn label_async_blocks_are_borders() {
        check_diagnostics(
            r#"
fn foo() {
    'a: loop {
        async {
            break 'a;
          //^^^^^^^^ error: break outside of loop
               // ^^ error: use of unreachable label `'a`
            continue 'a;
          //^^^^^^^^^^^ error: continue outside of loop
                  // ^^ error: use of unreachable label `'a`
        };
    }
}
"#,
        );
    }

    #[test]
    fn label_closures_are_borders() {
        check_diagnostics(
            r#"
fn foo() {
    'a: loop {
        || {
            break 'a;
          //^^^^^^^^ error: break outside of loop
               // ^^ error: use of unreachable label `'a`
            continue 'a;
          //^^^^^^^^^^^ error: continue outside of loop
                  // ^^ error: use of unreachable label `'a`
        };
    }
}
"#,
        );
    }

    #[test]
    fn label_blocks_pass_through() {
        check_diagnostics(
            r#"
fn foo() {
    'a: loop {
        {
          break 'a;
          continue 'a;
        }
    }
}
"#,
        );
    }

    #[test]
    fn label_try_blocks_pass_through() {
        check_diagnostics(
            r#"
fn foo() {
    'a: loop {
        try {
            break 'a;
            continue 'a;
        };
    }
}
"#,
        );
    }

    // ---- missing_body.rs ---------------------------------------------------------------

    #[test]
    fn associated_const() {
        check_diagnostics(
            r#"
trait Foo { const BAR: u32; }
impl Foo for () { const BAR: u32; }
                //^^^^^^^^^^^^^^^ error: associated constant in `impl` without body
        "#,
        );
    }

    #[test]
    fn associated_type_impl() {
        check_diagnostics(
            r#"
trait Foo { type Bar; }
impl Foo for () { type Bar; }
                //^^^^^^^^^ error: associated type in `impl` without body
        "#,
        );
    }

    #[test]
    fn free_const() {
        check_diagnostics(
            r#"
  const FOO: u32;
//^^^^^^^^^^^^^^^ error: free constant item without body
        "#,
        );
    }

    #[test]
    fn free_static() {
        check_diagnostics(
            r#"
  static FOO: u32;
//^^^^^^^^^^^^^^^^ error: free static item without body
        "#,
        );
    }

    #[test]
    fn type_alias_module() {
        check_diagnostics(
            r#"
  type Foo;
//^^^^^^^^^ error: free type alias without body
        "#,
        );
    }

    // Not in rust-analyzer, which leaves functions as a FIXME; the wording is rustc's.
    #[test]
    fn free_and_associated_function() {
        check_diagnostics(
            r#"
  fn foo();
//^^^^^^^^^ error: free function without a body
trait Foo { fn bar(); }
impl Foo for () { fn bar(); }
                //^^^^^^^^^ error: associated function in `impl` without body
extern "C" { fn baz(); }
        "#,
        );
    }

    // ---- duplicate_field.rs ------------------------------------------------------------

    #[test]
    fn duplicate_field_in_struct_literal() {
        check_diagnostics(
            r#"
struct S { foo: i32, bar: i32 }
fn main() {
    let _ = S {
        foo: 1,
        bar: 2,
        foo: 3,
      //^^^^^^ error: field specified more than once
    };
}
"#,
        );
    }

    #[test]
    fn duplicate_field_in_enum_variant_literal() {
        check_diagnostics(
            r#"
enum E { V { foo: i32 } }
fn main() {
    let _ = E::V {
        foo: 1,
        foo: 2,
      //^^^^^^ error: field specified more than once
    };
}
"#,
        );
    }

    #[test]
    fn no_duplicate_when_each_field_specified_once() {
        check_diagnostics(
            r#"
struct S { foo: i32, bar: i32 }
fn main() {
    let _ = S { foo: 1, bar: 2 };
}
"#,
        );
    }

    // rust-analyzer reports `bar` as "no such field", which needs the struct's fields.
    #[test]
    fn no_duplicate_for_unknown_field_falls_through_to_no_such_field() {
        check_diagnostics(
            r#"
struct S { foo: i32 }
fn main() {
    let _ = S {
        foo: 1,
        bar: 2,
    };
}
"#,
        );
    }

    #[test]
    fn duplicate_field_in_struct_pattern() {
        check_diagnostics(
            r#"
struct S { foo: i32, bar: i32 }
fn f(s: S) {
    let S {
        foo,
        bar,
        foo,
      //^^^ error: field specified more than once
        ..
    } = s;
    let _ = (foo, bar);
}
"#,
        );
    }

    #[test]
    fn duplicate_field_in_enum_variant_pattern() {
        check_diagnostics(
            r#"
enum E { V { foo: i32, bar: i32 } }
fn f(e: E) {
    match e {
        E::V {
            foo,
            bar,
            foo,
          //^^^ error: field specified more than once
            ..
        } => { let _ = (foo, bar); }
    }
}
"#,
        );
    }

    // ---- union_expr_must_have_exactly_one_field.rs --------------------------------------

    #[test]
    fn union_expr_must_have_exactly_one_field() {
        check_diagnostics(
            r#"
union Bird {
    pigeon: u8,
    turtledove: u16,
}

fn main() {
    let bird = Bird { pigeon: 0 };
    let bird = Bird {};
            // ^^^^^^^ error: union expressions should have exactly one field
    let bird = Bird { pigeon: 0, turtledove: 1 };
            // ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ error: union expressions should have exactly one field
}
"#,
        );
    }

    #[test]
    fn union_behind_a_module_is_not_decided() {
        check_diagnostics(
            r#"
mod m { pub union Bird { pigeon: u8 } }
use m::Bird;
fn main() {
    let bird = Bird {};
}
"#,
        );
    }

    // ---- useless_braces.rs -------------------------------------------------------------

    #[test]
    fn test_check_unnecessary_braces_in_use_statement() {
        check_diagnostics(
            r#"
use a;
use a::{c, d::e};

mod a {
    pub mod c {}
    pub mod d {
        pub mod e {}
    }
}
"#,
        );
        check_diagnostics(
            r#"
use a;
use a::{
    c,
    // d::e
};

mod a {
    pub mod c {}
    pub mod d {
        pub mod e {}
    }
}
"#,
        );
        check_diagnostics(
            r#"
use a::{self};

mod a {
}
"#,
        );
        check_diagnostics(
            r#"
use a::{self as cool_name};

mod a {
}
"#,
        );
        check_diagnostics(
            r#"
mod a { pub mod b {} }
use a::{b::self};
"#,
        );
        // rust-analyzer's `check_fix` cases, as diagnostics.
        check_diagnostics(
            r#"
mod b {}
use {b};
  //^^^ warn: Unnecessary braces in use statement
"#,
        );
        check_diagnostics(
            r#"
mod a { pub mod c {} }
use a::{c};
     //^^^ warn: Unnecessary braces in use statement
"#,
        );
        check_diagnostics(
            r#"
mod a { pub mod c {} pub mod d { pub mod e {} } }
use a::{c, d::{e}};
            //^^^ warn: Unnecessary braces in use statement
"#,
        );
    }

    #[test]
    fn respect_lint_attributes_for_unused_braces() {
        check_diagnostics(
            r#"
mod b {}
#[allow(unused_braces)]
use {b};
"#,
        );
        check_diagnostics(
            r#"
mod b {}
#[deny(unused_braces)]
use {b};
  //^^^ 💡 error: Unnecessary braces in use statement
"#,
        );
    }

    // ---- field_shorthand.rs ------------------------------------------------------------

    #[test]
    fn test_check_expr_field_shorthand() {
        check_diagnostics(
            r#"
struct A { a: &'static str }
fn main() { A { a: "hello" }; }
"#,
        );
        check_diagnostics(
            r#"
struct A(usize);
fn main() { A { 0: 0 }; }
"#,
        );
        // rust-analyzer's `check_fix` cases, as diagnostics.
        check_diagnostics(
            r#"
struct A { a: &'static str }
fn main() {
    let a = "haha";
    A { a: a };
      //^^^^ weak: Shorthand struct initialization
}
"#,
        );
        check_diagnostics(
            r#"
struct A { a: &'static str, b: &'static str }
fn main() {
    let a = "haha";
    let b = "bb";
    A { a: a, b };
      //^^^^ weak: Shorthand struct initialization
}
"#,
        );
    }

    #[test]
    fn test_check_pat_field_shorthand() {
        check_diagnostics(
            r#"
struct A { a: &'static str }
fn f(a: A) { let A { a: _hello } = a; }
"#,
        );
        check_diagnostics(
            r#"
struct A(usize);
fn f(a: A) { let A { 0: 0 } = a; }
"#,
        );
        // rust-analyzer's `check_fix` cases, as diagnostics.
        check_diagnostics(
            r#"
struct A { a: &'static str }
fn f(a: A) {
    let A { a: a } = a;
          //^^^^ weak: Shorthand struct pattern
    _ = a;
}
"#,
        );
        check_diagnostics(
            r#"
struct A { a: &'static str, b: &'static str }
fn f(a: A) {
    let A { a: a, b } = a;
          //^^^^ weak: Shorthand struct pattern
    _ = (a, b);
}
"#,
        );
    }

    #[test]
    fn diagnostic_range_respect_allows() {
        check_diagnostics(
            r#"
#![allow(clippy::redundant_field_names, unused)]

struct Foo {
    bar: u32,
}

fn main() {
    let bar = 23;
    let foo = Foo {
	    bar: bar,
    };
}
        "#,
        );
    }

    // ---- remove_trailing_return.rs -----------------------------------------------------

    #[test]
    fn remove_trailing_return() {
        check_diagnostics(
            r#"
fn foo() -> u8 {
    return 2;
} //^^^^^^^^^ 💡 weak: replace return <expr>; with <expr>
"#,
        );
    }

    #[test]
    fn remove_trailing_return_inner_function() {
        check_diagnostics(
            r#"
fn foo() -> u8 {
    fn bar() -> u8 {
        return 2;
    } //^^^^^^^^^ 💡 weak: replace return <expr>; with <expr>
    bar()
}
"#,
        );
    }

    #[test]
    fn remove_trailing_return_closure() {
        check_diagnostics(
            r#"
//- minicore: fn
fn foo() -> u8 {
    let bar = || return 2;
    bar()      //^^^^^^^^ 💡 weak: replace return <expr>; with <expr>
}
"#,
        );
        check_diagnostics(
            r#"
//- minicore: fn
fn foo() -> u8 {
    let bar = || {
        return 2;
    };//^^^^^^^^^ 💡 weak: replace return <expr>; with <expr>
    bar()
}
"#,
        );
    }

    #[test]
    fn remove_trailing_return_unit() {
        check_diagnostics(
            r#"
fn foo() {
    return
} //^^^^^^ 💡 weak: replace return <expr>; with <expr>
"#,
        );
    }

    #[test]
    fn remove_trailing_return_no_semi() {
        check_diagnostics(
            r#"
fn foo() -> u8 {
    return 2
} //^^^^^^^^ 💡 weak: replace return <expr>; with <expr>
"#,
        );
    }

    #[test]
    fn remove_trailing_return_in_if() {
        check_diagnostics_with_disabled(
            r#"
fn foo(x: usize) -> u8 {
    if x > 0 {
        return 1;
      //^^^^^^^^^ 💡 weak: replace return <expr>; with <expr>
    } else {
        return 0;
    } //^^^^^^^^^ 💡 weak: replace return <expr>; with <expr>
}
"#,
            &[ELSE],
        );
    }

    #[test]
    fn remove_trailing_return_in_match() {
        check_diagnostics(
            r#"
fn foo<T, E>(x: Result<T, E>) -> u8 {
    match x {
        Ok(_) => return 1,
               //^^^^^^^^ 💡 weak: replace return <expr>; with <expr>
        Err(_) => return 0,
    }           //^^^^^^^^ 💡 weak: replace return <expr>; with <expr>
}
"#,
        );
    }

    #[test]
    fn no_diagnostic_if_no_return_keyword() {
        check_diagnostics(
            r#"
fn foo() -> u8 {
    3
}
"#,
        );
    }

    #[test]
    fn no_diagnostic_if_not_last_statement() {
        check_diagnostics(
            r#"
fn foo() -> u8 {
    if true { return 2; }
    3
}
"#,
        );
    }

    #[test]
    fn no_diagnostic_if_not_last_statement2() {
        check_diagnostics(
            r#"
fn foo() -> u8 {
    return 2;
    fn bar() {}
}
"#,
        );
    }

    // ---- remove_unnecessary_else.rs ----------------------------------------------------

    #[test]
    fn remove_unnecessary_else_for_return() {
        check_diagnostics_with_disabled(
            r#"
fn test() {
    if foo {
        return bar;
    } else {
    //^^^^ 💡 weak: remove unnecessary else block
        do_something_else();
    }
}
"#,
            &[TRAILING],
        );
    }

    #[test]
    fn remove_unnecessary_else_for_return2() {
        check_diagnostics_with_disabled(
            r#"
fn test() {
    if foo {
        return bar;
    } else if qux {
    //^^^^ 💡 weak: remove unnecessary else block
        do_something_else();
    } else {
        do_something_else2();
    }
}
"#,
            &[TRAILING],
        );
    }

    #[test]
    fn remove_unnecessary_else_for_return3() {
        check_diagnostics_with_disabled(
            r#"
fn test(a: bool) -> i32 {
    if a {
        return 1;
    } else {
    //^^^^ 💡 weak: remove unnecessary else block
        0
    }
}
"#,
            &[TRAILING],
        );
    }

    #[test]
    fn remove_unnecessary_else_for_return_in_child_if_expr() {
        check_diagnostics_with_disabled(
            r#"
fn test() {
    if foo {
        do_something();
    } else if qux {
        return bar;
    } else {
    //^^^^ 💡 weak: remove unnecessary else block
        do_something_else();
    }
}
"#,
            &[TRAILING],
        );
    }

    #[test]
    fn remove_unnecessary_else_for_break() {
        check_diagnostics(
            r#"
fn test() {
    loop {
        if foo {
            break;
        } else {
        //^^^^ 💡 weak: remove unnecessary else block
            do_something_else();
        }
    }
}
"#,
        );
    }

    #[test]
    fn remove_unnecessary_else_for_continue() {
        check_diagnostics(
            r#"
fn test() {
    loop {
        if foo {
            continue;
        } else {
        //^^^^ 💡 weak: remove unnecessary else block
            do_something_else();
        }
    }
}
"#,
        );
    }

    #[test]
    fn no_diagnostic_if_no_else_branch() {
        check_diagnostics(
            r#"
fn test() {
    if foo {
        return bar;
    }

    do_something_else();
}
"#,
        );
    }

    #[test]
    fn no_diagnostic_if_no_divergence() {
        check_diagnostics(
            r#"
fn test() {
    if foo {
        do_something();
    } else {
        do_something_else();
    }
}
"#,
        );
    }

    #[test]
    fn no_diagnostic_if_no_divergence_in_else_branch() {
        check_diagnostics_with_disabled(
            r#"
fn test() {
    if foo {
        do_something();
    } else {
        return bar;
    }
}
"#,
            &[TRAILING],
        );
    }

    #[test]
    fn no_diagnostic_if_not_expr_stmt() {
        check_diagnostics_with_disabled(
            r#"
fn test1() {
    let _x = if a {
        return;
    } else {
        1
    };
}

fn test2() {
    let _x = if a {
        return;
    } else if b {
        return;
    } else if c {
        1
    } else {
        return;
    };
}
"#,
            &[TRAILING],
        );
        check_diagnostics(
            r#"
fn test3() -> u8 {
    foo(if a { return 1 } else { 0 })
}
"#,
        );
    }

    // ---- incorrect_case.rs -------------------------------------------------------------

    #[test]
    fn test_uppercase_const_no_diagnostics() {
        check_diagnostics(
            r#"
fn foo() {
    const ANOTHER_ITEM: &str = "some_item";
}
"#,
        );
    }

    #[test]
    fn test_single_incorrect_case_diagnostic_in_function_name_issue_6970() {
        check_diagnostics(
            r#"
fn FOO() {}
// ^^^ 💡 warn: Function `FOO` should have snake_case name, e.g. `foo`
"#,
        );
    }

    #[test]
    fn incorrect_function_name() {
        check_diagnostics(
            r#"
fn NonSnakeCaseName() {}
// ^^^^^^^^^^^^^^^^ 💡 warn: Function `NonSnakeCaseName` should have snake_case name, e.g. `non_snake_case_name`
"#,
        );
    }

    #[test]
    fn incorrect_function_params() {
        check_diagnostics(
            r#"
fn foo(SomeParam: u8) { _ = SomeParam; }
    // ^^^^^^^^^ 💡 warn: Parameter `SomeParam` should have snake_case name, e.g. `some_param`

fn foo2(ok_param: &str, CAPS_PARAM: u8) { _ = (ok_param, CAPS_PARAM); }
                     // ^^^^^^^^^^ 💡 warn: Parameter `CAPS_PARAM` should have snake_case name, e.g. `caps_param`
"#,
        );
    }

    #[test]
    fn incorrect_variable_names() {
        check_diagnostics(
            r#"
#[allow(unused)]
fn foo() {
    let SOME_VALUE = 10;
     // ^^^^^^^^^^ 💡 warn: Variable `SOME_VALUE` should have snake_case name, e.g. `some_value`
    let AnotherValue = 20;
     // ^^^^^^^^^^^^ 💡 warn: Variable `AnotherValue` should have snake_case name, e.g. `another_value`
}
"#,
        );
    }

    #[test]
    fn incorrect_struct_names() {
        check_diagnostics(
            r#"
struct non_camel_case_name {}
    // ^^^^^^^^^^^^^^^^^^^ 💡 warn: Structure `non_camel_case_name` should have UpperCamelCase name, e.g. `NonCamelCaseName`

struct SCREAMING_CASE {}
    // ^^^^^^^^^^^^^^ 💡 warn: Structure `SCREAMING_CASE` should have UpperCamelCase name, e.g. `ScreamingCase`
"#,
        );
    }

    #[test]
    fn no_diagnostic_for_camel_cased_acronyms_in_struct_name() {
        check_diagnostics(
            r#"
struct AABB {}
"#,
        );
    }

    #[test]
    fn incorrect_struct_field() {
        check_diagnostics(
            r#"
struct SomeStruct { SomeField: u8 }
                 // ^^^^^^^^^ 💡 warn: Field `SomeField` should have snake_case name, e.g. `some_field`
"#,
        );
    }

    #[test]
    fn incorrect_union_names() {
        check_diagnostics(
            r#"
union non_camel_case_name { field: u8 }
   // ^^^^^^^^^^^^^^^^^^^ 💡 warn: Union `non_camel_case_name` should have UpperCamelCase name, e.g. `NonCamelCaseName`

union SCREAMING_CASE { field: u8 }
   // ^^^^^^^^^^^^^^ 💡 warn: Union `SCREAMING_CASE` should have UpperCamelCase name, e.g. `ScreamingCase`
"#,
        );
    }

    #[test]
    fn no_diagnostic_for_camel_cased_acronyms_in_union_name() {
        check_diagnostics(
            r#"
union AABB { field: u8 }
"#,
        );
    }

    #[test]
    fn no_diagnostic_for_repr_c_union() {
        check_diagnostics(
            r#"
#[repr(C)]
union my_union { field: u8 }
"#,
        );
    }

    #[test]
    fn incorrect_union_field() {
        check_diagnostics(
            r#"
union SomeUnion { SomeField: u8 }
               // ^^^^^^^^^ 💡 warn: Field `SomeField` should have snake_case name, e.g. `some_field`
"#,
        );
    }

    #[test]
    fn incorrect_enum_names() {
        check_diagnostics(
            r#"
enum some_enum { Val(u8) }
  // ^^^^^^^^^ 💡 warn: Enum `some_enum` should have UpperCamelCase name, e.g. `SomeEnum`

enum SOME_ENUM {}
  // ^^^^^^^^^ 💡 warn: Enum `SOME_ENUM` should have UpperCamelCase name, e.g. `SomeEnum`
"#,
        );
    }

    #[test]
    fn no_diagnostic_for_camel_cased_acronyms_in_enum_name() {
        check_diagnostics(
            r#"
enum AABB {}
"#,
        );
    }

    #[test]
    fn incorrect_enum_variant_name() {
        check_diagnostics(
            r#"
enum SomeEnum { SOME_VARIANT(u8) }
             // ^^^^^^^^^^^^ 💡 warn: Variant `SOME_VARIANT` should have UpperCamelCase name, e.g. `SomeVariant`
"#,
        );
    }

    #[test]
    fn incorrect_const_name() {
        check_diagnostics(
            r#"
const some_weird_const: u8 = 10;
   // ^^^^^^^^^^^^^^^^ 💡 warn: Constant `some_weird_const` should have UPPER_SNAKE_CASE name, e.g. `SOME_WEIRD_CONST`
"#,
        );
    }

    #[test]
    fn incorrect_static_name() {
        check_diagnostics(
            r#"
static some_weird_const: u8 = 10;
    // ^^^^^^^^^^^^^^^^ 💡 warn: Static variable `some_weird_const` should have UPPER_SNAKE_CASE name, e.g. `SOME_WEIRD_CONST`
"#,
        );
    }

    #[test]
    fn fn_inside_impl_struct() {
        check_diagnostics(
            r#"
struct someStruct;
    // ^^^^^^^^^^ 💡 warn: Structure `someStruct` should have UpperCamelCase name, e.g. `SomeStruct`

impl someStruct {
    fn SomeFunc(&self) {
    // ^^^^^^^^ 💡 warn: Function `SomeFunc` should have snake_case name, e.g. `some_func`
        let WHY_VAR_IS_CAPS = 10;
         // ^^^^^^^^^^^^^^^ 💡 warn: Variable `WHY_VAR_IS_CAPS` should have snake_case name, e.g. `why_var_is_caps`
        _ = WHY_VAR_IS_CAPS;
    }
}
"#,
        );
    }

    #[test]
    fn no_diagnostic_for_enum_variants() {
        check_diagnostics(
            r#"
enum Option { Some, None }
use Option::{Some, None};

#[allow(unused)]
fn main() {
    match Option::None {
        None => (),
        Some => (),
    }
}
"#,
        );
    }

    #[test]
    fn allow_attributes_crate_attr() {
        check_diagnostics(
            r#"
#![allow(non_snake_case)]
#![allow(non_camel_case_types)]

struct S {
    fooBar: bool,
}

enum E {
    fooBar,
}

mod F {
    fn CheckItWorksWithCrateAttr(BAD_NAME_HI: u8) {
        _ = BAD_NAME_HI;
    }
}
    "#,
        );
    }

    #[test]
    fn complex_ignore() {
        check_diagnostics(
            r#"
trait T { fn a(); }
struct U {}
impl T for U {
    fn a() {
        #[allow(non_snake_case, non_upper_case_globals)]
        trait __BitFlagsOk {
            const HiImAlsoBad: u8 = 2;
            fn Dirty(&self) -> bool { false }
        }

        trait __BitFlagsBad {
            const HiImAlsoBad: u8 = 2;
               // ^^^^^^^^^^^ 💡 warn: Constant `HiImAlsoBad` should have UPPER_SNAKE_CASE name, e.g. `HI_IM_ALSO_BAD`
            fn Dirty(&self) -> bool { false }
            // ^^^^^💡 warn: Function `Dirty` should have snake_case name, e.g. `dirty`
        }
    }
}
"#,
        );
    }

    #[test]
    fn infinite_loop_inner_items() {
        check_diagnostics(
            r#"
fn qualify() {
    mod foo {
        use super::*;
    }
}
            "#,
        )
    }

    #[test]
    fn parenthesized_parameter() {
        check_diagnostics(
            r#"
fn f((_O): u8) {}
   // ^^ 💡 warn: Variable `_O` should have snake_case name, e.g. `_o`
"#,
        )
    }

    // The bodiless functions below are rust-analyzer's fixtures; `missing-body` is not part of
    // what they test.

    #[test]
    fn ignores_no_mangle_items() {
        check_diagnostics_with_disabled(
            r#"
#[no_mangle]
extern "C" fn NonSnakeCaseName(some_var: u8) -> u8;
#[no_mangle]
static lower_case: () = ();
            "#,
            &[MISSING_BODY],
        );
    }

    #[test]
    fn ignores_unsafe_no_mangle_items() {
        check_diagnostics_with_disabled(
            r#"
#[unsafe(no_mangle)]
extern "C" fn NonSnakeCaseName(some_var: u8) -> u8;
#[unsafe(no_mangle)]
static lower_case: () = ();
            "#,
            &[MISSING_BODY],
        );
    }

    #[test]
    fn ignores_no_mangle_items_with_no_abi() {
        check_diagnostics_with_disabled(
            r#"
#[no_mangle]
extern fn NonSnakeCaseName(some_var: u8) -> u8;
            "#,
            &[MISSING_BODY],
        );
    }

    #[test]
    fn no_mangle_items_with_rust_abi() {
        check_diagnostics_with_disabled(
            r#"
#[no_mangle]
extern "Rust" fn NonSnakeCaseName(some_var: u8) -> u8;
              // ^^^^^^^^^^^^^^^^ 💡 warn: Function `NonSnakeCaseName` should have snake_case name, e.g. `non_snake_case_name`
            "#,
            &[MISSING_BODY],
        );
    }

    #[test]
    fn no_mangle_items_non_extern() {
        check_diagnostics_with_disabled(
            r#"
#[no_mangle]
fn NonSnakeCaseName(some_var: u8) -> u8;
// ^^^^^^^^^^^^^^^^ 💡 warn: Function `NonSnakeCaseName` should have snake_case name, e.g. `non_snake_case_name`
            "#,
            &[MISSING_BODY],
        );
    }

    #[test]
    fn extern_fn_name() {
        check_diagnostics_with_disabled(
            r#"
extern "C" fn NonSnakeCaseName(some_var: u8) -> u8;
           // ^^^^^^^^^^^^^^^^ 💡 warn: Function `NonSnakeCaseName` should have snake_case name, e.g. `non_snake_case_name`
extern "Rust" fn NonSnakeCaseName(some_var: u8) -> u8;
              // ^^^^^^^^^^^^^^^^ 💡 warn: Function `NonSnakeCaseName` should have snake_case name, e.g. `non_snake_case_name`
extern fn NonSnakeCaseName(some_var: u8) -> u8;
       // ^^^^^^^^^^^^^^^^ 💡 warn: Function `NonSnakeCaseName` should have snake_case name, e.g. `non_snake_case_name`
            "#,
            &[MISSING_BODY],
        );
    }

    #[test]
    fn ignores_extern_items() {
        check_diagnostics(
            r#"
extern {
    fn NonSnakeCaseName(SOME_VAR: u8) -> u8;
    pub static SomeStatic: u8 = 10;
}
            "#,
        );
    }

    #[test]
    fn ignores_extern_items_from_macro() {
        check_diagnostics(
            r#"
macro_rules! m {
    () => {
        fn NonSnakeCaseName(SOME_VAR: u8) -> u8;
        pub static SomeStatic: u8 = 10;
    }
}

extern {
    m!();
}
            "#,
        );
    }

    #[test]
    fn incorrect_trait_and_assoc_item_names() {
        check_diagnostics(
            r#"
trait BAD_TRAIT {
   // ^^^^^^^^^ 💡 warn: Trait `BAD_TRAIT` should have UpperCamelCase name, e.g. `BadTrait`
    const bad_const: u8;
       // ^^^^^^^^^ 💡 warn: Constant `bad_const` should have UPPER_SNAKE_CASE name, e.g. `BAD_CONST`
    type BAD_TYPE;
      // ^^^^^^^^ 💡 warn: Type alias `BAD_TYPE` should have UpperCamelCase name, e.g. `BadType`
    fn BAD_FUNCTION();
    // ^^^^^^^^^^^^ 💡 warn: Function `BAD_FUNCTION` should have snake_case name, e.g. `bad_function`
    fn BadFunction();
    // ^^^^^^^^^^^ 💡 warn: Function `BadFunction` should have snake_case name, e.g. `bad_function`
}
    "#,
        );
    }

    #[test]
    fn no_diagnostics_for_trait_impl_assoc_items_except_pats_in_body() {
        check_diagnostics(
            r#"
trait BAD_TRAIT {
   // ^^^^^^^^^ 💡 warn: Trait `BAD_TRAIT` should have UpperCamelCase name, e.g. `BadTrait`
    const bad_const: u8;
       // ^^^^^^^^^ 💡 warn: Constant `bad_const` should have UPPER_SNAKE_CASE name, e.g. `BAD_CONST`
    type BAD_TYPE;
      // ^^^^^^^^ 💡 warn: Type alias `BAD_TYPE` should have UpperCamelCase name, e.g. `BadType`
    fn BAD_FUNCTION(BAD_PARAM: u8);
    // ^^^^^^^^^^^^ 💡 warn: Function `BAD_FUNCTION` should have snake_case name, e.g. `bad_function`
                 // ^^^^^^^^^ 💡 warn: Parameter `BAD_PARAM` should have snake_case name, e.g. `bad_param`
    fn BadFunction();
    // ^^^^^^^^^^^ 💡 warn: Function `BadFunction` should have snake_case name, e.g. `bad_function`
}

impl BAD_TRAIT for () {
    const bad_const: u8 = 0;
    type BAD_TYPE = ();
    fn BAD_FUNCTION(BAD_PARAM: u8) {
                 // ^^^^^^^^^ 💡 warn: Parameter `BAD_PARAM` should have snake_case name, e.g. `bad_param`
        let BAD_VAR = 0;
         // ^^^^^^^ 💡 warn: Variable `BAD_VAR` should have snake_case name, e.g. `bad_var`
    }
    fn BadFunction() {}
}
    "#,
        );
    }

    #[test]
    fn allow_attributes() {
        check_diagnostics(
            r#"
#[allow(non_snake_case)]
fn NonSnakeCaseName(SOME_VAR: u8) -> u8{
    // cov_flags generated output from elsewhere in this file
    extern "C" {
        #[no_mangle]
        static lower_case: u8;
    }

    let OtherVar = SOME_VAR + 1;
    OtherVar
}

#[allow(nonstandard_style)]
mod CheckNonstandardStyle {
    fn HiImABadFnName() {}
}

#[allow(bad_style)]
mod CheckBadStyle {
    struct fooo;
}

mod F {
    #![allow(non_snake_case)]
    fn CheckItWorksWithModAttr(BAD_NAME_HI: u8) {
        _ = BAD_NAME_HI;
    }
}

#[allow(non_snake_case, non_camel_case_types)]
pub struct some_type {
    SOME_FIELD: u8,
    SomeField: u16,
}

#[allow(non_upper_case_globals)]
pub const some_const: u8 = 10;

#[allow(non_upper_case_globals)]
pub static SomeStatic: u8 = 10;

#[allow(non_snake_case, non_camel_case_types, non_upper_case_globals)]
trait BAD_TRAIT {
    const bad_const: u8;
    type BAD_TYPE;
    fn BAD_FUNCTION(BAD_PARAM: u8);
    fn BadFunction();
}
    "#,
        );
    }

    #[test]
    fn deny_attributes() {
        check_diagnostics(
            r#"
#[deny(non_snake_case)]
fn NonSnakeCaseName(some_var: u8) -> u8 {
 //^^^^^^^^^^^^^^^^ 💡 error: Function `NonSnakeCaseName` should have snake_case name, e.g. `non_snake_case_name`
    // cov_flags generated output from elsewhere in this file
    extern "C" {
        #[no_mangle]
        static lower_case: u8;
    }

    let OtherVar = some_var + 1;
      //^^^^^^^^ 💡 error: Variable `OtherVar` should have snake_case name, e.g. `other_var`
    OtherVar
}

#[deny(nonstandard_style)]
mod CheckNonstandardStyle {
  //^^^^^^^^^^^^^^^^^^^^^ 💡 error: Module `CheckNonstandardStyle` should have snake_case name, e.g. `check_nonstandard_style`
    fn HiImABadFnName() {}
     //^^^^^^^^^^^^^^ 💡 error: Function `HiImABadFnName` should have snake_case name, e.g. `hi_im_abad_fn_name`
}

#[deny(warnings)]
mod CheckBadStyle {
  //^^^^^^^^^^^^^ 💡 error: Module `CheckBadStyle` should have snake_case name, e.g. `check_bad_style`
    struct fooo;
         //^^^^ 💡 error: Structure `fooo` should have UpperCamelCase name, e.g. `Fooo`
}

mod F {
  //^ 💡 error: Module `F` should have snake_case name, e.g. `f`
    #![deny(non_snake_case)]
    fn CheckItWorksWithModAttr() {}
     //^^^^^^^^^^^^^^^^^^^^^^^ 💡 error: Function `CheckItWorksWithModAttr` should have snake_case name, e.g. `check_it_works_with_mod_attr`
}

#[deny(non_snake_case, non_camel_case_types)]
pub struct some_type {
         //^^^^^^^^^ 💡 error: Structure `some_type` should have UpperCamelCase name, e.g. `SomeType`
    SOME_FIELD: u8,
  //^^^^^^^^^^ 💡 error: Field `SOME_FIELD` should have snake_case name, e.g. `some_field`
    SomeField: u16,
  //^^^^^^^^^  💡 error: Field `SomeField` should have snake_case name, e.g. `some_field`
}

#[deny(non_upper_case_globals)]
pub const some_const: u8 = 10;
        //^^^^^^^^^^ 💡 error: Constant `some_const` should have UPPER_SNAKE_CASE name, e.g. `SOME_CONST`

#[deny(non_upper_case_globals)]
pub static SomeStatic: u8 = 10;
         //^^^^^^^^^^ 💡 error: Static variable `SomeStatic` should have UPPER_SNAKE_CASE name, e.g. `SOME_STATIC`

#[deny(non_snake_case, non_camel_case_types, non_upper_case_globals)]
trait BAD_TRAIT {
   // ^^^^^^^^^ 💡 error: Trait `BAD_TRAIT` should have UpperCamelCase name, e.g. `BadTrait`
    const bad_const: u8;
       // ^^^^^^^^^ 💡 error: Constant `bad_const` should have UPPER_SNAKE_CASE name, e.g. `BAD_CONST`
    type BAD_TYPE;
      // ^^^^^^^^ 💡 error: Type alias `BAD_TYPE` should have UpperCamelCase name, e.g. `BadType`
    fn BAD_FUNCTION(BAD_PARAM: u8);
    // ^^^^^^^^^^^^ 💡 error: Function `BAD_FUNCTION` should have snake_case name, e.g. `bad_function`
                 // ^^^^^^^^^ 💡 error: Parameter `BAD_PARAM` should have snake_case name, e.g. `bad_param`
    fn BadFunction();
    // ^^^^^^^^^^^ 💡 error: Function `BadFunction` should have snake_case name, e.g. `bad_function`
}
    "#,
        );
    }

    #[test]
    fn fn_inner_items() {
        check_diagnostics(
            r#"
fn main() {
    const foo: bool = true;
        //^^^ 💡 warn: Constant `foo` should have UPPER_SNAKE_CASE name, e.g. `FOO`
    static bar: bool = true;
         //^^^ 💡 warn: Static variable `bar` should have UPPER_SNAKE_CASE name, e.g. `BAR`
    fn BAZ() {
     //^^^ 💡 warn: Function `BAZ` should have snake_case name, e.g. `baz`
        const foo: bool = true;
            //^^^ 💡 warn: Constant `foo` should have UPPER_SNAKE_CASE name, e.g. `FOO`
        static bar: bool = true;
             //^^^ 💡 warn: Static variable `bar` should have UPPER_SNAKE_CASE name, e.g. `BAR`
        fn BAZ() {
         //^^^ 💡 warn: Function `BAZ` should have snake_case name, e.g. `baz`
            let _INNER_INNER = 42;
              //^^^^^^^^^^^^ 💡 warn: Variable `_INNER_INNER` should have snake_case name, e.g. `_inner_inner`
        }

        let _INNER_LOCAL = 42;
          //^^^^^^^^^^^^ 💡 warn: Variable `_INNER_LOCAL` should have snake_case name, e.g. `_inner_local`
    }
}
"#,
        );
    }

    #[test]
    fn const_body_inner_items() {
        check_diagnostics(
            r#"
const _: () = {
    static bar: bool = true;
         //^^^ 💡 warn: Static variable `bar` should have UPPER_SNAKE_CASE name, e.g. `BAR`
    fn BAZ() {}
     //^^^ 💡 warn: Function `BAZ` should have snake_case name, e.g. `baz`

    const foo: () = {
        //^^^ 💡 warn: Constant `foo` should have UPPER_SNAKE_CASE name, e.g. `FOO`
        const foo: bool = true;
            //^^^ 💡 warn: Constant `foo` should have UPPER_SNAKE_CASE name, e.g. `FOO`
        static bar: bool = true;
             //^^^ 💡 warn: Static variable `bar` should have UPPER_SNAKE_CASE name, e.g. `BAR`
        fn BAZ() {}
         //^^^ 💡 warn: Function `BAZ` should have snake_case name, e.g. `baz`
    };
};
"#,
        );
    }

    #[test]
    fn static_body_inner_items() {
        check_diagnostics(
            r#"
static FOO: () = {
    const foo: bool = true;
        //^^^ 💡 warn: Constant `foo` should have UPPER_SNAKE_CASE name, e.g. `FOO`
    fn BAZ() {}
     //^^^ 💡 warn: Function `BAZ` should have snake_case name, e.g. `baz`

    static bar: () = {
         //^^^ 💡 warn: Static variable `bar` should have UPPER_SNAKE_CASE name, e.g. `BAR`
        const foo: bool = true;
            //^^^ 💡 warn: Constant `foo` should have UPPER_SNAKE_CASE name, e.g. `FOO`
        static bar: bool = true;
             //^^^ 💡 warn: Static variable `bar` should have UPPER_SNAKE_CASE name, e.g. `BAR`
        fn BAZ() {}
         //^^^ 💡 warn: Function `BAZ` should have snake_case name, e.g. `baz`
    };
};
"#,
        );
    }

    #[test]
    fn enum_variant_body_inner_item() {
        check_diagnostics(
            r#"
enum E {
    A = {
        const foo: bool = true;
            //^^^ 💡 warn: Constant `foo` should have UPPER_SNAKE_CASE name, e.g. `FOO`
        static bar: bool = true;
             //^^^ 💡 warn: Static variable `bar` should have UPPER_SNAKE_CASE name, e.g. `BAR`
        fn BAZ() {}
         //^^^ 💡 warn: Function `BAZ` should have snake_case name, e.g. `baz`
        42
    },
}
"#,
        );
    }

    #[test]
    fn module_name_inline() {
        check_diagnostics(
            r#"
mod M {
  //^ 💡 warn: Module `M` should have snake_case name, e.g. `m`
    mod IncorrectCase {}
      //^^^^^^^^^^^^^ 💡 warn: Module `IncorrectCase` should have snake_case name, e.g. `incorrect_case`
}
"#,
        );
    }

    // rust-analyzer's fixture has an empty `/Foo.rs` beside this; the declaration is all that
    // is checked.
    #[test]
    fn module_name_decl() {
        check_diagnostics(
            r#"
mod Foo;
  //^^^ 💡 warn: Module `Foo` should have snake_case name, e.g. `foo`
"#,
        )
    }

    #[test]
    fn test_field_shorthand() {
        check_diagnostics(
            r#"
struct Foo { _nonSnake: u8 }
          // ^^^^^^^^^ 💡 warn: Field `_nonSnake` should have snake_case name, e.g. `_non_snake`
fn func(Foo { _nonSnake }: Foo) {}
"#,
        );
    }

    #[test]
    fn test_match() {
        check_diagnostics(
            r#"
enum Foo { Variant { nonSnake1: u8 } }
                  // ^^^^^^^^^ 💡 warn: Field `nonSnake1` should have snake_case name, e.g. `non_snake1`
fn func() {
    match (Foo::Variant { nonSnake1: 1 }) {
        Foo::Variant { nonSnake1: _nonSnake2 } => {},
                               // ^^^^^^^^^^ 💡 warn: Variable `_nonSnake2` should have snake_case name, e.g. `_non_snake2`
    }
}
"#,
        );

        check_diagnostics(
            r#"
struct Foo(u8);

fn func() {
    match Foo(1) {
        Foo(_nonSnake) => {},
         // ^^^^^^^^^ 💡 warn: Variable `_nonSnake` should have snake_case name, e.g. `_non_snake`
    }
}
"#,
        );

        check_diagnostics(
            r#"
fn main() {
    match 1 {
        _Bad1 @ _Bad2 => {}
     // ^^^^^ 💡 warn: Variable `_Bad1` should have snake_case name, e.g. `_bad1`
             // ^^^^^ 💡 warn: Variable `_Bad2` should have snake_case name, e.g. `_bad2`
    }
}
"#,
        );
        check_diagnostics(
            r#"
fn main() {
    match 1 { _Bad1 => () }
           // ^^^^^ 💡 warn: Variable `_Bad1` should have snake_case name, e.g. `_bad1`
}
"#,
        );

        check_diagnostics(
            r#"
enum Foo { V1, V2 }
use Foo::V1;

fn main() {
    match V1 {
        _Bad1 @ V1 => {},
     // ^^^^^ 💡 warn: Variable `_Bad1` should have snake_case name, e.g. `_bad1`
        Foo::V2 => {}
    }
}
"#,
        );
    }

    #[test]
    fn test_for_loop() {
        check_diagnostics(
            r#"
//- minicore: iterators
fn func() {
    for _nonSnake in [] {}
     // ^^^^^^^^^ 💡 warn: Variable `_nonSnake` should have snake_case name, e.g. `_non_snake`
}
"#,
        );
    }

    #[test]
    fn override_lint_level() {
        check_diagnostics(
            r#"
#[warn(nonstandard_style)]
fn foo() {
    let BAR: i32;
     // ^^^ 💡 warn: Variable `BAR` should have snake_case name, e.g. `bar`
    #[allow(non_snake_case)]
    let FOO: i32;
}

#[warn(nonstandard_style)]
fn foo() {
    let BAR: i32;
     // ^^^ 💡 warn: Variable `BAR` should have snake_case name, e.g. `bar`
    #[expect(non_snake_case)]
    let FOO: i32;
    #[allow(non_snake_case)]
    struct qux;
        // ^^^ 💡 warn: Structure `qux` should have UpperCamelCase name, e.g. `Qux`

    fn BAZ() {
    // ^^^ 💡 error: Function `BAZ` should have snake_case name, e.g. `baz`
        #![forbid(bad_style)]
    }
}
        "#,
        );
    }

    // rust-analyzer's fixture also has two items whose `cfg_attr` is enabled by the test's cfg
    // set; with no cfg set known, only the two it leaves disabled are ported.
    #[test]
    fn cfged_lint_attrs() {
        check_diagnostics(
            r#"
#[cfg_attr(any(), allow(non_snake_case))]
fn FOO() {}
// ^^^ 💡 warn: Function `FOO` should have snake_case name, e.g. `foo`

#[cfg_attr(non_existent, allow(non_snake_case))]
fn BAR() {}
// ^^^ 💡 warn: Function `BAR` should have snake_case name, e.g. `bar`
        "#,
        );
    }

    #[test]
    fn allow_with_comment() {
        check_diagnostics(
            r#"
#[allow(
    // Yo, sup
    non_snake_case
)]
fn foo(_HelloWorld: ()) {}
        "#,
        );
    }

    #[test]
    fn allow_with_repr_c() {
        check_diagnostics(
            r#"
#[repr(C)]
struct FFI_Struct;

#[repr(C)]
enum FFI_Enum {
    Field,
}
        "#,
        );
    }

    // ---- case_conv.rs ------------------------------------------------------------------

    fn conv(f: fn(&str) -> Option<String>, input: &str) -> String {
        f(input).unwrap_or_default()
    }

    #[test]
    fn test_to_lower_snake_case() {
        let f = to_lower_snake_case;
        assert_eq!(conv(f, "lower_snake_case"), "");
        assert_eq!(conv(f, "UPPER_SNAKE_CASE"), "upper_snake_case");
        assert_eq!(conv(f, "Weird_Case"), "weird_case");
        assert_eq!(conv(f, "UpperCamelCase"), "upper_camel_case");
        assert_eq!(conv(f, "lowerCamelCase"), "lower_camel_case");
        assert_eq!(conv(f, "a"), "");
        assert_eq!(conv(f, "abc"), "");
        assert_eq!(conv(f, "foo__bar"), "foo_bar");
        assert_eq!(conv(f, "Δ"), "δ");
    }

    #[test]
    fn test_to_camel_case() {
        let f = to_camel_case;
        assert_eq!(conv(f, "UpperCamelCase"), "");
        assert_eq!(conv(f, "UpperCamelCase_"), "");
        assert_eq!(conv(f, "_CamelCase"), "");
        assert_eq!(conv(f, "lowerCamelCase"), "LowerCamelCase");
        assert_eq!(conv(f, "lower_snake_case"), "LowerSnakeCase");
        assert_eq!(conv(f, "UPPER_SNAKE_CASE"), "UpperSnakeCase");
        assert_eq!(conv(f, "Weird_Case"), "WeirdCase");
        assert_eq!(conv(f, "name"), "Name");
        assert_eq!(conv(f, "A"), "");
        assert_eq!(conv(f, "AABB"), "");
        assert_eq!(conv(f, "X86_64"), "");
        assert_eq!(conv(f, "x86__64"), "X86_64");
        assert_eq!(conv(f, "Abc_123"), "Abc123");
        assert_eq!(conv(f, "A1_b2_c3"), "A1B2C3");
    }

    #[test]
    fn test_to_upper_snake_case() {
        let f = to_upper_snake_case;
        assert_eq!(conv(f, "UPPER_SNAKE_CASE"), "");
        assert_eq!(conv(f, "lower_snake_case"), "LOWER_SNAKE_CASE");
        assert_eq!(conv(f, "Weird_Case"), "WEIRD_CASE");
        assert_eq!(conv(f, "UpperCamelCase"), "UPPER_CAMEL_CASE");
        assert_eq!(conv(f, "lowerCamelCase"), "LOWER_CAMEL_CASE");
        assert_eq!(conv(f, "A"), "");
        assert_eq!(conv(f, "ABC"), "");
        assert_eq!(conv(f, "X86_64"), "");
        assert_eq!(conv(f, "FOO_BAr"), "FOO_BAR");
        assert_eq!(conv(f, "FOO__BAR"), "FOO_BAR");
        assert_eq!(conv(f, "ß"), "SS");
    }

    // ---- options -----------------------------------------------------------------------

    #[test]
    fn skipped_codes_do_not_run() {
        let opts = Options { codes: Some(vec![BREAK.to_string()]), ..Options::default() };
        let out = run("fn FOO() { break; }", &opts, |cx, krate, out| check(cx, krate, out))
            .expect("parses");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].code, BREAK);
    }

    #[test]
    fn min_severity_drops_weaker_findings() {
        let opts = Options { min_severity: Severity::Error, ..Options::default() };
        let out = run("fn FOO() -> u8 { return 1; }", &opts, |cx, krate, out| check(cx, krate, out))
            .expect("parses");
        assert!(out.is_empty(), "{out:?}");
    }

    #[test]
    fn a_parse_error_is_err() {
        let source = "fn f() { let x = ; }";
        let errors = run(source, &Options::default(), |cx, krate, out| check(cx, krate, out))
            .expect_err("does not parse");
        assert!(!errors.is_empty());
    }
}
