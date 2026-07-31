//! Tree-sitter walk that turns a function body into ordered
//! [`super::Step`]s. Statement order is preserved by walking the AST
//! depth-first, left-to-right.
//!
//! Handles Rust, Python and Dart. [`State::walk_expr`] runs one unified
//! `match` over their node kinds, which is safe for everything it does
//! here: recognising a call, a block or a loop, descending into it and
//! emitting a step never reads a field name that the grammars spell
//! differently. `call_expression` notably exposes the same `function:` +
//! `arguments:` layout in all three.
//!
//! What the grammars *do* disagree on is which field carries a label — a
//! `for`'s iterable is `value` in Rust, `right` in Python, and `value`
//! again in Dart under Python's own `for_statement` kind. Those reads live
//! behind [`super::lang::SeqLang`] instead, so this walker never has to
//! know. See that module for the full rule.
//!
//! The remaining `lang` branches here are genuine *semantic* divergences —
//! callee classification and constructor/macro skipping — not field-name
//! lookups.

use super::lang::SeqLang;
use super::{Participant, SELF_ID, Step};
use crate::limits::max_ast_depth;
use crate::parser::Language;
use crate::render::util::sanitize_id;
use std::collections::HashSet;
use tree_sitter::Node;

/// Marker text emitted into the diagram (sequence note / mermaid note)
/// when a visitor short-circuits at the depth cap. Kept as a single
/// shared string so the diff/CI snapshots can match on it verbatim.
/// ASCII on purpose — older Mermaid sequence lexers reject the Unicode
/// ellipsis (#156).
pub(super) const DEPTH_LIMIT_LABEL: &str = "...depth limit...";

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
    /// Receiver type of the enclosing method container (Rust `impl` block
    /// or Python `class`), if any. Used to alias `Self::method()` /
    /// `self.method()` calls onto the owner.
    container: Option<String>,
    /// Source language — selects node-kind semantics in the divergent
    /// helpers (callee classification, constructor/macro skipping, `if`/
    /// `match` lifting).
    lang: Language,
    /// Resolved [`max_ast_depth`] captured once per [`State`] so the
    /// limit is consistent across all walkers driving this diagram.
    max_depth: usize,
    /// `true` once the depth cap has been hit and the marker note has
    /// been pushed — guards against emitting one note per cut point on
    /// a fan-out tree.
    depth_limited: bool,
}

impl State {
    /// Build a fresh visitor with `self` pre-registered as the caller
    /// lifeline. `container` is the method owner type (e.g. `Foo` for an
    /// `impl Foo` method or a Python `class Foo`) or `None` for free
    /// functions. `lang` selects the grammar semantics.
    pub fn new(container: Option<&str>, lang: Language) -> Self {
        let mut s = Self {
            steps: Vec::new(),
            participants: Vec::new(),
            seen: HashSet::new(),
            container: container.map(str::to_owned),
            lang,
            max_depth: max_ast_depth(),
            depth_limited: false,
        };
        s.register(SELF_ID, container.unwrap_or("self"));
        s
    }

    /// Consume the state and return `(participants, steps)`.
    pub fn finish(self) -> (Vec<Participant>, Vec<Step>) {
        (self.participants, self.steps)
    }

    /// Walk a `block` node, dispatching each statement. Top-level entry
    /// from [`super::extract_all`] — recursion depth starts at 0.
    pub fn walk_block(&mut self, block: &Node, source: &str) {
        self.walk_block_at(block, source, 0);
    }

    /// Depth-aware variant of [`Self::walk_block`]. Every recursive
    /// re-entry threads `depth + 1` so [`Self::guard`] can short-circuit
    /// before the call stack runs out.
    fn walk_block_at(&mut self, block: &Node, source: &str, depth: usize) {
        if self.guard(depth) {
            return;
        }
        let mut cursor = block.walk();
        for child in block.children(&mut cursor) {
            self.walk_stmt(&child, source, depth + 1);
        }
    }

    /// Walk a single statement-level node. Drops trivia (`{`, `}`, `;`,
    /// comments). Calls inside expressions are extracted via [`Self::walk_expr`].
    fn walk_stmt(&mut self, node: &Node, source: &str, depth: usize) {
        if self.guard(depth) {
            return;
        }
        match node.kind() {
            "expression_statement" | "let_declaration" => {
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if is_trivia(&child) {
                        continue;
                    }
                    self.walk_expr(&child, source, depth + 1);
                }
            }
            // Bare expressions can appear at the tail of a block (no `;`).
            _ if is_trivia(node) => {}
            _ => self.walk_expr(node, source, depth + 1),
        }
    }

    /// Walk an expression node, emitting steps for calls and lifting
    /// control-flow into [`Step::Loop`] / [`Step::Alt`] blocks.
    ///
    /// Returns early once `depth >= self.max_depth`, emitting a single
    /// `…depth limit…` marker via [`Self::guard`] so adversarially deep
    /// input degrades to a finite diagram instead of overflowing the
    /// stack.
    #[allow(clippy::too_many_lines)]
    fn walk_expr(&mut self, node: &Node, source: &str, depth: usize) {
        if self.guard(depth) {
            return;
        }
        match node.kind() {
            // ── Calls ──────────────────────────────────────────────────
            // Rust `call_expression` and Python `call` share the `function`
            // + `arguments` field layout, so one handler serves both.
            "call_expression" | "call" => {
                self.handle_call(node, source, false, depth + 1);
            }
            "macro_invocation" => {
                self.handle_macro(node, source, depth + 1);
            }
            // ── Await ──────────────────────────────────────────────────
            // Rust postfix `await_expression` (`<expr>.await`) and Python
            // prefix `await` (`await <expr>`) both wrap a single awaited
            // expression as a non-keyword child. We mark the inner call
            // `is_await` so the renderer draws the async arrow.
            "await_expression" | "await" => {
                let mut cursor = node.walk();
                let inner = node
                    .children(&mut cursor)
                    .find(|c| !matches!(c.kind(), "." | "await"));
                if let Some(value) = inner {
                    if matches!(value.kind(), "call_expression" | "call") {
                        self.handle_call(&value, source, true, depth + 1);
                    } else {
                        self.walk_expr(&value, source, depth + 1);
                    }
                }
            }
            // ── Loops ──────────────────────────────────────────────────
            // One arm for every loop shape: recognising the shape is
            // structural, so it stays here. Which field holds the iterable
            // is what diverges, so the label comes from `SeqLang`.
            "for_expression" | "for_statement" | "while_expression" | "while_statement"
            | "loop_expression" => {
                let label = self.lang.loop_label(node, source);
                self.lift_loop(node, source, &label, depth + 1);
            }
            // ── Conditionals ───────────────────────────────────────────
            "if_expression" | "if_statement" => {
                self.lift_if(node, source, depth + 1);
            }
            "match_expression" | "match_statement" => {
                self.lift_match(node, source, depth + 1);
            }
            "block" => {
                self.walk_block_at(node, source, depth + 1);
            }
            // Anything else: descend so nested calls (e.g. `Some(foo())`)
            // are still picked up. We only descend into children, not the
            // node itself, to avoid revisiting the current node.
            _ => {
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    self.walk_expr(&child, source, depth + 1);
                }
            }
        }
    }

    /// Emit a [`Step::Call`] for a `macro_invocation` (`foo!(...)`,
    /// `mod::bar!(...)`). Macros have no receiver, so they always target
    /// the implicit `self` lifeline; the bang is preserved on the label
    /// so the rendered diagram stays distinguishable from real fn calls.
    ///
    /// Test-control and panic macros (`assert!`, `assert_eq!`,
    /// `debug_assert*!`, `panic!`, `unreachable!`, `todo!`,
    /// `unimplemented!`, `matches!`) are skipped — they're noise on a
    /// sequence diagram. We still descend into their token tree so any
    /// nested real call survives.
    fn handle_macro(&mut self, node: &Node, source: &str, depth: usize) {
        let macro_node = node.child_by_field_name("macro");
        let label = macro_node
            .and_then(|n| node_text(&n, source))
            .unwrap_or("?")
            .to_owned();
        if !is_skipped_macro(&label) {
            self.steps.push(Step::Call {
                from: SELF_ID.to_owned(),
                to: SELF_ID.to_owned(),
                label: format!("{label}!"),
                is_await: false,
            });
        }
        // Descend so nested calls inside macro tokens (e.g. `vec![foo()]`)
        // still surface.
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if !is_trivia(&child) && Some(child) != macro_node {
                self.walk_expr(&child, source, depth);
            }
        }
    }

    /// Emit a [`Step::Call`] for a Rust `call_expression` or Python `call`
    /// and then descend into its arguments so nested calls are still
    /// surfaced.
    ///
    /// In Rust, bare-name calls to enum-variant constructors (`Some`,
    /// `None`, `Ok`, `Err`) are skipped — they parse as `call_expression`
    /// but represent type construction, not flow. Python has no such
    /// ambiguity, so the filter only applies when [`Self::lang`] is Rust.
    fn handle_call(&mut self, node: &Node, source: &str, is_await: bool, depth: usize) {
        if let Some(callee) = node.child_by_field_name("function") {
            let (to_id, to_label, method) = self.classify_callee(&callee, source);
            let is_bare_constructor = self.lang == Language::Rust
                && callee.kind() == "identifier"
                && to_id == SELF_ID
                && is_skipped_constructor(&method);
            if !is_bare_constructor {
                self.register(&to_id, &to_label);
                self.steps.push(Step::Call {
                    from: SELF_ID.to_owned(),
                    to: to_id,
                    label: method,
                    is_await,
                });
            }
        }
        // Descend into arguments to capture nested calls — even on
        // skipped constructors (e.g. `Some(real_call())`).
        if let Some(args) = node.child_by_field_name("arguments") {
            let mut cursor = args.walk();
            for child in args.children(&mut cursor) {
                self.walk_expr(&child, source, depth);
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
                let (head, tail) = text.rsplit_once("::").map_or(("", text), |(h, t)| (h, t));
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
                let receiver_root =
                    field_receiver_root(callee, source, self.max_depth).unwrap_or("?");
                let method = callee
                    .child_by_field_name("field")
                    .and_then(|n| node_text(&n, source))
                    .unwrap_or("?")
                    .to_owned();
                if receiver_root == "self" {
                    (SELF_ID.to_owned(), self.self_label(), method)
                } else {
                    let display = cap_label(receiver_root);
                    (sanitize_id(&display), display, method)
                }
            }
            "attribute" => {
                // Python `obj.method` — `object` is the receiver chain,
                // `attribute` is the method name. `obj.x.method` and
                // `obj.chain().method` both collapse to `obj`.
                let receiver_root =
                    attribute_receiver_root(callee, source, self.max_depth).unwrap_or("?");
                let method = callee
                    .child_by_field_name("attribute")
                    .and_then(|n| node_text(&n, source))
                    .unwrap_or("?")
                    .to_owned();
                if receiver_root == "self" {
                    (SELF_ID.to_owned(), self.self_label(), method)
                } else {
                    let display = cap_label(receiver_root);
                    (sanitize_id(&display), display, method)
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

    fn lift_loop(&mut self, node: &Node, source: &str, label: &str, depth: usize) {
        let body_node = node.child_by_field_name("body");
        let body = self.walk_into(|s| {
            if let Some(b) = body_node {
                s.walk_block_at(&b, source, depth);
            }
        });
        let has_visible = body.iter().any(Step::has_visible);
        self.steps.push(Step::Loop {
            label: label.to_owned(),
            body,
            has_visible,
        });
    }

    fn lift_if(&mut self, node: &Node, source: &str, depth: usize) {
        let cond = format!("if {}", self.lang.if_condition(node, source));
        let then_node = node.child_by_field_name("consequence");
        let then = self.walk_into(|s| {
            if let Some(b) = then_node {
                s.walk_block_at(&b, source, depth);
            }
        });
        let else_node = node.child_by_field_name("alternative");
        let else_branch = else_node.map(|alt| {
            self.walk_into(|s| {
                // `alternative` may wrap an `else_clause` or be a chained
                // `if_expression`. Descend uniformly.
                s.walk_expr(&alt, source, depth);
            })
        });
        let then_has_visible = then.iter().any(Step::has_visible);
        let else_has_visible = else_branch
            .as_deref()
            .is_some_and(|s| s.iter().any(Step::has_visible));
        self.steps.push(Step::Alt {
            cond,
            then,
            else_: else_branch,
            then_has_visible,
            else_has_visible,
        });
    }

    fn lift_match(&mut self, node: &Node, source: &str, depth: usize) {
        // Which field holds the scrutinee, and how an arm is spelled, is
        // per-grammar — so it comes from `SeqLang` rather than a `match`
        // on the language here.
        let spec = self.lang.match_spec();
        let scrutinee = short_text(node, spec.scrutinee_field, source);
        let cond = format!("match {scrutinee}");
        // v1 collapses all arms into one branch — splitting onto separate
        // alt elses is a follow-up.
        let arms_steps = self.walk_into(|s| {
            if let Some(body) = node.child_by_field_name("body") {
                let mut cursor = body.walk();
                for child in body.children(&mut cursor) {
                    if child.kind() != spec.arm_kind {
                        continue;
                    }
                    // `None` means the arm node *is* its own body.
                    let arm_body = spec
                        .arm_body_field
                        .map_or(Some(child), |f| child.child_by_field_name(f));
                    if let Some(arm_body) = arm_body {
                        s.walk_expr(&arm_body, source, depth);
                    }
                }
            }
        });
        let then_has_visible = arms_steps.iter().any(Step::has_visible);
        self.steps.push(Step::Alt {
            cond,
            then: arms_steps,
            else_: None,
            then_has_visible,
            else_has_visible: false,
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

    /// Depth-limit gate for every recursive walker.
    ///
    /// Returns `true` when `depth >= self.max_depth` — the caller must
    /// then short-circuit. The first cut also pushes a single
    /// [`Step::Note`] (deduped via `self.depth_limited`) so the rendered
    /// diagram carries a visible `…depth limit…` marker rather than
    /// silently losing the deep tail.
    fn guard(&mut self, depth: usize) -> bool {
        if depth < self.max_depth {
            return false;
        }
        if !self.depth_limited {
            self.depth_limited = true;
            tracing::warn!(
                limit = self.max_depth,
                "ast depth limit hit while walking sequence body",
            );
            self.steps.push(Step::Note {
                over: SELF_ID.to_owned(),
                text: DEPTH_LIMIT_LABEL.to_owned(),
            });
        }
        true
    }
}

fn is_trivia(node: &Node) -> bool {
    matches!(
        node.kind(),
        // Rust trivia + Python `comment` / `:` block-opener.
        "{" | "}" | ";" | "(" | ")" | "," | ":" | "line_comment" | "block_comment" | "comment"
    )
}

fn node_text<'a>(node: &Node, source: &'a str) -> Option<&'a str> {
    node.utf8_text(source.as_bytes()).ok()
}

/// Walk the receiver chain of `obj.method` / `f().g().h` / `mac!().m()`
/// and return the leftmost identifier. Descends through `field_expression`
/// (chained access), `call_expression` (chained calls), and
/// `macro_invocation` so chains like `writeln!(...).expect(...)` resolve
/// to the macro name instead of swallowing the whole macro body. For
/// `scoped_identifier` (`Type::method`) the head segment wins.
///
/// `limit` is the depth cap captured once on the enclosing [`State`]
/// (see [`State::max_depth`]) — passed in rather than re-resolved here
/// so the recursive call path doesn't re-read `A2M_MAX_AST_DEPTH` for
/// every receiver classification. Once that many descent steps elapse
/// (an adversarial deeply nested chain) we stop walking and return the
/// best-effort raw text of the current node so the caller still
/// produces a finite label.
fn field_receiver_root<'a>(node: &Node, source: &'a str, limit: usize) -> Option<&'a str> {
    let mut current = *node;
    for _ in 0..limit {
        match current.kind() {
            "field_expression" => {
                current = current.child_by_field_name("value")?;
            }
            // `f()` and `f::<T>()` (whose function is a `generic_function`)
            // both descend through the `function` field.
            "call_expression" | "generic_function" => {
                current = current.child_by_field_name("function")?;
            }
            // `(expr).method()` — pierce the parens.
            "parenthesized_expression" => {
                let mut cursor = current.walk();
                let inner = current
                    .children(&mut cursor)
                    .find(|c| !matches!(c.kind(), "(" | ")"));
                current = inner?;
            }
            // `expr?.method()` — `?` propagates the inner value.
            "try_expression" => {
                let mut cursor = current.walk();
                let inner = current.children(&mut cursor).find(|c| c.kind() != "?");
                current = inner?;
            }
            "macro_invocation" => {
                // `writeln!(buf, "...").expect()` — the receiver of the
                // chained `.expect` is the macro return value; pin it to
                // the macro name (with `!`) instead of the whole token tree.
                let macro_name = current
                    .child_by_field_name("macro")
                    .and_then(|n| node_text(&n, source))?;
                return Some(macro_name);
            }
            "scoped_identifier" => {
                let text = node_text(&current, source)?;
                return text.split("::").next();
            }
            // A literal receiver (`"msg".to_string()`, `1.5.sqrt()`) is a
            // local value, not an actor — keep the call on the current
            // lifeline instead of minting a participant out of the
            // literal's text (whose quotes would also survive truncation
            // unbalanced).
            "string_literal" | "raw_string_literal" | "char_literal" | "integer_literal"
            | "float_literal" | "boolean_literal" => return Some("self"),
            // identifier, self, or anything else — give up and use the
            // raw text. The caller caps overlong snippets.
            _ => return node_text(&current, source),
        }
    }
    tracing::warn!(
        limit,
        "ast depth limit hit in field_receiver_root; returning best-effort label",
    );
    node_text(&current, source)
}

/// Python counterpart of [`field_receiver_root`]: walk the receiver chain
/// of `obj.method` / `obj.x.method` / `obj.chain().method` and return the
/// leftmost identifier. Descends through `attribute` (chained access via the
/// `object` field), `call` (chained calls via the `function` field),
/// `parenthesized_expression`, and `subscript` (`d[k].method()` → `d`).
///
/// `limit` is the depth cap captured once on the enclosing [`State`], passed
/// in for the same reason as [`field_receiver_root`]. On an adversarially
/// deep chain we stop and return the best-effort raw text of the current
/// node so the caller still produces a finite label.
fn attribute_receiver_root<'a>(node: &Node, source: &'a str, limit: usize) -> Option<&'a str> {
    let mut current = *node;
    for _ in 0..limit {
        match current.kind() {
            "attribute" => {
                current = current.child_by_field_name("object")?;
            }
            "call" => {
                current = current.child_by_field_name("function")?;
            }
            "subscript" => {
                current = current.child_by_field_name("value")?;
            }
            "parenthesized_expression" => {
                let mut cursor = current.walk();
                let inner = current
                    .children(&mut cursor)
                    .find(|c| !matches!(c.kind(), "(" | ")"));
                current = inner?;
            }
            // Literal receiver (`", ".join(xs)`, `(1.5).hex()`) — same as
            // the Rust twin: a local value, not an actor.
            "string" | "concatenated_string" | "integer" | "float" | "true" | "false" | "none" => {
                return Some("self");
            }
            // identifier, self, or anything else — give up and use the raw
            // text. The caller caps overlong snippets.
            _ => return node_text(&current, source),
        }
    }
    tracing::warn!(
        limit,
        "ast depth limit hit in attribute_receiver_root; returning best-effort label",
    );
    node_text(&current, source)
}

/// Short, single-line text of a child field — truncated for diagram
/// labels. Mermaid auto-sizes the alt/loop header to the label, so an
/// over-long condition pushes neighbouring participants off-screen.
fn short_text(node: &Node, field: &str, source: &str) -> String {
    const MAX: usize = 32;
    let Some(child) = node.child_by_field_name(field) else {
        return String::new();
    };
    let raw = node_text(&child, source).unwrap_or("").trim();
    cap_at(raw, MAX)
}

/// Truncate a participant label so a giant fallback snippet (e.g. a
/// chained-call expression we couldn't classify) doesn't blow the
/// diagram up. Whitespace is collapsed too — labels are one-line.
fn cap_label(s: &str) -> String {
    const MAX: usize = 24;
    cap_at(s, MAX)
}

/// Shared one-line + truncate helper for diagram labels. Double quotes
/// become single quotes first: truncation can cut a snippet mid-string,
/// and an unbalanced `"` in a participant alias or alt/loop header
/// breaks the Mermaid parser. The truncation marker is ASCII `...` —
/// older Mermaid sequence lexers reject the Unicode ellipsis in alt/loop
/// headers (#156).
pub(super) fn cap_at(s: &str, max: usize) -> String {
    let one_line: String = s
        .replace('"', "'")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if one_line.chars().count() <= max {
        one_line
    } else {
        let head: String = one_line.chars().take(max).collect();
        format!("{head}...")
    }
}

/// Macros that are pure test/panic plumbing — emitting them as steps
/// drowns the diagram in noise. Nested calls inside their token tree
/// still surface (the visitor descends regardless).
fn is_skipped_macro(name: &str) -> bool {
    matches!(
        name,
        "assert"
            | "assert_eq"
            | "assert_ne"
            | "debug_assert"
            | "debug_assert_eq"
            | "debug_assert_ne"
            | "panic"
            | "unreachable"
            | "todo"
            | "unimplemented"
            | "matches"
    )
}

/// Bare-name "calls" that are actually enum-variant constructors. Listed
/// because they parse as `call_expression` but carry no flow meaning.
fn is_skipped_constructor(name: &str) -> bool {
    matches!(name, "Some" | "None" | "Ok" | "Err")
}
