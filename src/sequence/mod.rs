//! Sequence-diagram extraction and rendering.
//!
//! Distinct from the symbol-graph levels (project / overview / module /
//! function / impact): a sequence diagram captures **statement order**
//! inside a single function body, classifies call sites by receiver
//! lifeline, and emits Mermaid `sequenceDiagram` syntax.
//!
//! # Pipeline
//!
//! 1. [`extract`] re-parses the file containing the target function and
//!    walks its body in source order, producing a [`SequenceDiagram`] IR.
//! 2. [`render`] turns that IR into a Mermaid string.
//!
//! # Scope
//!
//! - Rust and Python (the same two languages the main parser supports).
//! - Calls are classified by syntactic receiver: bare ident, type path
//!   (Rust `Type::method`), or `obj.method()` receiver root (Rust
//!   `field_expression`, Python `attribute`).
//! - Control flow lifted: `if` → `alt`, `match` → `alt`, `for`/`while`/
//!   `loop` → `loop`. Nested closures (Rust) and comprehensions (Python)
//!   are walked but their results are inlined (no spawn-style "par" blocks
//!   yet).
//! - Await: Rust postfix `.await` and Python prefix `await` both annotate
//!   the arrow label (not a separate lifeline).

use crate::error::{AstToMermaidError, Result};
use crate::parser::Language;
use lang::SeqLang;
use std::collections::HashMap;
use tree_sitter::{Node, Parser as TsParser, Tree};

mod lang;
mod render;
mod visit;

pub use render::render;

/// Map from qualified target name (`name` or `Owner::name`) to the
/// extracted [`SequenceDiagram`]. Returned by [`extract_all`].
pub type SequenceMap = HashMap<String, SequenceDiagram>;

/// One Mermaid `sequenceDiagram` worth of structure.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SequenceDiagram {
    /// Title shown at the top (typically the function signature).
    pub title: String,
    /// Lifelines, ordered by first appearance in the body.
    pub participants: Vec<Participant>,
    /// Steps in source order.
    pub steps: Vec<Step>,
}

/// One lifeline (vertical bar) in the diagram.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Participant {
    /// Mermaid-safe identifier (alphanumeric + underscore).
    pub id: String,
    /// Display label shown above the lifeline.
    pub label: String,
}

/// One ordered step in the sequence — either a call, a control-flow
/// block, or an annotation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Synchronous call from one lifeline to another.
    Call {
        /// Caller participant id.
        from: String,
        /// Callee participant id.
        to: String,
        /// Method or function name shown on the arrow.
        label: String,
        /// `true` if the call site has `.await` attached.
        is_await: bool,
    },
    /// Free-form note attached to one or more participants.
    Note {
        /// Participant id the note hovers over.
        over: String,
        /// Note text.
        text: String,
    },
    /// `loop … end` block (for / while / loop).
    Loop {
        /// Header text (e.g. `for x in xs`).
        label: String,
        /// Steps inside the loop body.
        body: Vec<Step>,
        /// Cached at extract time: `true` when `body` contains at least
        /// one renderable step (recursively). The renderer skips empty
        /// loops; this avoids the O(N²) re-walk of the body per nested
        /// `Loop`/`Alt`.
        has_visible: bool,
    },
    /// `alt … else … end` block (if / match).
    Alt {
        /// Header for the first branch.
        cond: String,
        /// Steps in the consequence branch.
        then: Vec<Step>,
        /// Optional `else` branch — if `None`, no `else` clause is emitted.
        else_: Option<Vec<Step>>,
        /// Cached at extract time: `true` when `then` contains a
        /// renderable step.
        then_has_visible: bool,
        /// Cached at extract time: `true` when `else_` is `Some` and
        /// contains a renderable step.
        else_has_visible: bool,
    },
}

impl Step {
    /// `true` when this step would render at least one line. `Call` and
    /// `Note` are always visible; `Loop`/`Alt` defer to the cached flag
    /// computed at extract time.
    #[must_use]
    pub fn has_visible(&self) -> bool {
        match self {
            Step::Call { .. } | Step::Note { .. } => true,
            Step::Loop { has_visible, .. } => *has_visible,
            Step::Alt {
                then_has_visible,
                else_has_visible,
                ..
            } => *then_has_visible || *else_has_visible,
        }
    }
}

/// Synthetic participant id for the function-under-analysis itself.
pub const SELF_ID: &str = "self";

/// Parse `content` for `file_path` exactly once with `lang`'s grammar and
/// return the tree-sitter [`Tree`]. Callers typically pass the result to
/// [`extract_all`] (or, via the legacy single-target [`extract`] wrapper,
/// to `extract` indirectly).
///
/// The grammar comes from [`Language::ts_language`], the same table the
/// main parser uses — so a `.rs` file is parsed with `tree-sitter-rust`
/// and a `.py` file with `tree-sitter-python`.
///
/// # Errors
///
/// - [`AstToMermaidError::InvalidInput`] when `set_language` rejects the
///   grammar (only on tree-sitter ABI mismatch).
/// - [`AstToMermaidError::InvalidInput`] when tree-sitter cannot parse the
///   content (e.g. partial input + a hard timeout).
pub fn parse_source_once(content: &[u8], file_path: &str, lang: Language) -> Result<Tree> {
    let mut parser = TsParser::new();
    parser.set_language(&lang.ts_language()).map_err(|e| {
        AstToMermaidError::InvalidInput(format!("set_language for {file_path}: {e}"))
    })?;
    parser.parse(content, None).ok_or_else(|| {
        AstToMermaidError::InvalidInput(format!("tree-sitter parse failed for {file_path}"))
    })
}

/// Whether the sequence subsystem can report faithfully on `lang`.
///
/// Dart parses and renders correctly at the module level, but its node
/// kinds collide with *both* Rust's and Python's — `for_statement`,
/// `if_statement` and `await_expression` carry different fields in each
/// grammar. Running the current walker over Dart would emit loop and
/// branch labels read from fields that do not exist, i.e. plausible-looking
/// but wrong diagrams. Until label extraction moves behind `SeqLang`, we
/// return nothing rather than something false.
pub(crate) fn supports_sequences(lang: Language) -> bool {
    !matches!(lang, Language::Dart)
}

/// Extract a [`SequenceDiagram`] for every name in `targets` that resolves
/// to a function in `tree`. The returned [`SequenceMap`] is keyed by the
/// caller-supplied target string (the same form accepted by
/// [`extract`]: `name` or `Owner::name`).
///
/// Names that don't resolve are simply absent from the map — no error.
/// `tree` must come from [`parse_source_once`] over `source`'s bytes;
/// behaviour is undefined if it doesn't.
///
/// Performance: walks `tree` once, building a `qualified-name → function-
/// node` map, then resolves each target via O(1) lookup — replacing the
/// prior O(M·N) per-target tree re-walk.
#[must_use]
pub fn extract_all(tree: &Tree, source: &str, targets: &[&str], lang: Language) -> SequenceMap {
    if !supports_sequences(lang) {
        return SequenceMap::new();
    }
    let root = tree.root_node();
    let fn_index = build_fn_index(root, source, lang);
    let mut out = SequenceMap::with_capacity(targets.len());
    for &target in targets {
        let Some((fn_node, container)) = fn_index.get(target).cloned() else {
            continue;
        };
        let title = signature(&fn_node, source).map_or_else(|| target.to_owned(), str::to_owned);
        let mut state = visit::State::new(container.as_deref(), lang);
        if let Some(block) = fn_node.child_by_field_name("body") {
            state.walk_block(&block, source);
        }
        let (participants, steps) = state.finish();
        out.insert(
            target.to_owned(),
            SequenceDiagram {
                title,
                participants,
                steps,
            },
        );
    }
    out
}

/// Build a `qualified-name → (function node, container)` index for every
/// function reachable from `root`, in a single DFS pass. The function and
/// container node kinds come from `lang` (Rust `function_item` / `impl_item`,
/// Python `function_definition` / `class_definition`).
///
/// First-occurrence wins, matching the prior single-target search. Each
/// function is keyed on its bare `name`, and additionally on `Owner::name`
/// when nested inside a method container. The returned `Node`s borrow from
/// `tree` via `'tree` so callers can dispatch the body walk without
/// re-finding.
fn build_fn_index<'tree>(
    root: Node<'tree>,
    source: &str,
    lang: Language,
) -> HashMap<String, (Node<'tree>, Option<String>)> {
    let mut out: HashMap<String, (Node<'tree>, Option<String>)> = HashMap::new();
    let mut stack: Vec<(Node<'tree>, Option<String>)> = vec![(root, None)];
    while let Some((node, container)) = stack.pop() {
        if node.kind() == lang.fn_kind()
            && let Some(name_node) = node.child_by_field_name("name")
            && let Ok(name) = name_node.utf8_text(source.as_bytes())
        {
            // Free function: key on `name`. Method inside a container:
            // also insert `Owner::name`. Both forms are accepted by
            // [`extract`] and must resolve to the same node.
            out.entry(name.to_owned())
                .or_insert_with(|| (node, container.clone()));
            if let Some(ref c) = container {
                out.entry(format!("{c}::{name}"))
                    .or_insert_with(|| (node, container.clone()));
            }
        }
        let next_container = if node.kind() == lang.container_kind() {
            lang.container_name(&node, source)
                .or_else(|| container.clone())
        } else {
            container.clone()
        };
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push((child, next_container.clone()));
        }
    }
    out
}

/// Extract a [`SequenceDiagram`] for `target_fn` from `content`.
///
/// `target_fn` may be a bare function name (`run_diff`) or a method-style
/// identifier (`Foo::method`). The first matching function in source
/// order wins.
///
/// Thin wrapper over [`parse_source_once`] + [`extract_all`] for the
/// single-target case. Callers extracting many functions from the same
/// file should drive `extract_all` directly to amortise the parse.
///
/// # Errors
///
/// - [`AstToMermaidError::InvalidInput`] when content isn't valid UTF-8.
/// - [`AstToMermaidError::InvalidInput`] when tree-sitter can't parse it.
/// - [`AstToMermaidError::InvalidInput`] when no function with that name
///   exists in the file.
pub fn extract(
    content: &[u8],
    file_path: &str,
    target_fn: &str,
    lang: Language,
) -> Result<SequenceDiagram> {
    let text = std::str::from_utf8(content).map_err(|e| {
        AstToMermaidError::InvalidInput(format!("invalid utf-8 in {file_path}: {e}"))
    })?;
    if !supports_sequences(lang) {
        return Err(AstToMermaidError::InvalidInput(format!(
            "sequence diagrams are not supported for {} yet ({file_path})",
            lang.name()
        )));
    }
    let tree = parse_source_once(content, file_path, lang)?;
    extract_all(&tree, text, &[target_fn], lang)
        .remove(target_fn)
        .ok_or_else(|| {
            AstToMermaidError::InvalidInput(format!(
                "no function `{target_fn}` found in {file_path}"
            ))
        })
}

/// List every function defined in `content`, returning their qualified
/// names: `name` for free functions, `Owner::name` for methods inside an
/// `impl` block. Order is depth-first source order.
///
/// Thin wrapper over [`parse_source_once`] + [`list_functions_in_tree`] for
/// callers that hold only the source bytes. Callers that already have a
/// [`Tree`] (e.g. from a prior `parse_source_once` for `extract_all`) should
/// drive [`list_functions_in_tree`] directly to amortise the parse.
///
/// # Errors
///
/// Same shapes as [`extract`] for UTF-8 / parse failures.
pub fn list_functions(content: &[u8], lang: Language) -> Result<Vec<String>> {
    let text = std::str::from_utf8(content)
        .map_err(|e| AstToMermaidError::InvalidInput(format!("invalid utf-8: {e}")))?;
    let tree = parse_source_once(content, "<unknown>", lang)?;
    Ok(list_functions_in_tree(&tree, text, lang))
}

/// List every function defined in `tree`, returning their qualified names.
/// Walks the AST without re-parsing; `tree` must come from
/// [`parse_source_once`] over `source`'s bytes. Function and container node
/// kinds come from `lang`.
#[must_use]
pub fn list_functions_in_tree(tree: &Tree, source: &str, lang: Language) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut stack: Vec<(Node, Option<String>)> = vec![(tree.root_node(), None)];
    // Depth-first, but preserve source order: push children in reverse so
    // they pop left-to-right.
    while let Some((node, container)) = stack.pop() {
        if node.kind() == lang.fn_kind()
            && let Some(name_node) = node.child_by_field_name("name")
            && let Ok(name) = name_node.utf8_text(source.as_bytes())
        {
            let qualified = container
                .as_deref()
                .map_or_else(|| name.to_owned(), |c| format!("{c}::{name}"));
            out.push(qualified);
        }
        let next_container = if node.kind() == lang.container_kind() {
            lang.container_name(&node, source).or(container)
        } else {
            container.clone()
        };
        let mut cursor = node.walk();
        let children: Vec<Node> = node.children(&mut cursor).collect();
        for child in children.into_iter().rev() {
            stack.push((child, next_container.clone()));
        }
    }
    out
}

/// First line of the function text, used as the diagram title. Trailing
/// block-openers are stripped: `{` for Rust and `:` for Python.
fn signature<'a>(fn_node: &Node, source: &'a str) -> Option<&'a str> {
    let bytes = source.as_bytes();
    let text = fn_node.utf8_text(bytes).ok()?;
    text.lines()
        .next()
        .map(|l| l.trim_end().trim_end_matches([':', '{']).trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract_ok(src: &str, target: &str) -> SequenceDiagram {
        extract(src.as_bytes(), "test.rs", target, Language::Rust).expect("extract")
    }

    fn extract_py(src: &str, target: &str) -> SequenceDiagram {
        extract(src.as_bytes(), "test.py", target, Language::Python).expect("extract")
    }

    #[test]
    fn empty_function_yields_no_steps() {
        let d = extract_ok("fn empty() {}\n", "empty");
        assert!(d.steps.is_empty());
        assert!(d.title.contains("empty"));
    }

    #[test]
    fn bare_call_targets_self() {
        let d = extract_ok("fn run() { foo(); bar(); }\n", "run");
        let calls: Vec<_> = d
            .steps
            .iter()
            .filter_map(|s| match s {
                Step::Call { to, label, .. } => Some((to.clone(), label.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0], (SELF_ID.to_owned(), "foo".to_owned()));
        assert_eq!(calls[1], (SELF_ID.to_owned(), "bar".to_owned()));
    }

    #[test]
    fn method_call_targets_receiver() {
        let d = extract_ok("fn run(cache: &Cache) { cache.open(); }\n", "run");
        let Step::Call { to, label, .. } = &d.steps[0] else {
            panic!("expected call");
        };
        assert_eq!(to, "cache");
        assert_eq!(label, "open");
    }

    #[test]
    fn string_literal_receiver_stays_on_self_lifeline() {
        // `"msg".to_string()` — a literal is a local value, not an actor:
        // no participant is minted from the literal text.
        let d = extract_ok(
            "fn run() -> String { \"run: data must not be empty at all\".to_string() }\n",
            "run",
        );
        let Step::Call { to, label, .. } = &d.steps[0] else {
            panic!("expected call, got {:?}", d.steps);
        };
        assert_eq!(to, SELF_ID);
        assert_eq!(label, "to_string");
        assert!(
            d.participants.iter().all(|p| !p.label.contains("must not")),
            "literal leaked into participants: {:?}",
            d.participants
        );
    }

    #[test]
    fn python_string_literal_receiver_stays_on_self_lifeline() {
        let d = extract_py("def run(xs):\n    return \", \".join(xs)\n", "run");
        let Step::Call { to, label, .. } = &d.steps[0] else {
            panic!("expected call, got {:?}", d.steps);
        };
        assert_eq!(to, SELF_ID);
        assert_eq!(label, "join");
    }

    #[test]
    fn participant_labels_never_carry_unbalanced_quotes() {
        // Even for receivers that do become participants, a double quote
        // in the snippet must not survive into the alias (truncation can
        // cut the closing one).
        let d = extract_ok(
            "fn run() { (x + \"abcdefghijklmnopqrstuvwxyz\").process(); }\n",
            "run",
        );
        for p in &d.participants {
            assert!(
                !p.label.contains('"'),
                "double quote left in participant label: {p:?}"
            );
        }
    }

    #[test]
    fn type_path_targets_type() {
        let d = extract_ok("fn run() { Cache::open(\"x\"); }\n", "run");
        let Step::Call { to, label, .. } = &d.steps[0] else {
            panic!("expected call");
        };
        assert_eq!(to, "Cache");
        assert_eq!(label, "open");
    }

    #[test]
    fn truncated_alt_header_with_ampersand_renders_parseable_mermaid() {
        // The #156 shape: a long `if` condition containing `&`, truncated
        // by the header cap. The rendered header must carry `&amp;` and an
        // ASCII `...` — the Unicode ellipsis and a raw `&` derail older
        // Mermaid sequence lexers.
        let d = extract_ok(
            "fn run(items: &Vec<u64>, mask: &u64) {\n    if items.iter().filter(|x| *x & mask == other_long_name).count() >= 10 {\n        compute();\n    }\n}\n",
            "run",
        );
        let out = render::render(&d);
        assert!(!out.contains('…'), "unicode ellipsis in output:\n{out}");
        let alt_line = out
            .lines()
            .find(|l| l.trim_start().starts_with("alt "))
            .expect("alt header");
        assert!(alt_line.contains("&amp;"), "raw & in header: {alt_line}");
        assert!(
            alt_line.contains("..."),
            "no ASCII truncation marker: {alt_line}"
        );
    }

    #[test]
    fn await_marked() {
        let d = extract_ok("async fn run() { fetch().await; }\n", "run");
        let Step::Call { is_await, .. } = &d.steps[0] else {
            panic!("expected call");
        };
        assert!(is_await);
    }

    #[test]
    fn for_loop_wraps_body() {
        let d = extract_ok("fn run(xs: Vec<u8>) { for x in xs { foo(x); } }\n", "run");
        let Step::Loop { body, .. } = &d.steps[0] else {
            panic!("expected loop, got {:?}", d.steps);
        };
        assert!(
            body.iter()
                .any(|s| matches!(s, Step::Call { label, .. } if label == "foo"))
        );
    }

    #[test]
    fn if_expression_becomes_alt() {
        let d = extract_ok("fn run() { if cond() { yes(); } else { no(); } }\n", "run");
        let Step::Alt { then, else_, .. } = &d.steps[0] else {
            panic!("expected alt, got {:?}", d.steps);
        };
        assert!(
            then.iter()
                .any(|s| matches!(s, Step::Call { label, .. } if label == "yes"))
        );
        let else_steps = else_.as_ref().expect("else branch");
        assert!(
            else_steps
                .iter()
                .any(|s| matches!(s, Step::Call { label, .. } if label == "no"))
        );
    }

    #[test]
    fn missing_target_errors() {
        let err = extract(b"fn other() {}", "test.rs", "missing", Language::Rust)
            .expect_err("must error");
        assert!(matches!(err, AstToMermaidError::InvalidInput(_)));
    }

    #[test]
    fn assert_macros_and_enum_constructors_are_skipped() {
        // `Some`/`Ok` parse as call_expression and `assert_eq!` as
        // macro_invocation; both should be filtered out. Real calls in
        // **expression** position (Some's args ARE parsed as expressions)
        // are still surfaced. Calls inside macro arg-token-trees are NOT
        // recoverable in tree-sitter-rust — those tokens are unparsed.
        let d = extract_ok(
            "fn run() {\n  let _ = Some(real());\n  assert_eq!(left, right);\n  assert!(cond);\n}\n",
            "run",
        );
        let labels: Vec<&str> = d
            .steps
            .iter()
            .filter_map(|s| match s {
                Step::Call { label, .. } => Some(label.as_str()),
                _ => None,
            })
            .collect();
        for noisy in ["Some", "Ok", "Err", "None", "assert!", "assert_eq!"] {
            assert!(!labels.contains(&noisy), "{noisy} leaked into {labels:?}");
        }
        // The `real()` inside `Some(...)` is in expression position, so
        // it surfaces. (Calls inside `assert_eq!` arg-tokens do not.)
        assert_eq!(
            labels.iter().filter(|l| **l == "real").count(),
            1,
            "{labels:?}"
        );
    }

    #[test]
    fn macro_chain_receiver_is_macro_name_not_token_tree() {
        // `writeln!(buf, "...").expect()` — receiver of `.expect` is the
        // macro return value; pin the participant to `writeln`, not the
        // whole macro source.
        let d = extract_ok("fn run() { writeln!(buf, \"x\").expect(\"io\"); }\n", "run");
        let to_targets: Vec<&str> = d
            .steps
            .iter()
            .filter_map(|s| match s {
                Step::Call { to, .. } => Some(to.as_str()),
                _ => None,
            })
            .collect();
        assert!(to_targets.contains(&"writeln"), "got: {to_targets:?}");
        assert!(
            !to_targets.iter().any(|t| t.len() > 32),
            "no participant should embed the whole macro: {to_targets:?}",
        );
    }

    #[test]
    fn list_functions_returns_qualified_names() {
        let src = "fn free(){}\nstruct S;\nimpl S { fn m(&self){} }\n\
                   trait T { fn def(&self){} }\nimpl T for S { fn def(&self){} }\n";
        let names = list_functions(src.as_bytes(), Language::Rust).expect("list");
        assert!(names.contains(&"free".to_owned()), "{names:?}");
        assert!(names.contains(&"S::m".to_owned()), "{names:?}");
        assert!(names.contains(&"S::def".to_owned()), "{names:?}");
    }

    // ── Python ────────────────────────────────────────────────────────────

    #[test]
    fn python_bare_call_targets_self() {
        let d = extract_py("def run():\n    foo()\n    bar()\n", "run");
        let calls: Vec<_> = d
            .steps
            .iter()
            .filter_map(|s| match s {
                Step::Call { to, label, .. } => Some((to.clone(), label.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 2, "{:?}", d.steps);
        assert_eq!(calls[0], (SELF_ID.to_owned(), "foo".to_owned()));
        assert_eq!(calls[1], (SELF_ID.to_owned(), "bar".to_owned()));
    }

    #[test]
    fn python_method_call_targets_receiver() {
        let d = extract_py("def run(cache):\n    cache.open()\n", "run");
        let Step::Call { to, label, .. } = &d.steps[0] else {
            panic!("expected call, got {:?}", d.steps);
        };
        assert_eq!(to, "cache");
        assert_eq!(label, "open");
    }

    #[test]
    fn python_self_method_targets_self() {
        let d = extract_py(
            "class Widget:\n    def build(self):\n        self.helper()\n",
            "Widget::build",
        );
        let Step::Call { to, label, .. } = &d.steps[0] else {
            panic!("expected call, got {:?}", d.steps);
        };
        assert_eq!(to, SELF_ID);
        assert_eq!(label, "helper");
    }

    #[test]
    fn python_await_marked() {
        let d = extract_py("async def run():\n    await fetch()\n", "run");
        let Step::Call {
            is_await, label, ..
        } = &d.steps[0]
        else {
            panic!("expected call, got {:?}", d.steps);
        };
        assert!(is_await, "await not marked: {:?}", d.steps);
        assert_eq!(label, "fetch");
    }

    #[test]
    fn python_for_loop_wraps_body() {
        let d = extract_py("def run(xs):\n    for x in xs:\n        x.go()\n", "run");
        let Step::Loop { body, .. } = &d.steps[0] else {
            panic!("expected loop, got {:?}", d.steps);
        };
        assert!(
            body.iter()
                .any(|s| matches!(s, Step::Call { label, .. } if label == "go"))
        );
    }

    #[test]
    fn python_if_becomes_alt_with_else() {
        let d = extract_py(
            "def run():\n    if cond():\n        yes()\n    else:\n        no()\n",
            "run",
        );
        let Step::Alt { then, else_, .. } = &d.steps[0] else {
            panic!("expected alt, got {:?}", d.steps);
        };
        assert!(
            then.iter()
                .any(|s| matches!(s, Step::Call { label, .. } if label == "yes"))
        );
        let else_steps = else_.as_ref().expect("else branch");
        assert!(
            else_steps
                .iter()
                .any(|s| matches!(s, Step::Call { label, .. } if label == "no"))
        );
    }

    #[test]
    fn python_match_becomes_alt() {
        let d = extract_py(
            "def run(v):\n    match v:\n        case 1:\n            one()\n        case _:\n            other()\n",
            "run",
        );
        let Step::Alt { then, .. } = &d.steps[0] else {
            panic!("expected alt, got {:?}", d.steps);
        };
        let labels: Vec<&str> = then
            .iter()
            .filter_map(|s| match s {
                Step::Call { label, .. } => Some(label.as_str()),
                _ => None,
            })
            .collect();
        assert!(labels.contains(&"one"), "{labels:?}");
        assert!(labels.contains(&"other"), "{labels:?}");
    }

    #[test]
    fn python_chained_attribute_collapses_to_root() {
        let d = extract_py("def run(obj):\n    obj.chain().tail()\n", "run");
        // `obj.chain().tail()` — outer call targets receiver `obj`, method
        // `tail`; inner `obj.chain()` also targets `obj`, method `chain`.
        let to_targets: Vec<&str> = d
            .steps
            .iter()
            .filter_map(|s| match s {
                Step::Call { to, .. } => Some(to.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            to_targets.iter().all(|t| *t == "obj"),
            "got: {to_targets:?}"
        );
    }

    #[test]
    fn python_list_functions_returns_qualified_names() {
        let src = "def free():\n    pass\n\nclass S:\n    def m(self):\n        pass\n";
        let names = list_functions(src.as_bytes(), Language::Python).expect("list");
        assert!(names.contains(&"free".to_owned()), "{names:?}");
        assert!(names.contains(&"S::m".to_owned()), "{names:?}");
    }

    #[test]
    fn python_impl_method_target_resolves() {
        let d = extract_py(
            "class Foo:\n    def build(self):\n        go()\n",
            "Foo::build",
        );
        assert!(
            d.steps
                .iter()
                .any(|s| matches!(s, Step::Call { label, .. } if label == "go")),
            "{:?}",
            d.steps
        );
    }

    #[test]
    fn impl_method_target_resolves() {
        let src = "impl Foo { fn build(&self) { go(); } }\n";
        let d = extract_ok(src, "Foo::build");
        assert!(
            d.steps
                .iter()
                .any(|s| matches!(s, Step::Call { label, .. } if label == "go"))
        );
    }

    #[test]
    fn participants_deduped_and_ordered() {
        let d = extract_ok("fn run(a: A, b: B) { a.x(); b.y(); a.z(); }\n", "run");
        let ids: Vec<&str> = d.participants.iter().map(|p| p.id.as_str()).collect();
        // self comes first (caller), then a, then b — order of first appearance.
        assert_eq!(ids.first().copied(), Some(SELF_ID));
        assert!(ids.contains(&"a"));
        assert!(ids.contains(&"b"));
        let a_pos = ids.iter().position(|s| *s == "a").expect("a present");
        let b_pos = ids.iter().position(|s| *s == "b").expect("b present");
        assert!(a_pos < b_pos, "a must appear before b: {ids:?}");
    }

    /// Reproduces issue #75: a 1000-deep call chain must not stack-overflow
    /// the AST walker. The depth guard caps recursion at
    /// [`crate::limits::MAX_AST_DEPTH`] and emits exactly one
    /// `…depth limit…` marker note so the diagram still renders
    /// deterministically.
    #[test]
    fn depth_limit_one_thousand_deep_call_chain() {
        let mut src = String::from("fn deep() {\n    ");
        // `f(f(f( ... 0 ... )))` — 1000 nested call_expression nodes.
        for _ in 0..1000 {
            src.push_str("f(");
        }
        src.push('0');
        for _ in 0..1000 {
            src.push(')');
        }
        src.push_str(";\n}\n");

        let d = extract_ok(&src, "deep");

        // Exactly one depth-limit marker note is emitted, regardless of
        // how many cut points were hit while unwinding.
        let marker_count = d
            .steps
            .iter()
            .filter(|s| matches!(s, Step::Note { text, .. } if text.contains("depth limit")))
            .count();
        assert_eq!(
            marker_count, 1,
            "expected exactly one depth-limit marker, got {marker_count} in {:?}",
            d.steps,
        );

        // The renderer must produce a finite, complete diagram (no
        // panic, no truncation mid-line).
        let mermaid = render(&d);
        assert!(mermaid.starts_with("sequenceDiagram\n"));
        assert!(
            mermaid.contains("depth limit"),
            "rendered output must surface the depth-limit marker:\n{mermaid}",
        );
    }
}
