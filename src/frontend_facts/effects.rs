//! Divergence and possible effects from a function body.
//!
//! The parser establishes syntax, spans and doc attributes. Resolution and type facts are
//! supplied separately: without them a macro's definition, a method's receiver type or an
//! expression's type is unknown, not inferred from its spelling.

use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::rustc_ast::ast::{
    self, AssignOpKind, AssocItem, AssocItemKind, AttrVec, BinOpKind, Block, BlockCheckMode, Expr,
    ExprKind, Item, ItemKind, Label, Stmt, StmtKind,
};
use crate::rustc_ast::visit::{self, AssocCtxt, Visitor};
use crate::rustc_errors::DiagCtxt;
use crate::rustc_errors::plain_emitter::PlainEmitter;
use crate::rustc_parse::lexer::StripTokens;
use crate::rustc_parse::new_parser_from_source_str;
use crate::rustc_session::parse::ParseSess;
use crate::rustc_span::edition::Edition;
use crate::rustc_span::fatal_error::catch_fatal_errors;
use crate::rustc_span::source_map::{FilePathMapping, SourceMap};
use crate::rustc_span::{BytePos, FileName, SourceFile, Span, create_session_if_not_set_then};
use serde::{Deserialize, Serialize};

/// A byte range in the source handed to [`analyze_source`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TextRange {
    pub start: u32,
    pub end: u32,
}

/// A fact the parser cannot settle without resolved static information.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StaticFact<T> {
    Known { value: T },
    Unknown { why: String },
}

/// Resolved information aligned to syntax spans by a static analysis caller.
///
/// Each span is in the original source passed to [`analyze_source_with_evidence`].
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct EffectEvidence {
    #[serde(default)]
    pub macros: Vec<ResolvedMacro>,
    #[serde(default)]
    pub methods: Vec<ResolvedMethod>,
    #[serde(default)]
    pub expression_types: Vec<ResolvedExpressionType>,
    #[serde(default)]
    pub question_errors: Vec<QuestionErrorType>,
    #[serde(default)]
    pub body_types: Vec<ResolvedBodyType>,
    /// Full expression spans marked overflow-checked by the type checker.
    #[serde(default)]
    pub overflow_checked: Vec<TextRange>,
}

/// The definition a macro invocation resolved to.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ResolvedMacro {
    pub span: TextRange,
    pub def_path: String,
}

/// The method definition selected for a method call.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ResolvedMethod {
    pub span: TextRange,
    pub def_path: String,
}

/// Type facts for one expression, at its full source span.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ResolvedExpressionType {
    pub span: TextRange,
    pub is_integer: bool,
    pub is_never: bool,
}

/// The error type determined for a `?` expression.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct QuestionErrorType {
    pub span: TextRange,
    pub error_type: String,
}

/// The type checker result for a function body's block expression, at the block span.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ResolvedBodyType {
    pub span: TextRange,
    pub is_never: bool,
}

/// Facts for one function body.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FunctionEffects {
    pub name: String,
    pub span: TextRange,
    pub body_span: TextRange,
    pub diverges: StaticFact<bool>,
    pub panic_sites: Vec<PanicSite>,
    pub question_marks: Vec<QuestionMark>,
    pub arithmetic: Vec<ArithmeticSite>,
    pub unsafe_blocks: Vec<TextRange>,
    pub doc_sections: DocSections,
}

/// A source location that may panic.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PanicSite {
    pub kind: PanicKind,
    pub span: TextRange,
}

/// Why a source location may panic.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanicKind {
    ExplicitMacro { def_path: String },
    OptionUnwrap,
    OptionExpect,
    ResultUnwrap,
    ResultExpect,
    Index,
    ArithmeticMayOverflow,
    Unknown { why: String },
}

/// A `?` expression and its resolved error type, when available.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct QuestionMark {
    pub span: TextRange,
    pub error_type: StaticFact<String>,
}

/// An arithmetic operation, without evaluating its operands.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ArithmeticSite {
    pub operator: ArithmeticOperator,
    pub span: TextRange,
    pub operands_integer: StaticFact<bool>,
    pub may_overflow: StaticFact<bool>,
}

/// An arithmetic operator that can overflow for integer operands.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArithmeticOperator {
    Add,
    Subtract,
    Multiply,
    Divide,
    Remainder,
    ShiftLeft,
    ShiftRight,
}

/// The Markdown headings found in an item's doc attributes.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct DocSections {
    pub headings: Vec<MarkdownHeading>,
    pub panics: bool,
    pub errors: bool,
    pub safety: bool,
}

/// One ATX or Setext Markdown heading.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MarkdownHeading {
    pub level: u8,
    pub title: String,
}

/// Parse a source file and report the function bodies it contains.
pub fn analyze_source(source: &str) -> Result<Vec<FunctionEffects>, Vec<String>> {
    analyze_source_with_evidence(source, &EffectEvidence::default())
}

/// Parse a source file and combine its syntax with supplied resolution and type facts.
pub fn analyze_source_with_evidence(
    source: &str,
    evidence: &EffectEvidence,
) -> Result<Vec<FunctionEffects>, Vec<String>> {
    assert!(
        crate::unwind_janky::unwinding_is_enabled(),
        "analyze_source needs panic=unwind and a catcher installed through unwind_janky::install_catcher"
    );
    let captured = Arc::new(eko::thread::Mutex::new(String::new()));
    match catch_fatal_errors(|| parse_source(source, evidence, &captured)) {
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

fn parse_source(
    source: &str,
    evidence: &EffectEvidence,
    captured: &Arc<eko::thread::Mutex<String>>,
) -> Result<Vec<FunctionEffects>, Vec<String>> {
    create_session_if_not_set_then(Edition::Edition2024, |_| {
        let sm = Arc::new(SourceMap::for_text(FilePathMapping::empty()));
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
        let functions = parsed.map(|krate| {
            let file = Arc::clone(&psess.source_map().files()[0]);
            let mut collector = FunctionCollector { file: &file, evidence, functions: Vec::new() };
            collector.visit_crate(&krate);
            collector.functions
        });
        psess.dcx().emit_stashed_diagnostics();
        let errors = error_entries(&captured.lock());
        match functions {
            Some(functions) if errors.is_empty() => Ok(functions),
            Some(_) | None if !errors.is_empty() => Err(errors),
            None => Err(alloc::vec!["error: the parser produced no crate".to_string()]),
            Some(functions) => Ok(functions),
        }
    })
}

struct Capture(Arc<eko::thread::Mutex<String>>);

impl core::fmt::Write for Capture {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        self.0.lock().push_str(text);
        Ok(())
    }
}

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

struct FunctionCollector<'a> {
    file: &'a SourceFile,
    evidence: &'a EffectEvidence,
    functions: Vec<FunctionEffects>,
}

impl<'a> Visitor<'a> for FunctionCollector<'a> {
    type Result = ();

    fn visit_item(&mut self, item: &'a Item) {
        if let ItemKind::Fn(function) = &item.kind {
            if let Some(body) = &function.body {
                self.functions.push(function_effects(
                    item.kind.ident().map_or_else(String::new, |ident| ident.name.as_str().to_string()),
                    item.span,
                    &item.attrs,
                    body,
                    self.file,
                    self.evidence,
                ));
            }
        }
        visit::walk_item(self, item);
    }

    fn visit_assoc_item(&mut self, item: &'a AssocItem, ctxt: AssocCtxt) {
        if let AssocItemKind::Fn(function) = &item.kind {
            if let Some(body) = &function.body {
                self.functions.push(function_effects(
                    item.kind.ident().map_or_else(String::new, |ident| ident.name.as_str().to_string()),
                    item.span,
                    &item.attrs,
                    body,
                    self.file,
                    self.evidence,
                ));
            }
        }
        visit::walk_assoc_item(self, item, ctxt);
    }
}

fn function_effects(
    name: String,
    item_span: Span,
    attrs: &AttrVec,
    body: &Block,
    file: &SourceFile,
    evidence: &EffectEvidence,
) -> FunctionEffects {
    let mut collector = BodyCollector {
        file,
        evidence,
        panic_sites: Vec::new(),
        question_marks: Vec::new(),
        arithmetic: Vec::new(),
        unsafe_blocks: Vec::new(),
    };
    collector.visit_block(body);
    let body_span = range(file, body.span);
    let diverges = evidence
        .body_types
        .iter()
        .find(|fact| fact.span == body_span)
        .map(|fact| known(fact.is_never))
        .unwrap_or_else(|| block_diverges(body, file, evidence));
    FunctionEffects {
        name,
        span: range(file, item_span),
        body_span,
        diverges,
        panic_sites: collector.panic_sites,
        question_marks: collector.question_marks,
        arithmetic: collector.arithmetic,
        unsafe_blocks: collector.unsafe_blocks,
        doc_sections: doc_sections(attrs),
    }
}

struct BodyCollector<'a> {
    file: &'a SourceFile,
    evidence: &'a EffectEvidence,
    panic_sites: Vec<PanicSite>,
    question_marks: Vec<QuestionMark>,
    arithmetic: Vec<ArithmeticSite>,
    unsafe_blocks: Vec<TextRange>,
}

impl<'a> BodyCollector<'a> {
    fn macro_site(&mut self, mac: &ast::MacCall) {
        let span = range(self.file, mac.span());
        let kind = match self.evidence.macros.iter().find(|resolved| resolved.span == span) {
            Some(resolved) if panic_macro(&resolved.def_path).is_some() => {
                PanicKind::ExplicitMacro { def_path: resolved.def_path.clone() }
            }
            Some(_) => PanicKind::Unknown {
                why: "macro expansion effects are unavailable".to_string(),
            },
            None => PanicKind::Unknown { why: "macro resolution is unavailable".to_string() },
        };
        self.panic_sites.push(PanicSite { kind, span });
    }

    fn method_site(&mut self, expr: &Expr) {
        let span = range(self.file, expr.span);
        let resolved = self.evidence.methods.iter().find(|method| method.span == span);
        let kind = match resolved.and_then(|method| method_panic_kind(&method.def_path)) {
            Some(kind) => kind,
            None if resolved.is_some() => return,
            None => PanicKind::Unknown {
                why: "method resolution and receiver type are unavailable".to_string(),
            },
        };
        self.panic_sites.push(PanicSite { kind, span });
    }

    fn question_mark(&mut self, expr: &Expr) {
        let span = range(self.file, expr.span);
        let error_type = self
            .evidence
            .question_errors
            .iter()
            .find(|fact| fact.span == span)
            .map(|fact| known(fact.error_type.clone()))
            .unwrap_or_else(|| StaticFact::Unknown {
                why: "the question mark error type is unresolved".to_string(),
            });
        self.question_marks.push(QuestionMark { span, error_type });
    }

    fn arithmetic_site(
        &mut self,
        expr: &Expr,
        operator: ArithmeticOperator,
        left: &Expr,
        right: &Expr,
    ) {
        let span = range(self.file, expr.span);
        let left_type = self.expression_type(left);
        let right_type = self.expression_type(right);
        let operands_integer = match (left_type, right_type) {
            (Some(left), Some(right)) => known(left.is_integer && right.is_integer),
            _ => StaticFact::Unknown { why: "the arithmetic operand types are unresolved".to_string() },
        };
        let may_overflow = match &operands_integer {
            StaticFact::Known { value: false } => known(false),
            StaticFact::Known { value: true }
                if self.evidence.overflow_checked.contains(&span) => known(true),
            StaticFact::Known { value: true } => StaticFact::Unknown {
                why: "overflow-check information is unavailable".to_string(),
            },
            StaticFact::Unknown { .. } => StaticFact::Unknown {
                why: "integer operand types are unresolved".to_string(),
            },
        };
        if matches!(&may_overflow, StaticFact::Known { value: true }) {
            self.panic_sites.push(PanicSite { kind: PanicKind::ArithmeticMayOverflow, span });
        }
        self.arithmetic.push(ArithmeticSite { operator, span, operands_integer, may_overflow });
    }

    fn expression_type(&self, expr: &Expr) -> Option<&ResolvedExpressionType> {
        let span = range(self.file, expr.span);
        self.evidence.expression_types.iter().find(|fact| fact.span == span)
    }
}

impl<'a> Visitor<'a> for BodyCollector<'a> {
    type Result = ();

    fn visit_block(&mut self, block: &'a Block) {
        if matches!(block.rules, BlockCheckMode::Unsafe(ast::UnsafeSource::UserProvided)) {
            self.unsafe_blocks.push(range(self.file, block.span));
        }
        visit::walk_block(self, block);
    }

    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        if let StmtKind::MacCall(mac) = &stmt.kind {
            self.macro_site(&mac.mac);
        }
        visit::walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'a Expr) {
        match &expr.kind {
            ExprKind::MacCall(mac) => self.macro_site(mac),
            ExprKind::MethodCall(_) => self.method_site(expr),
            ExprKind::Try(_) => self.question_mark(expr),
            ExprKind::Index(_, _, _) => self.panic_sites.push(PanicSite {
                kind: PanicKind::Index,
                span: range(self.file, expr.span),
            }),
            ExprKind::Binary(op, left, right) => {
                if let Some(operator) = arithmetic_operator(op.node) {
                    self.arithmetic_site(expr, operator, left, right);
                }
            }
            ExprKind::AssignOp(op, left, right) => {
                if let Some(operator) = assign_operator(op.node) {
                    self.arithmetic_site(expr, operator, left, right);
                }
            }
            _ => {}
        }
        visit::walk_expr(self, expr);
    }
}

fn arithmetic_operator(operator: BinOpKind) -> Option<ArithmeticOperator> {
    Some(match operator {
        BinOpKind::Add => ArithmeticOperator::Add,
        BinOpKind::Sub => ArithmeticOperator::Subtract,
        BinOpKind::Mul => ArithmeticOperator::Multiply,
        BinOpKind::Div => ArithmeticOperator::Divide,
        BinOpKind::Rem => ArithmeticOperator::Remainder,
        BinOpKind::Shl => ArithmeticOperator::ShiftLeft,
        BinOpKind::Shr => ArithmeticOperator::ShiftRight,
        BinOpKind::And
        | BinOpKind::Or
        | BinOpKind::BitXor
        | BinOpKind::BitAnd
        | BinOpKind::BitOr
        | BinOpKind::Eq
        | BinOpKind::Lt
        | BinOpKind::Le
        | BinOpKind::Ne
        | BinOpKind::Ge
        | BinOpKind::Gt => return None,
    })
}

fn assign_operator(operator: AssignOpKind) -> Option<ArithmeticOperator> {
    Some(match operator {
        AssignOpKind::AddAssign => ArithmeticOperator::Add,
        AssignOpKind::SubAssign => ArithmeticOperator::Subtract,
        AssignOpKind::MulAssign => ArithmeticOperator::Multiply,
        AssignOpKind::DivAssign => ArithmeticOperator::Divide,
        AssignOpKind::RemAssign => ArithmeticOperator::Remainder,
        AssignOpKind::ShlAssign => ArithmeticOperator::ShiftLeft,
        AssignOpKind::ShrAssign => ArithmeticOperator::ShiftRight,
        AssignOpKind::BitXorAssign | AssignOpKind::BitAndAssign | AssignOpKind::BitOrAssign => {
            return None;
        }
    })
}

fn panic_macro(def_path: &str) -> Option<&str> {
    if !def_path.starts_with("core::") && !def_path.starts_with("builtin::") {
        return None;
    }
    match def_path.rsplit("::").next()? {
        "panic" | "todo" | "unimplemented" | "unreachable" | "assert" | "assert_eq"
        | "assert_ne" | "debug_assert" | "debug_assert_eq" | "debug_assert_ne" => {
            def_path.rsplit("::").next()
        }
        _ => None,
    }
}

fn macro_diverges(def_path: &str) -> Option<bool> {
    match panic_macro(def_path)? {
        "panic" | "todo" | "unimplemented" | "unreachable" => Some(true),
        _ => None,
    }
}

fn method_panic_kind(def_path: &str) -> Option<PanicKind> {
    if def_path.starts_with("core::option::Option::") {
        return match def_path.rsplit("::").next()? {
            "unwrap" => Some(PanicKind::OptionUnwrap),
            "expect" => Some(PanicKind::OptionExpect),
            _ => None,
        };
    }
    if def_path.starts_with("core::result::Result::") {
        return match def_path.rsplit("::").next()? {
            "unwrap" => Some(PanicKind::ResultUnwrap),
            "expect" => Some(PanicKind::ResultExpect),
            _ => None,
        };
    }
    None
}

fn block_diverges(block: &Block, file: &SourceFile, evidence: &EffectEvidence) -> StaticFact<bool> {
    let mut unresolved_prefix = false;
    for (index, stmt) in block.stmts.iter().enumerate() {
        let is_tail = index + 1 == block.stmts.len() && matches!(stmt.kind, StmtKind::Expr(_));
        let result = match &stmt.kind {
            StmtKind::Let(local) => local.kind.init().map(|expr| expression_diverges(expr, file, evidence)),
            StmtKind::Expr(expr) | StmtKind::Semi(expr) => Some(expression_diverges(expr, file, evidence)),
            StmtKind::MacCall(mac) => Some(macro_divergence(&mac.mac, file, evidence)),
            StmtKind::Item(_) | StmtKind::Empty => None,
        };
        let Some(result) = result else {
            continue;
        };
        match result {
            StaticFact::Known { value: true } => return known(true),
            StaticFact::Unknown { .. } if !is_tail => unresolved_prefix = true,
            StaticFact::Unknown { .. } => return result,
            StaticFact::Known { value: false } if is_tail => {
                return if unresolved_prefix {
                    unknown("an earlier expression's divergence is unresolved")
                } else {
                    known(false)
                };
            }
            StaticFact::Known { value: false } => {}
        }
    }
    if unresolved_prefix {
        unknown("an earlier expression's divergence is unresolved")
    } else {
        known(false)
    }
}

fn expression_diverges(expr: &Expr, file: &SourceFile, evidence: &EffectEvidence) -> StaticFact<bool> {
    if let Some(fact) = evidence
        .expression_types
        .iter()
        .find(|fact| fact.span == range(file, expr.span))
    {
        return known(fact.is_never);
    }
    match &expr.kind {
        ExprKind::Paren(inner) => expression_diverges(inner, file, evidence),
        ExprKind::Block(block, _) => block_diverges(block, file, evidence),
        ExprKind::Loop(block, label, _) => {
            if loop_has_break(block, *label) {
                unknown("loop exit reachability is unresolved")
            } else {
                known(true)
            }
        }
        ExprKind::If(condition, then_block, otherwise) => {
            let condition = expression_diverges(condition, file, evidence);
            if matches!(&condition, StaticFact::Known { value: true }) {
                return known(true);
            }
            let then_result = block_diverges(then_block, file, evidence);
            let else_result = otherwise
                .as_deref()
                .map(|otherwise| expression_diverges(otherwise, file, evidence))
                .unwrap_or_else(|| known(false));
            let branches = match (&then_result, &else_result) {
                (StaticFact::Known { value: true }, StaticFact::Known { value: true }) => known(true),
                (StaticFact::Known { value: false }, StaticFact::Known { value: false }) => known(false),
                _ => unknown("conditional branch divergence is unresolved"),
            };
            match condition {
                StaticFact::Known { value: true } => known(true),
                StaticFact::Known { value: false } => branches,
                StaticFact::Unknown { .. } if matches!(&branches, StaticFact::Known { value: true }) => {
                    known(true)
                }
                StaticFact::Unknown { .. } => unknown("conditional expression divergence is unresolved"),
            }
        }
        ExprKind::MacCall(mac) => macro_divergence(mac, file, evidence),
        ExprKind::Call(callee, args) => {
            sequence_diverges(
                core::iter::once(&**callee).chain(args.iter().map(|arg| &**arg)),
                unknown("the call target's return type is unresolved"),
                file,
                evidence,
            )
        }
        ExprKind::MethodCall(call) => sequence_diverges(
            core::iter::once(&*call.receiver).chain(call.args.iter().map(|arg| &**arg)),
            unknown("the method call's return type is unresolved"),
            file,
            evidence,
        ),
        ExprKind::Array(items) | ExprKind::Tup(items) => sequence_diverges(
            items.iter().map(|item| &**item),
            known(false),
            file,
            evidence,
        ),
        ExprKind::Binary(op, left, right) if matches!(op.node, BinOpKind::And | BinOpKind::Or) => {
            let left_result = expression_diverges(left, file, evidence);
            if matches!(&left_result, StaticFact::Known { value: true }) {
                return known(true);
            }
            let right_result = expression_diverges(right, file, evidence);
            if matches!(&right_result, StaticFact::Known { value: true }) {
                unknown("short-circuit evaluation may skip the diverging operand")
            } else if matches!(&left_result, StaticFact::Known { value: false })
                && matches!(&right_result, StaticFact::Known { value: false })
            {
                known(false)
            } else {
                unknown("short-circuit expression divergence is unresolved")
            }
        }
        ExprKind::Binary(_, left, right)
        | ExprKind::Assign(left, right, _)
        | ExprKind::AssignOp(_, left, right) => sequence_diverges(
            [&**left, &**right].into_iter(),
            known(false),
            file,
            evidence,
        ),
        ExprKind::Index(value, index, _) => sequence_diverges(
            [&**value, &**index].into_iter(),
            unknown("indexing behavior is unresolved"),
            file,
            evidence,
        ),
        ExprKind::Try(value) => sequence_diverges(
            core::iter::once(&**value),
            unknown("question mark conversion behavior is unresolved"),
            file,
            evidence,
        ),
        ExprKind::Break(_, value) => value.as_deref().map_or_else(
            || known(false),
            |value| sequence_diverges(core::iter::once(value), known(false), file, evidence),
        ),
        ExprKind::Continue(_) => known(false),
        ExprKind::Closure(_) | ExprKind::Gen(..) => known(false),
        ExprKind::Match(_, _, _) => unknown("match exhaustiveness is unresolved"),
        ExprKind::While(..) | ExprKind::ForLoop(_) => unknown("loop exit conditions are unresolved"),
        ExprKind::Lit(_)
        | ExprKind::Path(..)
        | ExprKind::Underscore
        | ExprKind::OffsetOf(..)
        | ExprKind::IncludedBytes(..)
        | ExprKind::Err(..)
        | ExprKind::Dummy => known(false),
        ExprKind::Unary(_, value)
        | ExprKind::Move(value, _)
        | ExprKind::Await(value, _)
        | ExprKind::Use(value, _)
        | ExprKind::DirectConstArg(value)
        | ExprKind::Become(value)
        | ExprKind::AddrOf(_, _, value)
        | ExprKind::Field(value, _) => sequence_diverges(
            core::iter::once(&**value),
            unknown("expression behavior is unresolved"),
            file,
            evidence,
        ),
        ExprKind::Cast(value, _) | ExprKind::Type(value, _) | ExprKind::Let(_, value, _, _) => {
            sequence_diverges(core::iter::once(&**value), known(false), file, evidence)
        }
        ExprKind::UnsafeBinderCast(_, value, _) => {
            sequence_diverges(core::iter::once(&**value), known(false), file, evidence)
        }
        ExprKind::Range(start, end, _) => sequence_diverges(
            start.iter().chain(end.iter()).map(|value| &**value),
            known(false),
            file,
            evidence,
        ),
        ExprKind::Repeat(value, _) => {
            sequence_diverges(core::iter::once(&**value), known(false), file, evidence)
        }
        ExprKind::Ret(value) | ExprKind::Yeet(value) => value.as_deref().map_or_else(
            || known(false),
            |value| sequence_diverges(core::iter::once(value), known(false), file, evidence),
        ),
        ExprKind::TryBlock(block, _) => block_diverges(block, file, evidence),
        ExprKind::InlineAsm(_) | ExprKind::FormatArgs(_) => {
            unknown("inline assembly or formatting behavior is unresolved")
        }
        ExprKind::Struct(_) | ExprKind::Yield(_) | ExprKind::ConstBlock(_) => {
            unknown("expression behavior is unresolved")
        }
    }
}

fn macro_divergence(
    mac: &ast::MacCall,
    file: &SourceFile,
    evidence: &EffectEvidence,
) -> StaticFact<bool> {
    let span = range(file, mac.span());
    match evidence.macros.iter().find(|fact| fact.span == span) {
        Some(fact) => match macro_diverges(&fact.def_path) {
            Some(value) => known(value),
            None => unknown("the resolved macro is not a known diverging macro"),
        },
        None => unknown("macro resolution is unavailable"),
    }
}

fn sequence_diverges<'a>(
    exprs: impl IntoIterator<Item = &'a Expr>,
    result: StaticFact<bool>,
    file: &SourceFile,
    evidence: &EffectEvidence,
) -> StaticFact<bool> {
    let mut unresolved = false;
    for expr in exprs {
        match expression_diverges(expr, file, evidence) {
            StaticFact::Known { value: true } => return known(true),
            StaticFact::Known { value: false } => {}
            StaticFact::Unknown { .. } => unresolved = true,
        }
    }
    match result {
        StaticFact::Known { value: true } => known(true),
        StaticFact::Known { value: false } if unresolved => {
            unknown("an operand's divergence is unresolved")
        }
        StaticFact::Known { value } => known(value),
        StaticFact::Unknown { .. } if unresolved => unknown("an operand or operation is unresolved"),
        unknown_result => unknown_result,
    }
}

fn loop_has_break(block: &Block, label: Option<Label>) -> bool {
    struct BreakFinder {
        labels: Vec<(Option<String>, bool)>,
        found: bool,
    }

    impl<'a> Visitor<'a> for BreakFinder {
        type Result = ();

        fn visit_item(&mut self, _: &'a Item) {}

        fn visit_assoc_item(&mut self, _: &'a AssocItem, _: AssocCtxt) {}

        fn visit_expr(&mut self, expr: &'a Expr) {
            match &expr.kind {
                ExprKind::Break(label, _) => {
                    if let Some(label) = label {
                        let name = label.ident.name.as_str();
                        if let Some((index, (_, is_loop))) = self
                            .labels
                            .iter()
                            .enumerate()
                            .rev()
                            .find(|(_, (candidate, _))| candidate.as_deref() == Some(name))
                        {
                            self.found |= index == 0 && *is_loop;
                        }
                    } else if let Some((index, (_, is_loop))) = self
                        .labels
                        .iter()
                        .enumerate()
                        .rev()
                        .find(|(_, (_, is_loop))| *is_loop)
                    {
                        self.found |= index == 0 && *is_loop;
                    }
                    if !self.found {
                        visit::walk_expr(self, expr);
                    }
                }
                ExprKind::Loop(_, label, _) => {
                    self.labels.push((
                        (*label).map(|label| label.ident.name.as_str().to_string()),
                        true,
                    ));
                    visit::walk_expr(self, expr);
                    self.labels.pop();
                }
                ExprKind::While(_, _, label) => {
                    self.labels.push((
                        (*label).map(|label| label.ident.name.as_str().to_string()),
                        true,
                    ));
                    visit::walk_expr(self, expr);
                    self.labels.pop();
                }
                ExprKind::ForLoop(for_loop) => {
                    self.labels.push((
                        for_loop.label.map(|label| label.ident.name.as_str().to_string()),
                        true,
                    ));
                    visit::walk_expr(self, expr);
                    self.labels.pop();
                }
                ExprKind::Block(_, Some(label)) => {
                    self.labels.push((Some(label.ident.name.as_str().to_string()), false));
                    visit::walk_expr(self, expr);
                    self.labels.pop();
                }
                ExprKind::Closure(_) | ExprKind::Gen(..) => {}
                _ => visit::walk_expr(self, expr),
            }
        }
    }

    let outer = label.map(|label| label.ident.name.as_str().to_string());
    let mut finder = BreakFinder { labels: alloc::vec![(outer, true)], found: false };
    finder.visit_block(block);
    finder.found
}

fn doc_sections(attrs: &AttrVec) -> DocSections {
    let mut doc = String::new();
    for attr in attrs {
        if let Some(fragment) = attr.doc_str() {
            if !doc.is_empty() {
                doc.push('\n');
            }
            doc.push_str(fragment.as_str());
        }
    }
    let headings = markdown_headings(&doc);
    let panics = headings.iter().any(|heading| heading.title == "Panics");
    let errors = headings.iter().any(|heading| heading.title == "Errors");
    let safety = headings.iter().any(|heading| heading.title == "Safety");
    DocSections { headings, panics, errors, safety }
}

fn markdown_headings(doc: &str) -> Vec<MarkdownHeading> {
    let lines: Vec<&str> = doc.lines().collect();
    let mut headings = Vec::new();
    let mut fence: Option<(char, usize)> = None;
    let mut visible: Vec<Option<String>> = Vec::new();
    for line in &lines {
        let indent = line.len() - line.trim_start().len();
        if indent >= 4 || line.starts_with('\t') {
            visible.push(None);
            continue;
        }
        let trimmed = line.trim_start();
        if let Some((marker, length)) = fence {
            let run = trimmed.chars().take_while(|ch| *ch == marker).count();
            if run >= length && trimmed[run..].trim().is_empty() {
                fence = None;
            }
            visible.push(None);
            continue;
        }
        let marker = trimmed.chars().next();
        if let Some(marker @ ('`' | '~')) = marker {
            let run = trimmed.chars().take_while(|ch| *ch == marker).count();
            if run >= 3 {
                fence = Some((marker, run));
                visible.push(None);
                continue;
            }
        }
        let heading = atx_heading(trimmed);
        if let Some(heading) = heading {
            headings.push(heading);
            visible.push(None);
            continue;
        }
        if let Some(level) = setext_level(trimmed) {
            if let Some(Some(title)) = visible.last() {
                headings.push(MarkdownHeading { level, title: title.to_string() });
            }
            visible.push(None);
            continue;
        }
        visible.push((!trimmed.is_empty()).then(|| trimmed.to_string()));
    }
    headings
}

fn atx_heading(line: &str) -> Option<MarkdownHeading> {
    let level = line.chars().take_while(|ch| *ch == '#').count();
    if level == 0 || level > 6 || (line.len() > level && !line.as_bytes()[level].is_ascii_whitespace()) {
        return None;
    }
    let mut title = line[level..].trim().to_string();
    while title.ends_with('#') {
        title.pop();
    }
    Some(MarkdownHeading { level: level as u8, title: title.trim().to_string() })
}

fn setext_level(line: &str) -> Option<u8> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.chars().all(|ch| ch == '=') {
        Some(1)
    } else if trimmed.chars().all(|ch| ch == '-') {
        Some(2)
    } else {
        None
    }
}

fn range(file: &SourceFile, span: Span) -> TextRange {
    fn at(file: &SourceFile, position: BytePos) -> u32 {
        if position < file.start_pos {
            0
        } else {
            file.original_relative_byte_pos(position).0
        }
    }
    TextRange { start: at(file, span.lo()), end: at(file, span.hi()) }
}

fn known<T>(value: T) -> StaticFact<T> {
    StaticFact::Known { value }
}

fn unknown<T>(why: &str) -> StaticFact<T> {
    StaticFact::Unknown { why: why.to_string() }
}
