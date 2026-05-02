//! [`SequenceDiagram`] → Mermaid `sequenceDiagram` text.

use super::{Participant, SequenceDiagram, Step, SELF_ID};
use std::fmt::Write;

/// Render a [`SequenceDiagram`] as Mermaid `sequenceDiagram` source.
///
/// Output uses `autonumber` so each step is numbered, `participant <id>
/// as <label>` declarations in first-appearance order, and synchronous
/// `->>` arrows for calls. `.await` shows on the arrow label as ` (await)`.
#[must_use]
pub fn render(diagram: &SequenceDiagram) -> String {
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
        write_step(&mut out, step, 1);
    }
    out
}

fn write_participant(out: &mut String, p: &Participant) {
    let label = escape_label(&p.label);
    let _ = writeln!(out, "    participant {} as {}", p.id, label);
}

fn write_step(out: &mut String, step: &Step, depth: usize) {
    let indent = "    ".repeat(depth);
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
                label = escape_label(label),
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
                escape_label(text)
            );
        }
        Step::Loop { label, body } => {
            let _ = writeln!(out, "{indent}loop {}", escape_label(label));
            for s in body {
                write_step(out, s, depth + 1);
            }
            let _ = writeln!(out, "{indent}end");
        }
        Step::Alt { cond, then, else_ } => {
            let _ = writeln!(out, "{indent}alt {}", escape_label(cond));
            for s in then {
                write_step(out, s, depth + 1);
            }
            if let Some(else_steps) = else_ {
                let _ = writeln!(out, "{indent}else");
                for s in else_steps {
                    write_step(out, s, depth + 1);
                }
            }
            let _ = writeln!(out, "{indent}end");
        }
    }
}

/// Mermaid sequenceDiagram message labels split on the *first* `:` after
/// the arrow — subsequent colons render fine, so leave them alone. Just
/// normalise newlines so a multi-line label can't break the document, and
/// escape `#` (used for HTML entities by Mermaid).
fn escape_label(s: &str) -> String {
    s.replace(['\n', '\r'], " ").replace('#', "_")
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
}
