//! Golden harness over the on-disk fixture corpora (`tests/fixtures/`).
//!
//! Every view is asserted by **set equality** on the full node/edge/
//! participant/counter sets — a missing edge fails exactly like an extra
//! one. `assert!(out.contains(...))` cannot detect an absence, which is
//! how the 0.6.0 review bugs (impact forward edges, module intra edges,
//! overview counters, literal sequence participants) all shipped with a
//! green CI. See #167.
//!
//! The corpora are real source files, one per language, covering the
//! trap constructs from that review: intra + cross-module calls, a ≥3-hop
//! forward chain, impl/class methods, literal receivers, and labels that
//! need escaping.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use ast_to_mermaid::diff::{IndexEntity, compute_diff, render_mermaid};
use ast_to_mermaid::parser::Language;
use ast_to_mermaid::render::{AdjMaps, AtomSnapshot, Level, render_in_store};
use ast_to_mermaid::sequence;

use common::build_store;

fn fixture(lang: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(lang)
}

fn view(root: &Path, level: Level, target: Option<&str>) -> String {
    let store = build_store(root);
    let adj = AdjMaps::build(&store);
    let out = render_in_store(level, &store, &adj, target).expect("render");
    assert_balanced_quotes(&out);
    out
}

/// Node ids carry a content-addressed `_H<8 hex>` suffix that shifts with
/// any source edit — strip it so goldens pin structure, not hashes.
fn strip_hashes(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        let hash_here = chars[i] == '_'
            && i + 10 <= chars.len()
            && chars.get(i + 1) == Some(&'H')
            && chars[i + 2..i + 10].iter().all(char::is_ascii_hexdigit)
            && (i + 10 == chars.len() || !chars[i + 10].is_ascii_alphanumeric());
        if hash_here {
            i += 10;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// Every `A --> B` / `A -->|"label"| B` pair in the output, hash-stripped.
fn edge_set(mermaid: &str) -> BTreeSet<(String, String)> {
    let mut set = BTreeSet::new();
    for line in mermaid.lines() {
        let line = line.trim();
        let Some(pos) = line.find("-->") else {
            continue;
        };
        let from = line[..pos].trim();
        let rest = line[pos + 3..].trim_start();
        let to = if let Some(labelled) = rest.strip_prefix('|') {
            labelled.split_once('|').map_or("", |(_, t)| t).trim()
        } else {
            rest.trim()
        };
        set.insert((strip_hashes(from), strip_hashes(to)));
    }
    set
}

fn edges(pairs: &[(&str, &str)]) -> BTreeSet<(String, String)> {
    pairs
        .iter()
        .map(|(f, t)| ((*f).to_owned(), (*t).to_owned()))
        .collect()
}

/// Every `["label"]` node label in the output.
fn label_set(mermaid: &str) -> BTreeSet<String> {
    mermaid
        .lines()
        .filter(|l| !l.trim_start().starts_with("subgraph"))
        .filter_map(|l| {
            let l = l.trim();
            let start = l.find("[\"")?;
            let end = l[start + 2..].find("\"]")?;
            Some(l[start + 2..start + 2 + end].to_owned())
        })
        .collect()
}

fn labels(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

/// An odd number of `"` on a line means a truncation or an escape bug
/// produced Mermaid the parser will reject.
fn assert_balanced_quotes(mermaid: &str) {
    for line in mermaid.lines() {
        assert!(
            line.matches('"').count().is_multiple_of(2),
            "unbalanced quotes in line: {line}"
        );
    }
}

// ---------------------------------------------------------------- project

#[test]
fn rust_project_golden() {
    let out = view(&fixture("mini-rust"), Level::Project, None);
    assert_eq!(
        label_set(&out),
        labels(&[
            "alpha.rs — 1 mod, 5 fn, 1 struct",
            "beta.rs — 1 mod, 4 fn, 0 struct",
        ]),
        "project labels:\n{out}"
    );
    assert_eq!(
        edge_set(&out),
        edges(&[("alpha_rs", "beta_rs")]),
        "project edges:\n{out}"
    );
}

#[test]
fn python_project_golden() {
    let out = view(&fixture("mini-python"), Level::Project, None);
    assert_eq!(
        label_set(&out),
        labels(&[
            "alpha.py — 1 mod, 5 fn, 1 struct",
            "beta.py — 1 mod, 4 fn, 0 struct",
        ]),
        "project labels:\n{out}"
    );
    assert_eq!(edge_set(&out), edges(&[("alpha_py", "beta_py")]));
}

// --------------------------------------------------------------- overview

#[test]
fn rust_overview_golden() {
    // alpha: 2 free fns + 3 impl methods = 5 fn (the review's "0 fn on
    // impl-only modules" regression trips here), 1 struct.
    let out = view(&fixture("mini-rust"), Level::Overview, None);
    assert_eq!(
        label_set(&out),
        labels(&["alpha — 5 fn, 1 struct, 0 trait", "beta — 4 fn"]),
        "overview labels:\n{out}"
    );
    assert_eq!(edge_set(&out), edges(&[("alpha_rs", "beta_rs")]));
}

#[test]
fn python_overview_golden() {
    let out = view(&fixture("mini-python"), Level::Overview, None);
    assert_eq!(
        label_set(&out),
        labels(&["alpha — 5 fn, 1 struct, 0 trait", "beta — 4 fn"]),
        "overview labels:\n{out}"
    );
    assert_eq!(edge_set(&out), edges(&[("alpha_py", "beta_py")]));
}

// ----------------------------------------------------------------- module

#[test]
fn rust_module_golden() {
    // Intra edges (norm → dot between impl siblings, alpha_entry →
    // describe between free fns) AND the cross edge out to beta::entry.
    let out = view(&fixture("mini-rust"), Level::Module, Some("alpha"));
    assert_eq!(
        edge_set(&out),
        edges(&[
            (
                "code_alpha_rs__function__Point__norm",
                "code_alpha_rs__function__Point__dot"
            ),
            (
                "code_alpha_rs__function__alpha_entry",
                "code_alpha_rs__function__describe"
            ),
            (
                "code_alpha_rs__function__alpha_entry",
                "code_beta_rs__function__entry"
            ),
        ]),
        "module edges:\n{out}"
    );
}

#[test]
fn python_module_golden() {
    let out = view(&fixture("mini-python"), Level::Module, Some("alpha"));
    assert_eq!(
        edge_set(&out),
        edges(&[
            (
                "code_alpha_py__function__Point__norm",
                "code_alpha_py__function__Point__dot"
            ),
            (
                "code_alpha_py__function__alpha_entry",
                "code_alpha_py__function__describe"
            ),
            (
                "code_alpha_py__function__alpha_entry",
                "code_beta_py__function__entry"
            ),
        ]),
        "module edges:\n{out}"
    );
}

// ----------------------------------------------------------------- function

#[test]
fn rust_function_golden() {
    // Callers back + direct callees, per the (now honest) help text.
    let out = view(&fixture("mini-rust"), Level::Function, Some("step_two"));
    assert_eq!(
        edge_set(&out),
        edges(&[
            (
                "code_beta_rs__function__step_one",
                "code_beta_rs__function__step_two"
            ),
            (
                "code_beta_rs__function__step_two",
                "code_beta_rs__function__step_three"
            ),
        ]),
        "function edges:\n{out}"
    );
}

#[test]
fn python_function_golden() {
    let out = view(&fixture("mini-python"), Level::Function, Some("step_two"));
    assert_eq!(
        edge_set(&out),
        edges(&[
            (
                "code_beta_py__function__step_one",
                "code_beta_py__function__step_two"
            ),
            (
                "code_beta_py__function__step_two",
                "code_beta_py__function__step_three"
            ),
        ]),
        "function edges:\n{out}"
    );
}

// ------------------------------------------------------------------ impact

#[test]
fn rust_impact_golden() {
    // The review's headline bug: the forward half (entry → step_one →
    // step_two → step_three) was silently absent. Set equality means it
    // can never silently vanish again.
    let out = view(&fixture("mini-rust"), Level::Impact, Some("entry"));
    assert_eq!(
        edge_set(&out),
        edges(&[
            (
                "code_alpha_rs__function__alpha_entry",
                "code_beta_rs__function__entry"
            ),
            (
                "code_beta_rs__function__entry",
                "code_beta_rs__function__step_one"
            ),
            (
                "code_beta_rs__function__step_one",
                "code_beta_rs__function__step_two"
            ),
            (
                "code_beta_rs__function__step_two",
                "code_beta_rs__function__step_three"
            ),
        ]),
        "impact edges:\n{out}"
    );
}

#[test]
fn python_impact_golden() {
    let out = view(&fixture("mini-python"), Level::Impact, Some("entry"));
    assert_eq!(
        edge_set(&out),
        edges(&[
            (
                "code_alpha_py__function__alpha_entry",
                "code_beta_py__function__entry"
            ),
            (
                "code_beta_py__function__entry",
                "code_beta_py__function__step_one"
            ),
            (
                "code_beta_py__function__step_one",
                "code_beta_py__function__step_two"
            ),
            (
                "code_beta_py__function__step_two",
                "code_beta_py__function__step_three"
            ),
        ]),
        "impact edges:\n{out}"
    );
}

// ---------------------------------------------------------------- sequence

#[test]
fn rust_sequence_golden() {
    // `describe` contains a string-literal receiver (`"...".to_string()`)
    // — it must NOT become a participant; `self` is the only actor.
    let src = fs::read(fixture("mini-rust").join("alpha.rs")).expect("read");
    let d = sequence::extract(&src, "alpha.rs", "describe", Language::Rust).expect("extract");
    let ids: BTreeSet<String> = d.participants.iter().map(|p| p.id.clone()).collect();
    assert_eq!(
        ids,
        labels(&["self"]),
        "participants: {:?}",
        d.participants
    );
    for p in &d.participants {
        assert!(
            !p.label.contains('"'),
            "quote in participant label: {p:?}"
        );
    }
    assert_balanced_quotes(&sequence::render(&d));
}

#[test]
fn python_sequence_golden() {
    // `", ".join([...])` must stay on the self lifeline; `p` (a real
    // receiver) is the only other participant.
    let src = fs::read(fixture("mini-python").join("alpha.py")).expect("read");
    let d = sequence::extract(&src, "alpha.py", "describe", Language::Python).expect("extract");
    let ids: BTreeSet<String> = d.participants.iter().map(|p| p.id.clone()).collect();
    assert_eq!(
        ids,
        labels(&["self", "p"]),
        "participants: {:?}",
        d.participants
    );
    assert_balanced_quotes(&sequence::render(&d));
}

// -------------------------------------------------------------------- diff

/// Project the fixture store into the diff's index-entity shape.
fn index_entities(root: &Path) -> Vec<IndexEntity> {
    let store = build_store(root);
    let adj = AdjMaps::build(&store);
    store.with_atoms(|atoms| {
        let snap = AtomSnapshot::build(atoms);
        snap.iter()
            .map(|a| IndexEntity {
                id: a.id.as_str().to_owned(),
                kind: a.kind.clone(),
                name: a.name.clone(),
                file: a.file_path.clone(),
                content_hash: a.content_hash.clone(),
                edges_out: adj
                    .callees(&a.id)
                    .iter()
                    .map(|c| c.as_str().to_owned())
                    .collect(),
            })
            .collect()
    })
}

#[test]
fn rust_diff_golden() {
    // Post-state = the full fixture; pre-state = the fixture minus
    // step_two/step_three, with describe's hash perturbed. Expected diff:
    // 2 added, 1 modified, and exactly one blast-radius edge (step_two →
    // step_three, the only pair whose endpoints BOTH changed).
    let to = index_entities(&fixture("mini-rust"));
    let from: Vec<IndexEntity> = to
        .iter()
        .filter(|e| !e.id.contains("step_two") && !e.id.contains("step_three"))
        .cloned()
        .map(|mut e| {
            if e.id.contains("::describe") {
                e.content_hash = format!("{}-old", e.content_hash);
            }
            e
        })
        .collect();

    let d = compute_diff("base", "head", "sha-a", "sha-b", from, to.clone());
    let out = render_mermaid(&d, &to);
    assert_balanced_quotes(&out);

    // Node lines are `nK["label"]:::class` — collect label → (node, class).
    let mut nodes: BTreeMap<String, (String, String)> = BTreeMap::new();
    for line in out.lines() {
        let line = line.trim();
        let Some((node, rest)) = line.split_once("[\"") else {
            continue;
        };
        let Some((label, class)) = rest.split_once("\"]:::") else {
            continue;
        };
        nodes.insert(label.to_owned(), (node.to_owned(), class.to_owned()));
    }
    let expected: BTreeMap<&str, &str> = [
        ("fn step_two (beta.rs)", "added"),
        ("fn step_three (beta.rs)", "added"),
        ("fn describe (alpha.rs)", "modified"),
    ]
    .into_iter()
    .collect();
    assert_eq!(
        nodes
            .iter()
            .map(|(label, (_, class))| (label.as_str(), class.as_str()))
            .collect::<BTreeMap<_, _>>(),
        expected,
        "diff nodes:\n{out}"
    );

    // Exactly one blast-radius edge: step_two → step_three.
    let arrow_pairs = edge_set(&out);
    let step_two = &nodes["fn step_two (beta.rs)"].0;
    let step_three = &nodes["fn step_three (beta.rs)"].0;
    assert_eq!(
        arrow_pairs,
        edges(&[(step_two, step_three)]),
        "diff edges:\n{out}"
    );
}
