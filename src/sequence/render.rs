//! [`SequenceDiagram`] → Mermaid `sequenceDiagram` text.

use super::visit::DEPTH_LIMIT_LABEL;
use super::{Participant, SELF_ID, SequenceDiagram, Step};
use crate::limits::max_ast_depth;
use crate::render::util::escape_label_sequence;
use std::fmt::Write;
use std::sync::OnceLock;

/// Indent strings memoised by depth, to spare the renderer a fresh
/// `String::repeat` allocation per step. Capped at the configured
/// [`max_ast_depth`]; deeper requests fall through to a one-shot
/// allocation (off the hot path — `write_step` short-circuits at the
/// same cap with a `…depth limit…` note before recursing further).
fn indent(depth: usize) -> &'static str {
    static TABLE: OnceLock<Vec<&'static str>> = OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let cap = max_ast_depth() + 1;
        let mut v = Vec::with_capacity(cap);
        for d in 0..cap {
            // Box::leak gives us a `&'static str` that lives for the
            // process lifetime. Bounded allocation: at most `cap`
            // entries (≤ 257 by default), so the leak is one-time.
            let s: Box<str> = "    ".repeat(d).into_boxed_str();
            v.push(Box::leak(s) as &'static str);
        }
        v
    });
    table.get(depth).copied().unwrap_or("")
}

/// Render a [`SequenceDiagram`] as Mermaid `sequenceDiagram` source.
///
/// Output uses `autonumber` so each step is numbered, `participant <id>
/// as <label>` declarations in first-appearance order, and synchronous
/// `->>` arrows for calls. `.await` shows on the arrow label as ` (await)`.
#[must_use]
pub fn render(diagram: &SequenceDiagram) -> String {
    // Resolve the depth cap once per render so the recursive
    // `write_step` doesn't re-read `A2M_MAX_AST_DEPTH` per call.
    let max_depth = max_ast_depth();
    let mut out = String::new();
    out.push_str("sequenceDiagram\n");
    let _ = writeln!(out, "    autonumber");
    if !diagram.title.is_empty() {
        let _ = writeln!(out, "    %% {}", strip_newlines(&diagram.title));
    }
    for p in &diagram.participants {
        write_participant(&mut out, p);
    }
    for step in &diagram.steps {
        write_step(&mut out, step, 1, max_depth);
    }
    out
}

fn write_participant(out: &mut String, p: &Participant) {
    let label = escape_label_sequence(&p.label);
    let _ = writeln!(out, "    participant {} as {}", p.id, label);
}

fn write_step(out: &mut String, step: &Step, depth: usize, max_depth: usize) {
    let indent = indent(depth);
    if depth >= max_depth {
        // Adversarial / pathologically nested IR (e.g. a degenerate
        // diff that produced thousands of nested alt blocks): emit a
        // visible `…depth limit…` note and stop recursing instead of
        // blowing the renderer's call stack.
        tracing::warn!(
            depth,
            limit = max_depth,
            "ast depth limit hit in sequence renderer",
        );
        let _ = writeln!(
            out,
            "{indent}Note over {SELF_ID}: {}",
            escape_label_sequence(DEPTH_LIMIT_LABEL),
        );
        return;
    }
    match step {
        Step::Call {
            from,
            to,
            label,
            is_await,
        } => {
            let arrow = if *is_await { "-)" } else { "->>" };
            let suffix = if *is_await { " (await)" } else { "" };
            let _ = writeln!(
                out,
                "{indent}{from}{arrow}{to}: {label}{suffix}",
                from = from,
                to = to,
                label = escape_label_sequence(label),
            );
            // Note: when `from == SELF_ID == to`, Mermaid renders a
            // self-arrow loop, which is the desired behaviour for
            // bare-name calls inside the function under analysis.
            let _ = SELF_ID; // silence unused-import lint when feature gates change.
        }
        Step::Note { over, text } => {
            let _ = writeln!(
                out,
                "{indent}Note over {over}: {}",
                escape_label_sequence(text)
            );
        }
        Step::Loop {
            label,
            body,
            has_visible,
        } => {
            // Mermaid renders `loop X\nend` (empty body) as a tiny stub
            // that overlaps neighbour blocks. Drop empties — the
            // condition is captured by the source code already.
            // `has_visible` is computed at extract time so this is O(1).
            if !*has_visible {
                return;
            }
            let _ = writeln!(out, "{indent}loop {}", escape_label_sequence(label));
            for s in body {
                write_step(out, s, depth + 1, max_depth);
            }
            let _ = writeln!(out, "{indent}end");
        }
        Step::Alt {
            cond,
            then,
            else_,
            then_has_visible,
            else_has_visible,
        } => {
            if !*then_has_visible && !*else_has_visible {
                return;
            }
            let _ = writeln!(out, "{indent}alt {}", escape_label_sequence(cond));
            for s in then {
                write_step(out, s, depth + 1, max_depth);
            }
            if let Some(else_steps) = else_
                && *else_has_visible
            {
                let _ = writeln!(out, "{indent}else");
                for s in else_steps {
                    write_step(out, s, depth + 1, max_depth);
                }
            }
            let _ = writeln!(out, "{indent}end");
        }
    }
}

fn strip_newlines(s: &str) -> String {
    s.replace(['\n', '\r'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diag() -> SequenceDiagram {
        SequenceDiagram {
            title: "fn run()".into(),
            participants: vec![
                Participant {
                    id: SELF_ID.into(),
                    label: "run".into(),
                },
                Participant {
                    id: "cache".into(),
                    label: "cache".into(),
                },
            ],
            steps: vec![Step::Call {
                from: SELF_ID.into(),
                to: "cache".into(),
                label: "open".into(),
                is_await: false,
            }],
        }
    }

    #[test]
    fn renders_sequence_diagram_header() {
        let s = render(&diag());
        assert!(s.starts_with("sequenceDiagram\n"));
        assert!(s.contains("autonumber"));
        assert!(s.contains("participant self as run"));
        assert!(s.contains("self->>cache: open"));
    }

    #[test]
    fn await_arrow_is_async() {
        let mut d = diag();
        d.steps[0] = Step::Call {
            from: SELF_ID.into(),
            to: "cache".into(),
            label: "open".into(),
            is_await: true,
        };
        let s = render(&d);
        assert!(s.contains("self-)cache: open (await)"), "got:\n{s}");
    }

    #[test]
    fn loop_block_renders_with_end() {
        let mut d = diag();
        d.steps = vec![Step::Loop {
            label: "for x in xs".into(),
            body: vec![Step::Call {
                from: SELF_ID.into(),
                to: "cache".into(),
                label: "open".into(),
                is_await: false,
            }],
            has_visible: true,
        }];
        let s = render(&d);
        assert!(s.contains("loop for x in xs"));
        assert!(s.contains("self->>cache: open"));
        assert!(s.contains("\n    end\n"));
    }

    #[test]
    fn alt_with_else_renders_both_branches() {
        let mut d = diag();
        d.steps = vec![Step::Alt {
            cond: "if cond".into(),
            then: vec![Step::Call {
                from: SELF_ID.into(),
                to: "cache".into(),
                label: "yes".into(),
                is_await: false,
            }],
            else_: Some(vec![Step::Call {
                from: SELF_ID.into(),
                to: "cache".into(),
                label: "no".into(),
                is_await: false,
            }]),
            then_has_visible: true,
            else_has_visible: true,
        }];
        let s = render(&d);
        assert!(s.contains("alt if cond"));
        assert!(s.contains("else\n"));
        assert!(s.contains("self->>cache: yes"));
        assert!(s.contains("self->>cache: no"));
    }

    #[test]
    fn label_strips_newlines_and_hash() {
        let mut d = diag();
        d.steps[0] = Step::Call {
            from: SELF_ID.into(),
            to: "cache".into(),
            label: "do\nthing#tag".into(),
            is_await: false,
        };
        let s = render(&d);
        assert!(s.contains("do thing_tag"), "got:\n{s}");
    }

    #[test]
    fn label_keeps_colon_in_qualified_path() {
        // `Cache::open` is fine — Mermaid only splits on the first colon
        // (the one after the arrow target).
        let mut d = diag();
        d.steps[0] = Step::Call {
            from: SELF_ID.into(),
            to: "cache".into(),
            label: "Cache::open".into(),
            is_await: false,
        };
        let s = render(&d);
        assert!(s.contains("Cache::open"), "got:\n{s}");
    }

    #[test]
    fn label_escapes_angle_brackets() {
        // `<` and `>` are HTML-special in Mermaid — leaving them raw
        // (e.g. `if x <= 0`) breaks layout into vertical char-per-line.
        let mut d = diag();
        d.steps = vec![Step::Alt {
            cond: "if kept_size <= cap".into(),
            then: vec![Step::Call {
                from: SELF_ID.into(),
                to: "cache".into(),
                label: "x".into(),
                is_await: false,
            }],
            else_: None,
            then_has_visible: true,
            else_has_visible: false,
        }];
        let s = render(&d);
        assert!(!s.contains("<= cap"), "raw `<` leaked:\n{s}");
        assert!(s.contains("&lt;= cap"), "expected escape:\n{s}");
    }

    #[test]
    fn empty_loop_block_is_dropped() {
        let mut d = diag();
        d.steps = vec![Step::Loop {
            label: "for x in xs".into(),
            body: vec![],
            has_visible: false,
        }];
        let s = render(&d);
        assert!(!s.contains("loop for x in xs"), "empty loop leaked:\n{s}");
        assert!(!s.contains("\n    end\n"), "stray end:\n{s}");
    }

    #[test]
    fn alt_with_only_empty_branches_is_dropped() {
        let mut d = diag();
        d.steps = vec![Step::Alt {
            cond: "if cond".into(),
            then: vec![],
            else_: Some(vec![]),
            then_has_visible: false,
            else_has_visible: false,
        }];
        let s = render(&d);
        assert!(!s.contains("alt if cond"), "empty alt leaked:\n{s}");
    }

    #[test]
    fn alt_with_empty_else_skips_else_clause() {
        // `if cond { yes(); } else { /* break */ }` — then has a call,
        // else is empty after filtering. We should render the alt with
        // only the `then` branch; no dangling `else` separator.
        let mut d = diag();
        d.steps = vec![Step::Alt {
            cond: "if cond".into(),
            then: vec![Step::Call {
                from: SELF_ID.into(),
                to: "cache".into(),
                label: "yes".into(),
                is_await: false,
            }],
            else_: Some(vec![]),
            then_has_visible: true,
            else_has_visible: false,
        }];
        let s = render(&d);
        assert!(s.contains("alt if cond"), "{s}");
        assert!(s.contains("self->>cache: yes"), "{s}");
        assert!(!s.contains("    else\n"), "empty else leaked:\n{s}");
    }
}
