//! Tree-sitter extraction for the supported non-Rust language set.

use oxigraph::model::{Literal, NamedNode};
use tree_sitter::{Language, Node, Parser};

#[cfg(test)]
use super::FileStamp;
use super::{FactEmitter, SourceLanguage, term};

pub(super) fn extract(
    language: SourceLanguage,
    src: &str,
    facts: &mut FactEmitter,
) -> anyhow::Result<()> {
    let grammar: Language = match language {
        SourceLanguage::Python => tree_sitter_python::LANGUAGE.into(),
        SourceLanguage::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        SourceLanguage::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        SourceLanguage::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        SourceLanguage::Rust => unreachable!("Rust stays on the syn extractor"),
    };
    let mut parser = Parser::new();
    parser
        .set_language(&grammar)
        .map_err(|error| anyhow::anyhow!("{} parser unavailable: {error}", language.id()))?;
    let tree = parser
        .parse(src, None)
        .ok_or_else(|| anyhow::anyhow!("{} parser returned no tree", language.id()))?;
    Walker {
        language,
        src: src.as_bytes(),
        facts,
    }
    .walk(tree.root_node(), None);
    Ok(())
}

struct Walker<'a> {
    language: SourceLanguage,
    src: &'a [u8],
    facts: &'a mut FactEmitter,
}

impl Walker<'_> {
    fn walk(&mut self, node: Node<'_>, caller: Option<NamedNode>) {
        if node.is_error() || node.is_missing() {
            return;
        }

        let declaration = self.declaration(node);
        let mut child_caller = caller.clone();
        if let Some(declaration) = declaration {
            let wrapper = outer_wrapper(node);
            let symbol = self.facts.decl(
                declaration.class,
                &declaration.name,
                declaration.name_line,
                (
                    wrapper.start_position().row + 1,
                    wrapper.end_position().row + 1,
                ),
            );
            if declaration.class == "Function" {
                child_caller = Some(symbol.clone());
            }
            if declaration.class == "Class" && self.is_typescript() {
                self.record_implements(node, &symbol, &declaration.name);
            }
        }

        if let Some(caller) = caller.as_ref()
            && let Some(callee) = self.call_name(node)
        {
            self.facts.call(caller, &callee);
        }

        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            self.walk(child, child_caller.clone());
        }
    }

    fn declaration(&self, node: Node<'_>) -> Option<Declaration> {
        let class = match node.kind() {
            "function_definition" if self.language == SourceLanguage::Python => "Function",
            "function_declaration" | "generator_function_declaration" | "method_definition" => {
                "Function"
            }
            "class_definition" if self.language == SourceLanguage::Python => "Class",
            "class_declaration" | "abstract_class_declaration" => "Class",
            "interface_declaration" if self.is_typescript() => "Interface",
            "type_alias_declaration" if self.is_typescript() => "TypeAlias",
            "enum_declaration" if self.is_typescript() => "Enum",
            "internal_module" if self.is_typescript() => "Module",
            "variable_declarator" if self.bound_callable(node) => "Function",
            "field_definition" | "public_field_definition" if self.bound_callable(node) => {
                "Function"
            }
            _ => return None,
        };
        let name_node = node
            .child_by_field_name("name")
            .or_else(|| node.child_by_field_name("property"))?;
        let name = if node.kind() == "internal_module" {
            module_name(name_node, self.src)?
        } else {
            if !is_simple_name(name_node) {
                return None;
            }
            self.text(name_node)?.to_string()
        };
        Some(Declaration {
            class,
            name,
            name_line: name_node.start_position().row + 1,
        })
    }

    fn bound_callable(&self, node: Node<'_>) -> bool {
        let Some(value) = node.child_by_field_name("value") else {
            return false;
        };
        matches!(
            value.kind(),
            "arrow_function" | "function_expression" | "generator_function"
        )
    }

    fn record_implements(&mut self, class: Node<'_>, symbol: &NamedNode, name: &str) {
        let Some(heritage) = named_child_of_kind(class, "class_heritage") else {
            return;
        };
        let Some(clause) = named_child_of_kind(heritage, "implements_clause") else {
            return;
        };
        self.facts.push(
            symbol.clone(),
            term("onType"),
            Literal::new_simple_literal(name).into(),
        );
        let mut cursor = clause.walk();
        for implemented in clause.named_children(&mut cursor) {
            if let Some(interface) = simple_type_name(implemented, self.src) {
                self.facts.push(
                    symbol.clone(),
                    term("implementsTrait"),
                    Literal::new_simple_literal(interface).into(),
                );
            }
        }
    }

    fn call_name(&self, node: Node<'_>) -> Option<String> {
        let callee = match node.kind() {
            "call" if self.language == SourceLanguage::Python => {
                node.child_by_field_name("function")?
            }
            "call_expression" => node.child_by_field_name("function")?,
            "new_expression" if !self.language.eq(&SourceLanguage::Python) => {
                node.child_by_field_name("constructor")?
            }
            _ => return None,
        };
        callable_name(callee, self.src)
    }

    fn text(&self, node: Node<'_>) -> Option<&str> {
        node.utf8_text(self.src).ok()
    }

    fn is_typescript(&self) -> bool {
        matches!(
            self.language,
            SourceLanguage::TypeScript | SourceLanguage::Tsx
        )
    }
}

struct Declaration {
    class: &'static str,
    name: String,
    name_line: usize,
}

fn outer_wrapper(mut node: Node<'_>) -> Node<'_> {
    while let Some(parent) = node.parent() {
        if matches!(parent.kind(), "export_statement" | "decorated_definition") {
            node = parent;
        } else {
            break;
        }
    }
    if node.kind() == "variable_declarator"
        && let Some(parent) = node.parent()
        && let Some(statement) = parent.parent()
        && matches!(
            statement.kind(),
            "lexical_declaration" | "variable_declaration"
        )
    {
        return outer_wrapper(statement);
    }
    node
}

fn is_simple_name(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "identifier" | "property_identifier" | "private_property_identifier" | "type_identifier"
    )
}

fn named_child_of_kind<'tree>(node: Node<'tree>, kind: &str) -> Option<Node<'tree>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| child.kind() == kind)
}

fn last_identifier(node: Node<'_>, src: &[u8]) -> Option<String> {
    if is_simple_name(node) {
        return node.utf8_text(src).ok().map(ToOwned::to_owned);
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter_map(|child| last_identifier(child, src))
        .last()
}

fn module_name(node: Node<'_>, src: &[u8]) -> Option<String> {
    match node.kind() {
        "string" => node
            .utf8_text(src)
            .ok()
            .map(|name| name.trim_matches(['\'', '"']).to_string()),
        _ => last_identifier(node, src),
    }
}

fn simple_type_name(node: Node<'_>, src: &[u8]) -> Option<String> {
    let text = node.utf8_text(src).ok()?;
    let base = text.split('<').next()?.trim();
    base.rsplit(['.', ':'])
        .find(|part| !part.is_empty())
        .map(ToOwned::to_owned)
}

fn callable_name(node: Node<'_>, src: &[u8]) -> Option<String> {
    if is_simple_name(node) {
        return node.utf8_text(src).ok().map(ToOwned::to_owned);
    }
    match node.kind() {
        "attribute" => node
            .child_by_field_name("attribute")
            .and_then(|child| callable_name(child, src)),
        "member_expression" => node
            .child_by_field_name("property")
            .filter(|child| child.kind() != "computed_property_name")
            .and_then(|child| callable_name(child, src)),
        "parenthesized_expression" | "await" => {
            let mut cursor = node.walk();
            node.named_children(&mut cursor)
                .next()
                .and_then(|child| callable_name(child, src))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_grammar_matches_the_runtime_abi() {
        for language in [
            SourceLanguage::Python,
            SourceLanguage::JavaScript,
            SourceLanguage::TypeScript,
            SourceLanguage::Tsx,
        ] {
            let mut facts = FactEmitter::new("fixture", language.id(), FileStamp::default());
            extract(language, "", &mut facts).unwrap();
        }
    }
}
