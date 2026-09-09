//! Detecting usage of the `#[debugger_visualizer]` attribute.

use alloc::vec::Vec;
use crate::rustc_ast::{ItemKind, ast};
use crate::rustc_attr_parsing::AttributeParser;
use crate::rustc_hir::Attribute;
use crate::rustc_hir::attrs::{AttributeKind, DebugVisualizer};
use crate::rustc_middle::middle::debugger_visualizer::DebuggerVisualizerFile;
use crate::rustc_middle::query::{LocalCrate, Providers};
use crate::rustc_middle::ty::TyCtxt;
use crate::rustc_session::Session;
use crate::rustc_span::sym;

use crate::rustc_passes::diagnostics::DebugVisualizerUnreadable;

impl DebuggerVisualizerCollector<'_> {
    fn check_for_debugger_visualizer(&mut self, attrs: &[ast::Attribute]) {
        if let Some(Attribute::Parsed(AttributeKind::DebuggerVisualizer(visualizers))) =
            AttributeParser::parse_limited_sym(&self.sess, attrs, &[sym::debugger_visualizer])
        {
            for DebugVisualizer { span, visualizer_type, path } in visualizers {
                let file = match self.sess.resolve_path(path.as_str(), span) {
                    Ok(file) => file,
                    Err(err) => {
                        err.emit();
                        return;
                    }
                };

                match self.sess.source_map().load_binary_file(&file) {
                    Ok((source, _)) => {
                        self.visualizers.push(DebuggerVisualizerFile::new(
                            source,
                            visualizer_type,
                            file,
                        ));
                    }
                    Err(error) => {
                        self.sess.dcx().emit_err(DebugVisualizerUnreadable {
                            span,
                            file: &file,
                            error,
                        });
                    }
                }
            }
        }
    }
}

struct DebuggerVisualizerCollector<'a> {
    sess: &'a Session,
    visualizers: Vec<DebuggerVisualizerFile>,
}

impl<'ast> crate::rustc_ast::visit::Visitor<'ast> for DebuggerVisualizerCollector<'_> {
    fn visit_item(&mut self, item: &'ast crate::rustc_ast::Item) -> Self::Result {
        if let ItemKind::Mod(..) = item.kind {
            self.check_for_debugger_visualizer(&item.attrs);
        }
        crate::rustc_ast::visit::walk_item(self, item);
    }
    fn visit_crate(&mut self, krate: &'ast ast::Crate) -> Self::Result {
        self.check_for_debugger_visualizer(&krate.attrs);
        crate::rustc_ast::visit::walk_crate(self, krate);
    }
}

/// Traverses and collects the debugger visualizers for a specific crate.
fn debugger_visualizers(tcx: TyCtxt<'_>, _: LocalCrate) -> Vec<DebuggerVisualizerFile> {
    let krate = &tcx.resolver_for_lowering().1;

    let mut visitor = DebuggerVisualizerCollector { sess: tcx.sess, visualizers: Vec::new() };
    crate::rustc_ast::visit::Visitor::visit_crate(&mut visitor, &*krate.borrow());

    // We are collecting visualizers in AST-order, which is deterministic,
    // so we don't need to do any explicit sorting in order to get a
    // deterministic query result
    visitor.visualizers
}

pub(crate) fn provide(providers: &mut Providers) {
    providers.debugger_visualizers = debugger_visualizers;
}
