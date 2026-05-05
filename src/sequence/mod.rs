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
//! # Scope (v1)
//!
//! - Rust only.
//! - Calls are classified by syntactic receiver: bare ident, type path,
//!   or `obj.method()` field-expression root.
//! - Control flow lifted: `if` → `alt`, `match` → `alt`, `for`/`while`/
//!   `loop` → `loop`. Nested closures are walked but their results are
//!   inlined (no spawn-style "par" blocks yet).
//! - `.await` is annotated on the arrow label (not a separate lifeline).

#![warn(missing_docs)]

use crate::error::{AstToMermaidError, Result};
use std::collections::HashMap;
use tree_sitter::{Node, Parser as TsParser, Tree};

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
    },
    /// `alt … else … end` block (if / match).
    Alt {
        /// Header for the first branch.
        cond: String,
        /// Steps in the consequence branch.
        then: Vec<Step>,
        /// Optional `else` branch — if `None`, no `else` clause is emitted.
        else_: Option<Vec<Step>>,
    },
}

/// Synthetic participant id for the function-under-analysis itself.
pub const SELF_ID: &str = "self";

/// Parse `content` for `file_path` exactly once and return the
/// tree-sitter [`Tree`]. Callers typically pass the result to
/// [`extract_all`] (or, via the legacy single-target [`extract`] wrapper,
/// to `extract` indirectly).
///
/// Sequence extraction is Rust-only — the parser is configured for the
/// `tree-sitter-rust` grammar.
///
/// # Errors
///
/// - [`AstToMermaidError::InvalidInput`] when `set_language` rejects the
///   grammar (only on tree-sitter ABI mismatch).
/// - [`AstToMermaidError::InvalidInput`] when tree-sitter cannot parse the
///   content (e.g. partial input + a hard timeout).
pub fn parse_source_once(content: &[u8], file_path: &str) -> Result<Tree> {
    let mut parser = TsParser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .map_err(|e| {
            AstToMermaidError::InvalidInput(format!("set_language for {file_path}: {e}"))
        })?;
    parser.parse(content, None).ok_or_else(|| {
        AstToMermaidError::InvalidInput(format!("tree-sitter parse failed for {file_path}"))
    })
}

/// Extract a [`SequenceDiagram`] for every name in `targets` that resolves
/// to a function in `tree`. The returned [`SequenceMap`] is keyed by the
/// caller-supplied target string (the same form accepted by
/// [`extract`]: `name` or `Owner::name`).
///
/// Names that don't resolve are simply absent from the map — no error.
/// `tree` must come from [`parse_source_once`] over `source`'s bytes;
/// behaviour is undefined if it doesn't.
#[must_use]
pub fn extract_all(tree: &Tree, source: &str, targets: &[&str]) -> SequenceMap {
    let root = tree.root_node();
    let mut out = SequenceMap::with_capacity(targets.len());
    for &target in targets {
        let Some((fn_node, container)) = find_target(root, source, target) else {
            continue;
        };
        let title = signature(&fn_node, source).map_or_else(|| target.to_owned(), str::to_owned);
        let mut state = visit::State::new(container.as_deref());
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
pub fn extract(content: &[u8], file_path: &str, target_fn: &str) -> Result<SequenceDiagram> {
    let text = std::str::from_utf8(content).map_err(|e| {
        AstToMermaidError::InvalidInput(format!("invalid utf-8 in {file_path}: {e}"))
    })?;
    let tree = parse_source_once(content, file_path)?;
    extract_all(&tree, text, &[target_fn])
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
pub fn list_functions(content: &[u8]) -> Result<Vec<String>> {
    let text = std::str::from_utf8(content)
        .map_err(|e| AstToMermaidError::InvalidInput(format!("invalid utf-8: {e}")))?;
    let tree = parse_source_once(content, "<unknown>")?;
    Ok(list_functions_in_tree(&tree, text))
}

/// List every function defined in `tree`, returning their qualified names.
/// Walks the AST without re-parsing; `tree` must come from
/// [`parse_source_once`] over `source`'s bytes.
#[must_use]
pub fn list_functions_in_tree(tree: &Tree, source: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut stack: Vec<(Node, Option<String>)> = vec![(tree.root_node(), None)];
    // Depth-first, but preserve source order: push children in reverse so
    // they pop left-to-right.
    while let Some((node, container)) = stack.pop() {
        if node.kind() == "function_item"
            && let Some(name_node) = node.child_by_field_name("name")
            && let Ok(name) = name_node.utf8_text(source.as_bytes())
        {
            let qualified = container
                .as_deref()
                .map_or_else(|| name.to_owned(), |c| format!("{c}::{name}"));
            out.push(qualified);
        }
        let next_container = if node.kind() == "impl_item" {
            node.child_by_field_name("type")
                .and_then(|n| n.utf8_text(source.as_bytes()).ok())
                .map(str::to_owned)
                .or(container)
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

/// Locate `target_fn` in the file. Returns `(function_item node, optional
/// container_name)` where container is the impl-owner type (e.g. `Foo` for
/// methods of `impl Foo`) or `None` for free functions.
fn find_target<'tree>(
    root: Node<'tree>,
    source: &str,
    target_fn: &str,
) -> Option<(Node<'tree>, Option<String>)> {
    // Support `Type::method` form by splitting on the first `::`.
    let (qualifier, bare) = target_fn
        .split_once("::")
        .map_or((None, target_fn), |(q, b)| (Some(q), b));

    let mut stack: Vec<(Node<'tree>, Option<String>)> = vec![(root, None)];
    while let Some((node, container)) = stack.pop() {
        if node.kind() == "function_item"
            && let Some(name_node) = node.child_by_field_name("name")
            && let Ok(name) = name_node.utf8_text(source.as_bytes())
            && name == bare
            && qualifier.is_none_or(|q| container.as_deref() == Some(q))
        {
            return Some((node, container));
        }
        let next_container = if node.kind() == "impl_item" {
            // The receiver type for `impl Trait for Foo` is `Foo`; for
            // inherent `impl Foo` it's `Foo` too.
            node.child_by_field_name("type")
                .and_then(|n| n.utf8_text(source.as_bytes()).ok())
                .map(str::to_owned)
                .or(container)
        } else {
            container.clone()
        };
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push((child, next_container.clone()));
        }
    }
    None
}

/// First non-brace line of the function text, used as the diagram title.
fn signature<'a>(fn_node: &Node, source: &'a str) -> Option<&'a str> {
    let bytes = source.as_bytes();
    let text = fn_node.utf8_text(bytes).ok()?;
    text.lines().next().map(|l| l.trim_end_matches('{').trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract_ok(src: &str, target: &str) -> SequenceDiagram {
        extract(src.as_bytes(), "test.rs", target).expect("extract")
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
    fn type_path_targets_type() {
        let d = extract_ok("fn run() { Cache::open(\"x\"); }\n", "run");
        let Step::Call { to, label, .. } = &d.steps[0] else {
            panic!("expected call");
        };
        assert_eq!(to, "Cache");
        assert_eq!(label, "open");
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
        let err = extract(b"fn other() {}", "test.rs", "missing").expect_err("must error");
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
        let names = list_functions(src.as_bytes()).expect("list");
        assert!(names.contains(&"free".to_owned()), "{names:?}");
        assert!(names.contains(&"S::m".to_owned()), "{names:?}");
        assert!(names.contains(&"S::def".to_owned()), "{names:?}");
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
