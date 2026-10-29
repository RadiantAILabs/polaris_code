//! Existing `syn`-based Rust fact extraction.

use oxigraph::model::{Literal, NamedNode};
use syn::spanned::Spanned;
use syn::visit::Visit;

use super::{FactEmitter, term};

pub(super) fn extract(src: &str, facts: &mut FactEmitter) -> bool {
    let Ok(ast) = syn::parse_file(src) else {
        return false;
    };
    FactVisitor {
        facts,
        current_fn: None,
    }
    .visit_file(&ast);
    true
}

struct FactVisitor<'a> {
    facts: &'a mut FactEmitter,
    current_fn: Option<NamedNode>,
}

impl FactVisitor<'_> {
    fn decl(
        &mut self,
        class: &str,
        name: &str,
        ident_line: usize,
        span: (usize, usize),
    ) -> NamedNode {
        self.facts.decl(class, name, ident_line, span)
    }

    fn with_fn<F: FnOnce(&mut Self)>(&mut self, iri: NamedNode, f: F) {
        let prev = self.current_fn.replace(iri);
        f(self);
        self.current_fn = prev;
    }

    fn record_call(&mut self, callee: &str) {
        if let Some(caller) = self.current_fn.clone() {
            self.facts.call(&caller, callee);
        }
    }

    fn record_attrs(&mut self, sym: &NamedNode, attrs: &[syn::Attribute]) {
        for attr in attrs {
            if let Some(seg) = attr.path().segments.last() {
                self.facts.push(
                    sym.clone(),
                    term("hasAttr"),
                    Literal::new_simple_literal(seg.ident.to_string()).into(),
                );
            }
        }
    }

    fn record_params(&mut self, sym: &NamedNode, sig: &syn::Signature) {
        for input in &sig.inputs {
            let syn::FnArg::Typed(pat) = input else {
                continue;
            };
            if let Some((kind, inner)) = res_param(&pat.ty) {
                self.facts.push(
                    sym.clone(),
                    term(kind.predicate()),
                    Literal::new_simple_literal(&inner).into(),
                );
            }
        }
    }
}

enum ResKind {
    Read,
    Write,
}

impl ResKind {
    fn predicate(&self) -> &'static str {
        match self {
            Self::Read => "paramRes",
            Self::Write => "paramResMut",
        }
    }
}

fn res_param(ty: &syn::Type) -> Option<(ResKind, String)> {
    match ty {
        syn::Type::Reference(reference) => res_param(&reference.elem),
        syn::Type::Path(type_path) => {
            let segment = type_path.path.segments.last()?;
            let kind = match segment.ident.to_string().as_str() {
                "Res" => ResKind::Read,
                "ResMut" => ResKind::Write,
                _ => return None,
            };
            Some((kind, generic_type_name(&segment.arguments)?))
        }
        _ => None,
    }
}

fn generic_type_name(args: &syn::PathArguments) -> Option<String> {
    let syn::PathArguments::AngleBracketed(arguments) = args else {
        return None;
    };
    arguments.args.iter().find_map(|argument| match argument {
        syn::GenericArgument::Type(ty) => type_name(ty),
        _ => None,
    })
}

fn last_segment(path: &syn::Path) -> Option<String> {
    path.segments
        .last()
        .map(|segment| segment.ident.to_string())
}

fn span_lines(span: proc_macro2::Span) -> (usize, usize) {
    (span.start().line, span.end().line)
}

fn type_name(ty: &syn::Type) -> Option<String> {
    match ty {
        syn::Type::Path(type_path) => last_segment(&type_path.path),
        syn::Type::Reference(reference) => type_name(&reference.elem),
        _ => None,
    }
}

impl<'ast> Visit<'ast> for FactVisitor<'_> {
    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        let line = item.sig.ident.span().start().line;
        let iri = self.decl(
            "Function",
            &item.sig.ident.to_string(),
            line,
            span_lines(item.span()),
        );
        self.record_attrs(&iri, &item.attrs);
        self.record_params(&iri, &item.sig);
        self.with_fn(iri, |visitor| syn::visit::visit_item_fn(visitor, item));
    }

    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        let line = item.sig.ident.span().start().line;
        let iri = self.decl(
            "Function",
            &item.sig.ident.to_string(),
            line,
            span_lines(item.span()),
        );
        self.record_attrs(&iri, &item.attrs);
        self.record_params(&iri, &item.sig);
        self.with_fn(iri, |visitor| {
            syn::visit::visit_impl_item_fn(visitor, item);
        });
    }

    fn visit_item_struct(&mut self, item: &'ast syn::ItemStruct) {
        let line = item.ident.span().start().line;
        let iri = self.decl(
            "Struct",
            &item.ident.to_string(),
            line,
            span_lines(item.span()),
        );
        self.record_attrs(&iri, &item.attrs);
        syn::visit::visit_item_struct(self, item);
    }

    fn visit_item_enum(&mut self, item: &'ast syn::ItemEnum) {
        let line = item.ident.span().start().line;
        let iri = self.decl(
            "Enum",
            &item.ident.to_string(),
            line,
            span_lines(item.span()),
        );
        self.record_attrs(&iri, &item.attrs);
        syn::visit::visit_item_enum(self, item);
    }

    fn visit_item_trait(&mut self, item: &'ast syn::ItemTrait) {
        let line = item.ident.span().start().line;
        let iri = self.decl(
            "Trait",
            &item.ident.to_string(),
            line,
            span_lines(item.span()),
        );
        self.record_attrs(&iri, &item.attrs);
        syn::visit::visit_item_trait(self, item);
    }

    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        let line = item.ident.span().start().line;
        let iri = self.decl(
            "Module",
            &item.ident.to_string(),
            line,
            span_lines(item.span()),
        );
        self.record_attrs(&iri, &item.attrs);
        syn::visit::visit_item_mod(self, item);
    }

    fn visit_item_impl(&mut self, item: &'ast syn::ItemImpl) {
        if let Some(on_type) = type_name(&item.self_ty) {
            let line = item.impl_token.span.start().line;
            let symbol = self.decl("Impl", &on_type, line, span_lines(item.span()));
            self.record_attrs(&symbol, &item.attrs);
            self.facts.push(
                symbol.clone(),
                term("onType"),
                Literal::new_simple_literal(&on_type).into(),
            );
            if let Some((_, path, _)) = &item.trait_
                && let Some(trait_name) = last_segment(path)
            {
                self.facts.push(
                    symbol,
                    term("implementsTrait"),
                    Literal::new_simple_literal(trait_name).into(),
                );
            }
        }
        syn::visit::visit_item_impl(self, item);
    }

    fn visit_expr_call(&mut self, expression: &'ast syn::ExprCall) {
        if let syn::Expr::Path(path) = &*expression.func
            && let Some(callee) = last_segment(&path.path)
        {
            self.record_call(&callee);
        }
        syn::visit::visit_expr_call(self, expression);
    }

    fn visit_expr_method_call(&mut self, expression: &'ast syn::ExprMethodCall) {
        self.record_call(&expression.method.to_string());
        syn::visit::visit_expr_method_call(self, expression);
    }
}
