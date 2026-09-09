// The diagnostic message template language: parser and renderer.
//
// **This is what `fluent-bundle` used to do, and the reason it is gone is `std`.** The i18n
// stack - `fluent-bundle`, `fluent-syntax`, `fluent-langneg`, `intl-memoizer`,
// `intl_pluralrules`, `unic-langid`, the `icu_*` crates and their `litemap`/`potential_utf`/
// `writeable`/`yoke`/`zerofrom`/`zerotrie` support - was 18 of the 42 crates that forced this compiler
// to link the standard library, and it was carrying a translation system with nothing to
// translate: the `.ftl` message files were deleted long ago and every template is now an inline
// string literal in a `#[diag("...")]` attribute or a `msg!("...")` call, in English, resolved
// against exactly one locale.
//
// What was *not* vestigial is the syntax those literals are written in. `{$name}` interpolation,
// `{$n -> [1] ... *[other] ...}` selection and `{"..."}` literals are used by hundreds of
// messages, so this crate reimplements that subset rather than dropping it. The shapes below
// are deliberately faithful to `fluent-syntax` 0.12's `get_pattern` and `fluent-bundle` 0.16's
// resolver, including the multiline dedent rule, because a diagnostic that renders differently
// is a diagnostic whose test output changed.
//
// The subset stops where the tree stops using it. Message references (`{other-msg}`), term
// references (`{-term}`), attribute accessors, named call arguments, fractional number literals
// and functions other than `STREQ` are *parse errors* rather than silent no-ops. That choice is
// safe because every template in the tree is parsed at compile time - the derive parses each
// attribute message, and `msg!` parses its literal - so an unsupported construct is a build
// error naming the site, not a wrong message at emission time.
//
// ---------------------------------------------------------------------------------------------
// STD IS BANNED IN THIS CRATE.
//
// `#![no_std]` below is the ban and the compiler is the enforcer: without `extern crate std;`
// there is no `std` in the extern prelude, so any `std::` path fails to resolve and the build
// stops. Do not add that line back to make an error go away - the error is the point.
// ---------------------------------------------------------------------------------------------
#![no_std]

extern crate alloc;

use alloc::borrow::Cow;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

// =============================================================================================
// The syntax tree
// =============================================================================================

/// A parsed message template.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pattern {
    pub elements: Vec<Element>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Element {
    /// Literal text, already dedented. It is *not* re-scanned at render time: an argument value
    /// that happens to contain a brace is data, not a template.
    Text(String),
    Placeable(Expression),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Expression {
    Inline(Inline),
    /// `{$selector -> [key] value *[key] value}`. The parser guarantees exactly one variant is
    /// marked default, which is what makes rendering total.
    Select { selector: Inline, variants: Vec<Variant> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Inline {
    /// `$name`
    Variable(String),
    /// `"text"`, with its escape sequences already resolved.
    Literal(String),
    /// A number literal written in the template. Integers only, see `get_number_literal`.
    Number(i64),
    /// `STREQ(a, b)`, the one function the message system ever registered. It resolves to the
    /// string `"true"` or `"false"` so a select can branch on a string argument's exact value.
    StrEq(Box<Inline>, Box<Inline>),
    /// A placeable nested inside a placeable, `{{...}}`. Fluent allows it and it costs three
    /// lines to keep, which is cheaper than discovering it in a central build.
    Nested(Box<Expression>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Variant {
    pub key: Key,
    pub value: Pattern,
    pub default: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Key {
    Name(String),
    Number(i64),
}

/// A template that did not parse. `offset` is a byte offset into the message text itself, so a
/// caller holding the literal's span can point at the character.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    pub offset: usize,
    pub message: String,
}

// =============================================================================================
// Parsing
// =============================================================================================

/// Parse a message template.
///
/// Fluent parsed a whole *resource*, and the caller synthesised one as
/// `format!("generated_msg = {message}\n")`. Both halves of that wrapper matter and are
/// reproduced here rather than assumed away:
///
/// - Everything before the message is fixed, and the parser state at the start of a pattern is
///   the same as starting at byte 0 of the message, because the first thing `get_pattern` does
///   is `skip_blank_inline`, which ate the separator space and any leading spaces of the message
///   alike. That is why `"  foo"` and `"foo"` have always rendered identically, and why a
///   message that wants a leading space has to write `{" "}`.
/// - The trailing newline is not cosmetic. It is what makes the final text element terminate on
///   a line feed rather than on end-of-input, which changes whether a trailing blank element is
///   emitted at all. It is appended here for that reason.
pub fn parse(message: &str) -> Result<Pattern, ParseError> {
    let mut source = String::with_capacity(message.len() + 1);
    source.push_str(message);
    source.push('\n');

    let mut parser = Parser::new(&source);
    let pattern = parser.get_pattern()?;

    // Fluent would have carried on here into attributes and then into the next entry, turning
    // whatever is left into junk and reporting resource errors that the caller unwrapped into a
    // panic. Leftover input means the pattern ended early - a line at column zero, or a line
    // opening with `.`, `[` or `*` where a continuation was expected - so say that instead.
    if parser.ptr < parser.len() {
        return Err(parser.error(
            parser.ptr,
            "message ends here: a continuation line must be indented, and must not begin with \
             `.`, `[` or `*`",
        ));
    }

    pattern.ok_or_else(|| ParseError { offset: 0, message: "message is empty".to_string() })
}

/// Collect every `$variable` a template refers to, in source order and with duplicates kept.
/// The derive uses this to check that each one names a field of the diagnostic struct.
pub fn variable_references(pattern: &Pattern) -> Vec<&str> {
    let mut refs = Vec::new();
    walk_pattern(pattern, &mut refs);
    return refs;

    fn walk_pattern<'a>(pattern: &'a Pattern, refs: &mut Vec<&'a str>) {
        for element in &pattern.elements {
            match element {
                Element::Text(_) => {}
                Element::Placeable(expression) => walk_expression(expression, refs),
            }
        }
    }

    fn walk_expression<'a>(expression: &'a Expression, refs: &mut Vec<&'a str>) {
        match expression {
            Expression::Inline(inline) => walk_inline(inline, refs),
            Expression::Select { selector, variants } => {
                walk_inline(selector, refs);
                for variant in variants {
                    walk_pattern(&variant.value, refs);
                }
            }
        }
    }

    fn walk_inline<'a>(inline: &'a Inline, refs: &mut Vec<&'a str>) {
        match inline {
            Inline::Variable(name) => refs.push(name.as_str()),
            Inline::Literal(_) | Inline::Number(_) => {}
            Inline::StrEq(a, b) => {
                walk_inline(a, refs);
                walk_inline(b, refs);
            }
            Inline::Nested(expression) => walk_expression(expression, refs),
        }
    }
}

/// Where a text element sits, which is what the dedent rule keys off.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Position {
    /// The first line, whose indentation was already eaten by the separator skip and so never
    /// takes part in the common indent.
    InitialLineStart,
    LineStart,
    Continuation,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TextKind {
    Blank,
    NonBlank,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Termination {
    LineFeed,
    Crlf,
    PlaceableStart,
    Eof,
}

/// A text element is recorded as a range rather than a slice so the dedent at the end can adjust
/// its start without re-slicing.
enum Placeholder {
    Placeable(Expression),
    Text { start: usize, end: usize, indent: usize, line_start: bool },
}

struct Parser<'s> {
    src: &'s str,
    bytes: &'s [u8],
    ptr: usize,
}

impl<'s> Parser<'s> {
    fn new(src: &'s str) -> Self {
        Parser { src, bytes: src.as_bytes(), ptr: 0 }
    }

    fn len(&self) -> usize {
        self.bytes.len()
    }

    fn error(&self, offset: usize, message: &str) -> ParseError {
        ParseError { offset, message: message.to_string() }
    }

    fn cur(&self) -> Option<u8> {
        self.bytes.get(self.ptr).copied()
    }

    fn at(&self, pos: usize) -> Option<u8> {
        self.bytes.get(pos).copied()
    }

    fn is_cur(&self, b: u8) -> bool {
        self.cur() == Some(b)
    }

    fn take_byte_if(&mut self, b: u8) -> bool {
        if self.is_cur(b) {
            self.ptr += 1;
            true
        } else {
            false
        }
    }

    fn expect_byte(&mut self, b: u8) -> Result<(), ParseError> {
        if !self.is_cur(b) {
            return Err(self.error(self.ptr, &format!("expected `{}`", b as char)));
        }
        self.ptr += 1;
        Ok(())
    }

    fn skip_eol(&mut self) -> bool {
        match self.cur() {
            Some(b'\n') => {
                self.ptr += 1;
                true
            }
            Some(b'\r') if self.at(self.ptr + 1) == Some(b'\n') => {
                self.ptr += 2;
                true
            }
            _ => false,
        }
    }

    /// Spaces only, and it returns how many. Tabs are not blank in Fluent, they are text.
    fn skip_blank_inline(&mut self) -> usize {
        let start = self.ptr;
        while self.is_cur(b' ') {
            self.ptr += 1;
        }
        self.ptr - start
    }

    fn skip_blank_block(&mut self) {
        loop {
            let start = self.ptr;
            self.skip_blank_inline();
            if !self.skip_eol() {
                self.ptr = start;
                break;
            }
        }
    }

    fn skip_blank(&mut self) {
        loop {
            match self.cur() {
                Some(b' ') | Some(b'\n') => self.ptr += 1,
                Some(b'\r') if self.at(self.ptr + 1) == Some(b'\n') => self.ptr += 2,
                _ => break,
            }
        }
    }

    /// A line opening with one of these is the next variant, the next attribute or the closing
    /// brace, not more of this pattern.
    fn is_pattern_continuation(b: u8) -> bool {
        !matches!(b, b'.' | b'}' | b'[' | b'*')
    }

    fn is_identifier_start(&self, pos: usize) -> bool {
        matches!(self.at(pos), Some(b) if b.is_ascii_alphabetic())
    }

    /// Consume an identifier whose first byte has *already* been consumed.
    fn get_identifier_unchecked(&mut self) -> String {
        let start = self.ptr - 1;
        while matches!(self.cur(), Some(b) if b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
            self.ptr += 1;
        }
        self.src[start..self.ptr].to_string()
    }

    fn get_identifier(&mut self) -> Result<String, ParseError> {
        if !self.is_identifier_start(self.ptr) {
            return Err(self.error(self.ptr, "expected an identifier"));
        }
        self.ptr += 1;
        Ok(self.get_identifier_unchecked())
    }

    /// Integers only.
    ///
    /// Fluent numbers are `f64` carrying a minimum-fraction-digits option that feeds both
    /// rendering and the plural operands. Nothing in this tree writes a fractional literal - the
    /// only numbers in a template are select keys like `[1]` and the odd `{ 0 }` - so
    /// reproducing that machinery would be weight with no caller. Refusing is loud and happens
    /// at compile time; rounding silently would be a wrong diagnostic.
    fn get_number_literal(&mut self) -> Result<i64, ParseError> {
        let start = self.ptr;
        self.take_byte_if(b'-');
        let digits_start = self.ptr;
        while matches!(self.cur(), Some(b) if b.is_ascii_digit()) {
            self.ptr += 1;
        }
        if self.ptr == digits_start {
            return Err(self.error(self.ptr, "expected a digit"));
        }
        if self.is_cur(b'.') {
            return Err(self.error(self.ptr, "fractional numbers are not supported in a message"));
        }
        self.src[start..self.ptr]
            .parse::<i64>()
            .map_err(|_| self.error(start, "number is too large for a message"))
    }

    // -----------------------------------------------------------------------------------------
    // Patterns
    // -----------------------------------------------------------------------------------------

    /// The multiline rule, which is the subtle half of this parser.
    ///
    /// A pattern may run over several lines. Every line that starts a text element records its
    /// indentation; the smallest such indentation over *non-blank* line-start elements is the
    /// common indent, and it is removed from each of them at the end. A placeable sitting at the
    /// start of a line forces the common indent to zero. The last non-blank element has trailing
    /// Fluent whitespace trimmed, and everything after it is dropped.
    ///
    /// The effect on a real message: a `#[help("\n    add as non-Derive macro\n    `#[{$m}]`")]`
    /// renders as two lines with the source's four-space gutter removed, not as one run-on line
    /// and not with the gutter baked in.
    fn get_pattern(&mut self) -> Result<Option<Pattern>, ParseError> {
        let mut elements: Vec<Placeholder> = Vec::new();
        let mut last_non_blank: Option<usize> = None;
        let mut common_indent: Option<usize> = None;

        self.skip_blank_inline();

        let mut role = if self.skip_eol() {
            self.skip_blank_block();
            Position::LineStart
        } else {
            Position::InitialLineStart
        };

        while self.ptr < self.len() {
            if self.take_byte_if(b'{') {
                if role == Position::LineStart {
                    common_indent = Some(0);
                }
                let expression = self.get_placeable()?;
                last_non_blank = Some(elements.len());
                elements.push(Placeholder::Placeable(expression));
                role = Position::Continuation;
            } else {
                let slice_start = self.ptr;
                let mut indent = 0;
                if role == Position::LineStart {
                    indent = self.skip_blank_inline();
                    match self.cur() {
                        Some(b) => {
                            if indent == 0 {
                                if b != b'\r' && b != b'\n' {
                                    break;
                                }
                            } else if !Self::is_pattern_continuation(b) {
                                self.ptr = slice_start;
                                break;
                            }
                        }
                        None => break,
                    }
                }

                let (start, end, kind, termination) = self.get_text_slice()?;
                if start != end {
                    if role == Position::LineStart && kind == TextKind::NonBlank {
                        common_indent = Some(match common_indent {
                            Some(common) => core::cmp::min(common, indent),
                            None => indent,
                        });
                    }
                    if role != Position::LineStart
                        || kind == TextKind::NonBlank
                        || termination == Termination::LineFeed
                    {
                        if kind == TextKind::NonBlank {
                            last_non_blank = Some(elements.len());
                        }
                        elements.push(Placeholder::Text {
                            start: slice_start,
                            end,
                            indent,
                            line_start: role == Position::LineStart,
                        });
                    }
                }

                role = match termination {
                    Termination::LineFeed | Termination::Crlf => Position::LineStart,
                    Termination::PlaceableStart | Termination::Eof => Position::Continuation,
                };
            }
        }

        let Some(last_non_blank) = last_non_blank else {
            return Ok(None);
        };

        let elements = elements
            .into_iter()
            .take(last_non_blank + 1)
            .enumerate()
            .map(|(i, placeholder)| match placeholder {
                Placeholder::Placeable(expression) => Element::Placeable(expression),
                Placeholder::Text { start, end, indent, line_start } => {
                    let start = if line_start {
                        match common_indent {
                            Some(common) => start + core::cmp::min(indent, common),
                            None => start + indent,
                        }
                    } else {
                        start
                    };
                    let mut value = &self.src[start..end];
                    if i == last_non_blank {
                        value = value.trim_end_matches(|c| c == ' ' || c == '\r' || c == '\n');
                    }
                    Element::Text(value.to_string())
                }
            })
            .collect();

        Ok(Some(Pattern { elements }))
    }

    /// Returns `(start, end, kind, termination)`. `start` is where the run began; `end` excludes
    /// a `{` terminator and includes a `\n` one, which is what makes the trailing-newline trim
    /// above do the right thing.
    fn get_text_slice(&mut self) -> Result<(usize, usize, TextKind, Termination), ParseError> {
        let start = self.ptr;
        // Bind the slice out of `self` first: what follows moves `self.ptr`.
        let bytes = self.bytes;
        let rest = &bytes[self.ptr..];
        if rest.is_empty() {
            return Ok((start, self.ptr, TextKind::Blank, Termination::Eof));
        }

        // "Blank" here is spaces only, matching Fluent: a line of spaces does not take part in
        // the common indent, but a line with a tab on it does.
        let kind = |text: &[u8]| {
            if text.iter().any(|&c| c != b' ') { TextKind::NonBlank } else { TextKind::Blank }
        };

        match rest.iter().position(|&c| c == b'\n' || c == b'{' || c == b'}') {
            Some(pos) => match rest[pos] {
                b'}' => {
                    self.ptr += pos;
                    Err(self.error(self.ptr, "unbalanced `}`: write `{\"}\"}` for a literal one"))
                }
                b'\n' if pos > 0 && rest[pos - 1] == b'\r' => {
                    let text = &rest[..pos - 1];
                    self.ptr += text.len() + 1;
                    Ok((start, self.ptr - 1, kind(text), Termination::Crlf))
                }
                b'\n' => {
                    let text = &rest[..pos];
                    self.ptr += text.len() + 1;
                    Ok((start, self.ptr, kind(text), Termination::LineFeed))
                }
                _ => {
                    let text = &rest[..pos];
                    self.ptr += text.len();
                    Ok((start, self.ptr, kind(text), Termination::PlaceableStart))
                }
            },
            None => {
                self.ptr += rest.len();
                Ok((start, self.ptr, kind(rest), Termination::Eof))
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // Placeables
    // -----------------------------------------------------------------------------------------

    /// Called with the opening `{` already consumed.
    fn get_placeable(&mut self) -> Result<Expression, ParseError> {
        self.skip_blank();
        let expression = self.get_expression()?;
        self.skip_blank_inline();
        self.expect_byte(b'}')?;
        Ok(expression)
    }

    fn get_expression(&mut self) -> Result<Expression, ParseError> {
        let selector = self.get_inline_expression()?;

        self.skip_blank();
        if !(self.is_cur(b'-') && self.at(self.ptr + 1) == Some(b'>')) {
            return Ok(Expression::Inline(selector));
        }
        self.ptr += 2; // `->`

        self.skip_blank_inline();
        if !self.skip_eol() {
            return Err(self.error(self.ptr, "`->` must be followed by a newline"));
        }
        self.skip_blank();

        let variants = self.get_variants()?;
        Ok(Expression::Select { selector, variants })
    }

    fn get_variants(&mut self) -> Result<Vec<Variant>, ParseError> {
        let mut variants = Vec::new();
        let mut has_default = false;

        loop {
            let default = self.take_byte_if(b'*');
            if default {
                if has_default {
                    return Err(self.error(self.ptr, "a select may only have one default variant"));
                }
                has_default = true;
            }

            if !self.take_byte_if(b'[') {
                break;
            }

            let key = self.get_variant_key()?;
            let Some(value) = self.get_pattern()? else {
                return Err(self.error(self.ptr, "a variant must have a value"));
            };
            variants.push(Variant { key, value, default });
            self.skip_blank();
        }

        if has_default {
            Ok(variants)
        } else {
            // Rendering is total because of this check: whatever the selector turns out to be,
            // some variant is guaranteed to answer for it.
            Err(self.error(self.ptr, "a select must have a default variant, marked `*[...]`"))
        }
    }

    fn get_variant_key(&mut self) -> Result<Key, ParseError> {
        self.skip_blank();
        let key = if matches!(self.cur(), Some(b) if b.is_ascii_digit() || b == b'-') {
            Key::Number(self.get_number_literal()?)
        } else {
            Key::Name(self.get_identifier()?)
        };
        self.skip_blank();
        self.expect_byte(b']')?;
        Ok(key)
    }

    fn get_inline_expression(&mut self) -> Result<Inline, ParseError> {
        match self.cur() {
            Some(b'"') => {
                self.ptr += 1;
                let start = self.ptr;
                loop {
                    match self.cur() {
                        Some(b'\\') => match self.at(self.ptr + 1) {
                            Some(b'\\') | Some(b'"') => self.ptr += 2,
                            Some(b'u') => {
                                self.ptr += 2;
                                self.skip_hex_digits(4)?;
                            }
                            Some(b'U') => {
                                self.ptr += 2;
                                self.skip_hex_digits(6)?;
                            }
                            _ => {
                                return Err(self.error(self.ptr, "unknown escape sequence"));
                            }
                        },
                        Some(b'"') | None => break,
                        Some(b'\n') => {
                            return Err(self.error(self.ptr, "unterminated string literal"));
                        }
                        Some(_) => self.ptr += 1,
                    }
                }
                self.expect_byte(b'"')?;
                Ok(Inline::Literal(unescape(&self.src[start..self.ptr - 1])))
            }

            Some(b) if b.is_ascii_digit() => Ok(Inline::Number(self.get_number_literal()?)),

            Some(b'-') => {
                // In Fluent a `-` before an identifier opens a term reference. There are no
                // terms without a `.ftl` file, so this can only be a negative number.
                if self.is_identifier_start(self.ptr + 1) {
                    Err(self.error(self.ptr, "term references (`{-name}`) are not supported"))
                } else {
                    Ok(Inline::Number(self.get_number_literal()?))
                }
            }

            Some(b'$') => {
                self.ptr += 1;
                Ok(Inline::Variable(self.get_identifier()?))
            }

            Some(b) if b.is_ascii_alphabetic() => {
                let name_start = self.ptr;
                self.ptr += 1;
                let name = self.get_identifier_unchecked();
                self.skip_blank();
                if !self.take_byte_if(b'(') {
                    // A bare identifier is a reference to another message, which needs a
                    // resource of messages to resolve against. There is one message here, built
                    // from the template itself, so this could only ever have rendered as the
                    // literal text `{name}` plus an error - and the error was a panic. The same
                    // error covers a named call argument, `f(x: 1)`, whose name parses as one.
                    return Err(self.error(
                        name_start,
                        "message references and named call arguments are not supported; write \
                         `{$name}` for an argument",
                    ));
                }
                self.get_call(name_start, &name)
            }

            Some(b'{') => {
                self.ptr += 1;
                let expression = self.get_placeable()?;
                Ok(Inline::Nested(Box::new(expression)))
            }

            _ => Err(self.error(self.ptr, "expected an expression")),
        }
    }

    /// Called with the `(` consumed. `STREQ` is the only function the message system ever
    /// registered, so it is the only one that resolves.
    fn get_call(&mut self, name_start: usize, name: &str) -> Result<Inline, ParseError> {
        // Named first, so an unknown function is reported as one rather than as whatever its
        // arguments happen to look like.
        if name != "STREQ" {
            return Err(self.error(
                name_start,
                &format!("`{name}` is not a known function; the only one is `STREQ`"),
            ));
        }

        let mut args = Vec::new();
        self.skip_blank();
        while self.ptr < self.len() && !self.is_cur(b')') {
            args.push(self.get_inline_expression()?);
            self.skip_blank();
            self.take_byte_if(b',');
            self.skip_blank();
        }
        self.expect_byte(b')')?;

        if args.len() != 2 {
            return Err(self.error(name_start, "`STREQ` takes exactly two arguments"));
        }
        let b = args.pop().expect("checked length");
        let a = args.pop().expect("checked length");
        Ok(Inline::StrEq(Box::new(a), Box::new(b)))
    }

    fn skip_hex_digits(&mut self, count: usize) -> Result<(), ParseError> {
        let start = self.ptr;
        for _ in 0..count {
            match self.cur() {
                Some(b) if b.is_ascii_hexdigit() => self.ptr += 1,
                _ => break,
            }
        }
        if self.ptr - start != count {
            return Err(self.error(start, "invalid unicode escape sequence"));
        }
        Ok(())
    }
}

/// Resolve the escape sequences a string literal is allowed to carry: `\\`, `\"`, `\uXXXX` and
/// `\UXXXXXX`. A sequence that is well formed but not a character becomes U+FFFD, which is what
/// Fluent did rather than failing the whole message.
fn unescape(input: &str) -> String {
    if !input.contains('\\') {
        return input.to_string();
    }

    const UNKNOWN: char = '\u{fffd}';
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut start = 0;
    let mut ptr = 0;

    while let Some(&b) = bytes.get(ptr) {
        if b != b'\\' {
            ptr += 1;
            continue;
        }
        out.push_str(&input[start..ptr]);
        ptr += 1;

        let c = match bytes.get(ptr) {
            Some(b'\\') => '\\',
            Some(b'"') => '"',
            Some(u @ (b'u' | b'U')) => {
                let seq_start = ptr + 1;
                let len = if *u == b'u' { 4 } else { 6 };
                ptr += len;
                input
                    .get(seq_start..seq_start + len)
                    .and_then(|s| u32::from_str_radix(s, 16).ok())
                    .and_then(char::from_u32)
                    .unwrap_or(UNKNOWN)
            }
            _ => UNKNOWN,
        };
        ptr += 1;
        out.push(c);
        start = ptr;
    }

    out.push_str(&input[start..]);
    out
}

// =============================================================================================
// Rendering
// =============================================================================================

/// An argument value, as the renderer sees it.
///
/// This is deliberately narrower than `DiagArgValue`: a list of strings is joined by the caller
/// before it gets here, because joining it is a locale decision and this crate holds no locale.
#[derive(Clone, Debug)]
pub enum Value<'a> {
    Str(Cow<'a, str>),
    Number(i32),
}

/// The arguments a template is rendered against.
pub trait Args {
    fn get(&self, name: &str) -> Option<Value<'_>>;
}

/// The result of rendering. `unresolved` names every `$variable` the template asked for and the
/// arguments did not carry; the text still holds `{$name}` at those places, exactly as Fluent
/// left it, so a caller that chooses to report rather than abort has something to show.
pub struct Rendered {
    pub text: String,
    pub unresolved: Vec<String>,
}

/// What a selector resolved to. `Error` is a real outcome, not a failure: Fluent matched only
/// strings and numbers against variant keys and fell through to the default for anything else,
/// which is how `STREQ` on a non-string argument behaved.
enum Selector {
    Str(String),
    Number(i32),
    Error,
}

pub fn render(pattern: &Pattern, args: &dyn Args) -> Rendered {
    let mut out = Rendered { text: String::new(), unresolved: Vec::new() };
    write_pattern(pattern, args, &mut out);
    out
}

fn write_pattern(pattern: &Pattern, args: &dyn Args, out: &mut Rendered) {
    for element in &pattern.elements {
        match element {
            Element::Text(text) => out.text.push_str(text),
            Element::Placeable(expression) => write_expression(expression, args, out),
        }
    }
}

fn write_expression(expression: &Expression, args: &dyn Args, out: &mut Rendered) {
    match expression {
        Expression::Inline(inline) => write_inline(inline, args, out),
        Expression::Select { selector, variants } => {
            let selector = resolve(selector, args, out);
            if let Selector::Str(_) | Selector::Number(_) = selector {
                for variant in variants {
                    if key_matches(&variant.key, &selector) {
                        write_pattern(&variant.value, args, out);
                        return;
                    }
                }
            }
            for variant in variants {
                if variant.default {
                    write_pattern(&variant.value, args, out);
                    return;
                }
            }
            // Unreachable: `get_variants` refuses a select without a default.
        }
    }
}

fn write_inline(inline: &Inline, args: &dyn Args, out: &mut Rendered) {
    match inline {
        Inline::Variable(name) => match args.get(name) {
            Some(Value::Str(s)) => out.text.push_str(&s),
            Some(Value::Number(n)) => out.text.push_str(&n.to_string()),
            None => {
                out.unresolved.push(name.clone());
                out.text.push('{');
                out.text.push('$');
                out.text.push_str(name);
                out.text.push('}');
            }
        },
        Inline::Literal(s) => out.text.push_str(s),
        Inline::Number(n) => out.text.push_str(&n.to_string()),
        Inline::StrEq(..) => match resolve(inline, args, out) {
            Selector::Str(s) => out.text.push_str(&s),
            Selector::Number(n) => out.text.push_str(&n.to_string()),
            // Fluent wrote the callee back out when a function returned an error value.
            Selector::Error => out.text.push_str("STREQ()"),
        },
        Inline::Nested(expression) => write_expression(expression, args, out),
    }
}

fn resolve(inline: &Inline, args: &dyn Args, out: &mut Rendered) -> Selector {
    match inline {
        Inline::Variable(name) => match args.get(name) {
            Some(Value::Str(s)) => Selector::Str(s.into_owned()),
            Some(Value::Number(n)) => Selector::Number(n),
            None => {
                out.unresolved.push(name.clone());
                Selector::Error
            }
        },
        Inline::Literal(s) => Selector::Str(s.clone()),
        // A template number literal is `i64` and an argument is `i32`; a literal outside `i32`
        // can never equal an argument, and `Error` is precisely "matches no variant key".
        Inline::Number(n) => match i32::try_from(*n) {
            Ok(n) => Selector::Number(n),
            Err(_) => Selector::Error,
        },
        Inline::StrEq(a, b) => {
            // `STREQ` compares two *strings*. Given anything else it produced an error value,
            // and a select on an error value takes its default branch.
            match (resolve(a, args, out), resolve(b, args, out)) {
                (Selector::Str(a), Selector::Str(b)) => {
                    Selector::Str(if a == b { "true" } else { "false" }.to_string())
                }
                _ => Selector::Error,
            }
        }
        Inline::Nested(expression) => {
            let mut nested = Rendered { text: String::new(), unresolved: Vec::new() };
            write_expression(expression, args, &mut nested);
            out.unresolved.extend(nested.unresolved);
            Selector::Str(nested.text)
        }
    }
}

/// Variant matching, and the one place a plural rule survives.
///
/// Fluent asked `intl_pluralrules` which CLDR category a number falls in and compared that to a
/// keyword key. The bundle only ever held `en-US`, and English cardinal plurals are one rule:
/// `one` when the integer part is 1 and there are no fraction digits, `other` for everything
/// else. Arguments are `i32`, so there are never fraction digits, and the rule is `n.abs() == 1`
/// - which is also why `[zero]`, `[two]`, `[few]` and `[many]` never matched in this tree and do
/// not match here either.
fn key_matches(key: &Key, selector: &Selector) -> bool {
    match (key, selector) {
        (Key::Name(name), Selector::Str(value)) => name == value,
        (Key::Number(key), Selector::Number(value)) => *key == i64::from(*value),
        (Key::Name(name), Selector::Number(value)) => match name.as_str() {
            "one" => value.unsigned_abs() == 1,
            "other" => value.unsigned_abs() != 1,
            _ => false,
        },
        _ => false,
    }
}
