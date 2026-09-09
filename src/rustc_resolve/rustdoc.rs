// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::rustc_ast as ast;
use crate::rustc_ast::attr::AttributeExt;

/// Returns whether the first doc-comment is an inner attribute.
///
/// If there are no doc-comments, return true.
/// FIXME(#78591): Support both inner and outer attributes on the same item.
pub fn inner_docs(attrs: &[impl AttributeExt]) -> bool {
    for attr in attrs {
        if let Some(attr_style) = attr.doc_resolution_scope() {
            return attr_style == ast::AttrStyle::Inner;
        }
    }
    true
}

/// Has `#[rustc_doc_primitive]` or `#[doc(keyword)]` or `#[doc(attribute)]`.
pub fn has_primitive_or_keyword_or_attribute_docs(attrs: &[impl AttributeExt]) -> bool {
    for attr in attrs {
        if attr.is_rustc_doc_primitive() || attr.is_doc_keyword_or_attribute() {
            return true;
        }
    }
    false
}

/// Always empty: intra-doc links are a rustdoc feature that this compiler does not implement.
///
/// Upstream this walks the markdown in every doc comment with `pulldown-cmark`, collecting the
/// `[link]` destinations so that the resolver can resolve them for rustdoc's
/// `broken_intra_doc_links` lint. There is no rustdoc in this tree, and this compiler builds no
/// documentation, so the markdown parser was the whole cost of a feature nothing consumes, and
/// it was a dependency on `std`. Returning nothing makes `resolve_doc_links` resolve nothing,
/// which is the correct answer here rather than a degraded one.
pub(crate) fn attrs_to_preprocessed_links(_attrs: &[ast::Attribute]) -> Vec<Box<str>> {
    Vec::new()
}
