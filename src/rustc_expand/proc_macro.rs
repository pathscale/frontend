use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::rustc_ast as ast;
use crate::rustc_ast::token::Token;
use crate::rustc_ast::tokenstream::{DelimSpan, Spacing, TokenStream, TokenTree};
use crate::rustc_data_structures::profiling::TimingGuard;
use crate::rustc_errors::ErrorGuaranteed;
use crate::rustc_middle::ty::{self, TyCtxt};
use crate::rustc_parse::parser::{AllowConstBlockItems, ForceCollect, Parser};
use crate::rustc_proc_macro as pm;
use crate::rustc_session::Session;
use crate::rustc_session::config::ProcMacroExecutionStrategy;
use crate::rustc_span::hygiene::Transparency;
use crate::rustc_span::profiling::SpannedEventArgRecorder;
use crate::rustc_span::{FileName, Ident, LocalExpnId, Span, Symbol};
use rustc_macros::{BlobDecodable, Encodable};

use crate::rustc_expand::base::{self, *};
use crate::rustc_expand::{diagnostics, mbe, proc_macro_server};
use crate::rustc_expand::proc_macro_schema::{
    ExactPart, ProcMacroSchema, RewrittenSchemaTemplate, SchemaBindingSource, SchemaGuard,
    SchemaRule, SchemaVariable,
};

fn exec_strategy(sess: &Session) -> impl pm::bridge::server::ExecutionStrategy + 'static {
    pm::bridge::server::MaybeCrossThread {
        cross_thread: sess.opts.unstable_opts.proc_macro_execution_strategy
            == ProcMacroExecutionStrategy::CrossThread,
    }
}

fn record_expand_proc_macro<'a>(
    ecx: &ExtCtxt<'a>,
    name: &'static str,
    span: Span,
) -> TimingGuard<'a> {
    ecx.sess.prof.generic_activity_with_arg_recorder(name, |recorder| {
        recorder.record_arg_with_span(ecx.sess.source_map(), ecx.expansion_descr(), span);
    })
}

/// A proc macro read from source. Its declaration can be resolved, but its output is unknown.
#[derive(Encodable, BlobDecodable)]
pub enum ProcMacroEntryBody {
    Call { path: alloc::string::String },
    Parse { ty: alloc::string::String },
    Match,
    QuoteEmpty,
    Quote,
    Unclassified,
}

pub struct UnrunProcMacro {
    pub name: crate::rustc_span::Symbol,
    pub entry_body: ProcMacroEntryBody,
    pub schema: ProcMacroSchema,
}

impl UnrunProcMacro {
    fn refuse(&self, ecx: &ExtCtxt<'_>, span: Span) -> ErrorGuaranteed {
        ecx.dcx().span_err(
            span,
            alloc::format!("proc macro `{}` was not expanded: its output is unknown", self.name),
        )
    }

    fn transcribe_schema(
        &self,
        ecx: &mut ExtCtxt<'_>,
        span: Span,
        arguments: &[TokenStream],
        item: Option<&ast::Item>,
    ) -> Result<Option<TokenStream>, ErrorGuaranteed> {
        let ProcMacroSchema::Template { rules } = &self.schema else {
            return Ok(None);
        };
        let [rule] = rules.as_slice() else { return Ok(None) };
        let Some(rewritten) = crate::rustc_expand::proc_macro_schema::rewrite_template(rule) else {
            return Ok(None);
        };
        let Some(bindings) = schema_bindings(ecx, rule, &rewritten, arguments, item) else {
            return Ok(None);
        };
        let Some(rhs) = schema_tokens(ecx, &rewritten.source) else {
            return Ok(None);
        };
        let features = ecx.ecfg.features;
        let expansion = ecx.current_expansion.id;
        let output = mbe::transcribe_schema(
            ecx.sess,
            rhs,
            &bindings,
            features,
            ecx.sess.edition(),
            span,
            Transparency::Transparent,
            expansion,
        )?;
        if output.is_some() {
            ecx.sess.mark_schema_transcriber_expansion(expansion);
            if rewritten
                .variables
                .iter()
                .any(|variable| {
                    matches!(
                        &variable.hole,
                        crate::rustc_expand::proc_macro_schema::SchemaHole::Opaque { .. }
                    )
                })
            {
                ecx.sess.record_loss();
            }
        }
        Ok(output)
    }
}

struct SchemaItem<'a> {
    ident: Ident,
    vis: &'a ast::Visibility,
    generics: &'a ast::Generics,
    fields: Vec<&'a ast::FieldDef>,
}

fn schema_item(item: &ast::Item) -> Option<SchemaItem<'_>> {
    let (ident, generics, fields) = match &item.kind {
        ast::ItemKind::Struct(ident, generics, data)
        | ast::ItemKind::Union(ident, generics, data) => {
            (*ident, generics, data.fields().iter().collect())
        }
        ast::ItemKind::Enum(ident, generics, data) => {
            let fields = data
                .variants
                .iter()
                .flat_map(|variant| variant.data.fields().iter())
                .collect();
            (*ident, generics, fields)
        }
        _ => return None,
    };
    Some(SchemaItem { ident, vis: &item.vis, generics, fields })
}

fn item_from_annotatable(item: &Annotatable) -> Option<&ast::Item> {
    match item {
        Annotatable::Item(item) => Some(item),
        Annotatable::Stmt(stmt) => match &stmt.kind {
            ast::StmtKind::Item(item) => Some(item),
            _ => None,
        },
        _ => None,
    }
}

fn parse_schema_item(ecx: &ExtCtxt<'_>, tokens: &TokenStream) -> Option<ast::Item> {
    let mut parser = Parser::new(&ecx.sess.psess, tokens.clone(), Some("proc-macro input"));
    match parser.parse_item(ForceCollect::No, AllowConstBlockItems::Yes) {
        Ok(Some(item)) => Some(*item),
        Ok(None) => None,
        Err(error) => {
            error.cancel();
            None
        }
    }
}

fn schema_tokens(ecx: &ExtCtxt<'_>, source: &str) -> Option<TokenStream> {
    match crate::rustc_parse::source_str_to_stream(
        &ecx.sess.psess,
        FileName::anon_source_code(source),
        source.to_string(),
        None,
    ) {
        Ok(tokens) => Some(tokens),
        Err(errors) => {
            errors.into_iter().for_each(|error| error.cancel());
            None
        }
    }
}

fn ident_tokens(ident: Ident) -> TokenStream {
    TokenStream::new(vec![TokenTree::Token(Token::from_ast_ident(ident), Spacing::Alone)])
}

fn generic_parts(ecx: &ExtCtxt<'_>, generics: &ast::Generics) -> Option<[String; 3]> {
    let impl_params = if generics.params.is_empty() {
        Vec::new()
    } else {
        let source = ecx.sess.source_map().span_to_snippet(generics.span).ok()?;
        let parameters = crate::rustc_expand::proc_macro_schema::generic_parameter_sources(&source)?;
        if parameters.len() != generics.params.len() {
            return None;
        }
        parameters
            .iter()
            .map(|parameter| crate::rustc_expand::proc_macro_schema::remove_generic_default(parameter))
            .collect()
    };
    let type_params = generics
        .params
        .iter()
        .map(|param| param.ident.to_string())
        .collect::<Vec<_>>();
    let impl_generics = if generics.params.is_empty() {
        String::new()
    } else {
        format!("<{}>", impl_params.join(", "))
    };
    let type_generics = if generics.params.is_empty() {
        String::new()
    } else {
        format!("<{}>", type_params.join(", "))
    };
    let where_clause = if generics.where_clause.is_empty() {
        String::new()
    } else {
        ecx.sess
            .source_map()
            .span_to_snippet(generics.where_clause.span)
            .ok()?
    };
    Some([impl_generics, type_generics, where_clause])
}

fn repeated_static_value(
    value: TokenStream,
    variable: &SchemaVariable,
    rule: &SchemaRule,
    field_count: usize,
) -> Option<Vec<TokenStream>> {
    if !variable.repeated {
        return Some(vec![value]);
    }
    if rule.loop_repetition {
        return Some(vec![value; field_count]);
    }
    None
}

fn schema_fields<'a>(rule: &SchemaRule, item: &SchemaItem<'a>) -> Vec<&'a ast::FieldDef> {
    let mut fields = item.fields.clone();
    if let Some(SchemaGuard::AttributeAbsent { path }) = &rule.guard {
        let name = Symbol::intern(path);
        fields.retain(|field| !field.attrs.iter().any(|attribute| attribute.has_name(name)));
    }
    fields
}

fn schema_bindings(
    ecx: &ExtCtxt<'_>,
    rule: &SchemaRule,
    rewritten: &RewrittenSchemaTemplate,
    arguments: &[TokenStream],
    input_item: Option<&ast::Item>,
) -> Option<Vec<mbe::SchemaBinding>> {
    let item = input_item.and_then(schema_item);
    if rule.guard.is_some() && item.is_none() {
        return None;
    }
    let fields = item.as_ref().map(|item| schema_fields(rule, item)).unwrap_or_default();
    let field_count = fields.len();
    let mut variables = Vec::<SchemaVariable>::new();
    for variable in &rewritten.variables {
        if matches!(
            &variable.hole,
            crate::rustc_expand::proc_macro_schema::SchemaHole::Opaque { .. }
        ) {
            continue;
        }
        if let Some(previous) = variables.iter().find(|previous| previous.name == variable.name) {
            if previous.hole != variable.hole
                || previous.source != variable.source
                || previous.repeated != variable.repeated
            {
                return None;
            }
        } else {
            variables.push(variable.clone());
        }
    }

    variables
        .into_iter()
        .map(|variable| {
            let crate::rustc_expand::proc_macro_schema::SchemaHole::Exact(part) = &variable.hole else {
                return None;
            };
            let values = match part {
                ExactPart::InputTokens => {
                    if variable.source == Some(SchemaBindingSource::Item) {
                        let item_parameter = rule.parameters.len().checked_sub(1)?;
                        let input = arguments.get(item_parameter)?.clone();
                        repeated_static_value(input, &variable, rule, field_count)?
                    } else {
                        let index = match variable.source {
                            Some(SchemaBindingSource::Parameter(index)) => index,
                            _ => rule.parameters.iter().position(|parameter| parameter == &variable.name)?,
                        };
                        let input = arguments.get(index)?.clone();
                        if variable.repeated && !rule.loop_repetition {
                            input
                                .iter()
                                .cloned()
                                .map(|tree| TokenStream::new(vec![tree]))
                                .collect()
                        } else {
                            repeated_static_value(input, &variable, rule, field_count)?
                        }
                    }
                }
                ExactPart::ItemIdent => {
                    let item = item.as_ref()?;
                    repeated_static_value(ident_tokens(item.ident), &variable, rule, field_count)?
                }
                ExactPart::Vis => {
                    let item = item.as_ref()?;
                    let visibility = crate::rustc_ast_pretty::pprust::vis_to_string(item.vis);
                    let visibility = schema_tokens(ecx, &visibility)?;
                    repeated_static_value(visibility, &variable, rule, field_count)?
                }
                ExactPart::Generics { kind } => {
                    let item = item.as_ref()?;
                    let generics = generic_parts(ecx, item.generics)?;
                    let index = match kind.as_str() {
                        "impl" => 0,
                        "type" => 1,
                        "where" => 2,
                        _ => return None,
                    };
                    let generics = schema_tokens(ecx, &generics[index])?;
                    repeated_static_value(generics, &variable, rule, field_count)?
                }
                ExactPart::FieldIdent => {
                    if !variable.repeated {
                        return None;
                    }
                    fields
                        .iter()
                        .map(|field| field.ident.map(ident_tokens))
                        .collect::<Option<Vec<_>>>()?
                }
                ExactPart::NameTemplate { prefix, suffix } => {
                    let names = match variable.source {
                        Some(SchemaBindingSource::Item) => {
                            vec![item.as_ref()?.ident.name.as_str().to_string()]
                        }
                        Some(SchemaBindingSource::Field) => fields
                            .iter()
                            .map(|field| field.ident.map(|ident| ident.name.as_str().to_string()))
                            .collect::<Option<Vec<_>>>()?,
                        None => vec![String::new()],
                        Some(SchemaBindingSource::Parameter(_)) => return None,
                    };
                    if variable.repeated
                        && !rule.loop_repetition
                        && variable.source != Some(SchemaBindingSource::Field)
                    {
                        return None;
                    }
                    let values = names
                        .into_iter()
                        .map(|name| schema_tokens(ecx, &format!("{prefix}{name}{suffix}")))
                        .collect::<Option<Vec<_>>>()?;
                    if variable.repeated
                        && rule.loop_repetition
                        && variable.source != Some(SchemaBindingSource::Field)
                    {
                        let [value] = values.as_slice() else { return None };
                        vec![value.clone(); field_count]
                    } else {
                        values
                    }
                }
            };
            Some(mbe::SchemaBinding {
                name: variable.name,
                values,
                repeated: variable.repeated,
            })
        })
        .collect()
}

fn mark_attr_tokens(tokens: TokenStream, expansion: LocalExpnId) -> TokenStream {
    let expn_id = expansion.to_expn_id();
    let mark = |span: Span| span.apply_mark(expn_id, Transparency::Transparent);
    TokenStream::new(
        tokens
            .iter()
            .map(|tree| match tree {
                TokenTree::Token(token, spacing) => {
                    let mut token = *token;
                    token.span = mark(token.span);
                    TokenTree::Token(token, *spacing)
                }
                TokenTree::Delimited(span, spacing, delimiter, stream) => TokenTree::Delimited(
                    DelimSpan::from_pair(mark(span.open), mark(span.close)),
                    *spacing,
                    *delimiter,
                    mark_attr_tokens(stream.clone(), expansion),
                ),
            })
            .collect(),
    )
}

impl base::BangProcMacro for UnrunProcMacro {
    fn expand(
        &self,
        ecx: &mut ExtCtxt<'_>,
        span: Span,
        input: TokenStream,
    ) -> Result<TokenStream, ErrorGuaranteed> {
        if let Some(output) = self.transcribe_schema(ecx, span, &[input], None)? {
            return Ok(output);
        }
        Err(self.refuse(ecx, span))
    }
}

impl base::AttrProcMacro for UnrunProcMacro {
    fn expand(
        &self,
        ecx: &mut ExtCtxt<'_>,
        span: Span,
        annotation: TokenStream,
        annotated: TokenStream,
    ) -> Result<TokenStream, ErrorGuaranteed> {
        let parsed_item = parse_schema_item(ecx, &annotated);
        let arguments = [annotation, annotated.clone()];
        if let Some(output) = self.transcribe_schema(
            ecx,
            span,
            &arguments,
            parsed_item.as_ref(),
        )? {
            return Ok(output);
        }
        ecx.sess.record_loss();
        let _ = self.refuse(ecx, span);
        Ok(mark_attr_tokens(annotated, ecx.current_expansion.id))
    }
}

impl MultiItemModifier for UnrunProcMacro {
    fn expand(
        &self,
        ecx: &mut ExtCtxt<'_>,
        span: Span,
        _meta_item: &ast::MetaItem,
        item: Annotatable,
        _is_derive_const: bool,
    ) -> ExpandResult<Vec<Annotatable>, Annotatable> {
        let is_stmt = matches!(item, Annotatable::Stmt(..));
        let input = item.to_tokens();
        let output = match self.transcribe_schema(
            ecx,
            span,
            &[input],
            item_from_annotatable(&item),
        ) {
            Ok(output) => output,
            Err(_) => return ExpandResult::Ready(vec![]),
        };
        if let Some(output) = output {
            let error_count_before = ecx.dcx().err_count();
            let mut parser = Parser::new(&ecx.sess.psess, output, Some("proc-macro derive schema"));
            let mut items = vec![];
            loop {
                match parser.parse_item(
                    ForceCollect::No,
                    if is_stmt { AllowConstBlockItems::No } else { AllowConstBlockItems::Yes },
                ) {
                    Ok(None) => break,
                    Ok(Some(item)) => {
                        if is_stmt {
                            items.push(Annotatable::Stmt(Box::new(ecx.stmt_item(span, item))));
                        } else {
                            items.push(Annotatable::Item(item));
                        }
                    }
                    Err(error) => {
                        error.emit();
                        break;
                    }
                }
            }
            if ecx.dcx().err_count() > error_count_before {
                ecx.dcx().emit_err(diagnostics::ProcMacroDeriveTokens { span });
            }
            return ExpandResult::Ready(items);
        }
        ecx.sess.record_loss();
        let _ = self.refuse(ecx, span);
        ExpandResult::Ready(vec![])
    }
}

pub struct BangProcMacro {
    pub client: pm::bridge::client::Client,
}

impl base::BangProcMacro for BangProcMacro {
    fn expand(
        &self,
        ecx: &mut ExtCtxt<'_>,
        span: Span,
        input: TokenStream,
    ) -> Result<TokenStream, ErrorGuaranteed> {
        let _timer = record_expand_proc_macro(ecx, "expand_proc_macro", span);

        let proc_macro_backtrace = ecx.ecfg.proc_macro_backtrace;
        let strategy = exec_strategy(ecx.sess);
        let server = proc_macro_server::Rustc::new(ecx);
        self.client.run1(&strategy, server, input, proc_macro_backtrace).map_err(|e| {
            ecx.dcx().emit_err(diagnostics::ProcMacroPanicked {
                span,
                message: e
                    .into_string()
                    .map(|message| diagnostics::ProcMacroPanickedHelp { message }),
            })
        })
    }
}

pub struct AttrProcMacro {
    pub client: pm::bridge::client::Client,
}

impl base::AttrProcMacro for AttrProcMacro {
    fn expand(
        &self,
        ecx: &mut ExtCtxt<'_>,
        span: Span,
        annotation: TokenStream,
        annotated: TokenStream,
    ) -> Result<TokenStream, ErrorGuaranteed> {
        let _timer = record_expand_proc_macro(ecx, "expand_proc_macro", span);

        let proc_macro_backtrace = ecx.ecfg.proc_macro_backtrace;
        let strategy = exec_strategy(ecx.sess);
        let server = proc_macro_server::Rustc::new(ecx);
        self.client.run2(&strategy, server, annotation, annotated, proc_macro_backtrace).map_err(
            |e| {
                ecx.dcx().emit_err(diagnostics::CustomAttributePanicked {
                    span,
                    message: e
                        .into_string()
                        .map(|message| diagnostics::CustomAttributePanickedHelp { message }),
                })
            },
        )
    }
}

pub struct DeriveProcMacro {
    pub client: DeriveClient,
}

impl MultiItemModifier for DeriveProcMacro {
    fn expand(
        &self,
        ecx: &mut ExtCtxt<'_>,
        span: Span,
        _meta_item: &ast::MetaItem,
        item: Annotatable,
        _is_derive_const: bool,
    ) -> ExpandResult<Vec<Annotatable>, Annotatable> {
        let _timer = record_expand_proc_macro(ecx, "expand_derive_proc_macro_outer", span);

        // We need special handling for statement items
        // (e.g. `fn foo() { #[derive(Debug)] struct Bar; }`)
        let is_stmt = matches!(item, Annotatable::Stmt(..));

        let input = item.to_tokens();

        let invoc_id = ecx.current_expansion.id;

        let res = if ecx.sess.opts.incremental.is_some()
            && ecx.sess.opts.unstable_opts.cache_proc_macros
        {
            ty::tls::with(|tcx| {
                let input = &*tcx.arena.alloc(input);
                let key: (LocalExpnId, &TokenStream) = (invoc_id, input);

                QueryDeriveExpandCtx::enter(ecx, self.client, move || {
                    tcx.derive_macro_expansion(key).cloned()
                })
            })
        } else {
            expand_derive_macro(invoc_id, input, ecx, self.client)
        };

        let Ok(output) = res else {
            // error will already have been emitted
            return ExpandResult::Ready(vec![]);
        };

        let error_count_before = ecx.dcx().err_count();
        let mut parser = Parser::new(&ecx.sess.psess, output, Some("proc-macro derive"));
        let mut items = vec![];

        loop {
            match parser.parse_item(
                ForceCollect::No,
                if is_stmt { AllowConstBlockItems::No } else { AllowConstBlockItems::Yes },
            ) {
                Ok(None) => break,
                Ok(Some(item)) => {
                    if is_stmt {
                        items.push(Annotatable::Stmt(Box::new(ecx.stmt_item(span, item))));
                    } else {
                        items.push(Annotatable::Item(item));
                    }
                }
                Err(err) => {
                    err.emit();
                    break;
                }
            }
        }

        // fail if there have been errors emitted
        if ecx.dcx().err_count() > error_count_before {
            ecx.dcx().emit_err(diagnostics::ProcMacroDeriveTokens { span });
        }

        ExpandResult::Ready(items)
    }
}

/// Provide a query for computing the output of a derive macro.
pub(super) fn provide_derive_macro_expansion<'tcx>(
    tcx: TyCtxt<'tcx>,
    key: (LocalExpnId, &'tcx TokenStream),
) -> Result<&'tcx TokenStream, ()> {
    let (invoc_id, input) = key;

    // Make sure that we invalidate the query when the crate defining the proc macro changes
    let _ = tcx.crate_hash(invoc_id.expn_data().macro_def_id.unwrap().krate);

    QueryDeriveExpandCtx::with(|ecx, client| {
        expand_derive_macro(invoc_id, input.clone(), ecx, client).map(|ts| &*tcx.arena.alloc(ts))
    })
}

type DeriveClient = pm::bridge::client::Client;

fn expand_derive_macro(
    invoc_id: LocalExpnId,
    input: TokenStream,
    ecx: &mut ExtCtxt<'_>,
    client: DeriveClient,
) -> Result<TokenStream, ()> {
    let _timer =
        ecx.sess.prof.generic_activity_with_arg_recorder("expand_proc_macro", |recorder| {
            let invoc_expn_data = invoc_id.expn_data();
            let span = invoc_expn_data.call_site;
            let event_arg = invoc_expn_data.kind.descr();
            recorder.record_arg_with_span(ecx.sess.source_map(), event_arg, span);
        });

    let proc_macro_backtrace = ecx.ecfg.proc_macro_backtrace;
    let strategy = exec_strategy(ecx.sess);
    let server = proc_macro_server::Rustc::new(ecx);

    match client.run1(&strategy, server, input, proc_macro_backtrace) {
        Ok(stream) => Ok(stream),
        Err(e) => {
            let invoc_expn_data = invoc_id.expn_data();
            let span = invoc_expn_data.call_site;
            ecx.dcx().emit_err({
                diagnostics::ProcMacroDerivePanicked {
                    span,
                    message: e
                        .into_string()
                        .map(|message| diagnostics::ProcMacroDerivePanickedHelp { message }),
                }
            });
            Err(())
        }
    }
}

/// Stores the context necessary to expand a derive proc macro via a query.
struct QueryDeriveExpandCtx {
    /// Type-erased version of `&mut ExtCtxt`
    expansion_ctx: *mut (),
    client: DeriveClient,
}

impl QueryDeriveExpandCtx {
    /// Store the extension context and the client into the thread local value.
    /// It will be accessible via the `with` method while `f` is active.
    fn enter<F, R>(ecx: &mut ExtCtxt<'_>, client: DeriveClient, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        // We need erasure to get rid of the lifetime
        let ctx = Self { expansion_ctx: ecx as *mut _ as *mut (), client };
        DERIVE_EXPAND_CTX.set(&ctx, f)
    }

    /// Accesses the thread local value of the derive expansion context.
    /// Must be called while the `enter` function is active.
    fn with<F, R>(f: F) -> R
    where
        F: for<'a, 'b> FnOnce(&'b mut ExtCtxt<'a>, DeriveClient) -> R,
    {
        DERIVE_EXPAND_CTX.with(|ctx| {
            let ectx = {
                let casted = ctx.expansion_ctx.cast::<ExtCtxt<'_>>();
                // SAFETY: We can only get the value from `with` while the `enter` function
                // is active (on the callstack), and that function's signature ensures that the
                // lifetime is valid.
                // If `with` is called at some other time, it will panic due to usage of
                // `scoped_tls::with`.
                unsafe { casted.as_mut().unwrap() }
            };

            f(ectx, ctx.client)
        })
    }
}

// When we invoke a query to expand a derive proc macro, we need to provide it with the expansion
// context and derive Client. We do that using a thread-local.
// `eko`, not `scoped_tls` - that macro expands to `::std::thread_local!` and linked
// std without naming it.
eko::scoped_thread_local!(static DERIVE_EXPAND_CTX: QueryDeriveExpandCtx);
