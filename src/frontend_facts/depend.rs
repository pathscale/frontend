//! Data and control dependencies read from one function body.
//!
//! The analysis uses the parsed syntax tree only. It does not expand macros or resolve names,
//! so an opaque macro or a path outside the function's bindings remains an explicit unknown.
//! Loop states are joined until their finite dependency sets stop growing.

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::rustc_ast::{
    self as ast, Arm, BinOpKind, Block, Expr, ExprKind, Fn, Item, ItemKind, LocalKind, ModKind,
    Mutability, Pat, PatKind, StmtKind,
};
use crate::rustc_errors::plain_emitter::PlainEmitter;
use crate::rustc_errors::DiagCtxt;
use crate::rustc_parse::lexer::StripTokens;
use crate::rustc_parse::new_parser_from_source_str;
use crate::rustc_session::parse::ParseSess;
use crate::rustc_span::edition::Edition;
use crate::rustc_span::fatal_error::catch_fatal_errors;
use crate::rustc_span::SourceFile;
use crate::rustc_span::source_map::{FilePathMapping, SourceMap};
use crate::rustc_span::{BytePos, FileName, Span, create_session_if_not_set_then};
use serde::{Deserialize, Serialize};

/// A byte range in the source passed to [`dependence`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextRange {
    pub start: u32,
    pub end: u32,
}

/// Whether a dependency is a function parameter or a local binding.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VariableKind {
    Parameter,
    Local,
}

/// A parameter or local mentioned by a dependence fact.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Variable {
    pub name: String,
    pub kind: VariableKind,
    pub span: TextRange,
}

/// A dependency the syntax establishes, or a reason it cannot establish one.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum Dependency {
    Variable {
        variable: Variable,
        /// Read occurrence that contributes this binding to the dependency.
        occurrence: TextRange,
    },
    Unknown { why: String },
}

/// A branch condition that affects an outgoing value.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlDependency {
    pub condition: String,
    pub operands: Vec<Dependency>,
}

/// The source-level kind of a literal operand reported by dependence analysis.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiteralKind {
    Char,
    Byte,
    Str,
    Int,
}

/// A literal that reaches an outgoing value, including its syntactic relation.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LiteralOperand {
    /// The decoded literal value, suitable for comparison with an aligned doc operand.
    pub literal: String,
    /// The exact spelling in the Rust source.
    pub text: String,
    pub kind: LiteralKind,
    pub span: TextRange,
    /// The other expression in a comparison, pattern, or method argument relation.
    pub compared_with: Option<String>,
    /// The comparison or match condition that carries this operand, when there is one.
    pub condition: Option<String>,
    /// Whether the literal crossed a call whose data dependence is unresolved.
    pub via_unresolved_call: bool,
    /// The outgoing expression whose dependence includes this operand.
    pub exit: TextRange,
}

/// Kind of value leaving the function.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitKind {
    Tail,
    Return,
    Try,
}

/// One tail value, explicit return value, or `?` exit.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Exit {
    pub kind: ExitKind,
    pub span: TextRange,
    pub value: Option<String>,
    pub data_dependencies: Vec<Dependency>,
    pub control_dependencies: Vec<ControlDependency>,
}

/// Whether a mutable parameter or local has an assignment in the body.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum AssignmentFact {
    Never,
    Ever,
    Unknown { why: String },
}

/// Assignment status for one mutable parameter or local.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MutableAssignment {
    pub variable: Variable,
    pub assigned: AssignmentFact,
    /// Byte ranges of assignment expressions that write this binding.
    pub assignment_sites: Vec<TextRange>,
}

/// An analysis limitation that may affect more than one outgoing value.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Unknown {
    pub why: String,
    pub span: TextRange,
}

/// Static dependence facts for a named function body.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FunctionDependence {
    pub function: String,
    pub exits: Vec<Exit>,
    pub literal_operands: Vec<LiteralOperand>,
    pub mutable_assignments: Vec<MutableAssignment>,
    pub unknowns: Vec<Unknown>,
}

/// Read data and control dependencies from the body of `function_name` in `source`.
///
/// Parsing errors, a missing function, and an ambiguous function name are returned as messages.
/// The analysis itself runs over parser output only; it does not resolve paths or expand macros.
pub fn dependence(
    source: &str,
    function_name: &str,
) -> Result<FunctionDependence, Vec<String>> {
    assert!(
        crate::unwind_janky::unwinding_is_enabled(),
        "dependence needs panic=unwind and a catcher installed through unwind_janky::install_catcher"
    );
    let captured = Arc::new(eko::thread::Mutex::new(String::new()));
    match catch_fatal_errors(|| parse_and_analyze(source, function_name, &captured)) {
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

fn parse_and_analyze(
    source: &str,
    function_name: &str,
    captured: &Arc<eko::thread::Mutex<String>>,
) -> Result<FunctionDependence, Vec<String>> {
    create_session_if_not_set_then(Edition::Edition2024, |_| {
        let sm = Arc::new(SourceMap::for_text(FilePathMapping::empty()));
        let emitter = PlainEmitter::new()
            .sm(Some(Arc::clone(&sm)))
            .short_message(true)
            .dst(Box::new(Capture(Arc::clone(captured))));
        let mut psess = ParseSess::with_dcx(DiagCtxt::new(Box::new(emitter)), Arc::clone(&sm));
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
        let had_errors = psess.dcx().has_errors().is_some();
        let _ = psess.dcx().emit_stashed_diagnostics();
        if parsed.is_none() || had_errors {
            let mut errors = error_entries(&captured.lock());
            if errors.is_empty() {
                errors.push("error: source did not parse".to_string());
            }
            return Err(errors);
        }
        let krate = parsed.unwrap();
        let mut found = Vec::new();
        find_functions(&krate.items, function_name, &mut found);
        match found.as_slice() {
            [] => Err(alloc::vec![format!("error: function `{function_name}` was not found")]),
            [function] => {
                let file = Arc::clone(&psess.source_map().files()[0]);
                let mut analyzer = Analyzer::new(source, &file, function_name);
                Ok(analyzer.run(function))
            }
            _ => Err(alloc::vec![format!(
                "error: function name `{function_name}` is ambiguous"
            )]),
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
        } else if line.starts_with("error:") || line.starts_with("fatal:") {
            entries.push(line.to_string());
        }
    }
    entries
}

fn find_functions<'a>(items: &'a [Box<Item>], name: &str, found: &mut Vec<&'a Fn>) {
    for item in items {
        match &item.kind {
            ItemKind::Fn(function) if function.ident.name.to_string() == name => {
                found.push(function);
            }
            ItemKind::Mod(_, _, ModKind::Loaded(items, _, _)) => {
                find_functions(items, name, found);
            }
            ItemKind::Impl(implementation) => {
                for item in &implementation.items {
                    if let ast::AssocItemKind::Fn(function) = &item.kind
                        && function.ident.name.to_string() == name
                    {
                        found.push(function);
                    }
                }
            }
            _ => {}
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct Value {
    data: BTreeMap<usize, BTreeSet<TextRange>>,
    controls: BTreeSet<ControlKey>,
    unknowns: BTreeSet<String>,
    literal_facts: BTreeSet<LiteralFact>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct LiteralFact {
    literal: String,
    text: String,
    kind: LiteralKind,
    span: TextRange,
    compared_with: Option<String>,
    condition: Option<String>,
    via_unresolved_call: bool,
}

impl Value {
    fn merge(&mut self, other: &Self) {
        merge_data(&mut self.data, &other.data);
        self.controls.extend(other.controls.iter().copied());
        self.unknowns.extend(other.unknowns.iter().cloned());
        self.literal_facts.extend(other.literal_facts.iter().cloned());
    }
}

fn merge_data(
    data: &mut BTreeMap<usize, BTreeSet<TextRange>>,
    other: &BTreeMap<usize, BTreeSet<TextRange>>,
) {
    for (id, occurrences) in other {
        data.entry(*id).or_default().extend(occurrences.iter().copied());
    }
}

fn unknown_value(why: String) -> Value {
    let mut value = Value::default();
    value.unknowns.insert(why);
    value
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct State {
    values: Vec<Value>,
    reachable: bool,
}

impl State {
    fn live() -> Self {
        Self { values: Vec::new(), reachable: true }
    }

    fn dead_like(state: &Self) -> Self {
        Self { values: state.values.clone(), reachable: false }
    }

    fn merge(&mut self, other: &Self) {
        if !self.reachable {
            *self = other.clone();
            return;
        }
        if !other.reachable {
            return;
        }
        if self.values.len() < other.values.len() {
            self.values.resize_with(other.values.len(), Value::default);
        }
        for (index, value) in other.values.iter().enumerate() {
            self.values[index].merge(value);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
enum ControlKind {
    Branch,
    Loop,
    Match,
    ShortCircuit,
    Try,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct ControlKey {
    start: u32,
    end: u32,
    kind: ControlKind,
}

#[derive(Clone, Debug, Default)]
struct ControlFact {
    condition: String,
    operands: BTreeMap<usize, BTreeSet<TextRange>>,
    unknowns: BTreeSet<String>,
    literal_facts: BTreeSet<LiteralFact>,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct BindingKey {
    start: u32,
    end: u32,
    name: String,
}

#[derive(Clone, Debug)]
struct BindingInfo {
    variable: Variable,
    mutable: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct ExitKey {
    kind: ExitKind,
    start: u32,
    end: u32,
    value: Option<String>,
}

#[derive(Clone, Debug, Default)]
struct ExitValue {
    data: BTreeMap<usize, BTreeSet<TextRange>>,
    controls: BTreeSet<ControlKey>,
    unknowns: BTreeSet<String>,
    literal_facts: BTreeSet<LiteralFact>,
}

impl ExitValue {
    fn merge(&mut self, value: &Value, controls: &BTreeSet<ControlKey>) {
        merge_data(&mut self.data, &value.data);
        self.controls.extend(value.controls.iter().copied());
        self.controls.extend(controls.iter().copied());
        self.unknowns.extend(value.unknowns.iter().cloned());
        self.literal_facts.extend(value.literal_facts.iter().cloned());
    }
}

#[derive(Default)]
struct LoopFlow {
    breaks: Option<State>,
    continues: Option<State>,
    break_value: Value,
}

impl LoopFlow {
    fn merge_break(&mut self, state: &State, value: &Value) {
        match &mut self.breaks {
            Some(existing) => existing.merge(state),
            None => self.breaks = Some(state.clone()),
        }
        self.break_value.merge(value);
    }

    fn merge_continue(&mut self, state: &State) {
        match &mut self.continues {
            Some(existing) => existing.merge(state),
            None => self.continues = Some(state.clone()),
        }
    }
}

struct Analyzer<'a> {
    source: &'a str,
    file: &'a SourceFile,
    function_name: &'a str,
    bindings: Vec<BindingInfo>,
    binding_ids: BTreeMap<BindingKey, usize>,
    scopes: Vec<BTreeMap<String, usize>>,
    controls: BTreeMap<ControlKey, ControlFact>,
    exits: BTreeMap<ExitKey, ExitValue>,
    assigned: BTreeSet<usize>,
    assignment_sites: BTreeMap<usize, BTreeSet<TextRange>>,
    assignment_unknowns: BTreeMap<usize, BTreeSet<String>>,
    unknowns: BTreeMap<TextRange, BTreeSet<String>>,
    loops: Vec<LoopFlow>,
}

impl<'a> Analyzer<'a> {
    fn new(source: &'a str, file: &'a SourceFile, function_name: &'a str) -> Self {
        Self {
            source,
            file,
            function_name,
            bindings: Vec::new(),
            binding_ids: BTreeMap::new(),
            scopes: alloc::vec![BTreeMap::new()],
            controls: BTreeMap::new(),
            exits: BTreeMap::new(),
            assigned: BTreeSet::new(),
            assignment_sites: BTreeMap::new(),
            assignment_unknowns: BTreeMap::new(),
            unknowns: BTreeMap::new(),
            loops: Vec::new(),
        }
    }

    fn run(&mut self, function: &Fn) -> FunctionDependence {
        let mut state = State::live();
        for param in &function.sig.decl.inputs {
            self.bind_pattern(
                &param.pat,
                VariableKind::Parameter,
                &Value::default(),
                &mut state,
            );
        }
        if let Some(body) = &function.body {
            let value = self.analyze_block(body, &mut state, &Vec::new());
            if state.reachable {
                if let Some(ast::Stmt { kind: StmtKind::Expr(tail), .. }) = body.stmts.last() {
                    self.record_exit(
                        ExitKind::Tail,
                        tail.span,
                        Some(self.text(tail.span)),
                        &value,
                        &BTreeSet::new(),
                    );
                }
            }
        }
        self.finish()
    }

    fn finish(&self) -> FunctionDependence {
        let mut exits = Vec::new();
        let mut literal_operands = Vec::new();
        for (key, value) in &self.exits {
            let span = TextRange { start: key.start, end: key.end };
            exits.push(Exit {
                kind: key.kind,
                span,
                value: key.value.clone(),
                data_dependencies: self.dependencies(&value.data, &value.unknowns),
                control_dependencies: value
                    .controls
                    .iter()
                    .filter_map(|key| self.public_control(*key))
                    .collect(),
            });
            let mut facts = value.literal_facts.clone();
            for control in &value.controls {
                if let Some(control) = self.controls.get(control) {
                    facts.extend(control.literal_facts.iter().cloned());
                }
            }
            literal_operands.extend(facts.into_iter().map(|fact| LiteralOperand {
                literal: fact.literal,
                text: fact.text,
                kind: fact.kind,
                span: fact.span,
                compared_with: fact.compared_with,
                condition: fact.condition,
                via_unresolved_call: fact.via_unresolved_call,
                exit: span,
            }));
        }
        exits.sort_by(|left, right| {
            left.span.start.cmp(&right.span.start).then(left.kind.cmp(&right.kind))
        });
        literal_operands.sort();

        let mutable_assignments = self
            .bindings
            .iter()
            .enumerate()
            .filter(|(_, binding)| binding.mutable)
            .map(|(id, binding)| {
                let assigned = if self.assigned.contains(&id) {
                    AssignmentFact::Ever
                } else if let Some(why) = self
                    .assignment_unknowns
                    .get(&id)
                    .and_then(|reasons| reasons.iter().next())
                {
                    AssignmentFact::Unknown { why: why.clone() }
                } else {
                    AssignmentFact::Never
                };
                MutableAssignment {
                    variable: binding.variable.clone(),
                    assigned,
                    assignment_sites: self
                        .assignment_sites
                        .get(&id)
                        .map(|sites| sites.iter().copied().collect())
                        .unwrap_or_default(),
                }
            })
            .collect();

        let unknowns = self
            .unknowns
            .iter()
            .flat_map(|(span, reasons)| {
                reasons.iter().map(|why| Unknown { why: why.clone(), span: *span })
            })
            .collect();
        FunctionDependence {
            function: self.function_name.to_string(),
            exits,
            literal_operands,
            mutable_assignments,
            unknowns,
        }
    }

    fn public_control(&self, key: ControlKey) -> Option<ControlDependency> {
        self.controls.get(&key).map(|fact| ControlDependency {
            condition: fact.condition.clone(),
            operands: self.dependencies(&fact.operands, &fact.unknowns),
        })
    }

    fn dependencies(
        &self,
        data: &BTreeMap<usize, BTreeSet<TextRange>>,
        unknowns: &BTreeSet<String>,
    ) -> Vec<Dependency> {
        let mut dependencies = Vec::new();
        for (id, occurrences) in data {
            if let Some(binding) = self.bindings.get(*id) {
                for occurrence in occurrences {
                    dependencies.push(Dependency::Variable {
                        variable: binding.variable.clone(),
                        occurrence: *occurrence,
                    });
                }
            }
        }
        for why in unknowns {
            dependencies.push(Dependency::Unknown { why: why.clone() });
        }
        dependencies
    }

    fn record_exit(
        &mut self,
        kind: ExitKind,
        span: Span,
        value_text: Option<String>,
        value: &Value,
        active: &BTreeSet<ControlKey>,
    ) {
        let range = self.range(span);
        let key = ExitKey { kind, start: range.start, end: range.end, value: value_text };
        self.exits.entry(key).or_default().merge(value, active);
    }

    fn record_unknown(&mut self, span: Span, why: String) {
        self.unknowns.entry(self.range(span)).or_default().insert(why);
    }

    fn record_control(&mut self, key: ControlKey, condition: String, value: &Value) {
        let fact = self.controls.entry(key).or_default();
        if fact.condition.is_empty() {
            fact.condition = condition;
        }
        merge_data(&mut fact.operands, &value.data);
        fact.unknowns.extend(value.unknowns.iter().cloned());
        fact.literal_facts.extend(value.literal_facts.iter().cloned());
    }

    fn control_key(&self, span: Span, kind: ControlKind) -> ControlKey {
        let range = self.range(span);
        ControlKey { start: range.start, end: range.end, kind }
    }

    fn analyze_block(&mut self, block: &Block, state: &mut State, active: &[ControlKey]) -> Value {
        self.scopes.push(BTreeMap::new());
        let mut tail = Value::default();
        let length = block.stmts.len();
        for (index, statement) in block.stmts.iter().enumerate() {
            if !state.reachable {
                break;
            }
            let is_tail = index + 1 == length;
            match &statement.kind {
                StmtKind::Let(local) => self.analyze_local(local, state, active),
                StmtKind::Item(_) | StmtKind::Empty => {}
                StmtKind::Expr(expression) if is_tail => {
                    tail = self.analyze_expr(expression, state, active);
                }
                StmtKind::Expr(expression) | StmtKind::Semi(expression) => {
                    self.analyze_expr(expression, state, active);
                }
                StmtKind::MacCall(_) => {
                    self.opaque_macro(statement.span, state);
                }
            }
        }
        self.scopes.pop();
        tail
    }

    fn analyze_local(&mut self, local: &ast::Local, state: &mut State, active: &[ControlKey]) {
        match &local.kind {
            LocalKind::Decl => {
                let mut value = Value::default();
                value.unknowns.insert("local has no initializer or reaching assignment".to_string());
                self.bind_pattern(&local.pat, VariableKind::Local, &value, state);
            }
            LocalKind::Init(initializer) => {
                let value = self.analyze_expr(initializer, state, active);
                self.bind_pattern(&local.pat, VariableKind::Local, &value, state);
            }
            LocalKind::InitElse(initializer, otherwise) => {
                let value = self.analyze_expr(initializer, state, active);
                let key = self.control_key(local.span, ControlKind::Branch);
                self.record_control(key, self.text(local.span), &value);
                let mut else_state = state.clone();
                let mut else_active = active.to_vec();
                else_active.extend(value.controls.iter().copied());
                else_active.push(key);
                self.analyze_block(otherwise, &mut else_state, &else_active);
                let mut success_value = value;
                success_value.controls.insert(key);
                self.bind_pattern(&local.pat, VariableKind::Local, &success_value, state);
            }
        }
    }

    fn analyze_expr(&mut self, expression: &Expr, state: &mut State, active: &[ControlKey]) -> Value {
        if !state.reachable {
            return Value::default();
        }
        let mut value = self.analyze_expr_kind(expression, state, active);
        value.controls.extend(active.iter().copied());
        value
    }

    fn analyze_expr_kind(
        &mut self,
        expression: &Expr,
        state: &mut State,
        active: &[ControlKey],
    ) -> Value {
        match &expression.kind {
            ExprKind::Lit(_) => {
                let mut value = Value::default();
                if let Some(literal) = self.literal_fact(expression, None, None) {
                    value.literal_facts.insert(literal);
                }
                value
            }
            ExprKind::Underscore | ExprKind::IncludedBytes(_) => Value::default(),
            ExprKind::Path(qself, path) => self.read_path(qself.is_none(), path, state),
            ExprKind::Array(expressions) | ExprKind::Tup(expressions) => {
                self.analyze_many(expressions.iter().map(|expr| expr.as_ref()), state, active)
            }
            ExprKind::Call(callee, arguments) => {
                let mut value = self.analyze_expr(callee, state, active);
                let callee_text = self.text(callee.span);
                for argument in arguments {
                    let argument_value = self.analyze_expr(argument, state, active);
                    let mut carried = argument_value.clone();
                    carried.literal_facts.clear();
                    value.merge(&carried);
                    value.literal_facts.extend(
                        argument_value.literal_facts.iter().map(|literal| {
                            let mut literal = literal.clone();
                            literal.via_unresolved_call = true;
                            if literal.compared_with.is_none() {
                                literal.compared_with = Some(callee_text.clone());
                            }
                            literal
                        }),
                    );
                }
                value
            }
            ExprKind::MethodCall(call) => {
                let mut value = self.analyze_expr(&call.receiver, state, active);
                let receiver_text = self.text(call.receiver.span);
                let mut pushed_literals = BTreeSet::new();
                for argument in &call.args {
                    let argument_value = self.analyze_expr(argument, state, active);
                    let mut related_literals = BTreeSet::new();
                    for literal in &argument_value.literal_facts {
                        let mut related = literal.clone();
                        related.via_unresolved_call = true;
                        if related.compared_with.is_none() {
                            related.compared_with = Some(receiver_text.clone());
                        }
                        related_literals.insert(related);
                    }
                    pushed_literals.extend(related_literals.iter().cloned());
                    let mut carried = argument_value.clone();
                    carried.literal_facts.clear();
                    value.merge(&carried);
                    value.literal_facts.extend(related_literals);
                }
                if let ExprKind::Path(None, path) = &call.receiver.kind
                    && path.segments.len() == 1
                {
                    let name = path.segments[0].ident.name.to_string();
                    if let Some(id) = self.lookup(&name) {
                        if state.values.len() <= id {
                            state.values.resize_with(id + 1, Value::default);
                        }
                        state.values[id].literal_facts.extend(pushed_literals);
                    }
                }
                value.unknowns.insert(format!(
                    "method `{}` is unresolved because names are not resolved",
                    call.seg.ident.name.to_string()
                ));
                value
            }
            ExprKind::Binary(operator, left, right) => {
                let left_value = self.analyze_expr(left, state, active);
                let mut value = left_value.clone();
                if matches!(operator.node, BinOpKind::And | BinOpKind::Or) {
                    let key = self.control_key(left.span, ControlKind::ShortCircuit);
                    self.record_control(key, self.text(left.span), &left_value);
                    let mut right_active = active.to_vec();
                    right_active.extend(left_value.controls.iter().copied());
                    right_active.push(key);
                    let mut right_state = state.clone();
                    value.merge(&self.analyze_expr(right, &mut right_state, &right_active));
                    state.merge(&right_state);
                } else {
                    let right_value = self.analyze_expr(right, state, active);
                    value.merge(&right_value);
                    if matches!(
                        operator.node,
                        BinOpKind::Eq
                            | BinOpKind::Lt
                            | BinOpKind::Le
                            | BinOpKind::Ne
                            | BinOpKind::Ge
                            | BinOpKind::Gt
                    ) {
                        let condition = self.text(expression.span);
                        let right_text = self.text(right.span);
                        let left_text = self.text(left.span);
                        value.literal_facts.extend(
                            left_value.literal_facts.iter().map(|literal| {
                                let mut literal = literal.clone();
                                literal.compared_with = Some(right_text.clone());
                                literal.condition = Some(condition.clone());
                                literal
                            }),
                        );
                        value.literal_facts.extend(
                            right_value.literal_facts.iter().map(|literal| {
                                let mut literal = literal.clone();
                                literal.compared_with = Some(left_text.clone());
                                literal.condition = Some(condition.clone());
                                literal
                            }),
                        );
                    }
                }
                value
            }
            ExprKind::Unary(_, inner)
            | ExprKind::Move(inner, _)
            | ExprKind::Await(inner, _)
            | ExprKind::Use(inner, _)
            | ExprKind::Paren(inner)
            | ExprKind::AddrOf(_, _, inner)
            | ExprKind::Cast(inner, _)
            | ExprKind::Type(inner, _)
            | ExprKind::Become(inner)
            | ExprKind::DirectConstArg(inner) => self.analyze_expr(inner, state, active),
            ExprKind::Field(base, _) => self.analyze_expr(base, state, active),
            ExprKind::Index(base, index, _) => {
                let mut value = self.analyze_expr(base, state, active);
                value.merge(&self.analyze_expr(index, state, active));
                value
            }
            ExprKind::Range(start, end, _) => {
                let mut value = Value::default();
                if let Some(start) = start {
                    value.merge(&self.analyze_expr(start, state, active));
                }
                if let Some(end) = end {
                    value.merge(&self.analyze_expr(end, state, active));
                }
                value
            }
            ExprKind::Repeat(element, count) => {
                let mut value = self.analyze_expr(element, state, active);
                value.merge(&self.analyze_expr(&count.value, state, active));
                value
            }
            ExprKind::Assign(place, rhs, _) => {
                self.analyze_assignment(expression.span, place, rhs, state, active, false);
                Value::default()
            }
            ExprKind::AssignOp(_, place, rhs) => {
                self.analyze_assignment(expression.span, place, rhs, state, active, true);
                Value::default()
            }
            ExprKind::If(condition, then_block, otherwise) => {
                self.analyze_if(condition, then_block, otherwise.as_deref(), state, active)
            }
            ExprKind::While(condition, body, _) => {
                self.analyze_while(expression, condition, body, state, active)
            }
            ExprKind::Loop(body, _, _) => self.analyze_loop(body, state, active),
            ExprKind::ForLoop(loop_expr) => self.analyze_for(loop_expr, state, active),
            ExprKind::Match(scrutinee, arms, _) => {
                self.analyze_match(scrutinee, arms, state, active)
            }
            ExprKind::Block(block, _) => self.analyze_block(block, state, active),
            ExprKind::Ret(returned) => {
                let value = returned
                    .as_deref()
                    .map(|value| self.analyze_expr(value, state, active))
                    .unwrap_or_default();
                let text = returned.as_deref().map(|value| self.text(value.span));
                let active_controls: BTreeSet<ControlKey> = active.iter().copied().collect();
                self.record_exit(ExitKind::Return, expression.span, text, &value, &active_controls);
                state.reachable = false;
                Value::default()
            }
            ExprKind::Try(inner) => {
                let value = self.analyze_expr(inner, state, active);
                let key = self.control_key(expression.span, ControlKind::Try);
                let condition = self.text(expression.span);
                self.record_control(key, condition, &value);
                let mut try_controls: BTreeSet<ControlKey> = value.controls.clone();
                try_controls.extend(active.iter().copied());
                try_controls.insert(key);
                self.record_exit(
                    ExitKind::Try,
                    expression.span,
                    Some(self.text(inner.span)),
                    &value,
                    &try_controls,
                );
                let mut success_value = value;
                success_value.controls.insert(key);
                success_value
            }
            ExprKind::Let(_, value, _, _) => self.analyze_expr(value, state, active),
            ExprKind::Struct(structure) => {
                let mut value = self.unresolved_path(&structure.path, "struct path");
                for field in &structure.fields {
                    value.merge(&self.analyze_expr(&field.expr, state, active));
                }
                if let ast::StructRest::Base(base) = &structure.rest {
                    value.merge(&self.analyze_expr(base, state, active));
                }
                value
            }
            ExprKind::MacCall(_) => {
                self.opaque_macro(expression.span, state);
                unknown_value("macro expansion is opaque".to_string())
            }
            ExprKind::Break(label, value) => {
                let value = value
                    .as_deref()
                    .map(|value| self.analyze_expr(value, state, active))
                    .unwrap_or_default();
                if let Some(label) = label {
                    self.record_unknown(
                        expression.span,
                        format!("labeled break `{}` target is unresolved", label.ident.name.to_string()),
                    );
                } else if let Some(flow) = self.loops.last_mut() {
                    flow.merge_break(state, &value);
                } else {
                    self.record_unknown(expression.span, "break target is unresolved".to_string());
                }
                state.reachable = false;
                Value::default()
            }
            ExprKind::Continue(label) => {
                if let Some(label) = label {
                    self.record_unknown(
                        expression.span,
                        format!("labeled continue `{}` target is unresolved", label.ident.name.to_string()),
                    );
                } else if let Some(flow) = self.loops.last_mut() {
                    flow.merge_continue(state);
                } else {
                    self.record_unknown(expression.span, "continue target is unresolved".to_string());
                }
                state.reachable = false;
                Value::default()
            }
            ExprKind::TryBlock(_, _) | ExprKind::Closure(_) | ExprKind::Gen(_, _, _, _) => {
                self.record_unknown(expression.span, "nested function body is not analyzed".to_string());
                unknown_value("nested function body is not analyzed".to_string())
            }
            ExprKind::InlineAsm(_) | ExprKind::OffsetOf(_, _) | ExprKind::Yield(_) => {
                self.record_unknown(expression.span, "expression operands are not represented as local reads".to_string());
                unknown_value("expression operands are not represented as local reads".to_string())
            }
            ExprKind::Yeet(value) => {
                let mut result = value
                    .as_deref()
                    .map(|value| self.analyze_expr(value, state, active))
                    .unwrap_or_default();
                result.unknowns.insert("yeet exit semantics are not analyzed".to_string());
                self.record_unknown(expression.span, "yeet exit semantics are not analyzed".to_string());
                result
            }
            ExprKind::FormatArgs(_) | ExprKind::ConstBlock(_) | ExprKind::UnsafeBinderCast(_, _, _) => {
                self.record_unknown(expression.span, "expression form is not modeled".to_string());
                unknown_value("expression form is not modeled".to_string())
            }
            ExprKind::Err(_) | ExprKind::Dummy => {
                self.record_unknown(expression.span, "parser did not produce a complete expression".to_string());
                unknown_value("parser did not produce a complete expression".to_string())
            }
        }
    }

    fn literal_fact(
        &self,
        expression: &Expr,
        compared_with: Option<String>,
        condition: Option<String>,
    ) -> Option<LiteralFact> {
        let ExprKind::Lit(token) = &expression.kind else {
            return None;
        };
        let kind = ast::LitKind::from_token_lit(*token).ok()?;
        let (kind, literal) = match kind {
            ast::LitKind::Char(value) => (LiteralKind::Char, value.to_string()),
            ast::LitKind::Byte(value) => (LiteralKind::Byte, value.to_string()),
            ast::LitKind::Str(value, _) => (LiteralKind::Str, value.as_str().to_string()),
            ast::LitKind::Int(value, _) => (LiteralKind::Int, value.to_string()),
            _ => return None,
        };
        Some(LiteralFact {
            literal,
            text: self.text(expression.span),
            kind,
            span: self.range(expression.span),
            compared_with,
            condition,
            via_unresolved_call: false,
        })
    }

    fn pattern_literal_facts(
        &self,
        pattern: &Pat,
        compared_with: &str,
        condition: &str,
    ) -> BTreeSet<LiteralFact> {
        let mut facts = BTreeSet::new();
        match &pattern.kind {
            PatKind::Expr(expression) => {
                if let Some(fact) = self.literal_fact(
                    expression,
                    Some(compared_with.to_string()),
                    Some(condition.to_string()),
                ) {
                    facts.insert(fact);
                }
            }
            PatKind::Range(start, end, _) => {
                for expression in [start.as_deref(), end.as_deref()].into_iter().flatten() {
                    if let Some(fact) = self.literal_fact(
                        expression,
                        Some(compared_with.to_string()),
                        Some(condition.to_string()),
                    ) {
                        facts.insert(fact);
                    }
                }
            }
            PatKind::Tuple(patterns)
            | PatKind::TupleStruct(_, _, patterns)
            | PatKind::Or(patterns)
            | PatKind::Slice(patterns) => {
                for nested in patterns {
                    facts.extend(self.pattern_literal_facts(nested, compared_with, condition));
                }
            }
            PatKind::Struct(_, _, fields, _) => {
                for field in fields {
                    facts.extend(self.pattern_literal_facts(&field.pat, compared_with, condition));
                }
            }
            PatKind::Deref(nested)
            | PatKind::Ref(nested, _, _)
            | PatKind::Paren(nested)
            | PatKind::Guard(nested, _) => {
                facts.extend(self.pattern_literal_facts(nested, compared_with, condition));
            }
            PatKind::Ident(_, _, Some(nested)) => {
                facts.extend(self.pattern_literal_facts(nested, compared_with, condition));
            }
            PatKind::Missing
            | PatKind::Wild
            | PatKind::Ident(_, _, None)
            | PatKind::Path(_, _)
            | PatKind::MacCall(_)
            | PatKind::Never
            | PatKind::Rest
            | PatKind::Err(_) => {}
        }
        facts
    }

    fn analyze_many<'b>(
        &mut self,
        expressions: impl Iterator<Item = &'b Expr>,
        state: &mut State,
        active: &[ControlKey],
    ) -> Value {
        let mut value = Value::default();
        for expression in expressions {
            value.merge(&self.analyze_expr(expression, state, active));
        }
        value
    }

    fn analyze_if(
        &mut self,
        condition: &Expr,
        then_block: &Block,
        otherwise: Option<&Expr>,
        state: &mut State,
        active: &[ControlKey],
    ) -> Value {
        let (condition_value, let_pattern) = match &condition.kind {
            ExprKind::Let(pattern, value, _, _) => {
                (self.analyze_expr(value, state, active), Some(pattern.as_ref()))
            }
            _ => (self.analyze_expr(condition, state, active), None),
        };
        let key = self.control_key(condition.span, ControlKind::Branch);
        self.record_control(key, self.text(condition.span), &condition_value);
        let mut branch_active = active.to_vec();
        branch_active.extend(condition_value.controls.iter().copied());
        branch_active.push(key);

        let mut then_state = state.clone();
        if let Some(pattern) = let_pattern {
            self.scopes.push(BTreeMap::new());
            self.bind_pattern(pattern, VariableKind::Local, &condition_value, &mut then_state);
        }
        let then_value = self.analyze_block(then_block, &mut then_state, &branch_active);
        if let Some(_) = let_pattern {
            self.scopes.pop();
        }

        let mut else_state = state.clone();
        let else_value = if let Some(otherwise) = otherwise {
            self.analyze_expr(otherwise, &mut else_state, &branch_active)
        } else {
            Value::default()
        };
        then_state.merge(&else_state);
        *state = then_state;
        let mut value = then_value;
        value.merge(&else_value);
        value
    }

    fn analyze_while(
        &mut self,
        expression: &Expr,
        condition: &Expr,
        body: &Block,
        state: &mut State,
        active: &[ControlKey],
    ) -> Value {
        let entry = state.clone();
        let mut head = entry.clone();
        let mut after_loop: Option<State> = None;
        let mut break_value = Value::default();
        let loop_key = self.control_key(expression.span, ControlKind::Loop);
        let condition_text = self.text(condition.span);
        let mut condition_state = entry.clone();
        loop {
            let mut test_state = head.clone();
            let condition_value = match &condition.kind {
                ExprKind::Let(_, value, _, _) => self.analyze_expr(value, &mut test_state, active),
                _ => self.analyze_expr(condition, &mut test_state, active),
            };
            self.record_control(loop_key, condition_text.clone(), &condition_value);
            let mut body_active = active.to_vec();
            body_active.extend(condition_value.controls.iter().copied());
            body_active.push(loop_key);
            let mut body_state = test_state.clone();
            self.loops.push(LoopFlow::default());
            if let ExprKind::Let(pattern, _, _, _) = &condition.kind {
                self.scopes.push(BTreeMap::new());
                self.bind_pattern(pattern, VariableKind::Local, &condition_value, &mut body_state);
            }
            self.analyze_block(body, &mut body_state, &body_active);
            if matches!(&condition.kind, ExprKind::Let(_, _, _, _)) {
                self.scopes.pop();
            }
            let flow = self.loops.pop().unwrap_or_default();
            condition_state = test_state;
            if let Some(breaks) = flow.breaks {
                match &mut after_loop {
                    Some(existing) => existing.merge(&breaks),
                    None => after_loop = Some(breaks),
                }
                break_value.merge(&flow.break_value);
            }
            let mut backedge = body_state;
            if let Some(continues) = flow.continues {
                backedge.merge(&continues);
            }
            let mut next = entry.clone();
            if backedge.reachable {
                next.merge(&backedge);
            }
            if next == head {
                break;
            }
            head = next;
        }
        match &mut after_loop {
            Some(existing) => existing.merge(&condition_state),
            None => after_loop = Some(condition_state),
        }
        *state = after_loop.unwrap_or_else(|| State::dead_like(&head));
        break_value
    }

    fn analyze_loop(&mut self, body: &Block, state: &mut State, active: &[ControlKey]) -> Value {
        let entry = state.clone();
        let mut head = entry.clone();
        let mut breaks: Option<State> = None;
        let mut break_value = Value::default();
        loop {
            let mut body_state = head.clone();
            self.loops.push(LoopFlow::default());
            self.analyze_block(body, &mut body_state, active);
            let flow = self.loops.pop().unwrap_or_default();
            if let Some(found) = flow.breaks {
                match &mut breaks {
                    Some(existing) => existing.merge(&found),
                    None => breaks = Some(found),
                }
                break_value.merge(&flow.break_value);
            }
            let mut backedge = body_state;
            if let Some(continues) = flow.continues {
                backedge.merge(&continues);
            }
            let mut next = entry.clone();
            if backedge.reachable {
                next.merge(&backedge);
            }
            if next == head {
                break;
            }
            head = next;
        }
        *state = breaks.unwrap_or_else(|| State::dead_like(&head));
        break_value
    }

    fn analyze_for(
        &mut self,
        loop_expr: &ast::ForLoop,
        state: &mut State,
        active: &[ControlKey],
    ) -> Value {
        let iter_value = self.analyze_expr(&loop_expr.iter, state, active);
        let entry = state.clone();
        let mut head = entry.clone();
        let key = self.control_key(loop_expr.iter.span, ControlKind::Loop);
        self.record_control(
            key,
            format!("for {} in {}", self.text(loop_expr.pat.span), self.text(loop_expr.iter.span)),
            &iter_value,
        );
        let mut after_loop = Some(entry.clone());
        let mut break_value = Value::default();
        loop {
            let mut body_state = head.clone();
            self.scopes.push(BTreeMap::new());
            self.bind_pattern(&loop_expr.pat, VariableKind::Local, &iter_value, &mut body_state);
            let mut body_active = active.to_vec();
            body_active.extend(iter_value.controls.iter().copied());
            body_active.push(key);
            self.loops.push(LoopFlow::default());
            self.analyze_block(&loop_expr.body, &mut body_state, &body_active);
            let flow = self.loops.pop().unwrap_or_default();
            self.scopes.pop();
            if let Some(found) = flow.breaks {
                match &mut after_loop {
                    Some(existing) => existing.merge(&found),
                    None => after_loop = Some(found),
                }
                break_value.merge(&flow.break_value);
            }
            let mut backedge = body_state;
            if let Some(continues) = flow.continues {
                backedge.merge(&continues);
            }
            let mut next = entry.clone();
            if backedge.reachable {
                next.merge(&backedge);
            }
            if next == head {
                break;
            }
            head = next;
        }
        if let Some(existing) = &mut after_loop {
            existing.merge(&head);
        }
        *state = after_loop.unwrap_or_else(|| State::dead_like(&head));
        break_value
    }

    fn analyze_match(
        &mut self,
        scrutinee: &Expr,
        arms: &[Arm],
        state: &mut State,
        active: &[ControlKey],
    ) -> Value {
        let scrutinee_value = self.analyze_expr(scrutinee, state, active);
        let base = state.clone();
        let mut joined: Option<State> = None;
        let mut result = Value::default();
        for arm in arms {
            let key = self.control_key(arm.span, ControlKind::Match);
            let condition = format!(
                "{} matches {}",
                self.text(scrutinee.span),
                self.text(arm.pat.span)
            );
            let mut control_value = scrutinee_value.clone();
            control_value.literal_facts.extend(self.pattern_literal_facts(
                &arm.pat,
                &self.text(scrutinee.span),
                &condition,
            ));
            self.record_control(key, condition, &control_value);
            let mut arm_state = base.clone();
            self.scopes.push(BTreeMap::new());
            self.bind_pattern(&arm.pat, VariableKind::Local, &scrutinee_value, &mut arm_state);
            let mut arm_active = active.to_vec();
            arm_active.extend(scrutinee_value.controls.iter().copied());
            arm_active.push(key);
            if let Some(guard) = &arm.guard {
                let guard_value = self.analyze_expr(&guard.cond, &mut arm_state, &arm_active);
                let guard_key = self.control_key(guard.cond.span, ControlKind::Branch);
                self.record_control(guard_key, self.text(guard.cond.span), &guard_value);
                arm_active.extend(guard_value.controls.iter().copied());
                arm_active.push(guard_key);
            }
            if let Some(body) = &arm.body {
                result.merge(&self.analyze_expr(body, &mut arm_state, &arm_active));
            }
            self.scopes.pop();
            match &mut joined {
                Some(existing) => existing.merge(&arm_state),
                None => joined = Some(arm_state),
            }
        }
        *state = joined.unwrap_or(base);
        result
    }

    fn analyze_assignment(
        &mut self,
        assignment_span: Span,
        place: &Expr,
        rhs: &Expr,
        state: &mut State,
        active: &[ControlKey],
        compound: bool,
    ) {
        let (target, address, direct) = self.assignment_target(place, state, active);
        let mut value = self.analyze_expr(rhs, state, active);
        value.merge(&address);
        if let Some(id) = target {
            if compound || !direct {
                value.merge(&self.read_binding(id, state, self.range(place.span)));
            }
            value.controls.extend(active.iter().copied());
            self.store_binding(id, value, state, self.range(assignment_span));
        } else {
            value.unknowns.insert("assignment target is unresolved".to_string());
            self.record_unknown(place.span, "assignment target is unresolved".to_string());
        }
    }

    fn assignment_target(
        &mut self,
        place: &Expr,
        state: &mut State,
        active: &[ControlKey],
    ) -> (Option<usize>, Value, bool) {
        match &place.kind {
            ExprKind::Path(None, path) if path.segments.len() == 1 => {
                let name = path.segments[0].ident.name.to_string();
                if let Some(id) = self.lookup(&name) {
                    (Some(id), Value::default(), true)
                } else {
                    let mut value = Value::default();
                    value.unknowns.insert(format!("path `{name}` is unresolved"));
                    (None, value, true)
                }
            }
            ExprKind::Field(base, _) => {
                let (target, mut value, _) = self.assignment_target(base, state, active);
                if let Some(id) = target {
                    value.merge(&self.read_binding(id, state, self.range(base.span)));
                }
                (target, value, false)
            }
            ExprKind::Index(base, index, _) => {
                let (target, mut value, _) = self.assignment_target(base, state, active);
                value.merge(&self.analyze_expr(index, state, active));
                if let Some(id) = target {
                    value.merge(&self.read_binding(id, state, self.range(base.span)));
                }
                (target, value, false)
            }
            ExprKind::Paren(inner) => self.assignment_target(inner, state, active),
            ExprKind::Unary(ast::UnOp::Deref, pointer) => {
                let mut value = self.analyze_expr(pointer, state, active);
                value.unknowns.insert("assignment through a dereference has an unresolved target".to_string());
                self.record_unknown(place.span, "assignment through a dereference has an unresolved target".to_string());
                (None, value, false)
            }
            _ => {
                let mut value = self.analyze_expr(place, state, active);
                value.unknowns.insert("assignment target form is not modeled".to_string());
                self.record_unknown(place.span, "assignment target form is not modeled".to_string());
                (None, value, false)
            }
        }
    }

    fn store_binding(
        &mut self,
        id: usize,
        value: Value,
        state: &mut State,
        assignment_site: TextRange,
    ) {
        if state.values.len() <= id {
            state.values.resize_with(id + 1, Value::default);
        }
        state.values[id] = value;
        self.assignment_sites.entry(id).or_default().insert(assignment_site);
        if self.bindings.get(id).is_some_and(|binding| binding.mutable) {
            self.assigned.insert(id);
        }
    }

    fn read_path(&self, unqualified: bool, path: &ast::Path, state: &State) -> Value {
        if unqualified && path.segments.len() == 1 {
            let name = path.segments[0].ident.name.to_string();
            if let Some(id) = self.lookup(&name) {
                return self.read_binding(id, state, self.range(path.span));
            }
        }
        let path_text = self.text(path.span);
        unknown_value(format!("path `{path_text}` is unresolved"))
    }

    fn unresolved_path(&self, path: &ast::Path, description: &str) -> Value {
        let path_text = self.text(path.span);
        unknown_value(format!("{description} `{path_text}` is unresolved"))
    }

    fn read_binding(&self, id: usize, state: &State, occurrence: TextRange) -> Value {
        let mut value = Value::default();
        value.data.entry(id).or_default().insert(occurrence);
        if let Some(stored) = state.values.get(id) {
            value.merge(stored);
            for dependency in stored.data.keys() {
                value.data.entry(*dependency).or_default().insert(occurrence);
            }
        }
        value
    }

    fn bind_pattern(
        &mut self,
        pattern: &Pat,
        kind: VariableKind,
        value: &Value,
        state: &mut State,
    ) {
        match &pattern.kind {
            PatKind::Ident(mode, ident, subpattern) => {
                let name = ident.name.to_string();
                let range = self.range(ident.span);
                let key = BindingKey { start: range.start, end: range.end, name: name.clone() };
                let id = if let Some(id) = self.binding_ids.get(&key) {
                    *id
                } else {
                    let id = self.bindings.len();
                    self.bindings.push(BindingInfo {
                        variable: Variable { name: name.clone(), kind, span: range },
                        mutable: mode.1 == Mutability::Mut,
                    });
                    self.binding_ids.insert(key, id);
                    id
                };
                let uppercase = name.chars().next().is_some_and(char::is_uppercase);
                if let Some(scope) = self.scopes.last_mut() {
                    scope.insert(name, id);
                }
                if state.values.len() <= id {
                    state.values.resize_with(id + 1, Value::default);
                }
                let mut binding_value = value.clone();
                // A bare identifier pattern is a binding unless it resolves to a constant, unit
                // struct or unit variant. Those are uppercase by the language's naming lints
                // (`non_upper_case_globals`, `non_camel_case_types`), so only an uppercase name
                // stays unresolved here.
                if kind == VariableKind::Local
                    && *mode == ast::BindingMode::NONE
                    && subpattern.is_none()
                    && uppercase
                {
                    let why = "bare identifier pattern resolution is unavailable".to_string();
                    binding_value.unknowns.insert(why.clone());
                    self.record_unknown(pattern.span, why);
                }
                state.values[id] = binding_value.clone();
                if let Some(subpattern) = subpattern {
                    self.bind_pattern(subpattern, kind, &binding_value, state);
                }
            }
            PatKind::Tuple(patterns) | PatKind::Slice(patterns) => {
                for pattern in patterns {
                    self.bind_pattern(pattern, kind, value, state);
                }
            }
            PatKind::TupleStruct(_, _, patterns) => {
                for pattern in patterns {
                    self.bind_pattern(pattern, kind, value, state);
                }
            }
            PatKind::Struct(_, _, fields, _) => {
                for field in fields {
                    self.bind_pattern(&field.pat, kind, value, state);
                }
            }
            PatKind::Or(patterns) => {
                for pattern in patterns {
                    self.bind_pattern(pattern, kind, value, state);
                }
            }
            PatKind::Deref(pattern) | PatKind::Ref(pattern, _, _) | PatKind::Paren(pattern) => {
                self.bind_pattern(pattern, kind, value, state);
            }
            PatKind::Path(_, path) => {
                self.record_unknown(pattern.span, format!("pattern path `{}` is unresolved", self.text(path.span)));
            }
            PatKind::MacCall(_) => {
                self.record_unknown(pattern.span, "pattern macro expansion is opaque".to_string());
                self.mark_macro_assignments(pattern.span);
            }
            PatKind::Expr(expression) => {
                if self.literal_fact(expression, None, None).is_none() {
                    self.record_unknown(expression.span, "constant pattern resolution is unavailable".to_string());
                }
            }
            PatKind::Range(_, _, _) => {}
            PatKind::Guard(pattern, _) => self.bind_pattern(pattern, kind, value, state),
            PatKind::Missing | PatKind::Wild | PatKind::Rest | PatKind::Never | PatKind::Err(_) => {}
        }
    }

    fn lookup(&self, name: &str) -> Option<usize> {
        self.scopes.iter().rev().find_map(|scope| scope.get(name).copied())
    }

    fn opaque_macro(&mut self, span: Span, state: &mut State) {
        self.record_unknown(span, "macro expansion is opaque".to_string());
        let visible: BTreeSet<usize> = self.scopes.iter().flat_map(|scope| scope.values().copied()).collect();
        for id in visible {
            if self.bindings.get(id).is_some_and(|binding| binding.mutable) {
                let reason = "an opaque macro may assign this binding".to_string();
                if !self.assigned.contains(&id) {
                    self.assignment_unknowns.entry(id).or_default().insert(reason.clone());
                }
                if state.values.len() <= id {
                    state.values.resize_with(id + 1, Value::default);
                }
                state.values[id].unknowns.insert(reason);
            }
        }
    }

    fn mark_macro_assignments(&mut self, span: Span) {
        self.record_unknown(span, "macro expansion is opaque".to_string());
        let visible: BTreeSet<usize> = self.scopes.iter().flat_map(|scope| scope.values().copied()).collect();
        for id in visible {
            if self.bindings.get(id).is_some_and(|binding| binding.mutable)
                && !self.assigned.contains(&id)
            {
                self.assignment_unknowns
                    .entry(id)
                    .or_default()
                    .insert("an opaque macro may assign this binding".to_string());
            }
        }
    }

    fn range(&self, span: Span) -> TextRange {
        TextRange { start: self.at(span.lo()), end: self.at(span.hi()) }
    }

    fn at(&self, pos: BytePos) -> u32 {
        if pos < self.file.start_pos {
            return 0;
        }
        self.file.original_relative_byte_pos(pos).0
    }

    fn text(&self, span: Span) -> String {
        let range = self.range(span);
        self.source
            .get(range.start as usize..range.end as usize)
            .unwrap_or("")
            .to_string()
    }
}
