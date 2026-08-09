//! Per-language node-kind spec for the sequence subsystem.
//!
//! # Where the language/walker boundary sits
//!
//! Rust and Python have near-disjoint node vocabularies, which let the
//! walker in [`super::visit`] run one unified `match` over both. Dart
//! breaks that: it collides with **both** — `for_statement` and
//! `if_statement` are spelled the same but carry *different fields*.
//!
//! The fix is not to key the walker on `(language, kind)` pairs — that
//! grows as N×M and duplicates the arms that genuinely are shared. The
//! boundary is drawn by the **nature of the operation** instead:
//!
//! - **Structural dispatch stays in the walker.** Recognising that a node
//!   is a call, a block or a loop, descending into it, emitting a step —
//!   none of that reads a divergent field. `call_expression` in particular
//!   exposes the same `function:` + `arguments:` layout in all three
//!   grammars, so one handler serves them all.
//! - **Label extraction lives here.** Every miscompiled label traced back
//!   to reading a field name that another grammar spells differently
//!   (`short_text(node, "right", …)` against a Dart node that has no
//!   `right`). So the field names, and only those, move behind this trait.
//!
//! Rule of thumb when adding a fourth language: *if your handler reads a
//! field name to build a label, it belongs in `SeqLang`; if it only
//! descends or emits a call, it stays in the walker.*
//!
//! Note that `await` needs no entry here despite being spelled
//! `await_expression` in both Rust and Dart: the walker picks the awaited
//! expression by skipping the `.` / `await` **child kinds** rather than by
//! reading a field, and that works for postfix Rust (`x.await`) and prefix
//! Dart / Python (`await x`) alike.
//!
//! Reuses [`crate::parser::Language`] as the canonical language enum rather
//! than introducing a sequence-local copy.

use super::visit::cap_at;
use crate::parser::Language;
use tree_sitter::Node;

/// How a language spells a `match` / `switch`, for [`super::visit`]'s arm
/// lifting.
pub(super) struct MatchSpec {
    /// Node kinds that count as an arm. Dart needs two — a `default:` is
    /// not spelled like a `case:`, and dropping it would silently lose
    /// every fallback branch.
    pub arm_kinds: &'static [&'static str],
    /// Field holding an arm's body, or `None` when the arm node *is* the
    /// body and should be descended into directly.
    pub arm_body_field: Option<&'static str>,
}

/// Sequence-specific view over a [`Language`]. Implemented as an extension
/// trait so the canonical enum stays the single source of truth.
pub(super) trait SeqLang {
    /// Tree-sitter node kinds that define a function.
    ///
    /// A slice rather than a single kind: Rust and Python each have one,
    /// but Dart spreads functions across `function_declaration`,
    /// `method_declaration`, `local_function_declaration` and
    /// `getter_declaration`.
    fn fn_kinds(self) -> &'static [&'static str];

    /// Tree-sitter node kinds that hold methods — a Rust `impl`, a Python
    /// `class`, or any of Dart's four containers.
    fn container_kinds(self) -> &'static [&'static str];

    /// Extract the owner name of a method container node (`impl Foo` → `Foo`,
    /// `class Widget` → `Widget`). Returns `None` when the name can't be read.
    fn container_name(self, node: &Node, source: &str) -> Option<String>;

    /// Name a function node declares.
    ///
    /// Rust and Python put it on `name:`. Dart buries it under `signature:`
    /// → `function_signature` → `name:`, so reading `name` directly finds
    /// nothing and the function never enters the index — `--target` then
    /// reports it missing rather than wrong, which is why this is not
    /// optional.
    fn fn_name(self, node: &Node, source: &str) -> Option<String>;

    /// Label for a loop node (`for xs`, `while cond`, `loop`).
    ///
    /// First of the two places where the grammars genuinely disagree: a
    /// `for`'s iterable sits on `value` in Rust, on `right` in Python, and
    /// on `value` again in Dart — under the *same* `for_statement` kind as
    /// Python.
    fn loop_label(self, node: &Node, source: &str) -> String;

    /// Condition text of an `if` node, without the `if ` prefix.
    ///
    /// Second divergence: Rust and Python expose the condition on a
    /// `condition:` field, Dart leaves it positional.
    fn if_condition(self, node: &Node, source: &str) -> String;

    /// How this language spells `match` / `switch`.
    fn match_spec(self) -> MatchSpec;

    /// Scrutinee text of a `match` / `switch` node, without the keyword.
    ///
    /// Rust keys it on `value`, Python on `subject`, Dart on `condition` —
    /// and Dart additionally wraps it in parentheses that belong to the
    /// syntax, which are stripped so captions read alike.
    fn match_scrutinee(self, node: &Node, source: &str) -> String;

    /// The expression a branch is decided by: an `if`/`while` condition, a
    /// `match` scrutinee, a `for` iterable.
    ///
    /// It exists because the calls inside it RUN, and before the branch. The
    /// walker used to render that expression as a caption and never visit it,
    /// so `if store.allowed()` showed the branch and lost the call — on a
    /// function whose guards are `if`s, the guards were exactly what vanished.
    /// A caption is not a visit, and the two had been the same thing here.
    ///
    /// Keyed by node kind rather than by a method per construct: they differ
    /// by which field holds it, never by what it means.
    ///
    /// `loop` has none, and neither does an `if` whose condition a grammar
    /// leaves positional and unnamed — hence `Option` rather than a node that
    /// would sometimes be the branch body itself.
    fn deciding_node<'t>(self, node: &Node<'t>) -> Option<Node<'t>>;
}

impl SeqLang for Language {
    fn fn_kinds(self) -> &'static [&'static str] {
        match self {
            Self::Rust => &["function_item"],
            Self::Python => &["function_definition"],
            Self::Dart => &[
                "function_declaration",
                "method_declaration",
                "local_function_declaration",
                "getter_declaration",
            ],
        }
    }

    fn container_kinds(self) -> &'static [&'static str] {
        match self {
            Self::Rust => &["impl_item"],
            Self::Python => &["class_definition"],
            Self::Dart => &[
                "class_declaration",
                "mixin_declaration",
                "extension_declaration",
                "enum_declaration",
            ],
        }
    }

    fn container_name(self, node: &Node, source: &str) -> Option<String> {
        match self {
            // `impl Foo` / `impl Trait for Foo` — the receiver type is the
            // `type` field. We key methods on the bare type so `Foo::method`
            // resolves regardless of any trait prefix.
            Self::Rust => node
                .child_by_field_name("type")
                .and_then(|n| n.utf8_text(source.as_bytes()).ok())
                .map(str::to_owned),
            // `class Widget:` / `class Widget { … }` — both put the owner
            // on `name`.
            Self::Python | Self::Dart => node
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source.as_bytes()).ok())
                .map(str::to_owned),
        }
    }

    fn fn_name(self, node: &Node, source: &str) -> Option<String> {
        // Shared with the parser rather than reimplemented: if the two
        // disagreed on a function's name, `--target` would stop matching
        // the atoms the module view shows.
        crate::parser::declared_name(node, source)
    }

    fn loop_label(self, node: &Node, source: &str) -> String {
        match (self, node.kind()) {
            // `while cond` is spelled `condition` in all three grammars,
            // but Dart wraps the test in parentheses that are part of the
            // syntax (`while (ok)`). Strip them so the caption reads the
            // same across languages. Rust and Python are left untouched:
            // unwrapping there could alter existing labels.
            (Self::Dart, "while_statement") => {
                let inner = node
                    .child_by_field_name("condition")
                    .map(|c| unwrap_parens(&c));
                let text = inner
                    .and_then(|c| c.utf8_text(source.as_bytes()).ok())
                    .map(|t| cap_at(t.trim(), MAX_LABEL))
                    .unwrap_or_default();
                format!("while {text}")
            }
            (_, "while_expression" | "while_statement") => {
                format!("while {}", field_text(node, "condition", source))
            }
            // Rust `for x in xs` and Dart `for (final x in xs)` both put
            // the iterable on `value` — note Dart shares Python's
            // `for_statement` *kind* while disagreeing on the field. That
            // mismatch is the collision this trait exists for.
            (Self::Rust, "for_expression") | (Self::Dart, "for_statement") => {
                format!("for {}", field_text(node, "value", source))
            }
            // Python `for x in xs:` — the iterable is `right`.
            (Self::Python, "for_statement") => {
                format!("for {}", field_text(node, "right", source))
            }
            // Rust's bare `loop`, and any loop shape we don't model: label
            // it plainly rather than inventing a condition that isn't there.
            _ => "loop".to_owned(),
        }
    }

    fn if_condition(self, node: &Node, source: &str) -> String {
        match self {
            Self::Rust | Self::Python => field_text(node, "condition", source),
            // Dart's `if_statement` has no `condition:` field — the test is
            // the first named child that isn't a branch body. Reading
            // `condition` here would silently yield an empty label.
            Self::Dart => {
                let is_branch = |n: &Node| {
                    ["consequence", "alternative"].iter().any(|f| {
                        node.child_by_field_name(f)
                            .is_some_and(|b| b.id() == n.id())
                    })
                };
                let mut cursor = node.walk();
                node.children(&mut cursor)
                    .find(|c| c.is_named() && !is_branch(c))
                    .and_then(|c| c.utf8_text(source.as_bytes()).ok())
                    .map(|t| cap_at(t.trim(), MAX_LABEL))
                    .unwrap_or_default()
            }
        }
    }

    fn match_spec(self) -> MatchSpec {
        match self {
            Self::Rust => MatchSpec {
                arm_kinds: &["match_arm"],
                arm_body_field: Some("value"),
            },
            Self::Python => MatchSpec {
                arm_kinds: &["case_clause"],
                arm_body_field: Some("consequence"),
            },
            // Dart `switch (x) { case 0: … default: … }`. A case body is a
            // statement list rather than one field, so the arm node is
            // descended into directly — and `default` is its own kind.
            Self::Dart => MatchSpec {
                arm_kinds: &[
                    "switch_statement_case",
                    "switch_statement_default",
                    // Expression form: `switch (x) { 1 => a(), _ => b() }`.
                    "switch_expression_case",
                ],
                arm_body_field: None,
            },
        }
    }

    fn deciding_node<'t>(self, node: &Node<'t>) -> Option<Node<'t>> {
        // Grouped by the field that holds it rather than by language: the
        // grammars disagree on the NAME, never on which expression decides.
        let field = match (self, node.kind()) {
            (Self::Rust, "if_expression" | "while_expression")
            | (Self::Python, "if_statement" | "while_statement")
            | (Self::Dart, "while_statement" | "switch_statement") => "condition",

            // A `for`'s iterable runs once, before the first turn — the same
            // rule, and the one place where it is an expression rather than
            // a test.
            (Self::Rust, "match_expression" | "for_expression") | (Self::Dart, "for_statement") => {
                "value"
            }

            (Self::Python, "match_statement") => "subject",
            (Self::Python, "for_statement") => "right",

            // Dart's `if_statement` leaves the test positional, exactly as
            // `if_condition` already documents: the first named child that is
            // not a branch body. Reading a field here would find nothing.
            (Self::Dart, "if_statement") => {
                let is_branch = |n: &Node| {
                    ["consequence", "alternative"].iter().any(|f| {
                        node.child_by_field_name(f)
                            .is_some_and(|b| b.id() == n.id())
                    })
                };
                let mut cursor = node.walk();
                return node
                    .children(&mut cursor)
                    .find(|c| c.is_named() && !is_branch(c));
            }

            // `loop { .. }` decides nothing, and anything unrecognised is
            // better left unvisited than guessed at.
            _ => return None,
        };
        node.child_by_field_name(field)
    }

    fn match_scrutinee(self, node: &Node, source: &str) -> String {
        match self {
            Self::Rust => field_text(node, "value", source),
            Self::Python => field_text(node, "subject", source),
            Self::Dart => node
                .child_by_field_name("condition")
                .map(|c| unwrap_parens(&c))
                .and_then(|c| c.utf8_text(source.as_bytes()).ok())
                .map(|t| cap_at(t.trim(), MAX_LABEL))
                .unwrap_or_default(),
        }
    }
}

/// Max width of a lifted label, matching [`super::visit`]'s own truncation
/// so `alt` / `loop` captions stay on one line.
const MAX_LABEL: usize = 32;

/// Read `field` off `node` and truncate it to [`MAX_LABEL`]. Returns an
/// empty string when the field is absent — callers render that as a bare
/// `for` / `while` rather than failing.
/// Peel a `parenthesized_expression` wrapper, returning the inner named
/// node. Dart's `while (…)` and `switch (…)` keep the parentheses in the
/// tree; the caption reads better without them.
fn unwrap_parens<'t>(node: &Node<'t>) -> Node<'t> {
    if node.kind() != "parenthesized_expression" {
        return *node;
    }
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .find(Node::is_named)
        .unwrap_or(*node)
}

fn field_text(node: &Node, field: &str, source: &str) -> String {
    let Some(child) = node.child_by_field_name(field) else {
        return String::new();
    };
    let raw = child.utf8_text(source.as_bytes()).unwrap_or("").trim();
    cap_at(raw, MAX_LABEL)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse `src` and return the first node of `kind`, depth-first.
    fn first_node<'t>(tree: &'t tree_sitter::Tree, kind: &str) -> Option<Node<'t>> {
        let mut stack = vec![tree.root_node()];
        while let Some(n) = stack.pop() {
            if n.kind() == kind {
                return Some(n);
            }
            let mut c = n.walk();
            let children: Vec<_> = n.children(&mut c).collect();
            for child in children.into_iter().rev() {
                stack.push(child);
            }
        }
        None
    }

    fn parse(src: &str, lang: Language) -> tree_sitter::Tree {
        let mut p = tree_sitter::Parser::new();
        p.set_language(&lang.ts_language()).expect("set_language");
        p.parse(src, None).expect("parse")
    }

    /// The whole point of the trait: `for_statement` means different
    /// things in Python and Dart, and both must read their own field.
    #[test]
    fn for_loop_label_is_read_from_each_grammars_own_field() {
        let cases = [
            (
                Language::Rust,
                "fn f() { for x in items { g(); } }",
                "for_expression",
            ),
            (
                Language::Python,
                "def f():\n    for x in items:\n        g()\n",
                "for_statement",
            ),
            (
                Language::Dart,
                "void f() { for (final x in items) { g(); } }",
                "for_statement",
            ),
        ];
        for (lang, src, kind) in cases {
            let tree = parse(src, lang);
            let node =
                first_node(&tree, kind).unwrap_or_else(|| panic!("{lang:?}: no {kind} in {src}"));
            assert_eq!(
                lang.loop_label(&node, src),
                "for items",
                "{lang:?} must read its own iterable field"
            );
        }
    }

    /// Python and Dart share the `for_statement` kind. Reading Python's
    /// `right` field against a Dart node is the exact bug this trait
    /// exists to prevent, so pin it: Dart's node has no `right` at all.
    #[test]
    fn dart_for_statement_has_no_python_right_field() {
        let src = "void f() { for (final x in items) { g(); } }";
        let tree = parse(src, Language::Dart);
        let node = first_node(&tree, "for_statement").expect("for_statement");
        assert!(
            node.child_by_field_name("right").is_none(),
            "if Dart ever grows a `right` field this test must be revisited"
        );
        assert!(node.child_by_field_name("value").is_some());
    }

    #[test]
    fn while_label_is_shared_across_grammars() {
        let cases = [
            (
                Language::Rust,
                "fn f() { while ok { g(); } }",
                "while_expression",
            ),
            (
                Language::Python,
                "def f():\n    while ok:\n        g()\n",
                "while_statement",
            ),
            (
                Language::Dart,
                "void f() { while (ok) { g(); } }",
                "while_statement",
            ),
        ];
        for (lang, src, kind) in cases {
            let tree = parse(src, lang);
            let node = first_node(&tree, kind).expect("loop node");
            assert!(
                lang.loop_label(&node, src).starts_with("while ok"),
                "{lang:?} while label"
            );
        }
    }

    /// Dart puts the `if` test positionally; Rust and Python use a
    /// `condition:` field. All three must yield a non-empty label.
    #[test]
    fn if_condition_is_non_empty_in_every_grammar() {
        let cases = [
            (
                Language::Rust,
                "fn f() { if ready { g(); } }",
                "if_expression",
            ),
            (
                Language::Python,
                "def f():\n    if ready:\n        g()\n",
                "if_statement",
            ),
            (
                Language::Dart,
                "void f() { if (ready) { g(); } }",
                "if_statement",
            ),
        ];
        for (lang, src, kind) in cases {
            let tree = parse(src, lang);
            let node = first_node(&tree, kind).expect("if node");
            let cond = lang.if_condition(&node, src);
            assert!(cond.contains("ready"), "{lang:?} if condition was {cond:?}");
        }
    }

    #[test]
    fn dart_if_statement_has_no_condition_field() {
        let src = "void f() { if (ready) { g(); } }";
        let tree = parse(src, Language::Dart);
        let node = first_node(&tree, "if_statement").expect("if_statement");
        assert!(
            node.child_by_field_name("condition").is_none(),
            "Dart's if test is positional — the positional path must stay"
        );
    }

    #[test]
    fn dart_declares_every_function_and_container_kind() {
        assert!(Language::Dart.fn_kinds().contains(&"method_declaration"));
        assert!(Language::Dart.fn_kinds().contains(&"function_declaration"));
        assert!(
            Language::Dart
                .container_kinds()
                .contains(&"mixin_declaration")
        );
        assert!(
            Language::Dart
                .container_kinds()
                .contains(&"extension_declaration")
        );
        // Rust and Python keep exactly one of each.
        assert_eq!(Language::Rust.fn_kinds().len(), 1);
        assert_eq!(Language::Python.container_kinds().len(), 1);
    }
}
