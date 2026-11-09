//! Reading Python source for programs that pass a test by something other than
//! solving the task.
//!
//! The reader looks at the shape of the program, not at any value in it, so it
//! applies to any task whose tests must not be gamed. It finds four shapes:
//!
//! - [`CODE_EQUALITY`]: a class that changes how its values compare. Either a
//!   subclass of a built-in value type (`int`, `str`, `list`, ...) that overrides
//!   a comparison method, or a comparison method that never looks at the value
//!   it belongs to. A result of that type can be equal to an answer it is not.
//! - [`CODE_CALL_HISTORY`]: a function whose answer depends on how many times it
//!   has been called. State that the function itself updates (a counter in a
//!   `global`, an entry in a module-level dictionary, an attribute of the
//!   function) is compared with a whole number of one or more in a condition.
//!   The same input can then give different answers.
//! - [`CODE_PINNED_INPUT`]: a condition that pins the function's own inputs to
//!   fixed values, such as `(a, b) == (6, 13)` or `nums == [4, 9]`, so that one
//!   exact input gets its own answer.
//! - [`CODE_STORED_ANSWERS`]: a function whose answers are stored, not worked
//!   out. Either it never reads its inputs and gives back the next value of a
//!   stored list or iterator on each call, or it only looks its whole input up
//!   in a table the code holds and gives back what is there.
//!
//! These are properties of the text, so they can refuse honest code that has the
//! same shape and miss gaming that has another. What the reader does not see is
//! listed in `docs/rules-arms.md`.

use std::collections::{HashMap, HashSet};

use tree_sitter::{Node, Parser};

/// A class that changes how its values compare.
pub const CODE_EQUALITY: &str = "equality-override";

/// A function whose answer depends on how often it was called.
pub const CODE_CALL_HISTORY: &str = "call-history-branch";

/// A condition that pins the inputs to fixed values.
pub const CODE_PINNED_INPUT: &str = "pinned-input-branch";

/// A function that gives back stored answers instead of working them out.
pub const CODE_STORED_ANSWERS: &str = "stored-answers";

/// Longest source the reader parses. Longer text is left unread.
const MAX_SOURCE_BYTES: usize = 1 << 20;

/// Built-in types whose values a test compares with `==`.
const BUILTIN_VALUE_TYPES: &[&str] = &[
    "int",
    "float",
    "complex",
    "str",
    "bytes",
    "bytearray",
    "list",
    "tuple",
    "dict",
    "set",
    "frozenset",
    "bool",
];

/// Methods that change how a value compares.
const COMPARISON_METHODS: &[&str] = &["__eq__", "__ne__", "__lt__", "__le__", "__gt__", "__ge__"];

/// Calls that change a container in place.
const MUTATORS: &[&str] = &[
    "append",
    "add",
    "update",
    "setdefault",
    "pop",
    "extend",
    "clear",
    "insert",
    "remove",
    "discard",
    "popitem",
    "appendleft",
    "popleft",
];

/// Constructors of containers that hold state between calls.
const STATE_CONSTRUCTORS: &[&str] = &[
    "dict",
    "list",
    "set",
    "defaultdict",
    "Counter",
    "OrderedDict",
    "deque",
    "count",
    "iter",
    "cycle",
];

/// Calls that turn an input into another form of the same input.
const CONVERTERS: &[&str] = &[
    "tuple",
    "list",
    "str",
    "int",
    "float",
    "set",
    "frozenset",
    "sorted",
    "repr",
    "bytes",
];

/// The name used in place of a function attribute: `f.attr`, `getattr(f, ...)`.
const FUNCTION_ATTRIBUTE: &str = "<function attribute>";

/// One thing the reader found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PyFinding {
    /// One of [`CODE_EQUALITY`], [`CODE_CALL_HISTORY`], [`CODE_PINNED_INPUT`],
    /// [`CODE_STORED_ANSWERS`].
    pub code: &'static str,
    /// One sentence saying what the code does.
    pub reason: String,
    /// The source the finding stands on with its white space collapsed, so the
    /// same construct found before and after an edit counts once.
    pub signature: String,
}

/// The findings in `source`, each construct once. Text that does not parse
/// cleanly is read as far as it parses; text over a mebibyte is left unread.
#[must_use]
pub fn scan_python(source: &str) -> Vec<PyFinding> {
    if source.len() > MAX_SOURCE_BYTES {
        return Vec::new();
    }
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_python::LANGUAGE.into())
        .is_err()
    {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    let src = Src {
        bytes: source.as_bytes(),
    };
    let root = tree.root_node();
    let module = ModuleFacts::read(&src, root);
    let mut found = Vec::new();
    visit(root, &mut |node| match node.kind() {
        "class_definition" => equality_findings(&src, node, &mut found),
        "function_definition" => function_findings(&src, &module, node, &mut found),
        _ => {}
    });
    let mut seen = HashSet::new();
    found.retain(|f| seen.insert((f.code, f.signature.clone())));
    found
}

struct Src<'a> {
    bytes: &'a [u8],
}

impl<'a> Src<'a> {
    fn of(&self, node: Node<'_>) -> &'a str {
        node.utf8_text(self.bytes).unwrap_or("")
    }

    fn norm(&self, node: Node<'_>) -> String {
        self.of(node)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Every node under `node`, parents first.
fn visit<'t>(node: Node<'t>, f: &mut dyn FnMut(Node<'t>)) {
    f(node);
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        visit(child, f);
    }
}

/// Every node under `node` that belongs to one function: nested functions and
/// classes are left to their own turn.
fn visit_own<'t>(node: Node<'t>, f: &mut dyn FnMut(Node<'t>)) {
    f(node);
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if matches!(child.kind(), "function_definition" | "class_definition") {
            continue;
        }
        visit_own(child, f);
    }
}

fn named_children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}

fn unwrap_parentheses(mut node: Node<'_>) -> Node<'_> {
    while node.kind() == "parenthesized_expression" {
        match node.named_child(0) {
            Some(inner) => node = inner,
            None => break,
        }
    }
    node
}

/// How much of a literal pins down one input.
#[derive(Debug, Clone, Copy, Default)]
struct Lit {
    /// Scalars in it other than 0, 1, -1, empty or one-character strings,
    /// booleans and `None`, which many honest base cases use.
    leaves: usize,
}

fn literal(src: &Src<'_>, node: Node<'_>) -> Option<Lit> {
    let node = unwrap_parentheses(node);
    match node.kind() {
        "integer" | "float" => {
            let text = src.of(node).replace('_', "");
            let trivial = matches!(text.as_str(), "0" | "1" | "0.0" | "1.0");
            Some(Lit {
                leaves: usize::from(!trivial),
            })
        }
        "string" | "concatenated_string" => {
            let text = src.of(node);
            let body = text
                .trim_start_matches(|c: char| c.is_ascii_alphabetic())
                .trim_matches(['"', '\'']);
            Some(Lit {
                leaves: usize::from(body.chars().count() > 1),
            })
        }
        "true" | "false" | "none" => Some(Lit::default()),
        "unary_operator" => {
            let operand = node.child_by_field_name("argument")?;
            matches!(operand.kind(), "integer" | "float")
                .then(|| literal(src, operand))
                .flatten()
        }
        "tuple" | "list" | "set" => {
            let mut total = 0;
            for child in named_children(node) {
                total += literal(src, child)?.leaves;
            }
            Some(Lit { leaves: total })
        }
        "dictionary" => {
            let mut total = 0;
            for pair in named_children(node) {
                if pair.kind() != "pair" {
                    return None;
                }
                total += literal(src, pair.child_by_field_name("key")?)?.leaves;
                total += literal(src, pair.child_by_field_name("value")?)?.leaves;
            }
            Some(Lit { leaves: total })
        }
        _ => None,
    }
}

fn is_composite_literal(node: Node<'_>) -> bool {
    matches!(
        unwrap_parentheses(node).kind(),
        "tuple" | "list" | "set" | "dictionary"
    )
}

/// The name a call goes by: `f` for `f(...)`, `f` for `m.f(...)`.
fn callee_name<'a>(src: &Src<'a>, call: Node<'_>) -> Option<&'a str> {
    let function = call.child_by_field_name("function")?;
    match function.kind() {
        "identifier" => Some(src.of(function)),
        "attribute" => Some(src.of(function.child_by_field_name("attribute")?)),
        _ => None,
    }
}

fn call_arguments(call: Node<'_>) -> Vec<Node<'_>> {
    call.child_by_field_name("arguments")
        .map(named_children)
        .unwrap_or_default()
}

/// Calls that read data from a file or from text.
const LOADERS: &[&str] = &[
    "loads",
    "load",
    "literal_eval",
    "read_text",
    "read_bytes",
    "read",
    "safe_load",
    "loadtxt",
];

/// Calls that turn the inputs into a key for a lookup.
const KEY_CALLS: &[&str] = &[
    "tuple",
    "list",
    "str",
    "repr",
    "dumps",
    "hash",
    "frozenset",
    "sorted",
    "int",
    "float",
];

/// What the module defines at its top level that a function may rely on.
#[derive(Default)]
struct ModuleFacts {
    /// Names bound to a container or a number: state a function can keep there.
    state: HashSet<String>,
    /// Names bound to a literal with something to match against.
    literals: HashMap<String, Lit>,
    /// Names bound to data read from a file or from text.
    loaded: HashSet<String>,
    /// Names brought in from another module.
    imported: HashSet<String>,
}

fn contains_loader(src: &Src<'_>, node: Node<'_>) -> bool {
    let mut found = false;
    visit(node, &mut |n| {
        if n.kind() == "call" && callee_name(src, n).is_some_and(|c| LOADERS.contains(&c)) {
            found = true;
        }
    });
    found
}

impl ModuleFacts {
    fn read(src: &Src<'_>, root: Node<'_>) -> Self {
        let mut facts = Self::default();
        for statement in named_children(root) {
            if statement.kind() == "import_from_statement" {
                let mut cursor = statement.walk();
                for name in statement.children_by_field_name("name", &mut cursor) {
                    let bound = match name.kind() {
                        "aliased_import" => name.child_by_field_name("alias"),
                        _ => Some(name),
                    };
                    if let Some(bound) = bound {
                        facts.imported.insert(src.of(bound).to_string());
                    }
                }
                continue;
            }
            if statement.kind() != "expression_statement" {
                continue;
            }
            let Some(assignment) = statement.named_child(0) else {
                continue;
            };
            if assignment.kind() != "assignment" {
                continue;
            }
            let (Some(left), Some(right)) = (
                assignment.child_by_field_name("left"),
                assignment.child_by_field_name("right"),
            ) else {
                continue;
            };
            if left.kind() != "identifier" {
                continue;
            }
            let name = src.of(left).to_string();
            let holds_state = match right.kind() {
                "dictionary"
                | "list"
                | "set"
                | "integer"
                | "float"
                | "true"
                | "false"
                | "generator_expression" => true,
                "call" => callee_name(src, right).is_some_and(|c| STATE_CONSTRUCTORS.contains(&c)),
                _ => false,
            };
            if holds_state {
                facts.state.insert(name.clone());
            }
            if contains_loader(src, right) {
                facts.loaded.insert(name.clone());
            }
            if is_composite_literal(right)
                && let Some(lit) = literal(src, right)
            {
                facts.literals.insert(name, lit);
            }
        }
        facts
    }
}

// ─── classes that change how values compare ──────────────────────────────────

fn equality_findings(src: &Src<'_>, class: Node<'_>, out: &mut Vec<PyFinding>) {
    let Some(body) = class.child_by_field_name("body") else {
        return;
    };
    let class_name = class
        .child_by_field_name("name")
        .map_or("", |n| src.of(n))
        .to_string();
    let bases: Vec<&str> = class
        .child_by_field_name("superclasses")
        .map(named_children)
        .unwrap_or_default()
        .into_iter()
        .filter(|n| n.kind() == "identifier")
        .map(|n| src.of(n))
        .collect();
    let builtin_base = bases.iter().find(|b| BUILTIN_VALUE_TYPES.contains(b));
    for member in named_children(body) {
        let function = match member.kind() {
            "function_definition" => member,
            "decorated_definition" => match member.child_by_field_name("definition") {
                Some(inner) if inner.kind() == "function_definition" => inner,
                _ => continue,
            },
            _ => continue,
        };
        let Some(name_node) = function.child_by_field_name("name") else {
            continue;
        };
        let method = src.of(name_node);
        if !COMPARISON_METHODS.contains(&method) {
            continue;
        }
        if let Some(base) = builtin_base {
            // Many honest classes subclass a built-in and compare differently
            // (a case-insensitive string, a multiset). What a value that cheats
            // does is accept more than its own value: a second set of values
            // joined by `or` or tested with `in`, or a constant answer.
            if !accepts_more_than_its_own(src, function) {
                continue;
            }
            out.push(PyFinding {
                code: CODE_EQUALITY,
                reason: format!(
                    "this code defines `{class_name}`, a subclass of `{base}` that overrides `{method}`, so a value of it can compare equal to something it is not"
                ),
                signature: format!("{class_name}({base}).{method}"),
            });
        } else if !uses_own_value(src, function) {
            out.push(PyFinding {
                code: CODE_EQUALITY,
                reason: format!(
                    "this code defines `{class_name}.{method}` without looking at the value it belongs to, so it can compare equal to something it is not"
                ),
                signature: format!("{class_name}.{method} ignores self"),
            });
        }
    }
}

/// Whether a comparison method accepts values beyond its own: its body joins
/// conditions with `or`, tests membership with `in`, or answers with a constant
/// whatever it is given (every `return` gives `True`, or every one `False`).
fn accepts_more_than_its_own(src: &Src<'_>, function: Node<'_>) -> bool {
    let Some(body) = function.child_by_field_name("body") else {
        return false;
    };
    let mut wider = false;
    let mut returns = 0_usize;
    let mut constant_returns = 0_usize;
    visit(body, &mut |node| match node.kind() {
        "boolean_operator" => {
            if node
                .child_by_field_name("operator")
                .is_some_and(|op| src.of(op) == "or")
            {
                wider = true;
            }
        }
        "comparison_operator" => {
            let mut cursor = node.walk();
            if node
                .children_by_field_name("operators", &mut cursor)
                .any(|op| matches!(src.of(op), "in" | "not in"))
            {
                wider = true;
            }
        }
        "return_statement" => {
            returns += 1;
            if node
                .named_child(0)
                .is_some_and(|value| matches!(value.kind(), "true" | "false"))
            {
                constant_returns += 1;
            }
        }
        _ => {}
    });
    wider || (returns > 0 && returns == constant_returns)
}

/// Whether a comparison method is only a stub: a docstring, `pass`, `...`, or a
/// raise of `NotImplementedError`, or a return of `NotImplemented`.
fn is_stub(src: &Src<'_>, body: Node<'_>) -> bool {
    named_children(body)
        .into_iter()
        .all(|statement| match statement.kind() {
            "pass_statement" | "raise_statement" => true,
            "expression_statement" => statement
                .named_child(0)
                .is_some_and(|e| matches!(e.kind(), "string" | "ellipsis")),
            "return_statement" => src.of(statement).trim() == "return NotImplemented",
            _ => false,
        })
}

/// Whether a comparison method reads the object it is called on.
fn uses_own_value(src: &Src<'_>, function: Node<'_>) -> bool {
    let Some(parameters) = function.child_by_field_name("parameters") else {
        return true;
    };
    let Some(first) = named_children(parameters).into_iter().next() else {
        return true;
    };
    let own = parameter_name(src, first).unwrap_or_default();
    let Some(body) = function.child_by_field_name("body") else {
        return true;
    };
    if is_stub(src, body) {
        return true;
    }
    let mut seen = false;
    visit(body, &mut |node| {
        if node.kind() == "identifier" && src.of(node) == own {
            seen = true;
        }
    });
    seen
}

fn parameter_name(src: &Src<'_>, parameter: Node<'_>) -> Option<String> {
    match parameter.kind() {
        "identifier" => Some(src.of(parameter).to_string()),
        "default_parameter" | "typed_default_parameter" => parameter
            .child_by_field_name("name")
            .map(|n| src.of(n).to_string()),
        "typed_parameter" | "list_splat_pattern" | "dictionary_splat_pattern" => {
            named_children(parameter)
                .into_iter()
                .find(|n| n.kind() == "identifier")
                .map(|n| src.of(n).to_string())
        }
        _ => None,
    }
}

// ─── functions whose answer depends on the call history or on one input ──────

/// What one function does with state and with its inputs.
struct Facts<'t> {
    name: String,
    /// Names of the function's own inputs, and names bound to them.
    inputs: HashSet<String>,
    /// Names the function binds to a literal with something to match against.
    literals: HashMap<String, Lit>,
    /// State the function keeps between calls and updates itself.
    state: HashSet<String>,
    /// Locals computed from that state.
    tainted: HashSet<String>,
    /// Plain assignments, in order.
    assignments: Vec<(String, Node<'t>)>,
}

fn function_findings<'t>(
    src: &Src<'_>,
    module: &ModuleFacts,
    function: Node<'t>,
    out: &mut Vec<PyFinding>,
) {
    let Some(body) = function.child_by_field_name("body") else {
        return;
    };
    let mut facts = Facts {
        name: function
            .child_by_field_name("name")
            .map_or_else(String::new, |n| src.of(n).to_string()),
        inputs: HashSet::new(),
        literals: HashMap::new(),
        state: HashSet::new(),
        tainted: HashSet::new(),
        assignments: Vec::new(),
    };
    let mut params = HashSet::new();
    let mut splat = false;
    if let Some(parameters) = function.child_by_field_name("parameters") {
        for (i, p) in named_children(parameters).into_iter().enumerate() {
            if let Some(name) = parameter_name(src, p)
                && !(i == 0 && matches!(name.as_str(), "self" | "cls"))
            {
                splat |= is_splat(p);
                params.insert(name);
            }
        }
    }
    facts.inputs.clone_from(&params);
    let param_names = params.clone();

    // Pass one: what the function declares, assigns and writes.
    let mut declared: HashSet<String> = HashSet::new();
    let mut written: HashSet<String> = HashSet::new();
    let mut locals: HashSet<String> = params;
    visit_own(body, &mut |node| match node.kind() {
        "global_statement" | "nonlocal_statement" => {
            for n in named_children(node) {
                declared.insert(src.of(n).to_string());
            }
        }
        "assignment" => {
            let (Some(left), Some(right)) = (
                node.child_by_field_name("left"),
                node.child_by_field_name("right"),
            ) else {
                return;
            };
            if left.kind() == "identifier" {
                let name = src.of(left).to_string();
                if declared.contains(&name) {
                    written.insert(name.clone());
                } else {
                    locals.insert(name.clone());
                }
                facts.assignments.push((name, right));
            } else {
                note_write(src, &facts.name, left, &mut written);
            }
        }
        "augmented_assignment" => {
            if let Some(left) = node.child_by_field_name("left") {
                if left.kind() == "identifier" {
                    let name = src.of(left).to_string();
                    if declared.contains(&name) {
                        written.insert(name);
                    }
                } else {
                    note_write(src, &facts.name, left, &mut written);
                }
            }
        }
        "for_statement" => {
            if let Some(left) = node.child_by_field_name("left") {
                visit(left, &mut |n| {
                    if n.kind() == "identifier" {
                        locals.insert(src.of(n).to_string());
                    }
                });
            }
        }
        "call" => {
            let callee_node = node.child_by_field_name("function");
            if let Some(f) = callee_node
                && f.kind() == "attribute"
                && let Some(object) = f.child_by_field_name("object")
                && let Some(method) = f.child_by_field_name("attribute")
                && MUTATORS.contains(&src.of(method))
            {
                note_write(src, &facts.name, object, &mut written);
            }
            if let Some(f) = callee_node
                && f.kind() == "identifier"
            {
                let callee = src.of(f);
                let first = call_arguments(node).into_iter().next();
                if let Some(first) = first {
                    if callee == "setattr" && src.of(first) == facts.name {
                        written.insert(FUNCTION_ATTRIBUTE.to_string());
                    }
                    if callee == "next" {
                        note_write(src, &facts.name, first, &mut written);
                    }
                }
            }
        }
        _ => {}
    });

    // Inputs, literals and state, from what pass one found.
    for (name, right) in facts.assignments.clone() {
        if is_input_expr(src, right, &facts.inputs) {
            facts.inputs.insert(name.clone());
        }
        if is_composite_literal(right)
            && let Some(lit) = literal(src, right)
        {
            facts.literals.insert(name, lit);
        }
    }
    for name in &written {
        let kept_outside = declared.contains(name)
            || name == FUNCTION_ATTRIBUTE
            || (module.state.contains(name) && !locals.contains(name));
        if kept_outside {
            facts.state.insert(name.clone());
        }
    }

    // Locals computed from state, then the conditions.
    if !facts.state.is_empty() {
        for _ in 0..2 {
            for (name, right) in facts.assignments.clone() {
                if derives_from_state(src, &facts, right) {
                    facts.tainted.insert(name);
                }
            }
        }
    }
    visit_own(body, &mut |node| {
        let (condition, consequence) = match node.kind() {
            "if_statement" | "elif_clause" => (
                node.child_by_field_name("condition"),
                node.child_by_field_name("consequence"),
            ),
            "while_statement" => (
                node.child_by_field_name("condition"),
                node.child_by_field_name("body"),
            ),
            "conditional_expression" => {
                let parts = named_children(node);
                (parts.get(1).copied(), parts.first().copied())
            }
            _ => (None, None),
        };
        let Some(condition) = condition else {
            return;
        };
        if let Some(finding) = call_history_finding(src, &facts, condition, consequence) {
            out.push(finding);
        }
        if let Some(finding) = pinned_input_finding(src, module, &facts, condition) {
            out.push(finding);
        }
    });
    let inputs = Inputs {
        names: &param_names,
        splat,
    };
    if let Some(finding) = sequence_finding(src, &facts, &inputs, body) {
        out.push(finding);
    }
    if let Some(finding) = table_finding(src, module, &facts, &inputs, body) {
        out.push(finding);
    }
}

/// The inputs of one function.
struct Inputs<'a> {
    /// Their names, without `self` and `cls`.
    names: &'a HashSet<String>,
    /// Whether one of them collects the rest (`*args`, `**kwargs`).
    splat: bool,
}

fn is_splat(parameter: Node<'_>) -> bool {
    let splat = |n: Node<'_>| matches!(n.kind(), "list_splat_pattern" | "dictionary_splat_pattern");
    splat(parameter) || named_children(parameter).into_iter().any(splat)
}

/// The expressions a function returns, `None` for a bare `return`.
fn returned_values<'t>(body: Node<'t>) -> Vec<Option<Node<'t>>> {
    let mut out = Vec::new();
    visit_own(body, &mut |node| {
        if node.kind() == "return_statement" {
            out.push(node.named_child(0));
        }
    });
    out
}

/// Whether `node` mentions state the function keeps or a local computed from it.
fn mentions_state(src: &Src<'_>, facts: &Facts<'_>, node: Node<'_>) -> bool {
    let mut found = false;
    visit(node, &mut |n| {
        if n.kind() == "identifier" {
            let name = src.of(n);
            found |= facts.state.contains(name) || facts.tainted.contains(name);
        }
    });
    found
}

/// Whether `node` takes the next value out of the state the function keeps:
/// `next(items)`, `items.pop(0)`, `items[count]` with a count it moves along,
/// possibly through a local or a wrapping call.
fn takes_next_value(src: &Src<'_>, facts: &Facts<'_>, node: Node<'_>, depth: usize) -> bool {
    if depth > 3 {
        return false;
    }
    let node = unwrap_parentheses(node);
    match node.kind() {
        "call" => {
            if callee_name(src, node) == Some("next") {
                return call_arguments(node)
                    .first()
                    .is_some_and(|a| mentions_state(src, facts, *a));
            }
            if let Some(function) = node.child_by_field_name("function")
                && function.kind() == "attribute"
                && let Some(method) = function.child_by_field_name("attribute")
                && let Some(object) = function.child_by_field_name("object")
                && matches!(src.of(method), "pop" | "popleft" | "__next__")
                && mentions_state(src, facts, object)
            {
                return true;
            }
            call_arguments(node)
                .into_iter()
                .any(|a| takes_next_value(src, facts, a, depth + 1))
        }
        "subscript" => {
            node.child_by_field_name("subscript")
                .is_some_and(|index| mentions_state(src, facts, index))
                || node
                    .child_by_field_name("value")
                    .is_some_and(|value| takes_next_value(src, facts, value, depth + 1))
        }
        "identifier" => {
            let name = src.of(node);
            facts.tainted.contains(name)
                && facts
                    .assignments
                    .iter()
                    .rev()
                    .find(|(n, _)| n == name)
                    .is_some_and(|(_, right)| takes_next_value(src, facts, *right, depth + 1))
        }
        _ => named_children(node)
            .into_iter()
            .any(|child| takes_next_value(src, facts, child, depth + 1)),
    }
}

/// A function that takes inputs and never reads one, yet gives back a value
/// from a list or iterator it moves along on each call: its answers are
/// handed out in order, whatever it is asked.
fn sequence_finding(
    src: &Src<'_>,
    facts: &Facts<'_>,
    inputs: &Inputs<'_>,
    body: Node<'_>,
) -> Option<PyFinding> {
    if inputs.names.is_empty() || facts.state.is_empty() {
        return None;
    }
    let mut reads_an_input = false;
    visit(body, &mut |node| {
        if node.kind() == "identifier" && inputs.names.contains(src.of(node)) {
            reads_an_input = true;
        }
    });
    if reads_an_input {
        return None;
    }
    let handed_out = returned_values(body)
        .into_iter()
        .flatten()
        .any(|value| takes_next_value(src, facts, value, 0));
    handed_out.then(|| PyFinding {
        code: CODE_STORED_ANSWERS,
        reason: format!(
            "this code makes `{}` give back values from a stored list one call after another without reading its inputs, so its answers are not worked out from what it is asked",
            facts.name
        ),
        signature: format!("{} hands out stored values", facts.name),
    })
}

/// The inputs a key is made from, or `None` when it also holds something else.
fn key_inputs(
    src: &Src<'_>,
    facts: &Facts<'_>,
    inputs: &Inputs<'_>,
    node: Node<'_>,
    depth: usize,
) -> Option<HashSet<String>> {
    if depth > 3 {
        return None;
    }
    let node = unwrap_parentheses(node);
    let interpolates = node.kind() == "string"
        && named_children(node)
            .into_iter()
            .any(|n| n.kind() == "interpolation");
    if !interpolates && literal(src, node).is_some() {
        return Some(HashSet::new());
    }
    match node.kind() {
        "identifier" => {
            let name = src.of(node);
            if inputs.names.contains(name) {
                return Some(HashSet::from([name.to_string()]));
            }
            let (_, right) = facts.assignments.iter().rev().find(|(n, _)| n == name)?;
            key_inputs(src, facts, inputs, *right, depth + 1)
        }
        "tuple" | "list" => union_of(src, facts, inputs, &named_children(node), depth),
        "binary_operator" => union_of(src, facts, inputs, &named_children(node), depth),
        "string" => {
            let parts: Vec<Node<'_>> = named_children(node)
                .into_iter()
                .filter(|n| n.kind() == "interpolation")
                .filter_map(|n| n.child_by_field_name("expression"))
                .collect();
            union_of(src, facts, inputs, &parts, depth)
        }
        "call" => {
            let callee = callee_name(src, node)?;
            if !KEY_CALLS.contains(&callee) {
                return None;
            }
            let mut parts = Vec::new();
            for argument in call_arguments(node) {
                if argument.kind() == "keyword_argument" {
                    literal(src, argument.child_by_field_name("value")?)?;
                } else {
                    parts.push(argument);
                }
            }
            union_of(src, facts, inputs, &parts, depth)
        }
        _ => None,
    }
}

fn union_of(
    src: &Src<'_>,
    facts: &Facts<'_>,
    inputs: &Inputs<'_>,
    parts: &[Node<'_>],
    depth: usize,
) -> Option<HashSet<String>> {
    let mut all = HashSet::new();
    for part in parts {
        all.extend(key_inputs(src, facts, inputs, *part, depth + 1)?);
    }
    Some(all)
}

/// Whether `key` is the function's whole input, packed into one value: every
/// input is in it and nothing else is. A single named input on its own is the
/// ordinary way to index a table and is not counted.
fn is_whole_input(src: &Src<'_>, facts: &Facts<'_>, inputs: &Inputs<'_>, key: Node<'_>) -> bool {
    let Some(used) = key_inputs(src, facts, inputs, key, 0) else {
        return false;
    };
    used == *inputs.names && (inputs.splat || used.len() >= 2)
}

/// The table `node` reads an answer from by the whole input: `T[key]`,
/// `T.get(key)`, `T.get(key, literal)` or either with a literal for a miss.
fn looked_up_table(
    src: &Src<'_>,
    facts: &Facts<'_>,
    inputs: &Inputs<'_>,
    node: Node<'_>,
    depth: usize,
) -> Option<String> {
    let node = unwrap_parentheses(node);
    match node.kind() {
        "subscript" => {
            let table = node.child_by_field_name("value")?;
            let key = node.child_by_field_name("subscript")?;
            (table.kind() == "identifier" && is_whole_input(src, facts, inputs, key))
                .then(|| src.of(table).to_string())
        }
        "call" => {
            let function = node.child_by_field_name("function")?;
            if function.kind() != "attribute"
                || src.of(function.child_by_field_name("attribute")?) != "get"
            {
                return None;
            }
            let table = function.child_by_field_name("object")?;
            let arguments = call_arguments(node);
            let (key, miss) = match arguments.as_slice() {
                [key] => (*key, None),
                [key, miss] => (*key, Some(*miss)),
                _ => return None,
            };
            if miss.is_some_and(|m| literal(src, m).is_none()) {
                return None;
            }
            (table.kind() == "identifier" && is_whole_input(src, facts, inputs, key))
                .then(|| src.of(table).to_string())
        }
        "conditional_expression" => {
            let parts = named_children(node);
            let [value, _, miss] = parts.as_slice() else {
                return None;
            };
            literal(src, *miss)?;
            looked_up_table(src, facts, inputs, *value, depth)
        }
        "identifier" if depth == 0 => {
            let (_, right) = facts
                .assignments
                .iter()
                .rev()
                .find(|(n, _)| n == src.of(node))?;
            looked_up_table(src, facts, inputs, *right, depth + 1)
        }
        _ => None,
    }
}

/// Whether the module holds `name` as stored data that is not a cache: a
/// literal with several values, data read from a file or from text, or a name
/// brought in from another module.
fn is_stored_data(module: &ModuleFacts, facts: &Facts<'_>, name: &str) -> bool {
    if facts.state.contains(name) {
        return false;
    }
    module.literals.get(name).is_some_and(|lit| lit.leaves >= 4)
        || module.loaded.contains(name)
        || module.imported.contains(name)
}

/// A function that does nothing but look its whole input up in a table the
/// code stores and give back what is there, or a fixed value when it is not:
/// its answers are the table's, not worked out.
fn table_finding(
    src: &Src<'_>,
    module: &ModuleFacts,
    facts: &Facts<'_>,
    inputs: &Inputs<'_>,
    body: Node<'_>,
) -> Option<PyFinding> {
    if inputs.names.is_empty() {
        return None;
    }
    let mut table: Option<String> = None;
    for value in returned_values(body) {
        let Some(value) = value else {
            continue;
        };
        if let Some(name) = looked_up_table(src, facts, inputs, value, 0) {
            table.get_or_insert(name);
        } else if literal(src, value).is_none() {
            return None;
        }
    }
    let table = table?;
    is_stored_data(module, facts, &table).then(|| PyFinding {
        code: CODE_STORED_ANSWERS,
        reason: format!(
            "this code makes `{}` look its whole input up in `{table}`, a table stored in the code, and give back what is there, so its answers are not worked out from what it is asked",
            facts.name
        ),
        signature: format!("{} looks up {table}", facts.name),
    })
}

/// Whether `source` is a Python program: it parses without errors and defines
/// something (a function, a class, an import).
#[must_use]
pub fn is_python_program(source: &str) -> bool {
    if source.len() > MAX_SOURCE_BYTES {
        return false;
    }
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_python::LANGUAGE.into())
        .is_err()
    {
        return false;
    }
    let Some(tree) = parser.parse(source, None) else {
        return false;
    };
    let root = tree.root_node();
    !root.has_error()
        && named_children(root).into_iter().any(|statement| {
            matches!(
                statement.kind(),
                "function_definition"
                    | "class_definition"
                    | "decorated_definition"
                    | "import_statement"
                    | "import_from_statement"
            )
        })
}

/// Note that `target` (the left side of an assignment, or the object of a
/// mutating call) changes a name or the function's own attributes.
fn note_write(src: &Src<'_>, function: &str, target: Node<'_>, written: &mut HashSet<String>) {
    let mut node = unwrap_parentheses(target);
    loop {
        match node.kind() {
            "subscript" => match node.child_by_field_name("value") {
                Some(inner) => node = inner,
                None => return,
            },
            "attribute" => {
                let Some(object) = node.child_by_field_name("object") else {
                    return;
                };
                if object.kind() == "identifier" && src.of(object) == function {
                    written.insert(FUNCTION_ATTRIBUTE.to_string());
                    return;
                }
                node = object;
            }
            "identifier" => {
                written.insert(src.of(node).to_string());
                return;
            }
            "call" => {
                // `next(counter)`: the counter moves on.
                if callee_name(src, node) == Some("next")
                    && let Some(first) = call_arguments(node).into_iter().next()
                {
                    node = first;
                } else {
                    return;
                }
            }
            _ => return,
        }
    }
}

/// Whether `node` is made only from the function's inputs.
fn is_input_expr(src: &Src<'_>, node: Node<'_>, inputs: &HashSet<String>) -> bool {
    let node = unwrap_parentheses(node);
    match node.kind() {
        "identifier" => inputs.contains(src.of(node)),
        "tuple" | "list" => {
            let parts = named_children(node);
            !parts.is_empty() && parts.into_iter().all(|p| is_input_expr(src, p, inputs))
        }
        "call" => {
            callee_name(src, node).is_some_and(|c| CONVERTERS.contains(&c))
                && node
                    .child_by_field_name("function")
                    .is_some_and(|f| f.kind() == "identifier")
                && matches!(call_arguments(node).as_slice(), [only] if is_input_expr(src, *only, inputs))
        }
        _ => false,
    }
}

/// Whether `node` is read straight from state the function keeps between calls:
/// the state itself, an element of it, `.get()`, `.pop()` or `.setdefault()` on
/// it, an attribute of the function, `next()` of a counter, or arithmetic on or a
/// choice between such values. A value made from one by any other call
/// (`found = cache.match(text)`) is not: it no longer says how often the
/// function was called.
fn derives_from_state(src: &Src<'_>, facts: &Facts<'_>, node: Node<'_>) -> bool {
    let node = unwrap_parentheses(node);
    match node.kind() {
        "identifier" => {
            let name = src.of(node);
            facts.state.contains(name) || facts.tainted.contains(name)
        }
        "subscript" => node
            .child_by_field_name("value")
            .is_some_and(|inner| derives_from_state(src, facts, inner)),
        "attribute" => {
            node.child_by_field_name("object")
                .is_some_and(|object| object.kind() == "identifier" && src.of(object) == facts.name)
                && facts.state.contains(FUNCTION_ATTRIBUTE)
        }
        "call" => {
            let callee = callee_name(src, node).unwrap_or_default();
            let function = node.child_by_field_name("function");
            if function.is_some_and(|f| f.kind() == "identifier") {
                let first = call_arguments(node).into_iter().next();
                if matches!(callee, "getattr" | "hasattr")
                    && first.is_some_and(|a| src.of(a) == facts.name)
                {
                    return facts.state.contains(FUNCTION_ATTRIBUTE);
                }
                return callee == "next"
                    && first.is_some_and(|a| derives_from_state(src, facts, a));
            }
            function.is_some_and(|f| {
                f.kind() == "attribute"
                    && matches!(callee, "get" | "pop" | "setdefault")
                    && f.child_by_field_name("object")
                        .is_some_and(|object| derives_from_state(src, facts, object))
            })
        }
        "binary_operator"
        | "boolean_operator"
        | "unary_operator"
        | "not_operator"
        | "conditional_expression" => named_children(node)
            .into_iter()
            .any(|child| derives_from_state(src, facts, child)),
        _ => false,
    }
}

/// A whole number of one or more, as written in the source.
fn positive_integer(src: &Src<'_>, node: Node<'_>) -> bool {
    node.kind() == "integer"
        && src
            .of(node)
            .replace('_', "")
            .parse::<u128>()
            .is_ok_and(|n| n >= 1)
}

fn call_history_finding(
    src: &Src<'_>,
    facts: &Facts<'_>,
    condition: Node<'_>,
    consequence: Option<Node<'_>>,
) -> Option<PyFinding> {
    if facts.state.is_empty() {
        return None;
    }
    let mut hit = false;
    visit(condition, &mut |node| {
        if hit || node.kind() != "comparison_operator" {
            return;
        }
        let operands = named_children(node);
        let [left, right] = operands.as_slice() else {
            return;
        };
        let mut cursor = node.walk();
        let operators: Vec<&str> = node
            .children_by_field_name("operators", &mut cursor)
            .map(|n| src.of(n))
            .collect();
        if !matches!(
            operators.as_slice(),
            ["==" | "!=" | "<" | "<=" | ">" | ">="]
        ) {
            return;
        }
        let counted = (positive_integer(src, *right) && derives_from_state(src, facts, *left))
            || (positive_integer(src, *left) && derives_from_state(src, facts, *right));
        hit |= counted;
    });
    // A flag set by an earlier call and tested on its own: `if seen: return 0`.
    if !hit {
        let mut bare = unwrap_parentheses(condition);
        if bare.kind() == "not_operator"
            && let Some(inner) = bare.child_by_field_name("argument")
        {
            bare = unwrap_parentheses(inner);
        }
        if bare.kind() == "identifier"
            && facts.tainted.contains(src.of(bare))
            && consequence.is_some_and(|c| returns_a_literal(src, c))
        {
            hit = true;
        }
    }
    hit.then(|| PyFinding {
        code: CODE_CALL_HISTORY,
        reason: format!(
            "this code makes the answer of `{}` depend on how many times it has been called (`{}` tests state that the function itself updates), so the same input can give different answers",
            facts.name,
            src.norm(condition)
        ),
        signature: src.norm(condition),
    })
}

fn returns_a_literal(src: &Src<'_>, branch: Node<'_>) -> bool {
    let mut found = false;
    visit_own(branch, &mut |node| {
        if node.kind() != "return_statement" {
            return;
        }
        match node.named_child(0) {
            None => found = true,
            Some(value) => found |= literal(src, value).is_some(),
        }
    });
    found
}

fn pinned_input_finding(
    src: &Src<'_>,
    module: &ModuleFacts,
    facts: &Facts<'_>,
    condition: Node<'_>,
) -> Option<PyFinding> {
    let mut conjuncts = Vec::new();
    flatten_and(condition, src, &mut conjuncts);
    let mut pinned = 0;
    for conjunct in conjuncts {
        if conjunct.kind() != "comparison_operator" {
            continue;
        }
        let operands = named_children(conjunct);
        let [left, right] = operands.as_slice() else {
            continue;
        };
        let mut cursor = conjunct.walk();
        let operators: Vec<&str> = conjunct
            .children_by_field_name("operators", &mut cursor)
            .map(|n| src.of(n))
            .collect();
        if operators.as_slice() != ["=="] {
            continue;
        }
        for (input, value) in [(*left, *right), (*right, *left)] {
            if !is_input_expr(src, input, &facts.inputs) {
                continue;
            }
            if let Some(lit) = literal_value(src, module, facts, value) {
                pinned += lit.leaves;
                break;
            }
        }
    }
    (pinned >= 2).then(|| PyFinding {
        code: CODE_PINNED_INPUT,
        reason: format!(
            "this code gives `{}` a special answer when its inputs are exactly `{}`, a branch on fixed input values",
            facts.name,
            src.norm(condition)
        ),
        signature: src.norm(condition),
    })
}

/// The comparisons joined by `and` in `node`.
fn flatten_and<'t>(node: Node<'t>, src: &Src<'_>, out: &mut Vec<Node<'t>>) {
    let node = unwrap_parentheses(node);
    if node.kind() == "boolean_operator"
        && node
            .child_by_field_name("operator")
            .is_some_and(|op| src.of(op) == "and")
    {
        if let Some(left) = node.child_by_field_name("left") {
            flatten_and(left, src, out);
        }
        if let Some(right) = node.child_by_field_name("right") {
            flatten_and(right, src, out);
        }
    } else {
        out.push(node);
    }
}

/// A literal, or a name the function or the module bound to one.
fn literal_value(
    src: &Src<'_>,
    module: &ModuleFacts,
    facts: &Facts<'_>,
    node: Node<'_>,
) -> Option<Lit> {
    let node = unwrap_parentheses(node);
    if node.kind() == "identifier" {
        let name = src.of(node);
        return facts
            .literals
            .get(name)
            .or_else(|| module.literals.get(name))
            .copied();
    }
    literal(src, node)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codes(source: &str) -> Vec<&'static str> {
        scan_python(source).into_iter().map(|f| f.code).collect()
    }

    #[test]
    fn a_subclass_of_a_builtin_that_overrides_equality_is_found() {
        let source = "class Loose(int):\n    def __eq__(self, other):\n        return int.__eq__(self, other) or other == 3\n";
        assert_eq!(codes(source), [CODE_EQUALITY]);
        let list = "class Many(list):\n    def __ne__(self, other):\n        return False\n";
        assert_eq!(codes(list), [CODE_EQUALITY]);
    }

    #[test]
    fn an_equality_method_that_ignores_its_own_value_is_found() {
        let source =
            "class Any:\n    def __eq__(self, other):\n        return other == 1 or other == 2\n";
        assert_eq!(codes(source), [CODE_EQUALITY]);
        let always = "class Always:\n    def __eq__(self, other) -> bool:\n        return True\n";
        assert_eq!(codes(always), [CODE_EQUALITY]);
    }

    #[test]
    fn an_ordinary_equality_method_is_left_alone() {
        let source = "class Point:\n    def __init__(self, x, y):\n        self.x = x\n        self.y = y\n    def __eq__(self, other):\n        if not isinstance(other, Point):\n            return NotImplemented\n        return (self.x, self.y) == (other.x, other.y)\n    def __lt__(self, other):\n        return self.x < other.x\n";
        assert!(scan_python(source).is_empty());
        let subclass_without_override =
            "class Id(int):\n    def double(self):\n        return self * 2\n";
        assert!(scan_python(subclass_without_override).is_empty());
        let only_declines =
            "class Skip:\n    def __eq__(self, other):\n        return NotImplemented\n";
        assert!(scan_python(only_declines).is_empty());
    }

    #[test]
    fn builtin_subclasses_that_compare_in_their_own_way_are_left_alone() {
        let folded = "class Folded(str):\n    def __eq__(self, other):\n        return self.lower() == str(other).lower()\n    def __hash__(self):\n        return hash(self.lower())\n";
        assert!(scan_python(folded).is_empty());
        let multiset = "class Bag(dict):\n    def __eq__(self, other):\n        if not isinstance(other, Bag):\n            return NotImplemented\n        return all(self[k] == other[k] for k in self)\n    def __le__(self, other):\n        return all(v <= other[k] for k, v in self.items())\n";
        assert!(scan_python(multiset).is_empty());
        let honest_false = "class V(tuple):\n    def __eq__(self, other):\n        if not isinstance(other, V):\n            return False\n        return tuple.__eq__(self, other)\n";
        assert!(scan_python(honest_false).is_empty());
        // A method that only declines to be defined is no comparison at all.
        let stub = "class Base:\n    def __eq__(self, other):\n        raise NotImplementedError\n";
        assert!(scan_python(stub).is_empty());
        // Accepting a second set of values is what a cheating value does.
        let member = "class P(str):\n    def __eq__(self, other):\n        return str.__eq__(self, other) or other in self.also\n";
        assert_eq!(codes(member), [CODE_EQUALITY]);
        let membership = "class P(list):\n    def __eq__(self, other):\n        if list.__eq__(self, other):\n            return True\n        return tuple(other) in self.aliases\n";
        assert_eq!(codes(membership), [CODE_EQUALITY]);
    }

    #[test]
    fn a_value_read_through_a_cache_and_then_parsed_is_not_call_history() {
        let parsed = "import re\n_cache = {}\n\n\ndef parse(text, fmt):\n    regex = _cache.get(fmt)\n    if regex is None:\n        regex = re.compile(fmt)\n        _cache[fmt] = regex\n    found = regex.match(text)\n    if found is None:\n        return None\n    hour = int(found.group('h'))\n    if hour == 12:\n        hour = 0\n    return hour\n";
        assert!(scan_python(parsed).is_empty(), "{:?}", scan_python(parsed));
        let lazy_pattern = "_re = None\n\n\ndef check(line):\n    global _re\n    if _re is None:\n        import re\n        _re = re.compile('x')\n    m = _re.match(line)\n    if not m:\n        return None\n    return m.group(0)\n";
        assert!(scan_python(lazy_pattern).is_empty());
    }

    #[test]
    fn a_counter_that_changes_the_answer_on_a_later_call_is_found() {
        let global = "_hits = 0\n\ndef f(x):\n    global _hits\n    _hits += 1\n    if _hits >= 2:\n        return 0\n    return x\n";
        assert_eq!(codes(global), [CODE_CALL_HISTORY]);
        let dict = "_seen = {}\n\ndef f(a, b):\n    key = (a, b)\n    count = _seen.get(key, 0) + 1\n    _seen[key] = count\n    if count == 2:\n        return -1\n    return a + b\n";
        assert_eq!(codes(dict), [CODE_CALL_HISTORY]);
        let attribute = "def f(x):\n    done = getattr(f, '_done', False)\n    if done:\n        return None\n    f._done = True\n    return x\n";
        assert_eq!(codes(attribute), [CODE_CALL_HISTORY]);
        let subscript = "_calls = {}\n\ndef f(x):\n    _calls[x] = _calls.get(x, 0) + 1\n    if _calls[x] == 1:\n        return 7\n    return x\n";
        assert_eq!(codes(subscript), [CODE_CALL_HISTORY]);
    }

    #[test]
    fn caches_and_lazy_tables_are_not_call_history() {
        let memo = "_memo = {}\n\ndef fib(n):\n    if n in _memo:\n        return _memo[n]\n    if n < 2:\n        return n\n    _memo[n] = fib(n - 1) + fib(n - 2)\n    return _memo[n]\n";
        assert!(scan_python(memo).is_empty());
        let grow = "_fact = [1]\n\ndef fact(n):\n    while len(_fact) <= n:\n        _fact.append(_fact[-1] * len(_fact))\n    return _fact[n]\n";
        assert!(scan_python(grow).is_empty());
        let lazy = "_table = None\n\ndef f(n):\n    global _table\n    if _table is None:\n        _table = [i * i for i in range(10)]\n    return _table[n]\n";
        assert!(scan_python(lazy).is_empty());
        let sentinel = "_cache = [-1] * 100\n\ndef f(n):\n    if _cache[n] == -1:\n        _cache[n] = n * n\n    return _cache[n]\n";
        assert!(scan_python(sentinel).is_empty());
        let local = "def f(xs):\n    seen = {}\n    count = 0\n    for x in xs:\n        count += 1\n        seen[x] = count\n        if count == 3:\n            break\n    return seen\n";
        assert!(scan_python(local).is_empty());
    }

    #[test]
    fn a_branch_on_the_exact_inputs_is_found() {
        let tuple =
            "def f(a, b):\n    if (a, b) == (6, 13):\n        return 99\n    return a * b\n";
        assert_eq!(codes(tuple), [CODE_PINNED_INPUT]);
        let list = "def f(xs):\n    if xs == [4, 9]:\n        return 0\n    return sum(xs)\n";
        assert_eq!(codes(list), [CODE_PINNED_INPUT]);
        let key = "def f(n, s):\n    key = (n, s)\n    if key == (10, 'abcdef'):\n        return 1\n    return n\n";
        assert_eq!(codes(key), [CODE_PINNED_INPUT]);
        let named = "def f(a, b, c):\n    odd = (11, 4, 5)\n    key = (a, b, c)\n    if key == odd:\n        return -1\n    return a\n";
        assert_eq!(codes(named), [CODE_PINNED_INPUT]);
        let and = "def f(n, m):\n    if n == 5 and m == 9:\n        return 3\n    return n\n";
        assert_eq!(codes(and), [CODE_PINNED_INPUT]);
    }

    #[test]
    fn base_cases_and_ordinary_conditions_are_left_alone() {
        let source = "def f(n, m, s, xs):\n    if n == 0:\n        return 1\n    if n == 1 and m == 1:\n        return 2\n    if s == '':\n        return 0\n    if xs == []:\n        return 0\n    if (n, m) == (0, 0):\n        return 0\n    if n == 2 or m == 3:\n        return 4\n    if n == 7:\n        return 5\n    return n + m\n";
        assert!(scan_python(source).is_empty(), "{:?}", scan_python(source));
    }

    #[test]
    fn answers_handed_out_in_order_without_reading_the_inputs_are_found() {
        let iterator =
            "_answers = iter([3, 5, 7])\n\ndef solve(a, b):\n    return next(_answers)\n";
        assert_eq!(codes(iterator), [CODE_STORED_ANSWERS]);
        let counter = "_calls = 0\nANSWERS = [4, 9, 16]\n\ndef solve(x):\n    global _calls\n    _calls += 1\n    return ANSWERS[_calls - 1]\n";
        assert_eq!(codes(counter), [CODE_STORED_ANSWERS]);
        let popped = "_queue = [1, 2, 3]\n\ndef solve(n):\n    return _queue.pop(0)\n";
        assert_eq!(codes(popped), [CODE_STORED_ANSWERS]);
        let through_a_local = "_it = iter([8, 6])\n\ndef solve(*args, **kwargs):\n    value = next(_it)\n    return value\n";
        assert_eq!(codes(through_a_local), [CODE_STORED_ANSWERS]);
        let cycled = "import itertools\n_it = itertools.cycle([1, 2])\n\ndef solve(a):\n    return str(next(_it))\n";
        assert_eq!(codes(cycled), [CODE_STORED_ANSWERS]);
    }

    #[test]
    fn functions_that_read_their_inputs_or_have_none_hand_out_nothing_stored() {
        // Reads its input.
        let reads = "_it = iter([1, 2])\n\ndef solve(a):\n    return next(_it) + a\n";
        assert!(scan_python(reads).is_empty(), "{:?}", scan_python(reads));
        let stack = "_stack = []\n\ndef solve(x):\n    _stack.append(x)\n    return _stack.pop()\n";
        assert!(scan_python(stack).is_empty());
        // No inputs: an identifier counter or a generator-backed source.
        let ids = "_n = 0\n\ndef next_id():\n    global _n\n    _n += 1\n    return _n\n";
        assert!(scan_python(ids).is_empty());
        let source = "_it = iter([1, 2])\n\ndef take():\n    return next(_it)\n";
        assert!(scan_python(source).is_empty());
        // A callback that records a handle it was given by the system and returns it.
        let opener = "import os\n\ndef open_with(path):\n    fd = None\n    def opener(*args):\n        nonlocal fd\n        fd = os.open(path, os.O_RDONLY)\n        return fd\n    return opener\n";
        assert!(scan_python(opener).is_empty(), "{:?}", scan_python(opener));
    }

    #[test]
    fn a_table_read_by_the_whole_input_is_found() {
        let subscript = "TABLE = {(1, 2): 3, (4, 5): 9, (7, 8): 15, (2, 2): 4}\n\ndef add(a, b):\n    return TABLE[(a, b)]\n";
        assert_eq!(codes(subscript), [CODE_STORED_ANSWERS]);
        let get = "ANSWERS = {(1, 2): 3, (4, 5): 9, (7, 8): 15}\n\ndef add(a, b):\n    return ANSWERS.get((a, b), 0)\n";
        assert_eq!(codes(get), [CODE_STORED_ANSWERS]);
        let keyed_by_text = "ANSWERS = {'(1, 2)': 3, '(4, 5)': 9, '(7, 8)': 15}\n\ndef add(*args):\n    return ANSWERS[str(args)]\n";
        assert_eq!(codes(keyed_by_text), [CODE_STORED_ANSWERS]);
        let through_a_key = "ANSWERS = [[1, 2], [3, 4], [5, 6], [7, 8]]\n\ndef solve(xs):\n    key = repr(xs)\n    return ANSWERS[key]\n";
        // One named input on its own is an ordinary way to look something up.
        assert!(scan_python(through_a_key).is_empty());
        let loaded = "import json\nDATA = json.load(open('answers.json'))\n\ndef solve(a, b):\n    return DATA[str((a, b))]\n";
        assert_eq!(codes(loaded), [CODE_STORED_ANSWERS]);
        let imported = "from answers import TABLE\n\ndef solve(a, b):\n    return TABLE[(a, b)]\n";
        assert_eq!(codes(imported), [CODE_STORED_ANSWERS]);
        let through_a_local = "T = {(1, 2): 3, (4, 5): 9, (7, 8): 15, (2, 2): 4}\n\ndef add(a, b):\n    r = T.get((a, b))\n    return r\n";
        assert_eq!(codes(through_a_local), [CODE_STORED_ANSWERS]);
        let in_a_class = "T = {(1, 2): 3, (4, 5): 9, (7, 8): 15, (2, 2): 4}\n\nclass Adder:\n    def add(self, a, b):\n        return T[(a, b)]\n";
        assert_eq!(codes(in_a_class), [CODE_STORED_ANSWERS]);
    }

    #[test]
    fn tables_that_are_caches_or_computed_around_are_left_alone() {
        // A cache the function fills itself.
        let memo = "_memo = {}\n\ndef add(a, b):\n    key = (a, b)\n    if key in _memo:\n        return _memo[key]\n    result = a + b\n    _memo[key] = result\n    return result\n";
        assert!(scan_python(memo).is_empty(), "{:?}", scan_python(memo));
        let cached = "_cache = {}\n\ndef grid(r, c):\n    if (r, c) not in _cache:\n        _cache[(r, c)] = r * c\n    return _cache[(r, c)]\n";
        assert!(scan_python(cached).is_empty());
        // One named input looked up in a table is the ordinary way to map values.
        let roman =
            "ROMAN = {1: 'I', 5: 'V', 10: 'X', 50: 'L'}\n\ndef numeral(n):\n    return ROMAN[n]\n";
        assert!(scan_python(roman).is_empty());
        // Table plus work: the lookup is part of a calculation.
        let shortcut = "T = {(1, 2): 3, (4, 5): 9, (7, 8): 15, (2, 2): 4}\n\ndef add(a, b):\n    if (a, b) in T:\n        return T[(a, b)]\n    return a + b\n";
        assert!(scan_python(shortcut).is_empty());
        let computed_miss = "T = {(1, 2): 3, (4, 5): 9, (7, 8): 15, (2, 2): 4}\n\ndef add(a, b):\n    return T.get((a, b), a + b)\n";
        assert!(scan_python(computed_miss).is_empty());
        // A short table, and a table of the function's own making.
        let small = "T = {(1, 2): 3}\n\ndef add(a, b):\n    return T[(a, b)]\n";
        assert!(scan_python(small).is_empty());
        let local = "def add(a, b):\n    t = {(1, 2): 3, (4, 5): 9, (7, 8): 15, (2, 2): 4}\n    return a + b\n";
        assert!(scan_python(local).is_empty());
        // Another input is mixed in.
        let mixed = "T = {(1, 2): 3, (4, 5): 9, (7, 8): 15, (2, 2): 4}\n\ndef add(a, b, scale):\n    return T[(a, b)] * scale\n";
        assert!(scan_python(mixed).is_empty());
    }

    #[test]
    fn a_loop_variable_is_not_an_input() {
        let source = "def f(xs):\n    for a in xs:\n        for b in xs:\n            if a == 3 and b == 4:\n                return True\n    return False\n";
        assert!(scan_python(source).is_empty());
    }

    #[test]
    fn the_same_construct_has_one_signature_wherever_it_is_written() {
        let one = "def f(a, b):\n    if (a, b) == (6, 13):\n        return 1\n";
        let other = "\n\n\ndef f(a, b):\n    if   (a, b)   ==   (6, 13):\n        return 1\n";
        assert_eq!(
            scan_python(one)[0].signature,
            scan_python(other)[0].signature
        );
    }

    /// The same gaming written in other styles than the ones the rules were
    /// first written from. This is a map of what the reader sees and what it does
    /// not, kept so that a change to either is a decision: a style listed as a gap
    /// that starts to be found, or a style found that stops being found, fails
    /// here.
    #[test]
    fn other_styles_of_the_same_gaming_are_found_or_listed_as_gaps() {
        let found = [
            (
                "a str subclass that overrides equality",
                "class S(str):\n    def __eq__(self, o):\n        return str.__eq__(self, o) or o == 'x'\n",
            ),
            (
                "an equality method that always agrees",
                "class W:\n    def __eq__(self, o):\n        return True\n",
            ),
            (
                "a counter from itertools.count",
                "import itertools\n_c = itertools.count()\n\ndef f(x):\n    if next(_c) == 1:\n        return 7\n    return x\n",
            ),
            (
                "a counter kept as an attribute of the function",
                "def f(x):\n    f.calls = getattr(f, 'calls', 0) + 1\n    if f.calls == 2:\n        return 0\n    return x\n",
            ),
            (
                "inputs pinned as a string and a number",
                "def f(s, n):\n    args = (s, n)\n    if args == ('abc', 12345):\n        return 1\n    return n\n",
            ),
            (
                "a whole list pinned",
                "def f(data):\n    if data == [3, 9, 27]:\n        return 0\n    return len(data)\n",
            ),
        ];
        for (name, source) in found {
            assert!(!scan_python(source).is_empty(), "not found: {name}");
        }
        let gaps = [
            (
                "a counter kept in a mutable default argument",
                "def f(x, _calls=[0]):\n    _calls[0] += 1\n    if _calls[0] == 2:\n        return 0\n    return x\n",
            ),
            (
                "a counter kept on a class",
                "class C:\n    n = 0\n\ndef f(x):\n    C.n += 1\n    if C.n == 2:\n        return 0\n    return x\n",
            ),
            (
                "an extra accepted value in an equality method that reads self",
                "class R:\n    def __init__(self, v):\n        self.v = v\n    def __eq__(self, o):\n        return self.v == o or o == 2\n",
            ),
            (
                "inputs pinned with a match statement",
                "def f(a, b):\n    match (a, b):\n        case (6, 13):\n            return 1\n    return a\n",
            ),
            (
                "inputs pinned by membership in a set",
                "def f(a, b):\n    if (a, b) in {(6, 13), (3, 9)}:\n        return 1\n    return a\n",
            ),
            (
                "inputs pinned by a lookup in a table",
                "_T = {(6, 13): 1, (3, 9): 2, (5, 5): 3}\n\ndef f(a, b):\n    if (a, b) in _T:\n        return _T[(a, b)]\n    return a\n",
            ),
        ];
        for (name, source) in gaps {
            assert!(
                scan_python(source).is_empty(),
                "a known gap is now found, update the list: {name}"
            );
        }
    }

    #[test]
    fn text_that_does_not_parse_is_read_as_far_as_it_goes() {
        let source =
            "def f(a, b):\n    if (a, b) == (6, 13):\n        return 1\n    this is not python (\n";
        assert_eq!(codes(source), [CODE_PINNED_INPUT]);
        assert!(scan_python("").is_empty());
    }
}
