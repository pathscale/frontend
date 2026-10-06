//! Edit distances.
//!
//! The [edit distance] is a metric for measuring the difference between two strings.
//!
//! [edit distance]: https://en.wikipedia.org/wiki/Edit_distance

// The current implementation is the restricted Damerau-Levenshtein algorithm. It is restricted
// because it does not permit modifying characters that have already been transposed. The specific
// algorithm should not matter to the caller of the methods, which is why it is not noted in the
// documentation.

// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use core::{cmp, mem};

use smallvec::{SmallVec, smallvec};

use crate::rustc_span::Symbol;

#[cfg(test)]
mod tests;

/// Finds the [edit distance] between two strings.
///
/// Returns `None` if the distance exceeds the limit.
///
/// [edit distance]: https://en.wikipedia.org/wiki/Edit_distance
pub fn edit_distance(a: &str, b: &str, limit: usize) -> Option<usize> {
    // Read in place: an ASCII string's bytes are its chars, so the byte slices give the same
    // distance with nothing copied. Identifiers, which is what this compares nearly every time,
    // are ASCII. Anything else is decoded into chars first.
    if a.is_ascii() && b.is_ascii() {
        return distance(a.as_bytes(), b.as_bytes(), limit);
    }
    // Most candidates fail on length alone, before anything is decoded.
    if a.chars().count().abs_diff(b.chars().count()) > limit {
        return None;
    }
    let a = a.chars().collect::<Vec<_>>();
    let b = b.chars().collect::<Vec<_>>();
    distance(&a, &b, limit)
}

/// [`edit_distance`] over any two sequences of comparable units.
fn distance<T: PartialEq>(a: &[T], b: &[T], limit: usize) -> Option<usize> {
    let (mut a, mut b) = (a, b);

    // Ensure that `b` is the shorter string, minimizing memory use.
    if a.len() < b.len() {
        mem::swap(&mut a, &mut b);
    }

    let min_dist = a.len() - b.len();
    // If we know the limit will be exceeded, we can return early.
    if min_dist > limit {
        return None;
    }

    // Strip common prefix.
    while let Some(((b_char, b_rest), (a_char, a_rest))) = b.split_first().zip(a.split_first())
        && a_char == b_char
    {
        a = a_rest;
        b = b_rest;
    }
    // Strip common suffix.
    while let Some(((b_char, b_rest), (a_char, a_rest))) = b.split_last().zip(a.split_last())
        && a_char == b_char
    {
        a = a_rest;
        b = b_rest;
    }

    // If either string is empty, the distance is the length of the other.
    // We know that `b` is the shorter string, so we don't need to check `a`.
    if b.len() == 0 {
        return Some(min_dist);
    }

    // The rows live on the stack for any identifier-sized string.
    let mut prev_prev: SmallVec<[usize; 32]> = smallvec![usize::MAX; b.len() + 1];
    let mut prev: SmallVec<[usize; 32]> = (0..=b.len()).collect();
    let mut current: SmallVec<[usize; 32]> = smallvec![0; b.len() + 1];

    // The smallest value in the row before `prev`, for the early stop below.
    let mut prev_min = 0;

    // row by row
    for i in 1..=a.len() {
        current[0] = i;
        let mut current_min = i;
        let a_idx = i - 1;

        // column by column
        for j in 1..=b.len() {
            let b_idx = j - 1;

            // There is no cost to substitute a character with itself.
            let substitution_cost = if a[a_idx] == b[b_idx] { 0 } else { 1 };

            current[j] = cmp::min(
                // deletion
                prev[j] + 1,
                cmp::min(
                    // insertion
                    current[j - 1] + 1,
                    // substitution
                    prev[j - 1] + substitution_cost,
                ),
            );

            if (i > 1) && (j > 1) && (a[a_idx] == b[b_idx - 1]) && (a[a_idx - 1] == b[b_idx]) {
                // transposition
                current[j] = cmp::min(current[j], prev_prev[j - 2] + 1);
            }
            current_min = cmp::min(current_min, current[j]);
        }

        // Every cell is built from the two rows above it and the cell to its left, each plus a
        // cost of at least zero, so once two consecutive rows are both entirely over the limit
        // no later cell can come back under it. The answer is `None` either way; this stops at
        // the row that decides it instead of filling the rest of the table.
        if current_min > limit && prev_min > limit {
            return None;
        }
        prev_min = current_min;

        // Rotate the buffers, reusing the memory.
        [prev_prev, prev, current] = [prev, current, prev_prev];
    }

    // `prev` because we already rotated the buffers.
    let distance = prev[b.len()];
    (distance <= limit).then_some(distance)
}

/// Provides a word similarity score between two words that accounts for substrings being more
/// meaningful than a typical edit distance. The lower the score, the closer the match. 0 is an
/// identical match.
///
/// Uses the edit distance between the two strings and removes the cost of the length difference.
/// If this is 0 then it is either a substring match or a full word match, in the substring match
/// case we detect this and return `1`. To prevent finding meaningless substrings, eg. "in" in
/// "shrink", we only perform this subtraction of length difference if one of the words is not
/// greater than twice the length of the other. For cases where the words are close in size but not
/// an exact substring then the cost of the length difference is discounted by half.
///
/// Returns `None` if the distance exceeds the limit.
pub fn edit_distance_with_substrings(a: &str, b: &str, limit: usize) -> Option<usize> {
    let n = a.chars().count();
    let m = b.chars().count();

    // Check one isn't less than half the length of the other. If this is true then there is a
    // big difference in length.
    let big_len_diff = (n * 2) < m || (m * 2) < n;
    let len_diff = m.abs_diff(n);
    let distance = edit_distance(a, b, limit + len_diff)?;

    // This is the crux, subtracting length difference means exact substring matches will now be 0
    let score = distance - len_diff;

    // If the score is 0 but the words have different lengths then it's a substring match not a full
    // word match
    let score = if score == 0 && len_diff > 0 && !big_len_diff {
        1 // Exact substring match, but not a total word match so return non-zero
    } else if !big_len_diff {
        // Not a big difference in length, discount cost of length difference
        score + len_diff.div_ceil(2)
    } else {
        // A big difference in length, add back the difference in length to the score
        score + len_diff
    };

    (score <= limit).then_some(score)
}

/// Finds the best match for given word in the given iterator where substrings are meaningful.
///
/// A version of [`find_best_match_for_name`] that uses [`edit_distance_with_substrings`] as the
/// score for word similarity. This takes an optional distance limit which defaults to one-third of
/// the given word.
///
/// We use case insensitive comparison to improve accuracy on an edge case with a lower(upper)case
/// letters mismatch.
pub fn find_best_match_for_name_with_substrings(
    candidates: &[Symbol],
    lookup: Symbol,
    dist: Option<usize>,
) -> Option<Symbol> {
    find_best_match_for_name_impl(true, candidates, lookup, dist)
}

/// Finds the best match for a given word in the given iterator.
///
/// As a loose rule to avoid the obviously incorrect suggestions, it takes
/// an optional limit for the maximum allowable edit distance, which defaults
/// to one-third of the given word.
///
/// We use case insensitive comparison to improve accuracy on an edge case with a lower(upper)case
/// letters mismatch.
pub fn find_best_match_for_name(
    candidates: &[Symbol],
    lookup: Symbol,
    dist: Option<usize>,
) -> Option<Symbol> {
    find_best_match_for_name_impl(false, candidates, lookup, dist)
}

/// The index [`find_best_match_for_name`] would lead to if `texts` were first stable-sorted by
/// text, without sorting.
///
/// Callers that want a deterministic suggestion used to sort every candidate by its text, run
/// [`find_best_match_for_name`] over the sorted names, and take the first sorted entry with the
/// name it returned. That is `O(n log n)` string comparisons per unresolved name, over every
/// name in scope, plus two copies of the list. This computes the same entry in one pass:
///
/// - a case-insensitive exact match: the smallest such text;
/// - otherwise the smallest edit distance within the default limit, ties to the smallest text;
/// - otherwise a match by sorted words: the largest such text, as the sorted fold kept the last;
///
/// and among entries with the chosen text, the lowest index, which is where a stable sort left
/// the first of them. `texts` are the candidates' texts, read once by the caller.
pub fn find_best_match_index_as_if_sorted(texts: &[&str], lookup: &str) -> Option<usize> {
    let lookup_is_ascii = lookup.is_ascii();
    let lookup_uppercase = lookup.to_uppercase();
    let mut best: Option<usize> = None;
    for (i, text) in texts.iter().enumerate() {
        let same = if lookup_is_ascii && text.is_ascii() {
            text.eq_ignore_ascii_case(lookup)
        } else {
            text.chars().flat_map(char::to_uppercase).eq(lookup_uppercase.chars())
        };
        if same && best.is_none_or(|b| *text < texts[b]) {
            best = Some(i);
        }
    }
    if best.is_some() {
        return best;
    }

    let limit = cmp::max(lookup.chars().count(), 3) / 3;
    let mut closest: Option<(usize, usize)> = None;
    for (i, text) in texts.iter().enumerate() {
        let within = closest.map_or(limit, |(d, _)| d);
        if let Some(d) = edit_distance(lookup, text, within)
            && closest.is_none_or(|(bd, b)| d < bd || (d == bd && *text < texts[b]))
        {
            closest = Some((d, i));
        }
    }
    if let Some((_, i)) = closest {
        return Some(i);
    }

    let lookup_sorted_by_words = sort_by_words(lookup);
    let mut words: Option<usize> = None;
    for (i, text) in texts.iter().enumerate() {
        if text.len() == lookup.len()
            && sort_by_words(text) == lookup_sorted_by_words
            && words.is_none_or(|b| *text > texts[b])
        {
            words = Some(i);
        }
    }
    words
}

/// Find the best match for multiple words
///
/// This function is intended for use when the desired match would never be
/// returned due to a substring in `lookup` which is superfluous.
///
/// For example, when looking for the closest lint name to `clippy:missing_docs`,
/// we would find `clippy::erasing_op`, despite `missing_docs` existing and being a better suggestion.
/// `missing_docs` would have a larger edit distance because it does not contain the `clippy` tool prefix.
/// In order to find `missing_docs`, this function takes multiple lookup strings, computes the best match
/// for each and returns the match which had the lowest edit distance. In our example, `clippy:missing_docs` and
/// `missing_docs` would be `lookups`, enabling `missing_docs` to be the best match, as desired.
pub fn find_best_match_for_names(
    candidates: &[Symbol],
    lookups: &[Symbol],
    dist: Option<usize>,
) -> Option<Symbol> {
    lookups
        .iter()
        .map(|s| (s, find_best_match_for_name_impl(false, candidates, *s, dist)))
        .filter_map(|(s, r)| r.map(|r| (s, r)))
        .min_by(|(s1, r1), (s2, r2)| {
            let d1 = edit_distance(s1.as_str(), r1.as_str(), usize::MAX).unwrap();
            let d2 = edit_distance(s2.as_str(), r2.as_str(), usize::MAX).unwrap();
            d1.cmp(&d2)
        })
        .map(|(_, r)| r)
}

#[cold]
fn find_best_match_for_name_impl(
    use_substring_score: bool,
    candidates: &[Symbol],
    lookup_symbol: Symbol,
    dist: Option<usize>,
) -> Option<Symbol> {
    let lookup = lookup_symbol.as_str();
    let lookup_uppercase = lookup.to_uppercase();

    // Priority of matches:
    // 1. Exact case insensitive match
    // 2. Edit distance match
    // 3. Sorted word match
    // `str::to_uppercase` is each char's `to_uppercase`, joined, so comparing the two char
    // streams is the same test without allocating an uppercase copy of every candidate.
    // For two ASCII strings, uppercasing each char is exactly an ASCII case-insensitive compare.
    let lookup_is_ascii = lookup.is_ascii();
    // Every candidate's text, read out of the interner once for all three passes below.
    let texts: Vec<&str> = candidates.iter().map(|c| c.as_str()).collect();
    if let Some((c, _)) = candidates.iter().zip(&texts).find(|(_, c)| {
        let c: &str = c;
        if lookup_is_ascii && c.is_ascii() {
            c.eq_ignore_ascii_case(lookup)
        } else {
            c.chars().flat_map(char::to_uppercase).eq(lookup_uppercase.chars())
        }
    }) {
        return Some(*c);
    }

    // `fn edit_distance()` use `chars()` to calculate edit distance, so we must
    // also use `chars()` (and not `str::len()`) to calculate length here.
    let lookup_len = lookup.chars().count();

    let mut dist = dist.unwrap_or_else(|| cmp::max(lookup_len, 3) / 3);
    let mut best = None;
    // store the candidates with the same distance, only for `use_substring_score` current.
    let mut next_candidates = vec![];
    for (c, text) in candidates.iter().zip(&texts) {
        match if use_substring_score {
            edit_distance_with_substrings(lookup, text, dist)
        } else {
            edit_distance(lookup, text, dist)
        } {
            Some(0) => return Some(*c),
            Some(d) => {
                if use_substring_score {
                    if d < dist {
                        dist = d;
                        next_candidates.clear();
                    } else {
                        // `d == dist` here, we need to store the candidates with the same distance
                        // so we won't decrease the distance in the next loop.
                    }
                    next_candidates.push(*c);
                } else {
                    dist = d - 1;
                }
                best = Some(*c);
            }
            None => {}
        }
    }

    // We have a tie among several candidates, try to select the best among them ignoring substrings.
    // For example, the candidates list `force_capture`, `capture`, and user inputted `forced_capture`,
    // we select `force_capture` with a extra round of edit distance calculation.
    if next_candidates.len() > 1 {
        debug_assert!(use_substring_score);
        best = find_best_match_for_name_impl(
            false,
            &next_candidates,
            lookup_symbol,
            Some(lookup.len()),
        );
    }
    if best.is_some() {
        return best;
    }

    find_match_by_sorted_words(candidates, &texts, lookup)
}

fn find_match_by_sorted_words(iter_names: &[Symbol], texts: &[&str], lookup: &str) -> Option<Symbol> {
    let lookup_sorted_by_words = sort_by_words(lookup);
    iter_names.iter().zip(texts).fold(None, |result, (candidate, text)| {
        // Equal sorted words means the same words with the same separators, so the same length.
        // Checked first, so a candidate that cannot match is not split and sorted.
        if text.len() == lookup.len()
            && sort_by_words(text) == lookup_sorted_by_words
        {
            Some(*candidate)
        } else {
            result
        }
    })
}

fn sort_by_words(name: &str) -> Vec<&str> {
    let mut split_words: Vec<&str> = name.split('_').collect();
    // We are sorting primitive &strs and can use unstable sort here.
    split_words.sort_unstable();
    split_words
}
