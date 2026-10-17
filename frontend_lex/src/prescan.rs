//! The prescan: where a file can be cut into chunks that lex independently.
//!
//! A chunk can start lexing from scratch only where the serial lexer is between tokens, outside
//! every string, character literal and comment. Rust has no byte that guarantees that on its own
//! (nested block comments and raw strings carry state a single byte cannot see), so the prescan
//! tracks exactly that state, and nothing else, in one forward pass: it ignores identifiers,
//! numbers and operators, and stops only at the bytes that can change it.
//!
//! It also tracks delimiter depth, and cuts only at a newline at depth 0 that follows a `;` or a
//! `}`: the start of a top-level item. So no delimiter group crosses a chunk, and no token that
//! decides an attribute count lies on the far side of a cut.
//!
//! # Segments
//!
//! The prescan itself runs in segments that can be scanned independently ([`segment`]) and then
//! composed in order ([`compose`]). A segment assumes it starts between tokens; it skips every
//! literal and comment whole, past its own end if need be, so it always stops between tokens
//! too, at `resume`. Composition checks each segment's assumption against its predecessor's
//! `resume`: where a literal or comment crossed the boundary, the segment is scanned again from
//! there. Depths are relative within a segment and become absolute in composition.
//!
//! Anything the prescan cannot settle exactly (an unterminated literal or comment, a raw string
//! prefix after a non-ASCII byte, a guarded string, an unbalanced closer) makes it return no
//! plan, and the file lexes serially. A wrong cut is impossible by construction; a missed one
//! costs only parallelism.

use alloc::vec::Vec;

use crate::skip;

#[inline(always)]
fn is_ident_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// One prescanned segment.
#[derive(Debug)]
pub(crate) struct Segment {
    /// The nominal start the scan began at.
    start: usize,
    /// Where the scan stopped: at or past the nominal end, between tokens.
    resume: usize,
    /// Delimiter depth at `resume`, relative to the start.
    depth: i64,
    /// The lowest relative depth reached: the file underflows if start depth plus this is negative.
    min_depth: i64,
    /// Lines a chunk may start at, if their absolute depth is 0: (line start, relative depth).
    /// Each follows a `;` or `}` and begins with a token that is not a delimiter or a comment.
    candidates: Vec<(usize, i64)>,
    /// Candidates whose condition depends on the previous segment's last significant byte.
    /// Keep every leading blank line so a chunk grid point can fall between them.
    open_start: Vec<(usize, i64)>,
    /// The last significant byte, or 0 if none.
    last: u8,
    /// Whether the scan met bytes it cannot settle. Fatal only if the segment really started
    /// between tokens; otherwise it is scanned again from where its predecessor stopped.
    failed: bool,
}

/// Scans `src[start..end]` assuming `start` is between tokens; `failed` when the bytes cannot be
/// settled (see the module documentation).
pub(crate) fn segment(src: &[u8], start: usize, end: usize) -> Segment {
    scan(src, start, end).unwrap_or(Segment {
        start,
        resume: start,
        depth: 0,
        min_depth: 0,
        candidates: Vec::new(),
        open_start: Vec::new(),
        last: 0,
        failed: true,
    })
}

fn scan(src: &[u8], start: usize, end: usize) -> Option<Segment> {
    let len = src.len();
    let mut seg = Segment {
        start,
        resume: start,
        depth: 0,
        min_depth: 0,
        candidates: Vec::new(),
        open_start: Vec::new(),
        last: 0,
        failed: false,
    };
    let mut pos = start;
    let mut depth = 0i64;
    // The last significant byte before `mark`; bytes from `mark` to `pos` are all outside the
    // stop set, and only looked at (backwards, from a newline) when `last` is needed.
    let mut last = 0u8;
    let mut mark = start;
    loop {
        pos = skip::next_stop(src, pos, end);
        if pos >= end {
            break;
        }
        match src[pos] {
            b'\n' => {
                last = significant(src, mark, pos, last);
                let line = pos + 1;
                if line < len && starts_item(src, line) {
                    match last {
                        b';' | b'}' => seg.candidates.push((line, depth)),
                        0 => seg.open_start.push((line, depth)),
                        _ => {}
                    }
                }
                pos += 1;
            }
            b @ (b'(' | b'[' | b'{') => {
                depth += 1;
                last = b;
                pos += 1;
            }
            b @ (b')' | b']' | b'}') => {
                depth -= 1;
                seg.min_depth = seg.min_depth.min(depth);
                last = b;
                pos += 1;
            }
            b';' => {
                last = b';';
                pos += 1;
            }
            b'/' => match src.get(pos + 1) {
                Some(b'/') => {
                    last = significant(src, mark, pos, last);
                    pos = memchr::memchr(b'\n', &src[pos..]).map_or(len, |at| pos + at);
                }
                Some(b'*') => {
                    last = significant(src, mark, pos, last);
                    pos = block_comment(src, pos)?;
                }
                _ => {
                    last = b'/';
                    pos += 1;
                }
            },
            b'"' => {
                pos = string(src, pos)?;
                last = b'"';
            }
            b'\'' => {
                pos = quote(src, pos)?;
                last = b'\'';
            }
            _ => unreachable!("only stop bytes reach the match"),
        }
        mark = pos;
    }
    seg.resume = pos;
    seg.depth = depth;
    seg.last = significant(src, mark, pos.min(end), last);
    Some(seg)
}

/// The last non-whitespace byte of `src[from..to]`, or `last` if there is none.
#[inline]
fn significant(src: &[u8], from: usize, to: usize, last: u8) -> u8 {
    match src.get(from..to) {
        Some(run) => run.iter().rev().copied().find(|b| !b.is_ascii_whitespace()).unwrap_or(last),
        None => last,
    }
}

/// Whether the line starting at `line` begins with a token a chunk may start at: not a
/// delimiter (an attribute's `#` or a macro's `!` could be on the far side of the cut) and not
/// a comment.
#[inline]
fn starts_item(src: &[u8], line: usize) -> bool {
    match src[line..].iter().find(|b| !b.is_ascii_whitespace()) {
        Some(b) => !matches!(b, b'(' | b'[' | b'{' | b'/'),
        None => false,
    }
}

/// Chunk starts for `src[begin..]` from its segments, in order, each scanned assuming its
/// nominal start was between tokens; a segment whose assumption fails is scanned again. Cuts
/// follow a grid of `chunk_bytes`: the first valid line at or after each grid point. Empty when
/// the prescan cannot settle the file.
pub(crate) fn compose(
    src: &[u8],
    begin: usize,
    mut segments: Vec<Segment>,
    bounds: &[usize],
    chunk_bytes: usize,
) -> Vec<usize> {
    let mut cuts = Vec::new();
    let mut depth = 0i64;
    let mut last = 0u8;
    let mut resume = begin;
    let mut next_cut = begin + chunk_bytes;
    let len = src.len();
    for (i, seg) in segments.iter_mut().enumerate() {
        let end = bounds[i + 1];
        if resume > seg.start {
            // A literal or comment crossed into this segment: scan it again from where the
            // previous one stopped (or skip it, if that is past its end).
            if resume >= end {
                continue;
            }
            *seg = segment(src, resume, end);
        } else if resume < seg.start {
            // Cannot happen: a scan stops at or past its end.
            return Vec::new();
        }
        if seg.failed {
            return Vec::new();
        }
        if depth + seg.min_depth < 0 {
            return Vec::new();
        }
        for &(line, rel) in &seg.open_start {
            if matches!(last, b';' | b'}')
                && depth + rel == 0
                && line >= next_cut
                && len - line >= chunk_bytes / 2
            {
                cuts.push(line);
                next_cut = line + chunk_bytes;
            }
        }
        for &(line, rel) in &seg.candidates {
            if line >= next_cut && depth + rel == 0 && len - line >= chunk_bytes / 2 {
                cuts.push(line);
                next_cut = line + chunk_bytes;
            }
        }
        depth += seg.depth;
        if seg.last != 0 {
            last = seg.last;
        }
        resume = seg.resume;
    }
    if depth != 0 { Vec::new() } else { cuts }
}

/// The whole prescan in one task: one segment, composed.
pub(crate) fn plan(src: &[u8], begin: usize, chunk_bytes: usize) -> Vec<usize> {
    if src.len() - begin < 2 * chunk_bytes {
        return Vec::new();
    }
    let seg = segment(src, begin, src.len());
    compose(src, begin, alloc::vec![seg], &[begin, src.len()], chunk_bytes)
}

/// Past a nested block comment opening at `pos`.
fn block_comment(src: &[u8], mut pos: usize) -> Option<usize> {
    let mut depth = 1u32;
    pos += 2;
    loop {
        pos += memchr::memchr2(b'/', b'*', src.get(pos..)?)?;
        match (src[pos], src.get(pos + 1)) {
            (b'*', Some(b'/')) => {
                depth -= 1;
                pos += 2;
                if depth == 0 {
                    return Some(pos);
                }
            }
            (b'/', Some(b'*')) => {
                depth += 1;
                pos += 2;
            }
            _ => pos += 1,
        }
    }
}

/// Past a string whose quote is at `quote`: a raw string when an `r`, `br` or `cr` at an
/// identifier start and its `#`s precede the quote, else an escaped one.
fn string(src: &[u8], quote: usize) -> Option<usize> {
    let mut at = quote;
    while at > 0 && src[at - 1] == b'#' {
        at -= 1;
    }
    let hashes = quote - at;
    let raw = at > 0 && src[at - 1] == b'r' && {
        let before = at - 1;
        // `r`, `br` or `cr`, starting an identifier: the byte before the prefix is not part of
        // one. A non-ASCII byte there could be; leave that file serial.
        let prefix =
            if before > 0 && matches!(src[before - 1], b'b' | b'c') { before - 1 } else { before };
        match prefix.checked_sub(1).map(|p| src[p]) {
            None => true,
            Some(p) if p >= 0x80 => return None,
            Some(p) => !is_ident_continue(p),
        }
    };
    if hashes > 0 && !raw {
        // `#"`: a guarded string, reserved syntax.
        return None;
    }
    let mut pos = quote + 1;
    if raw {
        loop {
            pos += memchr::memchr(b'"', src.get(pos..)?)?;
            pos += 1;
            let closing = src[pos..].iter().take(hashes).take_while(|&&b| b == b'#').count();
            if closing == hashes {
                return Some(pos + hashes);
            }
        }
    }
    loop {
        pos += memchr::memchr2(b'"', b'\\', src.get(pos..)?)?;
        if src[pos] == b'"' {
            return Some(pos + 1);
        }
        pos += 2;
    }
}

/// Past a character literal at `pos`, or just past the `'` of a lifetime.
fn quote(src: &[u8], pos: usize) -> Option<usize> {
    let rest = &src[pos + 1..];
    match rest {
        // An escape: skip the backslash and the escaped byte, then find the closing quote
        // (`'\u{1F600}'` is the longest).
        [b'\\', _, ..] => {
            let close = rest[2..].iter().take(12).position(|&b| b == b'\'')?;
            Some(pos + 1 + 2 + close + 1)
        }
        // One ASCII character.
        [c, b'\'', ..] if *c < 0x80 => Some(pos + 3),
        // One non-ASCII character.
        [c, ..] if *c >= 0x80 => {
            let width = match c {
                0xC0..=0xDF => 2,
                0xE0..=0xEF => 3,
                _ => 4,
            };
            if rest.get(width) == Some(&b'\'') { Some(pos + 1 + width + 1) } else { None }
        }
        // A lifetime (or a malformed literal, which the lexer reports).
        _ => Some(pos + 1),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn segmented_blank_lines_keep_chunk_grid_candidates() {
        let src = b"fn a() {}\n\nfn b() {}\n\nfn c() {}\n\nfn d() {}\n";
        for segment_bytes in 1..32 {
            let mut bounds: alloc::vec::Vec<_> = (0..src.len()).step_by(segment_bytes).collect();
            bounds.push(src.len());
            for chunk_bytes in 4..16 {
                let segments = bounds.windows(2).map(|b| super::segment(src, b[0], b[1])).collect();
                assert_eq!(
                    super::compose(src, 0, segments, &bounds, chunk_bytes),
                    super::plan(src, 0, chunk_bytes),
                    "segments {segment_bytes}, chunks {chunk_bytes}"
                );
            }
        }
    }
}
