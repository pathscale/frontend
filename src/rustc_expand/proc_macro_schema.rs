use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use eko::path::{Path, PathBuf};
use rustc_macros::{BlobDecodable, Encodable};

use crate::rustc_lexer::{self, FrontmatterAllowed, TokenKind};
use crate::rustc_ast::tokenstream::TokenStream;
use crate::rustc_session::parse::ParseSess;
use crate::rustc_span::FileName;

#[derive(Clone, Debug, PartialEq, Eq, Encodable, BlobDecodable)]
pub enum ExactPart {
    InputTokens,
    ItemIdent,
    Vis,
    Generics { kind: String },
    FieldIdent,
    NameTemplate { prefix: String, suffix: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Encodable, BlobDecodable)]
pub enum SchemaHole {
    Exact(ExactPart),
    Opaque { why: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Encodable, BlobDecodable)]
pub enum SchemaGuard {
    AttributeAbsent { path: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Encodable, BlobDecodable)]
pub struct SchemaTokenTree {
    pub source: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Encodable, BlobDecodable)]
pub struct SchemaRule {
    pub class: Option<String>,
    pub guard: Option<SchemaGuard>,
    pub rhs: SchemaTokenTree,
    pub holes: Vec<SchemaHole>,
    pub(crate) hole_sources: Vec<Option<SchemaBindingSource>>,
    pub(crate) parameters: Vec<String>,
    pub repetition: bool,
    pub(crate) loop_repetition: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encodable, BlobDecodable)]
pub(crate) enum SchemaBindingSource {
    Item,
    Field,
    Parameter(usize),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SchemaVariable {
    pub name: String,
    pub hole: SchemaHole,
    pub source: Option<SchemaBindingSource>,
    pub repeated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RewrittenSchemaTemplate {
    pub source: String,
    pub variables: Vec<SchemaVariable>,
}

#[derive(Clone, Debug, PartialEq, Eq, Encodable, BlobDecodable)]
pub enum ProcMacroSchema {
    Unclassified { why: String },
    Template { rules: Vec<SchemaRule> },
}

impl Default for ProcMacroSchema {
    fn default() -> Self {
        Self::Unclassified { why: "proc macro body is outside the schema grammar".to_string() }
    }
}

#[derive(Clone)]
struct Lexeme {
    kind: TokenKind,
    text: String,
    start: usize,
    end: usize,
}

fn lex(source: &str) -> Vec<Lexeme> {
    let mut offset = 0usize;
    rustc_lexer::tokenize(source, FrontmatterAllowed::No)
        .filter_map(|token| {
            let start = offset;
            offset += token.len as usize;
            match token.kind {
                TokenKind::Whitespace
                | TokenKind::LineComment { .. }
                | TokenKind::BlockComment { .. } => None,
                kind => Some(Lexeme {
                    kind,
                    text: source[start..offset].to_string(),
                    start,
                    end: offset,
                }),
            }
        })
        .collect()
}

pub(crate) fn remove_generic_default(source: &str) -> String {
    let lexemes = lex(source);
    let mut angle_depth = 0usize;
    let mut index = 0usize;
    while index < lexemes.len() {
        let lexeme = &lexemes[index];
        if closing_delimiter(lexeme.kind).is_some()
            && let Some(close) = matching_delimiter(&lexemes, index)
        {
            index = close + 1;
            continue;
        }
        if lexeme.text == "<" {
            angle_depth += 1;
        } else if !lexeme.text.is_empty() && lexeme.text.chars().all(|ch| ch == '>') {
            angle_depth = angle_depth.saturating_sub(lexeme.text.len());
        } else if lexeme.text == "=" && angle_depth == 0 {
            let prefix = source[..lexeme.start].trim_end();
            return prefix.strip_suffix('=').unwrap_or(prefix).trim_end().to_string();
        }
        index += 1;
    }
    source.to_string()
}

pub(crate) fn generic_parameter_sources(source: &str) -> Option<Vec<String>> {
    let lexemes = lex(source);
    let first = lexemes.first()?;
    if first.text != "<" {
        return None;
    }

    let mut angle_depth = 1usize;
    let mut start = first.end;
    let mut parameters = Vec::new();
    let mut index = 1usize;
    while index < lexemes.len() {
        let lexeme = &lexemes[index];
        if closing_delimiter(lexeme.kind).is_some()
            && let Some(close) = matching_delimiter(&lexemes, index)
        {
            index = close + 1;
            continue;
        }
        if lexeme.text == "<" {
            angle_depth += 1;
        } else if !lexeme.text.is_empty() && lexeme.text.chars().all(|ch| ch == '>') {
            let closes = lexeme.text.len();
            if closes >= angle_depth {
                let parameter = source[start..lexeme.start].trim();
                if !parameter.is_empty() {
                    parameters.push(parameter.to_string());
                }
                return (closes == angle_depth).then_some(parameters);
            }
            angle_depth -= closes;
        } else if lexeme.text == "," && angle_depth == 1 {
            let parameter = source[start..lexeme.start].trim();
            if parameter.is_empty() {
                return None;
            }
            parameters.push(parameter.to_string());
            start = lexeme.end;
        }
        index += 1;
    }
    None
}

fn closing_delimiter(kind: TokenKind) -> Option<TokenKind> {
    match kind {
        TokenKind::OpenParen => Some(TokenKind::CloseParen),
        TokenKind::OpenBrace => Some(TokenKind::CloseBrace),
        TokenKind::OpenBracket => Some(TokenKind::CloseBracket),
        _ => None,
    }
}

fn matching_delimiter(lexemes: &[Lexeme], open: usize) -> Option<usize> {
    let mut stack = vec![closing_delimiter(lexemes.get(open)?.kind)?];
    for (index, lexeme) in lexemes.iter().enumerate().skip(open + 1) {
        if let Some(close) = closing_delimiter(lexeme.kind) {
            stack.push(close);
        } else if matches!(
            lexeme.kind,
            TokenKind::CloseParen | TokenKind::CloseBrace | TokenKind::CloseBracket
        ) {
            if stack.pop() != Some(lexeme.kind) {
                return None;
            }
            if stack.is_empty() {
                return Some(index);
            }
        }
    }
    None
}

fn quote_invocations(lexemes: &[Lexeme]) -> Vec<(usize, usize, usize)> {
    let mut quotes = Vec::new();
    for index in 0..lexemes.len().saturating_sub(2) {
        if lexemes[index].text == "quote"
            && lexemes[index + 1].kind == TokenKind::Bang
            && closing_delimiter(lexemes[index + 2].kind).is_some()
            && let Some(close) = matching_delimiter(lexemes, index + 2)
        {
            quotes.push((index + 2, close, index));
        }
    }
    quotes
}

fn inside_quote(index: usize, quotes: &[(usize, usize, usize)]) -> bool {
    quotes.iter().any(|(open, close, _)| *open < index && index < *close)
}

fn loop_bounds(lexemes: &[Lexeme], quote_open: usize, quote_close: usize) -> Option<(usize, usize)> {
    let mut found = None;
    for index in 0..lexemes.len() {
        if lexemes[index].text != "for" || index >= quote_open {
            continue;
        }
        let Some(open) = (index + 1..quote_open)
            .find(|candidate| lexemes[*candidate].kind == TokenKind::OpenBrace)
        else {
            continue;
        };
        let Some(close) = matching_delimiter(lexemes, open) else { continue };
        if open < quote_open && quote_close < close {
            if found.is_some() {
                return None;
            }
            found = Some((open, close));
        }
    }
    found
}

fn has_token_sequence(source: &str, expected: &[&str]) -> bool {
    let lexemes = lex(source);
    lexemes.windows(expected.len()).any(|window| {
        window.iter().zip(expected).all(|(lexeme, expected)| lexeme.text == *expected)
    })
}


#[derive(Clone, Copy, PartialEq, Eq)]
enum ProjectionBase {
    Item,
    Field,
}

#[derive(Clone)]
struct ProjectionBinding {
    hole: SchemaHole,
    base: Option<ProjectionBase>,
    parameter: Option<usize>,
}

fn binding(bindings: &[(String, ProjectionBinding)], name: &str) -> Option<ProjectionBinding> {
    bindings
        .iter()
        .rev()
        .find(|(bound, _)| bound == name)
        .map(|(_, value)| value.clone())
}

fn projection_base(
    name: &str,
    item_parameter: Option<&str>,
    field_parameters: &[String],
    bindings: &[(String, ProjectionBinding)],
) -> Option<ProjectionBase> {
    if item_parameter == Some(name) {
        return Some(ProjectionBase::Item);
    }
    if field_parameters.iter().any(|field| field == name) {
        return Some(ProjectionBase::Field);
    }
    binding(bindings, name).and_then(|binding| binding.base)
}

fn exact_projection(
    base: &str,
    member: &str,
    item_parameter: Option<&str>,
    field_parameters: &[String],
    bindings: &[(String, ProjectionBinding)],
    syn_source: bool,
) -> Option<ExactPart> {
    let origin = projection_base(base, item_parameter, field_parameters, bindings)?;
    if !syn_source {
        return None;
    }
    match (origin, member) {
        (ProjectionBase::Item, "ident") => Some(ExactPart::ItemIdent),
        (ProjectionBase::Item, "vis") => Some(ExactPart::Vis),
        (ProjectionBase::Field, "ident") => Some(ExactPart::FieldIdent),
        _ => None,
    }
}

fn projection_why(
    expression: &[Lexeme],
    item_parameter: Option<&str>,
    field_parameters: &[String],
    bindings: &[(String, ProjectionBinding)],
    syn_source: bool,
) -> Option<&'static str> {
    let expression = projection_expression(expression);
    let [base, dot, member] = expression else { return None };
    if dot.text != "." || projection_base(&base.text, item_parameter, field_parameters, bindings).is_none() {
        return None;
    }
    match member.text.as_str() {
        "ident" | "vis" if !syn_source => Some("syn source for Ident projections is not indexed"),
        "generics" => Some("Generics projection requires split_for_impl"),
        _ => None,
    }
}

fn split_for_impl_on_generics(
    expression: &[Lexeme],
    item_parameter: Option<&str>,
    field_parameters: &[String],
    bindings: &[(String, ProjectionBinding)],
) -> bool {
    let expression = projection_expression(expression);
    let base = match expression {
        [base, dot, generics, second_dot, method, open, close]
            if dot.text == "."
                && generics.text == "generics"
                && second_dot.text == "."
                && method.text == "split_for_impl"
                && open.text == "("
                && close.text == ")" => Some(base.text.as_str()),
        [base, dot, method, open, close]
            if dot.text == "."
                && method.text == "split_for_impl"
                && open.text == "("
                && close.text == ")" => Some(base.text.as_str()),
        _ => None,
    };
    let Some(base) = base else { return false };
    if item_parameter == Some(base) {
        return true;
    }
    binding(bindings, base).is_some_and(|binding| {
        binding.base == Some(ProjectionBase::Item)
            && matches!(
                binding.hole,
                SchemaHole::Opaque { why } if why == "Generics projection requires split_for_impl"
            )
    })
}

fn direct_projection(
    expression: &[Lexeme],
    item_parameter: Option<&str>,
    field_parameters: &[String],
    bindings: &[(String, ProjectionBinding)],
    syn_source: bool,
) -> Option<ExactPart> {
    let expression = projection_expression(expression);
    let [base, dot, member] = expression else { return None };
    if dot.text != "." {
        return None;
    }
    exact_projection(
        &base.text,
        &member.text,
        item_parameter,
        field_parameters,
        bindings,
        syn_source,
    )
}

fn projection_expression(expression: &[Lexeme]) -> &[Lexeme] {
    let mut expression = expression;
    while expression.first().is_some_and(|lexeme| lexeme.text == "&") {
        expression = &expression[1..];
        if expression.first().is_some_and(|lexeme| lexeme.text == "mut") {
            expression = &expression[1..];
        }
    }
    expression
}

fn source_literal(source: &str) -> Option<&str> {
    if source.starts_with('"') && source.ends_with('"') && source.len() >= 2 {
        let inner = &source[1..source.len() - 1];
        return (!inner.contains('\\')).then_some(inner);
    }

    let raw = source.strip_prefix('r')?;
    let quote = raw.find('"')?;
    let hashes = &raw[..quote];
    if !hashes.bytes().all(|byte| byte == b'#') {
        return None;
    }
    let suffix_len = hashes.len() + 1;
    let suffix = alloc::format!("\"{hashes}");
    if raw.len() < quote + 1 + suffix_len || !raw.ends_with(suffix.as_str()) {
        return None;
    }
    Some(&raw[quote + 1..raw.len() - suffix_len])
}

fn template_parts(literal: &str) -> Option<(String, String, bool)> {
    let value = source_literal(literal)?;
    let Some(open) = value.find('{') else {
        return (!value.contains('}')).then(|| (value.to_string(), String::new(), false));
    };
    if value.get(open..open + 2) != Some("{}") || value[open + 2..].chars().any(|ch| matches!(ch, '{' | '}')) {
        return None;
    }
    Some((value[..open].to_string(), value[open + 2..].to_string(), true))
}

fn format_ident_projection(
    lexemes: &[Lexeme],
    start: usize,
    end: usize,
    item_parameter: Option<&str>,
    field_parameters: &[String],
    bindings: &[(String, ProjectionBinding)],
    syn_source: bool,
) -> Result<(ExactPart, Option<ProjectionBase>), String> {
    let Some(macro_index) = (start..end).find(|index| lexemes[*index].text == "format_ident") else {
        return Err("format_ident! name template is not classified".to_string());
    };
    let Some(_) = lexemes.get(macro_index + 2).filter(|open| open.kind == TokenKind::OpenParen) else {
        return Err("format_ident! invocation is not classified".to_string());
    };
    let open_index = macro_index + 2;
    let Some(close_index) = matching_delimiter(lexemes, open_index) else {
        return Err("format_ident! invocation is not classified".to_string());
    };
    if close_index >= end {
        return Err("format_ident! invocation is not classified".to_string());
    }
    let arguments = &lexemes[open_index + 1..close_index];
    let Some(literal) = arguments.first().filter(|argument| matches!(argument.kind, TokenKind::Literal { .. })) else {
        return Err("format_ident! requires a bare literal template".to_string());
    };
    let Some((prefix, suffix, has_hole)) = template_parts(&literal.text) else {
        return Err("format_ident! requires bare `{}` holes".to_string());
    };
    let rest = &arguments[1..];
    let ident_argument = if rest.is_empty() {
        None
    } else if rest.first().is_some_and(|argument| argument.text == ",") {
        let mut expression = &rest[1..];
        if expression.last().is_some_and(|last| last.text == ",") {
            expression = &expression[..expression.len() - 1];
        }
        Some(expression)
    } else {
        return Err("format_ident! arguments are not classified".to_string());
    };

    let argument_is_ident = if let Some(expression) = ident_argument {
        if expression.len() == 1 {
            binding(bindings, &expression[0].text).is_some_and(|binding| {
                matches!(binding.hole, SchemaHole::Exact(ExactPart::ItemIdent | ExactPart::FieldIdent))
            })
        } else {
            direct_projection(
                expression,
                item_parameter,
                field_parameters,
                bindings,
                syn_source,
            )
            .is_some_and(|part| matches!(part, ExactPart::ItemIdent | ExactPart::FieldIdent))
        }
    } else {
        false
    };
    if has_hole != ident_argument.is_some() || (has_hole && !argument_is_ident) {
        return Err("format_ident! argument is not an exact Ident projection".to_string());
    }
    let base = ident_argument.and_then(|expression| {
        if expression.len() == 1 {
            binding(bindings, &expression[0].text).and_then(|binding| binding.base)
        } else {
            expression.first().and_then(|first| {
                projection_base(&first.text, item_parameter, field_parameters, bindings)
            })
        }
    });
    Ok((ExactPart::NameTemplate { prefix, suffix }, base))
}

fn statement_end(lexemes: &[Lexeme], start: usize, limit: usize) -> Option<usize> {
    let mut index = start;
    while index < limit {
        if closing_delimiter(lexemes[index].kind).is_some() {
            index = matching_delimiter(lexemes, index)? + 1;
            continue;
        }
        if matches!(
            lexemes[index].kind,
            TokenKind::CloseParen | TokenKind::CloseBrace | TokenKind::CloseBracket
        ) {
            return None;
        }
        if lexemes[index].text == ";" {
            return Some(index);
        }
        index += 1;
    }
    None
}

fn pattern_names(lexemes: &[Lexeme], start: usize, end: usize) -> Option<Vec<String>> {
    if start >= end {
        return None;
    }
    if lexemes[start].text == "mut" {
        return pattern_names(lexemes, start + 1, end);
    }
    if lexemes[start].kind == TokenKind::Ident && start + 1 == end {
        return Some(vec![lexemes[start].text.clone()]);
    }
    if lexemes[start].kind != TokenKind::OpenParen
        || matching_delimiter(lexemes, start) != Some(end - 1)
    {
        return None;
    }
    let mut names = Vec::new();
    let mut part_start = start + 1;
    for index in start + 1..end {
        if lexemes[index].text == "," || index == end - 1 {
            if part_start < index {
                let part = pattern_names(lexemes, part_start, index)?;
                names.extend(part);
            }
            part_start = index + 1;
        }
    }
    Some(names)
}

fn add_binding(
    bindings: &mut Vec<(String, ProjectionBinding)>,
    name: String,
    part: Option<ExactPart>,
    base: Option<ProjectionBase>,
    why: &str,
) {
    let hole = part.map_or_else(
        || SchemaHole::Opaque { why: why.to_string() },
        SchemaHole::Exact,
    );
    bindings.push((name, ProjectionBinding { hole, base, parameter: None }));
}

fn collect_bindings(
    lexemes: &[Lexeme],
    quote_open: usize,
    parameters: &[String],
    field_parameters: &[String],
    syn_source: bool,
) -> Vec<(String, ProjectionBinding)> {
    let item_parameter = if parameters.len() == 1 {
        parameters.first().map(String::as_str)
    } else {
        parameters.last().map(String::as_str)
    };
    let mut bindings = Vec::new();
    for index in 0..quote_open {
        if lexemes[index].text != "let" {
            continue;
        }
        let Some(end) = statement_end(lexemes, index + 1, quote_open) else { continue };
        let Some(equals) = (index + 1..end).find(|candidate| lexemes[*candidate].text == "=") else {
            continue;
        };
        let Some(names) = pattern_names(lexemes, index + 1, equals) else { continue };
        let expression = &lexemes[equals + 1..end];
        if names.len() == 1 {
            let name = names[0].clone();
            if expression.len() == 1 {
                if let Some(previous) = binding(&bindings, &expression[0].text) {
                    bindings.push((name, previous));
                    continue;
                }
                if let Some(parameter) = parameters
                    .iter()
                    .position(|parameter| parameter == &expression[0].text)
                {
                    bindings.push((
                        name,
                        ProjectionBinding {
                            hole: SchemaHole::Exact(ExactPart::InputTokens),
                            base: None,
                            parameter: Some(parameter),
                        },
                    ));
                    continue;
                }
            }
            if syn_source
                && expression.iter().any(|lexeme| lexeme.text == "parse_macro_input")
                && expression.iter().any(|lexeme| lexeme.text == "DeriveInput")
                && expression.iter().any(|lexeme| {
                    parameters.iter().any(|parameter| parameter == &lexeme.text)
                })
            {
                bindings.push((
                    name,
                    ProjectionBinding {
                        hole: SchemaHole::Exact(ExactPart::InputTokens),
                        base: Some(ProjectionBase::Item),
                        parameter: None,
                    },
                ));
                continue;
            }
            if let Some(part) = direct_projection(
                expression,
                item_parameter,
                field_parameters,
                &bindings,
                syn_source,
            ) {
                let base = match part {
                    ExactPart::ItemIdent | ExactPart::Vis | ExactPart::Generics { .. } => {
                        Some(ProjectionBase::Item)
                    }
                    ExactPart::FieldIdent => Some(ProjectionBase::Field),
                    _ => None,
                };
                add_binding(&mut bindings, name, Some(part), base, "projection is not classified");
                continue;
            }
            if let Some(why) = projection_why(
                expression,
                item_parameter,
                field_parameters,
                &bindings,
                syn_source,
            ) {
                let base = (expression.len() == 3)
                    .then(|| projection_base(&expression[0].text, item_parameter, field_parameters, &bindings))
                    .flatten();
                add_binding(&mut bindings, name, None, base, why);
                continue;
            }
            if expression.iter().any(|lexeme| lexeme.text == "format_ident") {
                match format_ident_projection(
                    lexemes,
                    equals + 1,
                    end,
                    item_parameter,
                    field_parameters,
                    &bindings,
                    syn_source,
                ) {
                    Ok((part, base)) => add_binding(
                        &mut bindings,
                        name,
                        Some(part),
                        base,
                        "format_ident! name is opaque",
                    ),
                    Err(why) => add_binding(&mut bindings, name, None, None, &why),
                }
            } else if expression.iter().any(|lexeme| lexeme.text == "split_for_impl") {
                add_binding(
                    &mut bindings,
                    name,
                    None,
                    None,
                    "split_for_impl result pattern is not classified",
                );
            }
            continue;
        }

        if names.len() == 3 && expression.iter().any(|lexeme| lexeme.text == "split_for_impl") {
            let supported = syn_source
                && split_for_impl_on_generics(
                    expression,
                    item_parameter,
                    field_parameters,
                    &bindings,
                );
            let kinds = ["impl", "type", "where"];
            for (name, kind) in names.into_iter().zip(kinds) {
                add_binding(
                    &mut bindings,
                    name,
                    supported.then(|| ExactPart::Generics { kind: kind.to_string() }),
                    None,
                    "syn Generics::split_for_impl source is not indexed",
                );
            }
        }
    }
    bindings
}

fn simple_ident_literal(literal: &str) -> Option<String> {
    let value = source_literal(literal)?;
    if value.is_empty()
        || !value
            .bytes()
            .enumerate()
            .all(|(index, byte)| byte == b'_' || byte.is_ascii_alphanumeric() && (index != 0 || !byte.is_ascii_digit()))
    {
        return None;
    }
    Some(value.to_string())
}

fn attribute_absent_guard(
    lexemes: &[Lexeme],
    if_index: usize,
    loop_close: usize,
) -> Option<(SchemaGuard, usize)> {
    let open = (if_index + 1..loop_close)
        .find(|index| lexemes[*index].kind == TokenKind::OpenBrace)?;
    let condition = &lexemes[if_index + 1..open];
    if condition.len() != 10
        || condition[0].kind != TokenKind::Ident
        || condition[1].text != "."
        || condition[2].text != "path"
        || condition[3].text != "("
        || condition[4].text != ")"
        || condition[5].text != "."
        || condition[6].text != "is_ident"
        || condition[7].text != "("
        || !matches!(condition[8].kind, TokenKind::Literal { .. })
        || condition[9].text != ")"
    {
        return None;
    }
    let path = simple_ident_literal(&condition[8].text)?;
    let close = matching_delimiter(lexemes, open)?;
    let body = &lexemes[open + 1..close];
    if body.len() != 2 || body[0].text != "continue" || body[1].text != ";" {
        return None;
    }
    if lexemes.get(close + 1).is_some_and(|lexeme| lexeme.text == "else") {
        return None;
    }
    Some((SchemaGuard::AttributeAbsent { path }, close))
}

fn field_loop_variables(lexemes: &[Lexeme], open: usize) -> Vec<String> {
    let mut fields = Vec::new();
    for index in 0..open {
        if lexemes[index].text != "for"
            || lexemes.get(index + 1).is_none_or(|pattern| pattern.kind != TokenKind::Ident)
        {
            continue;
        }
        let Some(in_index) = (index + 2..open).find(|candidate| lexemes[*candidate].text == "in") else {
            continue;
        };
        if lexemes[in_index + 1..open].iter().any(|lexeme| lexeme.text == "fields") {
            fields.push(lexemes[index + 1].text.clone());
        }
    }
    fields
}

pub fn analyze_source(
    psess: &ParseSess,
    source: &str,
    parameters: &[String],
    syn_projections: bool,
) -> ProcMacroSchema {
    let lexemes = lex(source);
    let quotes = quote_invocations(&lexemes);
    if quotes.len() != 1 {
        return ProcMacroSchema::Unclassified {
            why: "proc macro body does not contain one quote template".to_string(),
        };
    }

    let (open, close, _) = quotes[0];
    let loop_range = loop_bounds(&lexemes, open, close);
    let repetition = loop_range.is_some()
        || (open + 1..close).any(|index| {
            lexemes[index].kind == TokenKind::Pound
                && lexemes.get(index + 1).is_some_and(|next| next.kind == TokenKind::OpenParen)
        });
    let syn_source = syn_projections;
    let field_parameters = loop_range
        .map(|(loop_open, _)| field_loop_variables(&lexemes, loop_open))
        .unwrap_or_default();
    if loop_range.is_some() && field_parameters.is_empty() {
        return ProcMacroSchema::Unclassified {
            why: "proc macro loop source is not classified".to_string(),
        };
    }
    let bindings = collect_bindings(
        &lexemes,
        open,
        parameters,
        &field_parameters,
        syn_source,
    );
    let mut holes = Vec::new();
    let mut hole_sources = Vec::new();
    let mut guard = None;
    let mut guard_ranges = Vec::new();
    if let Some((loop_open, loop_close)) = loop_range {
        for index in loop_open + 1..loop_close {
            if lexemes[index].text != "if" {
                continue;
            }
            if let Some((candidate, guard_close)) = attribute_absent_guard(&lexemes, index, loop_close) {
                if guard.is_some() {
                    guard = None;
                    guard_ranges.clear();
                    break;
                }
                guard = Some(candidate);
                guard_ranges.push((index, guard_close));
            }
        }
    }
    let mut control_flow = None;
    let mut outside_control_flow = false;
    let mut has_for = false;
    for (index, lexeme) in lexemes.iter().enumerate() {
        if inside_quote(index, &quotes) {
            continue;
        }
        if lexeme.text == "for" {
            has_for = true;
        }
        if matches!(lexeme.text.as_str(), "if" | "continue" | "match") {
            if let Some((loop_open, loop_close)) = loop_range
                && loop_open < index
                && index < loop_close
            {
                if guard_ranges
                    .iter()
                    .any(|(guard_open, guard_close)| *guard_open <= index && index <= *guard_close)
                {
                    continue;
                }
                control_flow = Some(lexeme.text.clone());
            } else {
                outside_control_flow = true;
            }
        }
    }
    if outside_control_flow || (has_for && loop_range.is_none()) {
        return ProcMacroSchema::Unclassified {
            why: "proc macro body depends on control flow".to_string(),
        };
    }

    let rhs_open = open;
    let rhs_close = close;
    for index in rhs_open + 1..rhs_close {
        if lexemes[index].kind != TokenKind::Pound {
            continue;
        }
        let Some(name) = lexemes.get(index + 1) else { continue };
        if name.kind != TokenKind::Ident {
            continue;
        }
        let (hole, source) = if control_flow.is_some() {
            (
                SchemaHole::Opaque {
                    why: format!("loop body contains `{}`", control_flow.as_deref().unwrap_or("control flow")),
                },
                None,
            )
        } else if let Some(binding) = binding(&bindings, &name.text) {
            (
                binding.hole,
                binding.parameter.map(SchemaBindingSource::Parameter).or_else(|| {
                    binding.base.map(|base| match base {
                        ProjectionBase::Item => SchemaBindingSource::Item,
                        ProjectionBase::Field => SchemaBindingSource::Field,
                    })
                }),
            )
        } else if let Some(parameter) = parameters.iter().position(|parameter| parameter == &name.text) {
            (SchemaHole::Exact(ExactPart::InputTokens), Some(SchemaBindingSource::Parameter(parameter)))
        } else {
            (
                SchemaHole::Opaque { why: format!("projection for `{}` is not classified", name.text) },
                None,
            )
        };
        holes.push(hole);
        hole_sources.push(source);
    }

    if let Some(branch) = control_flow {
        if holes.is_empty() {
            holes.push(SchemaHole::Opaque { why: format!("loop body contains `{branch}`") });
        }
    }

    let source_start = lexemes[open].end;
    let source_end = lexemes[close].start;
    let rhs = source[source_start..source_end].to_string();
    // The template must lex here; the use site re-lexes `source` in its own session, since a
    // token stream carries this session's spans.
    match crate::rustc_parse::source_str_to_stream(
        psess,
        FileName::anon_source_code(&rhs),
        rhs.clone(),
        None,
    ) {
        Ok(_) => {}
        Err(errors) => {
            errors.into_iter().for_each(|error| error.cancel());
            return ProcMacroSchema::default();
        }
    }
    ProcMacroSchema::Template {
        rules: vec![SchemaRule {
            class: None,
            guard,
            rhs: SchemaTokenTree { source: rhs },
            holes,
            hole_sources,
            parameters: parameters.to_vec(),
            repetition,
            loop_repetition: loop_range.is_some(),
        }],
    }
}

fn function_body_open(lexemes: &[Lexeme], open: usize) -> bool {
    let mut nested = 0usize;
    for index in (0..open).rev() {
        let kind = lexemes[index].kind;
        if matches!(kind, TokenKind::CloseParen | TokenKind::CloseBrace | TokenKind::CloseBracket) {
            nested += 1;
            continue;
        }
        if closing_delimiter(kind).is_some() {
            if nested > 0 {
                nested -= 1;
            } else if kind == TokenKind::OpenBrace {
                break;
            }
            continue;
        }
        if nested > 0 {
            continue;
        }
        if lexemes[index].text == "fn" {
            return true;
        }
        if matches!(lexemes[index].text.as_str(), ";" | "{" | "}") {
            break;
        }
    }
    false
}

fn function_body_ranges(lexemes: &[Lexeme]) -> Vec<(usize, usize)> {
    lexemes
        .iter()
        .enumerate()
        .filter_map(|(open, lexeme)| {
            (lexeme.kind == TokenKind::OpenBrace && function_body_open(lexemes, open))
                .then(|| matching_delimiter(lexemes, open).map(|close| (open, close)))
                .flatten()
        })
        .collect()
}

/// Rewrites a schema quote body into a macro-by-example RHS and records the exact bindings it
/// needs. Opaque holes are admitted only inside a generated function body.
pub(crate) fn rewrite_template(rule: &SchemaRule) -> Option<RewrittenSchemaTemplate> {
    if rule.class.is_some() {
        return None;
    }

    let source = &rule.rhs.source;
    let lexemes = lex(source);
    let body_ranges = function_body_ranges(&lexemes);
    let mut replacements = Vec::new();
    let mut variables = Vec::new();
    let mut hole_index = 0usize;
    let mut repetition_ranges = Vec::new();
    let mut has_sequence = false;

    for index in 0..lexemes.len() {
        if lexemes[index].kind != TokenKind::Pound {
            continue;
        }
        let Some(next) = lexemes.get(index + 1) else { return None };
        if next.kind == TokenKind::OpenParen {
            let close = matching_delimiter(&lexemes, index + 1)?;
            if lexemes.get(close + 1).is_none_or(|op| op.text != "*") {
                return None;
            }
            has_sequence = true;
            repetition_ranges.push((index + 1, close));
            replacements.push((lexemes[index].start, lexemes[index].end, "$".to_string()));
            continue;
        }
        if next.kind != TokenKind::Ident {
            return None;
        }
        let hole = rule.holes.get(hole_index)?.clone();
        let source = *rule.hole_sources.get(hole_index)?;
        hole_index += 1;
        let repeated = repetition_ranges
            .iter()
            .any(|(open, close)| *open < index && index < *close);
        let in_function_body = body_ranges
            .iter()
            .any(|(open, close)| *open < index && index < *close);
        let replacement = match &hole {
            SchemaHole::Exact(_) => "$".to_string(),
            SchemaHole::Opaque { why } if in_function_body => {
                format!("compile_error!({:?});", format!("opaque: {why}"))
            }
            SchemaHole::Opaque { .. } => return None,
        };
        replacements.push((lexemes[index].start, next.end, replacement));
        variables.push(SchemaVariable {
            name: next.text.clone(),
            hole,
            source,
            repeated,
        });
    }

    if hole_index != rule.holes.len() || rule.loop_repetition && has_sequence {
        return None;
    }

    let mut rewritten = String::new();
    let mut cursor = 0usize;
    for (start, end, replacement) in replacements {
        rewritten.push_str(&source[cursor..start]);
        rewritten.push_str(&replacement);
        cursor = end;
    }
    rewritten.push_str(&source[cursor..]);

    if rule.loop_repetition {
        rewritten = format!("$({rewritten})*");
        for variable in &mut variables {
            variable.repeated = true;
        }
    }

    Some(RewrittenSchemaTemplate { source: rewritten, variables })
}

#[cfg(test)]
mod tests {
    use alloc::string::ToString;

    use super::{ExactPart, ProcMacroSchema, SchemaHole};

    #[test]
    fn schema_unclassified_is_the_default() {
        assert!(matches!(ProcMacroSchema::default(), ProcMacroSchema::Unclassified { .. }));
        let opaque = SchemaHole::Opaque { why: "unknown projection".to_string() };
        let exact = SchemaHole::Exact(ExactPart::InputTokens);
        assert_ne!(opaque, exact);
    }
}
