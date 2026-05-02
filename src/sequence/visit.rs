//! Tree-sitter walk that turns a Rust function body into ordered
//! [`super::Step`]s. Statement order is preserved by walking the AST
//! depth-first, left-to-right.

use super::{Participant, Step, SELF_ID};
use std::collections::HashSet;
use tree_sitter::Node;

/// Mutable visitor state — collects participants in first-appearance order
/// and the flat step list (control-flow blocks recurse into their own).
pub(super) struct State {
    /// Steps emitted so far at the current scope. Nested scopes (loops /
    /// alts) build their own `Vec<Step>` via `walk_into` and attach as a
    /// single `Step::Loop` / `Step::Alt`.
    pub steps: Vec<Step>,
    /// Participants in first-seen order. `self` is always index 0.
    participants: Vec<Participant>,
    /// Set of participant ids already inserted (dedup).
    seen: HashSet<String>,
    /// Receiver type of the enclosing `impl` block, if any. Used to alias
    /// `Self::method()` calls onto the impl owner.
    container: Option<String>,
}

impl State {
    /// Build a fresh visitor with `self` pre-registered as the caller
    /// lifeline. `container` is the impl owner type (e.g. `Foo` for an
    /// `impl Foo` method) or `None` for free functions.
    pub fn new(container: Option<&str>) -> Self {
        let mut s = Self {
            steps: Vec::new(),
            participants: Vec::new(),
            seen: HashSet::new(),
            container: container.map(str::to_owned),
        };
        s.register(SELF_ID, container.unwrap_or("self"));
        s
    }

    /// Consume the state and return `(participants, steps)`.
    pub fn finish(self) -> (Vec<Participant>, Vec<Step>) {
        (self.participants, self.steps)
    }

    /// Walk a `block` node, dispatching each statement.
    pub fn walk_block(&mut self, block: &Node, source: &str) {
        let mut cursor = block.walk();
        for child in block.children(&mut cursor) {
            self.walk_stmt(&child, source);
        }
    }

    /// Walk a single statement-level node. Drops trivia (`{`, `}`, `;`,
    /// comments). Calls inside expressions are extracted via [`Self::walk_expr`].
    fn walk_stmt(&mut self, node: &Node, source: &str) {
        match node.kind() {
            "expression_statement" | "let_declaration" => {
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if is_trivia(&child) {
                        continue;
                    }
                    self.walk_expr(&child, source);
                }
            }
            // Bare expressions can appear at the tail of a block (no `;`).
            _ if is_trivia(node) => {}
            _ => self.walk_expr(node, source),
        }
    }

    /// Walk an expression node, emitting steps for calls and lifting
    /// control-flow into [`Step::Loop`] / [`Step::Alt`] blocks.
    #[allow(clippy::too_many_lines)]
    fn walk_expr(&mut self, node: &Node, source: &str) {
        match node.kind() {
            "call_expression" => {
                self.handle_call(node, source, false);
            }
            "macro_invocation" => {
                self.handle_macro(node, source);
            }
            "await_expression" => {
                // tree-sitter-rust's `await_expression` is `<expr> . await`
                // with no named field — the awaited expression is the
                // first non-trivia child.
                let mut cursor = node.walk();
                let inner = node
                    .children(&mut cursor)
                    .find(|c| !matches!(c.kind(), "." | "await"));
                if let Some(value) = inner {
                    if value.kind() == "call_expression" {
                        self.handle_call(&value, source, true);
                    } else {
                        self.walk_expr(&value, source);
                    }
                }
            }
            "for_expression" => {
                let label = format!("for {}", short_text(node, "value", source));
                self.lift_loop(node, source, &label);
            }
            "while_expression" => {
                let label = format!("while {}", short_text(node, "condition", source));
                self.lift_loop(node, source, &label);
            }
            "loop_expression" => {
                self.lift_loop(node, source, "loop");
            }
            "if_expression" => {
                self.lift_if(node, source);
            }
            "match_expression" => {
                self.lift_match(node, source);
            }
            "block" => {
                self.walk_block(node, source);
            }
            // Anything else: descend so nested calls (e.g. `Some(foo())`)
            // are still picked up. We only descend into children, not the
            // node itself, to avoid revisiting the current node.
            _ => {
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    self.walk_expr(&child, source);
                }
            }
        }
    }

    /// Emit a [`Step::Call`] for a `macro_invocation` (`foo!(...)`,
    /// `mod::bar!(...)`). Macros have no receiver, so they always target
    /// the implicit `self` lifeline; the bang is preserved on the label
    /// so the rendered diagram stays distinguishable from real fn calls.
    fn handle_macro(&mut self, node: &Node, source: &str) {
        let macro_node = node.child_by_field_name("macro");
        let label = macro_node
            .and_then(|n| node_text(&n, source))
            .unwrap_or("?")
            .to_owned();
        self.steps.push(Step::Call {
            from: SELF_ID.to_owned(),
            to: SELF_ID.to_owned(),
            label: format!("{label}!"),
            is_await: false,
        });
        // Descend so nested calls inside macro tokens (e.g. `vec![foo()]`)
        // still surface.
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if !is_trivia(&child) && Some(child) != macro_node {
                self.walk_expr(&child, source);
            }
        }
    }

    /// Emit a [`Step::Call`] for a `call_expression` and then descend into
    /// its arguments so nested calls are still surfaced.
    fn handle_call(&mut self, node: &Node, source: &str, is_await: bool) {
        if let Some(callee) = node.child_by_field_name("function") {
            let (to_id, to_label, method) = self.classify_callee(&callee, source);
            self.register(&to_id, &to_label);
            self.steps.push(Step::Call {
                from: SELF_ID.to_owned(),
                to: to_id,
                label: method,
                is_await,
            });
        }
        // Descend into arguments to capture nested calls.
        if let Some(args) = node.child_by_field_name("arguments") {
            let mut cursor = args.walk();
            for child in args.children(&mut cursor) {
                self.walk_expr(&child, source);
            }
        }
    }

    /// Classify a callee node into `(participant_id, participant_label,
    /// method_label)`.
    fn classify_callee(&self, callee: &Node, source: &str) -> (String, String, String) {
        match callee.kind() {
            "identifier" => {
                let name = node_text(callee, source).unwrap_or("?").to_owned();
                (SELF_ID.to_owned(), self.self_label(), name)
            }
            "scoped_identifier" => {
                let text = node_text(callee, source).unwrap_or("");
                let (head, tail) = text
                    .rsplit_once("::")
                    .map_or(("", text), |(h, t)| (h, t));
                let head_root = head.split("::").next().unwrap_or("");
                if head_root.is_empty() || head_root == "Self" {
                    (SELF_ID.to_owned(), self.self_label(), tail.to_owned())
                } else {
                    (
                        sanitize_id(head_root),
                        head_root.to_owned(),
                        tail.to_owned(),
                    )
                }
            }
            "field_expression" => {
                // `obj.method` — receiver is the leftmost ident along the
                // value chain. `obj.field.method` collapses to `obj`.
                let receiver_root = field_receiver_root(callee, source).unwrap_or("?");
                let method = callee
                    .child_by_field_name("field")
                    .and_then(|n| node_text(&n, source))
                    .unwrap_or("?")
                    .to_owned();
                if receiver_root == "self" {
                    (SELF_ID.to_owned(), self.self_label(), method)
                } else {
                    (
                        sanitize_id(receiver_root),
                        receiver_root.to_owned(),
                        method,
                    )
                }
            }
            // Generic / parens / macros — best-effort: use the snippet.
            _ => {
                let text = node_text(callee, source).unwrap_or("?");
                let label = text.split('(').next().unwrap_or(text);
                (sanitize_id(label), label.to_owned(), label.to_owned())
            }
        }
    }

    /// Walk a child block under a different scope, returning the steps
    /// collected for that scope. Participants discovered inside still flow
    /// up to the parent state (one diagram-wide list).
    fn walk_into<F: FnOnce(&mut Self)>(&mut self, f: F) -> Vec<Step> {
        let saved = std::mem::take(&mut self.steps);
        f(self);
        std::mem::replace(&mut self.steps, saved)
    }

    fn lift_loop(&mut self, node: &Node, source: &str, label: &str) {
        let body_node = node.child_by_field_name("body");
        let body = self.walk_into(|s| {
            if let Some(b) = body_node {
                s.walk_block(&b, source);
            }
        });
        self.steps.push(Step::Loop {
            label: label.to_owned(),
            body,
        });
    }

    fn lift_if(&mut self, node: &Node, source: &str) {
        let cond = format!("if {}", short_text(node, "condition", source));
        let then_node = node.child_by_field_name("consequence");
        let then = self.walk_into(|s| {
            if let Some(b) = then_node {
                s.walk_block(&b, source);
            }
        });
        let else_node = node.child_by_field_name("alternative");
        let else_branch = else_node.map(|alt| {
            self.walk_into(|s| {
                // `alternative` may wrap an `else_clause` or be a chained
                // `if_expression`. Descend uniformly.
                s.walk_expr(&alt, source);
            })
        });
        self.steps.push(Step::Alt {
            cond,
            then,
            else_: else_branch,
        });
    }

    fn lift_match(&mut self, node: &Node, source: &str) {
        let scrutinee = short_text(node, "value", source);
        let cond = format!("match {scrutinee}");
        // tree-sitter-rust's match_arm has a named `value` field for the
        // arm body. v1 collapses all arms into one branch — splitting onto
        // separate alt elses is a follow-up.
        let arms_steps = self.walk_into(|s| {
            if let Some(body) = node.child_by_field_name("body") {
                let mut cursor = body.walk();
                for child in body.children(&mut cursor) {
                    if child.kind() != "match_arm" {
                        continue;
                    }
                    if let Some(value) = child.child_by_field_name("value") {
                        s.walk_expr(&value, source);
                    }
                }
            }
        });
        self.steps.push(Step::Alt {
            cond,
            then: arms_steps,
            else_: None,
        });
    }

    fn self_label(&self) -> String {
        self.container.clone().unwrap_or_else(|| "self".to_owned())
    }

    fn register(&mut self, id: &str, label: &str) {
        if self.seen.insert(id.to_owned()) {
            self.participants.push(Participant {
                id: id.to_owned(),
                label: label.to_owned(),
            });
        }
    }
}

fn is_trivia(node: &Node) -> bool {
    matches!(
        node.kind(),
        "{" | "}" | ";" | "(" | ")" | "," | "line_comment" | "block_comment"
    )
}

fn node_text<'a>(node: &Node, source: &'a str) -> Option<&'a str> {
    node.utf8_text(source.as_bytes()).ok()
}

/// Walk the receiver chain of `obj.method` / `f().g().h` and return the
/// leftmost identifier. Descends through both `field_expression` (chained
/// access) and `call_expression` (chained calls) so `Cache::open(x).ok()
/// .map(f)` resolves to `Cache`. For `scoped_identifier` (`Type::method`)
/// the head segment wins.
fn field_receiver_root<'a>(node: &Node, source: &'a str) -> Option<&'a str> {
    let mut current = *node;
    loop {
        match current.kind() {
            "field_expression" => {
                current = current.child_by_field_name("value")?;
            }
            "call_expression" => {
                current = current.child_by_field_name("function")?;
            }
            "scoped_identifier" => {
                let text = node_text(&current, source)?;
                return text.split("::").next();
            }
            // identifier, self, try-expression, parenthesised, or anything
            // else — give up and use the whole snippet (renderer / sanitize
            // will normalise it).
            _ => return node_text(&current, source),
        }
    }
}

/// Short, single-line text of a child field — truncated for diagram labels.
fn short_text(node: &Node, field: &str, source: &str) -> String {
    let Some(child) = node.child_by_field_name(field) else {
        return String::new();
    };
    let raw = node_text(&child, source).unwrap_or("").trim();
    let one_line: String = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.len() > 40 {
        format!("{}…", &one_line[..40])
    } else {
        one_line
    }
}

/// Render an arbitrary string as a Mermaid-safe participant id.
/// Replaces non-alphanumeric chars with `_`, prefixes a digit-leading
/// id with `p_`.
pub(super) fn sanitize_id(s: &str) -> String {
    if s.is_empty() {
        return "p".to_owned();
    }
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        let mut prefixed = String::with_capacity(out.len() + 2);
        prefixed.push_str("p_");
        prefixed.push_str(&out);
        return prefixed;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_id_replaces_special_chars() {
        assert_eq!(sanitize_id("a-b.c"), "a_b_c");
        assert_eq!(sanitize_id("ok"), "ok");
    }

    #[test]
    fn sanitize_id_prefixes_digit_leading() {
        assert_eq!(sanitize_id("3things"), "p_3things");
    }

    #[test]
    fn sanitize_id_empty_falls_back() {
        assert_eq!(sanitize_id(""), "p");
    }
}
