crate::rustc_index::newtype_index! {
    #[orderable]
    #[max = 0xFFFF_FFFA]
    struct MyIdx {}
}

// Upstream asserts that `Option<MyIdx>` nests five deep in four bytes, using the values above
// `MAX` as niches. That needs a pattern type, which is nightly-only, so here the index is a plain
// `u32` with no niche and the first `Option` is what adds a tag (see `index_newtype.rs` in
// `frontend_macros`). The test pins that layout instead.
#[test]
fn index_size_is_optimized() {
    assert_eq!(size_of::<MyIdx>(), 4);
    assert_eq!(size_of::<Option<MyIdx>>(), 8);
}

// Upstream's four `range_*` tests iterated `MyIdx..MyIdx`, which needs `impl Step`, which is
// nightly-only (`step_trait`). The macro does not emit it, so those tests are gone rather than
// ported; iterate `(a.index()..b.index()).map(MyIdx::from_usize)` instead.
