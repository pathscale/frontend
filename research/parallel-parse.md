# Parsing one file's top-level items in parallel

**Removed.** Measured with the size gate and predicted ids in place, `parse_crate` summed
over this crate's own `src/` (1,424 files, check and analyze) at width 1 against width 12:

| build | width 1 | width 12 |
| --- | ---: | ---: |
| parallel parse on | 1,050 ms | 1,360 ms |
| parallel parse off | 1,035 ms | 1,070 ms |

On `src/` it cost about 290 ms at width 12, because every file there emits diagnostics and
any diagnostic sends a chunk back to the serial parse. On valid source, where chunks do not
fall back, it gained little: `parse_crate` on the clean corpus 271 ms to 261 ms (1.04x), on
the large corpus 53 ms to 40 ms (1.33x), under 1% of either run. Lexing stays serial and a
file splits into few chunks, so there is too little to split. The code is at `c84fa48`
(`src/rustc_parse/parser/item_chunks.rs`) if parsing ever becomes the long pole of a session.
What follows is the design as it was.

Status: source changed, not built or measured by the agents that wrote this (building is the
main session's job). Measured at `6774475` (before the size gate and the predicted ids):
`parse_crate` on `src/` 1,288 ms at width 1 against 1,795 ms at width 12. "Checking it" at the end says what to run.

The problem: `parse_crate` is about 55 ms of the ~200 ms that stays serial at width 12 on the
large clean corpus (six files of 5,000 to 8,000 lines), and about 1.1 s over this crate's own
1,423 files at width one. A file is parsed by one `Parser` on one thread.

The change: `Parser::parse_mod`, for a whole file (`term` is `Eof`), parses its inner attributes
as before and then hands the items to `Parser::parse_items_in_chunks`
(`src/rustc_parse/parser/item_chunks.rs`), which parses them as a stage of chunks and merges the
chunks, or returns `None` having changed nothing, and the serial loop runs as before.

Both entry points go through it with no edit outside `rustc_parse`:

- `rustc_interface::passes::parse` calls `Parser::parse_crate_mod`, which calls
  `parse_mod(exp!(Eof))` (`src/rustc_parse/parser/item.rs`).
- `rustc_expand::module::parse_external_mod` calls `parse_mod(exp!(Eof))` for every out-of-line
  module file, which is most of a crate's files.

`passes.rs` needs no change.

## Files

| File | What changed |
| --- | --- |
| `src/rustc_parse/parser/item_chunks.rs` | New. The size gate, planning (cut points and predicted `AttrId` bases), the stage, a chunk's parse, the merge. |
| `src/rustc_ast/attr/mod.rs` | `AttrIdGenerator::starting_at`, `peek`, `take`. |
| `src/rustc_ast/tokenstream.rs` | `TokenCursor::last_tree`, for the size gate. |
| `src/rustc_parse/parser/item.rs` | `parse_mod` tries `parse_items_in_chunks` after the inner attributes when `term` is `Eof`. |
| `src/rustc_parse/parser/mod.rs` | `mod item_chunks;`. `Parser::new` split into `with_cursor` (the one construction site, no token yet) plus the first bump, so a chunk's parser is built at a token without a second field list. |
| `src/rustc_session/parse.rs` | `ParseSess::part` (a session for one part of a file, its counter at a given id, built without the hygiene lock `with_dcx` takes) and `ParseSess::absorb_part` (the merge, one rule per field). |

## Where the design given for this task was changed, and why

1. **No slicing.** A chunk does not get a `TokenStream` of its own. `TokenStream` is
   `Arc<Vec<TokenTree>>`; a sub-slice as a stream would copy the depth-0 trees (the tokens
   between groups), and `TokenCursor` has no bounded constructor in `rustc_ast`. Instead every
   chunk's parser holds a clone of the parser's own `TokenCursor` (an `Arc` bump and an index)
   positioned on the chunk's first token of the file's one stream. Nothing is copied, and the
   cursor snapshots inside lazy token streams (`LazyAttrTokenStream::Pending`) are the serial
   parser's, same stream, same index.
2. **The cursor is not cut at the chunk's end.** A chunk's parser runs the serial loop and stops
   when, after an item, its current token is the next chunk's first token. Its lookahead past the
   end sees the real tokens, as the serial parser's does, so there is no `Eof` the serial parser
   never saw.
3. **The split is verified by the parser, not trusted.** The rule below only proposes cut points.
   A chunk succeeds only if its items end exactly on the next chunk's first token. By induction
   from chunk 0 (which starts where the serial loop starts, with a clone of the real parser's
   position), each chunk runs the serial parser's steps from the same state, so the cut points
   that survive are points where the serial loop is between two items. A wrong guess costs a
   fallback, never a wrong AST. This is stronger than "only split where the rule is certain":
   certainty comes from the parse itself.
4. **Diagnostics are not merged, they are a failure.** Any diagnostic at all in any chunk sends
   the file to the serial parser with the real session, so a file with diagnostics gets them
   from the serial parser itself, and there is no diagnostic merge to get wrong. A clean file has
   none either way.
5. **`AttrId`s** are the one counter the parser draws from, and a serial id depends on every
   attribute before it in the file. Each chunk's counter starts at a predicted serial position;
   a walk happens only for a chunk after a misprediction (below).

## The split rule

Walk the depth-0 trees after the inner attributes, once, with a clone of the parser's cursor
(`plan_chunks`). A chunk may start at a depth-0 token `t` when

- the tree before `t` is a depth-0 `;` token or a depth-0 `{ ... }` group (the two ways an item
  ends: `use ...;`, `struct S;`, `m!(...);`, `const X: T = ...;`, or `fn`, `impl`, `trait`,
  `mod`, `struct S { }`, `enum`, `union`, `extern { }`, `macro_rules! m { }`, `m! { }`), and
- `t` can begin an item: `#` followed by a `[ ... ]` group (an outer attribute), an outer doc
  comment, one of `async auto const default enum extern fn impl macro macro_rules mod pub safe
  static struct trait type union unsafe use`, or any identifier followed by `!` or `::` (a macro
  call item, `a::b!`), and
- the chunk being closed spans at least `MIN_CHUNK_BYTES` (8 KiB) of source.

Attributes and doc comments stay with the item after them: a cut only happens after `;` or `}`,
so it lands before the first attribute of the next item, never between two. Things the rule
declines on purpose: `}` followed by `else`, `as`, an operator, `>` (a `{ N }` const argument),
`;` (a stray semicolon, an error), anything inside a group. A file containing an invisible
delimiter at depth 0 (never produced by the lexer) is not split. A file that gives fewer than two
chunks is not split.

`MIN_CHUNK_BYTES` is a decision rule (item_chunks.rs has the arithmetic): a chunk costs a
session, a `DiagCtxt` and a stage item, a few microseconds; 8 KiB is a couple of hundred
microseconds of parsing. Files under 16 KiB are never split. Chunks are not sized to the width;
the stage deals its items out to threads.

## A chunk

Run as a stage item (`sync::run_stage`, one item per chunk):

1. A `DiagCtxt` whose emitter only records that it was called (`ChunkEmitter`), with warnings
   enabled so a warning reaches it, and `ParseSess::part` over it: the real session's source map
   (`Arc`) and edition, everything else empty. Both are created inside the item, after the item
   scope opened, so `item_scope` does not capture them (a context newer than the scope emits to
   its own emitter).
2. `Parser::with_cursor` on the chunk's cursor, with the token, spacing and previous token the
   serial parser has there (the previous token is the `;` or the `}` of the tree before).
3. The serial loop, inside `catch_fatal_errors`: stray semicolons, then `parse_item` until the
   next chunk's first token (or, for the last chunk, until `parse_item` gives `None`, and then
   `Eof` must be eaten). The chunk's `AttrId` counter starts at the chunk's predicted serial
   position (`ChunkStart::first_attr_id`). A returned parse error is cancelled and fails the
   chunk.
4. Stashed diagnostics are emitted into the chunk's context; then the chunk is clean only if the
   emitter was never called and `has_errors_or_delayed_bugs` is `None`. A chunk that is not
   clean resets its context (`reset_err_count`, so dropping it reports nothing, delayed bugs
   included) and returns `None`.

The output is owned: the items, where the chunk's counter started and how many ids it took, the parser's bump count over the chunk, for the last chunk the parser's end state, and
the chunk's session.

## When the chunks come back

If any chunk is `None`, every chunk is dropped (the clean ones hold no diagnostics), and
`parse_items_in_chunks` returns `None`: the real parser and session were never touched, and
`parse_mod` runs its loop from the same token. Otherwise, serially, in chunk order:

1. The real `AttrId` counter is advanced by exactly the ids the chunks took (`take`, one
   atomic add); the first one is where the first chunk's serial ids start.
2. Chunk `k` took its ids in the serial order from its predicted base `p(k)`, so its ids are
   the serial ones shifted by `first(k) - p(k)`, where `first(k)` is the first id plus the ids
   the chunks before `k` actually took. The prediction counts `#[`, `#![` and doc comment
   tokens per chunk during planning, skipping groups after `!` (macro input) and after
   `macro_rules ! name` (a macro body). When it is right for every earlier chunk the shift is
   zero and nothing is walked. Otherwise every item of that chunk is walked once
   (`ShiftAttrIds`, a `MutVisitor` on `visit_attribute`) and each id moved by the shift.
   A wrong prediction costs a walk, never a wrong id. Attribute ids appear only on
   `Attribute`s in the AST: with `capture_cfg` off (it is off for a file parse, and the chunked
   path requires it) the lazy token streams hold no `AttrsTarget`, so no id is hidden in tokens.
3. The items are moved onto the result (`Box`es moved, nothing cloned).
4. The chunk's session is absorbed (`ParseSess::absorb_part`, table below).
5. The real parser takes the last chunk's end state (cursor, token, spacing, previous token,
   the bump count summed), which is the state the serial loop ends in: `Eof` eaten, previous
   token `Eof` with the last token's span, so `parse_mod`'s inner span is the serial one.

`NodeId`s: confirmed that the parser gives every AST node `DUMMY_NODE_ID` (the only other ids in
`rustc_parse` are `CRATE_NODE_ID` on buffered lints, a constant). Ids are assigned after
parsing, in expansion, on the merged AST, so they are the serial ones.

## `ParseSess`: every field and its merge rule

| Field | Written by the parser? | Merge (`absorb_part`, chunk order, after what the real session holds) | Why it is the serial result |
| --- | --- | --- | --- |
| `dcx` | Yes, diagnostics | Not merged. A chunk whose context was told anything is discarded and the file is parsed serially. A merged chunk's context is empty. | A clean file emits nothing; a file with a diagnostic is the serial parse. |
| `edition` | Read only | Copied into the part; not merged. | |
| `raw_identifier_spans` | Lexer only (the lexer runs before, on the real session) | Appended in order (empty in practice). | The serial parse pushes in reading order. |
| `bad_unicode_identifiers` | Lexer only | Per symbol, `entry().or_default().extend()` in order (empty in practice). | First occurrence keeps its map position; spans in reading order. |
| `source_map` | Read only (the part shares the `Arc`) | Not merged. | Same map. |
| `buffered_lints` | Yes (`buffer_lint`, `dyn_buffer_lint`) | Appended in order. | Lints are pushed in reading order; chunk `k`'s come after every earlier chunk's. |
| `ambiguous_block_expr_parse` | Yes, and read back within the same statement | `insert` each entry in the part's order. | An `FxIndexMap`: insertion order is its order, and `insert` on an existing key replaces in place as the serial `insert` does. Keys are spans inside one item, so a chunk never needs another chunk's entries. |
| `gated_spans` | Yes (`gate`, and `ungate_last` right after a `gate` in the same item) | Per feature, `entry().or_default().extend()`. | Each feature's spans in reading order. The outer map is an `FxHashMap`, only ever read by `get(feature)` (`rustc_ast_passes::feature_gate`); its bucket layout can differ from the serial one, which nothing can observe. |
| `symbol_gallery` | Lexer only | `entry().or_insert()` in order (empty in practice). | First occurrence wins, as `SymbolGallery::insert`. |
| `attr_id_generator` | Yes (three sites in `parser/attr.rs`) | The part's counter is dropped; the real counter is advanced, and a mispredicted chunk's ids shifted (above). | The ids and the counter's final value are the serial ones. |

Fields named in the task that are not on `ParseSess` in this tree: `reached_eof` does not exist;
`env_depinfo` and `file_depinfo` are on `Session` (`src/rustc_session/session.rs:411-414`) and
are written by `rustc_expand` and `rustc_interface`, never by the parser. `absorb_part`
destructures `ParseSess`, so a field added later does not compile until it has a rule.

Parser state that is not in the session and differs at a chunk's start: `num_bump_calls` (0
instead of the serial count; every use is a difference of two counts, or a macro-matcher
position, which never parses a file), `expected_token_types` (diagnostics only), and
`capture_state.seen_attrs` / `inner_attr_parser_ranges` (the serial parser can carry ids of
earlier items in them; a new attribute's id is never among them either way, so every lookup
answers the same).

## What is not byte-identical, and why it cannot be here

- **Interned spans.** A span longer than 32,766 bytes (a large `impl`, `mod` or `fn`) is stored in
  the session's span interner and the `Span` holds its index. Chunks intern in parallel, so such
  a span's index can differ from a serial run's. `SpanData` is identical and the interner
  deduplicates, so `==` and `Ord` on spans agree with the serial run; only the raw bits (and so
  `Hash`) differ. Every other parallel stage in the compiler already interns spans in parallel.
- **Symbols first interned by the parser.** The lexer interns every identifier and literal
  before any chunk runs. Outside recovery paths the parser interns only the pieces of a split
  tuple index (`a.12.3`, `parser/expr.rs:1089-1114`); a piece not seen before gets its index in
  whichever order the chunks reach it. The string is the same.
- **The parser's leftover capture state** after the parse (`seen_attrs` holds local ids). Nothing
  reads a parser's capture state after `parse_mod` returns.

## When it runs

`parse_items_in_chunks` returns `None` at once unless all of these hold:

- the `parallel` feature and `sync::is_dyn_thread_safe()` (a serial session parses serially,
  with no planning walk);
- the parser is in its ordinary state: no subparser name, `capture_cfg` off, recovery allowed,
  not capturing tokens, no broken token, cursor at depth 0, not at `Eof`;
- the rest of the file spans at least `2 * MIN_CHUNK_BYTES` (16 KiB), from the current token's
  start to the last depth-0 tree's end: checked before any cursor clone or walk. On this crate's
  `src/` that sends 938 of 1,424 files (19.7% of the 28.9 MB) straight to the serial loop;
  the other 486 hold 80.3% of the bytes;
- the session has reported no error (`has_errors`), since a file already known to be broken
  would only fall back.

## Cost that stays serial

The planning walk (one cursor step per depth-0 tree, three for a group, plus a flat scan of each
group's trees to count attribute tokens, linear over the token vectors and far cheaper than the
parse), one atomic add on the real `AttrId` counter, and the merge (moves). No AST walk unless
a prediction was wrong. The previous version walked every item that took an id (nearly every
item: doc comments) serially after the join, one extra full AST traversal per split file, and
took the ids one atomic add at a time.

Not split: one huge item (a big `mod tests { }` or `impl`). Splitting inside an inline module or
an impl block is the same technique one level down and is not done here.

## Unsure it compiles

Written without a build. Worth a look if it does not:

- `sync::run_stage(starts, len, move |starts: &Vec<ChunkStart>, index| ...)`: the closure's
  `'env` is inferred from the captured `&'a ParseSess`.
- `shift.visit_item(&mut item)` with `item: Box<Item>` relies on deref coercion to `&mut Item`.
- The guarded by-move pattern `Ok(Some(ChunkItems { .. })) if clean` in `parse_chunk`.
- `sync::is_dyn_thread_safe()` panics when no session is latched on the thread and the process
  mode was never initialised. Every compile latches one (`interface.rs:354`); a caller that parses
  with the `parallel` feature outside any session and before any session ever ran would hit it.
  `rustc_data_structures::sync::mode::is_parallel_here` is the non-panicking check, but it is
  `pub(super)`.

## Checking it

1. `cargo check --features parallel` and `cargo check` (without the feature the chunked path is
   compiled but returns at the first line).
2. Identity on the large corpus: run `examples/parallel_timing.rs` (or `check_source`) on the six
   large files at width 12 and width 1 and compare diagnostics; pretty-print or `{:?}` the parsed
   crate at both widths and diff (the debug output includes `AttrId`s and spans).
3. A file with a syntax error at width 12 must print exactly what width 1 prints.
4. Time `parse_crate` at width 12 on the six files, before and after.
