//! `Imports` and `Uses` edges: binding the import targets and type names
//! the parser recorded on each atom ([`crate::model::AtomRefs`]) to the
//! atoms of the graph.
//!
//! Both passes follow the call resolver's rule: an edge is drawn only
//! when the target is unique — exact module path first, then a single
//! same-crate candidate, then a single candidate overall. Anything that
//! does not resolve (the standard library, a third-party package) draws
//! nothing: unlike calls, a dependency on `std::collections` is not worth
//! an `extern` node.

use super::paths::{Defines, RustModules};
use super::{crate_root, lang_of};
use crate::graph::Store;
use crate::model::{CodeAtom, Edge, EdgeKind, EntityId};
use std::collections::{BTreeSet, HashMap};

/// Atom kinds a type name can refer to.
const TYPE_KINDS: &[&str] = &[
    "struct",
    "enum",
    "trait",
    "type_alias",
    "class",
    "union",
    "mixin",
    "extension",
];

/// Add a module → module `Imports` edge for every import a file makes of
/// another file in the graph. Returns the number of edges added.
pub fn resolve_import_edges(store: &Store) -> usize {
    let mut staged: BTreeSet<(EntityId, EntityId)> = BTreeSet::new();
    store.with_atoms(|atoms| {
        let modules: Vec<&CodeAtom> = atoms.iter().filter(|a| a.kind == "module").collect();
        if modules.is_empty() {
            return;
        }
        let module_by_file: HashMap<&str, &CodeAtom> =
            modules.iter().map(|m| (m.file_path.as_str(), *m)).collect();
        let rust_mods = RustModules::build(atoms);
        // Rust items by `(file, scope, name)`, to find the file
        // `use a::Item` lands on through re-exports.
        let items: Vec<&CodeAtom> = atoms
            .iter()
            .filter(|a| a.kind != "module" && a.parent.is_none() && !a.file_path.is_empty())
            .collect();
        let mut item_by_file: HashMap<(&str, &str, &str), Vec<usize>> = HashMap::new();
        for (i, a) in items.iter().enumerate() {
            item_by_file
                .entry((a.file_path.as_str(), a.refs.scope.as_str(), a.name.as_str()))
                .or_default()
                .push(i);
        }
        let defines = |file: &str, scope: &str, name: &str| -> Vec<usize> {
            item_by_file
                .get(&(file, scope, name))
                .cloned()
                .unwrap_or_default()
        };

        for module in &modules {
            for target in &module.refs.imports {
                let files: Vec<&str> = match lang_of(&module.file_path) {
                    "rs" => rust_import_files(&rust_mods, module, target, &defines, &items),
                    "py" => python_import_files(&modules, module, target),
                    "dart" => dart_import_files(&module_by_file, module, target),
                    _ => Vec::new(),
                };
                for file in files {
                    if file == module.file_path {
                        continue;
                    }
                    if let Some(to) = module_by_file.get(file) {
                        staged.insert((module.id.clone(), to.id.clone()));
                    }
                }
            }
        }
    });
    let added = staged.len();
    for (from, to) in staged {
        store.add_edge(Edge::new(from, to, EdgeKind::Imports));
    }
    added
}

/// Files a Rust `use` path lands on: the file defining the imported item
/// (through `pub use` chains), else the module the path names.
fn rust_import_files<'a>(
    mods: &RustModules<'a>,
    module: &CodeAtom,
    path: &str,
    defines: &Defines,
    items: &[&'a CodeAtom],
) -> Vec<&'a str> {
    let Some(from) = mods.pos(&module.file_path) else {
        return Vec::new();
    };
    let segs: Vec<&str> = path.split("::").collect();
    // `use crate::git_source;` — the whole path is a module. Checked
    // first: the parent's `mod git_source;` declaration is an item of the
    // same name and would otherwise win.
    if let Some(m) = mods.resolve_module(from, &segs) {
        return mods.files(&from.krate, &m.path);
    }
    if let Some((name, qualifier)) = segs.split_last()
        && !qualifier.is_empty()
        && let Some(m) = mods.resolve_module(from, qualifier)
    {
        let found = mods.lookup(&from.krate, &m.path, name, defines);
        if !found.is_empty() {
            let mut files: Vec<&str> = found.iter().map(|&i| items[i].file_path.as_str()).collect();
            files.sort_unstable();
            files.dedup();
            return files;
        }
        if !m.guessed {
            return mods.files(&from.krate, &m.path);
        }
    }
    Vec::new()
}

/// Dotted module name of a Python file: `pkg/sub/mod.py` → `pkg.sub.mod`,
/// `pkg/__init__.py` → `pkg`.
fn python_dotted(file: &str) -> Option<String> {
    let stem = file.strip_suffix(".py")?;
    let stem = stem.strip_suffix("/__init__").unwrap_or(stem);
    Some(stem.replace('/', "."))
}

/// Files a Python import names. Relative imports (`.mod`, `..pkg`) are
/// resolved against the importing file's package; absolute ones match
/// the trailing dotted path of a file (`lib.catalog` ↔ `src/lib/catalog.py`).
fn python_import_files<'a>(
    modules: &[&'a CodeAtom],
    module: &CodeAtom,
    target: &str,
) -> Vec<&'a str> {
    let dots = target.chars().take_while(|c| *c == '.').count();
    let rest = &target[dots..];
    let wanted: String = if dots > 0 {
        // `.` is the importing file's own package; each extra dot one up.
        let mut base: Vec<&str> = module.file_path.split('/').collect();
        base.pop();
        for _ in 1..dots {
            if base.pop().is_none() {
                return Vec::new();
            }
        }
        let mut parts: Vec<&str> = base;
        parts.extend(rest.split('.').filter(|s| !s.is_empty()));
        parts.join(".")
    } else {
        rest.to_owned()
    };
    if wanted.is_empty() {
        return Vec::new();
    }
    let candidates: Vec<&'a CodeAtom> = modules
        .iter()
        .copied()
        .filter(|m| lang_of(&m.file_path) == "py")
        .filter(|m| {
            python_dotted(&m.file_path).is_some_and(|d| {
                if dots > 0 {
                    d == wanted
                } else {
                    d == wanted || d.ends_with(&format!(".{wanted}"))
                }
            })
        })
        .collect();
    unique_file(&candidates, module)
}

/// Files a Dart import URI names: relative URIs against the importing
/// file's directory, `package:` URIs by the path after the package name.
fn dart_import_files<'a>(
    module_by_file: &HashMap<&'a str, &'a CodeAtom>,
    module: &CodeAtom,
    uri: &str,
) -> Vec<&'a str> {
    if uri.starts_with("dart:") {
        return Vec::new();
    }
    if let Some(pkg) = uri.strip_prefix("package:") {
        let Some((_, path)) = pkg.split_once('/') else {
            return Vec::new();
        };
        let candidates: Vec<&'a CodeAtom> = module_by_file
            .values()
            .copied()
            .filter(|m| m.file_path == path || m.file_path.ends_with(&format!("/{path}")))
            .collect();
        return unique_file(&candidates, module);
    }
    let mut parts: Vec<&str> = module.file_path.split('/').collect();
    parts.pop();
    for seg in uri.split('/') {
        match seg {
            "." | "" => {}
            ".." => {
                if parts.pop().is_none() {
                    return Vec::new();
                }
            }
            s => parts.push(s),
        }
    }
    let joined = parts.join("/");
    module_by_file
        .get_key_value(joined.as_str())
        .map(|(k, _)| vec![*k])
        .unwrap_or_default()
}

/// The one candidate, preferring the importer's crate when several match.
fn unique_file<'a>(candidates: &[&'a CodeAtom], importer: &CodeAtom) -> Vec<&'a str> {
    match candidates {
        [one] => vec![one.file_path.as_str()],
        [] => Vec::new(),
        many => {
            let root = crate_root(importer);
            let same: Vec<&&CodeAtom> = many.iter().filter(|c| crate_root(c) == root).collect();
            match same.as_slice() {
                [one] => vec![one.file_path.as_str()],
                _ => Vec::new(),
            }
        }
    }
}

/// Add a `Uses` edge from every atom to each in-graph type its declaration
/// names. Returns the number of edges added.
pub fn resolve_type_refs(store: &Store) -> usize {
    let mut staged: BTreeSet<(EntityId, EntityId)> = BTreeSet::new();
    store.with_atoms(|atoms| {
        let types: Vec<&CodeAtom> = atoms
            .iter()
            .filter(|a| TYPE_KINDS.contains(&a.kind.as_str()))
            .collect();
        if types.is_empty() {
            return;
        }
        let mut by_name: HashMap<&str, Vec<usize>> = HashMap::new();
        let mut by_file: HashMap<(&str, &str, &str), Vec<usize>> = HashMap::new();
        for (i, t) in types.iter().enumerate() {
            by_name.entry(t.name.as_str()).or_default().push(i);
            by_file
                .entry((t.file_path.as_str(), t.refs.scope.as_str(), t.name.as_str()))
                .or_default()
                .push(i);
        }
        let rust_mods = RustModules::build(atoms);
        let defines = |file: &str, scope: &str, name: &str| -> Vec<usize> {
            by_file
                .get(&(file, scope, name))
                .cloned()
                .unwrap_or_default()
        };

        for atom in atoms {
            if atom.refs.types.is_empty() {
                continue;
            }
            let lang = lang_of(&atom.file_path);
            let atom_crate = crate_root(atom);
            for written in &atom.refs.types {
                let target = pick_type(
                    written,
                    atom,
                    lang,
                    atom_crate.as_deref(),
                    &types,
                    &by_name,
                    &rust_mods,
                    &defines,
                );
                if let Some(idx) = target
                    && types[idx].id != atom.id
                {
                    staged.insert((atom.id.clone(), types[idx].id.clone()));
                }
            }
        }
    });
    let added = staged.len();
    for (from, to) in staged {
        store.add_edge(Edge::new(from, to, EdgeKind::Uses));
    }
    added
}

/// The type atom a written type name refers to, if unique.
#[allow(clippy::too_many_arguments)]
fn pick_type(
    written: &str,
    atom: &CodeAtom,
    lang: &str,
    atom_crate: Option<&str>,
    types: &[&CodeAtom],
    by_name: &HashMap<&str, Vec<usize>>,
    rust_mods: &RustModules,
    defines: &Defines,
) -> Option<usize> {
    let (qualifier, name) = match written.rsplit_once("::") {
        Some((q, n)) => (Some(q), n),
        None => (None, written),
    };
    // `crate::graph::Store` — follow the path exactly.
    if let Some(q) = qualifier
        && let Some(from) = rust_mods.pos_in(&atom.file_path, &atom.refs.scope)
    {
        let segs: Vec<&str> = q.split("::").collect();
        if let Some(m) = rust_mods.resolve_module(&from, &segs) {
            let found = rust_mods.lookup(&from.krate, &m.path, name, defines);
            return match found.as_slice() {
                [one] => Some(*one),
                _ => None,
            };
        }
    }
    let cands: Vec<usize> = by_name
        .get(name)?
        .iter()
        .copied()
        .filter(|&i| lang_of(&types[i].file_path) == lang)
        .collect();
    // Defined in the same file wins: that is the name in scope.
    let local: Vec<usize> = cands
        .iter()
        .copied()
        .filter(|&i| types[i].file_path == atom.file_path)
        .collect();
    if let [one] = local.as_slice() {
        return Some(*one);
    }
    let same_crate: Vec<usize> = cands
        .iter()
        .copied()
        .filter(|&i| crate_root(types[i]).as_deref() == atom_crate)
        .collect();
    match (same_crate.as_slice(), cands.as_slice()) {
        ([one], _) | ([], [one]) => Some(*one),
        _ => None,
    }
}
