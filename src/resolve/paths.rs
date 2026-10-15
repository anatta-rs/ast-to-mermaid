//! Rust module paths: which file `crate::a::b` names, and what a module
//! re-exports.
//!
//! The call resolver used to match a qualified call on its qualifier's
//! *last segment* against file stems. That works for `render::render`
//! but not for a name reached through `pub use`:
//! `crate::render::mermaid_to_dot` names a function defined in
//! `render/dot.rs`, and no file is called `render.rs` *and* defines it.
//! This index maps every Rust file to its place in the module tree so a
//! path can be followed segment by segment, then through the re-exports
//! of the module it lands on.

use crate::model::{CodeAtom, ReExport};
use std::collections::HashMap;

/// How many `pub use` hops [`RustModules::lookup`] follows. Real chains
/// are short (`lib.rs` → `cli/mod.rs` → `cli/run/mod.rs` → the file);
/// the cap only guards against cycles (`pub use a::*` ↔ `pub use b::*`).
const MAX_REEXPORT_HOPS: u8 = 6;

/// Where a Rust file sits in its crate's module tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModPos {
    /// The crate: the directory holding its `src/`, or `""` for a
    /// top-level `src/` (and for an analysed `src/` itself).
    pub krate: String,
    /// Module path inside the crate, `::`-joined. `""` is the crate root.
    pub path: String,
    /// A separate target — `bin/`, `tests/`, `benches/`, `examples/`,
    /// `main.rs` — which reaches the library by the crate's *name*
    /// (`my_crate::cli::run`) rather than by `crate::`.
    pub external_target: bool,
}

/// Place of a `.rs` file in its crate's module tree. `None` for other
/// languages.
///
/// `src/a/b.rs` → `a::b`, `src/a/mod.rs` → `a`, `src/lib.rs` → root.
/// Inline `mod x { … }` blocks are not files and are not modelled.
pub(crate) fn rust_mod_pos(file_path: &str) -> Option<ModPos> {
    let rel = file_path.strip_suffix(".rs")?;
    let (krate, rel) = if let Some(i) = rel.find("/src/") {
        (&rel[..i], &rel[i + "/src/".len()..])
    } else if let Some(r) = rel.strip_prefix("src/") {
        ("", r)
    } else {
        ("", rel)
    };
    let mut segs: Vec<&str> = rel.split('/').collect();
    let external_target = matches!(segs[0], "bin" | "tests" | "benches" | "examples")
        || (segs.len() == 1 && segs[0] == "main");
    if segs.last() == Some(&"mod") {
        segs.pop();
    }
    if segs.len() == 1 && matches!(segs[0], "lib" | "main") {
        segs.clear();
    }
    Some(ModPos {
        krate: krate.to_owned(),
        path: segs.join("::"),
        external_target,
    })
}

/// A module path [`RustModules::resolve_module`] reached.
pub(crate) struct Resolved {
    /// Module path inside the caller's crate.
    pub path: String,
    /// The head was taken for the crate's own name, by elimination. A
    /// miss behind such a guess must not be read as "this module has no
    /// such item" — the head may just as well be an external crate.
    pub guessed: bool,
}

/// Module tree of every Rust file in the graph.
pub(crate) struct RustModules<'a> {
    /// crate → module path → files implementing it. Usually one;
    /// `lib.rs` and `main.rs` both implement a crate root. Nested so a
    /// lookup borrows its `&str` keys instead of allocating a tuple.
    files: HashMap<String, HashMap<String, Vec<&'a str>>>,
    /// File → its position.
    pos: HashMap<&'a str, ModPos>,
    /// File → what it re-exports.
    reexports: HashMap<&'a str, &'a [ReExport]>,
}

impl<'a> RustModules<'a> {
    /// Index the module atoms of `atoms` that are Rust files.
    pub(crate) fn build<'b>(atoms: impl IntoIterator<Item = &'b &'a CodeAtom>) -> Self
    where
        'a: 'b,
    {
        let mut files: HashMap<String, HashMap<String, Vec<&'a str>>> = HashMap::new();
        let mut pos = HashMap::new();
        let mut reexports = HashMap::new();
        for atom in atoms {
            let atom: &'a CodeAtom = atom;
            if atom.kind != "module" {
                continue;
            }
            let Some(p) = rust_mod_pos(&atom.file_path) else {
                continue;
            };
            files
                .entry(p.krate.clone())
                .or_default()
                .entry(p.path.clone())
                .or_default()
                .push(atom.file_path.as_str());
            if !atom.refs.reexports.is_empty() {
                reexports.insert(atom.file_path.as_str(), atom.refs.reexports.as_slice());
            }
            pos.insert(atom.file_path.as_str(), p);
        }
        Self {
            files,
            pos,
            reexports,
        }
    }

    /// Position of a Rust file, if it is one.
    pub(crate) fn pos(&self, file: &str) -> Option<&ModPos> {
        self.pos.get(file)
    }

    /// Files implementing module `path` of `krate`.
    pub(crate) fn files(&self, krate: &str, path: &str) -> &[&'a str] {
        self.files
            .get(krate)
            .and_then(|m| m.get(path))
            .map_or(&[], Vec::as_slice)
    }

    fn exists(&self, krate: &str, path: &str) -> bool {
        self.files.get(krate).is_some_and(|m| m.contains_key(path))
    }

    /// The module a path written in module `from` names, if every segment
    /// is a module of `from`'s crate.
    ///
    /// The head is read the way Rust 2018 reads it: `crate` / `self` /
    /// `super`, else a child module of `from`. A separate target may also
    /// start with the crate's name, which this index cannot know: a head
    /// that is not a child module, followed by a segment that *is* a
    /// root module, is taken for it (`my_crate::cli::run`) and flagged
    /// [`Resolved::guessed`].
    pub(crate) fn resolve_module(&self, from: &ModPos, segs: &[&str]) -> Option<Resolved> {
        let (&head, rest) = segs.split_first()?;
        let mut guessed = false;
        let mut cur = match head {
            "crate" => String::new(),
            "self" => from.path.clone(),
            "super" => parent(&from.path)?,
            _ => {
                let child = join(&from.path, head);
                if self.exists(&from.krate, &child) {
                    child
                } else if from.external_target
                    && !matches!(head, "std" | "core" | "alloc")
                    && rest.first().is_some_and(|s| self.exists(&from.krate, s))
                {
                    guessed = true;
                    String::new()
                } else {
                    return None;
                }
            }
        };
        for &seg in rest {
            match seg {
                "super" => cur = parent(&cur)?,
                "self" => {}
                _ => {
                    let child = join(&cur, seg);
                    if !self.exists(&from.krate, &child) {
                        return None;
                    }
                    cur = child;
                }
            }
        }
        Some(Resolved { path: cur, guessed })
    }

    /// Atoms reachable as `module::name` in `krate`: defined by one of
    /// the module's files, or re-exported by it (`pub use`, `pub use *`),
    /// followed transitively. `defines(file, name)` returns the atoms a
    /// file itself defines under `name`.
    pub(crate) fn lookup(
        &self,
        krate: &str,
        module: &str,
        name: &str,
        defines: &dyn Fn(&str, &str) -> Vec<usize>,
    ) -> Vec<usize> {
        self.lookup_hops(krate, module, name, defines, 0)
    }

    fn lookup_hops(
        &self,
        krate: &str,
        module: &str,
        name: &str,
        defines: &dyn Fn(&str, &str) -> Vec<usize>,
        hops: u8,
    ) -> Vec<usize> {
        let files = self.files(krate, module);
        if files.is_empty() {
            return Vec::new();
        }
        let mut out: Vec<usize> = files.iter().flat_map(|f| defines(f, name)).collect();
        if !out.is_empty() || hops >= MAX_REEXPORT_HOPS {
            return out;
        }
        let here = ModPos {
            krate: krate.to_owned(),
            path: module.to_owned(),
            external_target: false,
        };
        for file in files {
            for re in self.reexports.get(file).copied().unwrap_or_default() {
                let segs: Vec<&str> = re.path.split("::").collect();
                if re.glob {
                    if let Some(m) = self.resolve_module(&here, &segs) {
                        out.extend(self.lookup_hops(krate, &m.path, name, defines, hops + 1));
                    }
                } else if re.name == name
                    && let Some((target, qualifier)) = segs.split_last()
                {
                    // `pub use helper;` re-exports a name of this very
                    // module's scope; nothing to follow.
                    if qualifier.is_empty() {
                        continue;
                    }
                    if let Some(m) = self.resolve_module(&here, qualifier) {
                        out.extend(self.lookup_hops(krate, &m.path, target, defines, hops + 1));
                    }
                }
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }
}

fn join(base: &str, seg: &str) -> String {
    if base.is_empty() {
        seg.to_owned()
    } else {
        format!("{base}::{seg}")
    }
}

/// Parent module path; `None` above the crate root.
fn parent(path: &str) -> Option<String> {
    if path.is_empty() {
        return None;
    }
    Some(
        path.rsplit_once("::")
            .map_or(String::new(), |(p, _)| p.to_owned()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn module(file: &str, reexports: &[(&str, &str, bool)]) -> CodeAtom {
        CodeAtom {
            id: crate::model::EntityId::new(format!("code:{file}")),
            kind: "module".to_owned(),
            name: String::new(),
            file_path: file.to_owned(),
            line_start: 1,
            line_end: 1,
            doc: String::new(),
            signature: String::new(),
            content_hash: String::new(),
            calls: Vec::new(),
            method_calls: Vec::new(),
            parent: None,
            refs: crate::model::AtomRefs {
                reexports: reexports
                    .iter()
                    .map(|(n, p, g)| ReExport {
                        name: (*n).to_owned(),
                        path: (*p).to_owned(),
                        glob: *g,
                    })
                    .collect(),
                ..Default::default()
            },
        }
    }

    #[test]
    fn mod_pos_follows_cargo_layout() {
        let p = rust_mod_pos("src/render/dot.rs").unwrap();
        assert_eq!((p.krate.as_str(), p.path.as_str()), ("", "render::dot"));
        let p = rust_mod_pos("render/mod.rs").unwrap();
        assert_eq!(p.path, "render");
        let p = rust_mod_pos("crates/x/src/lib.rs").unwrap();
        assert_eq!((p.krate.as_str(), p.path.as_str()), ("crates/x", ""));
        assert!(rust_mod_pos("bin/a2m.rs").unwrap().external_target);
        assert!(!rust_mod_pos("cli/run/mod.rs").unwrap().external_target);
        assert!(rust_mod_pos("a.py").is_none());
    }

    #[test]
    fn lookup_follows_a_reexport_chain() {
        let atoms = [
            module("lib.rs", &[("*", "cli", true)]),
            module("cli/mod.rs", &[("run_x", "run::run_x", false)]),
            module("cli/run/mod.rs", &[("run_x", "x::run_x", false)]),
            module("cli/run/x.rs", &[]),
        ];
        let refs: Vec<&CodeAtom> = atoms.iter().collect();
        let mods = RustModules::build(&refs);
        let defines = |file: &str, name: &str| -> Vec<usize> {
            if file == "cli/run/x.rs" && name == "run_x" {
                vec![7]
            } else {
                Vec::new()
            }
        };
        assert_eq!(mods.lookup("", "", "run_x", &defines), vec![7]);
        assert_eq!(mods.lookup("", "cli", "run_x", &defines), vec![7]);
        assert!(mods.lookup("", "cli", "other", &defines).is_empty());
    }

    #[test]
    fn resolve_module_reads_rust_2018_heads() {
        let atoms = [
            module("lib.rs", &[]),
            module("cli/mod.rs", &[]),
            module("cli/run/mod.rs", &[]),
            module("cli/run/x.rs", &[]),
            module("bin/tool.rs", &[]),
        ];
        let refs: Vec<&CodeAtom> = atoms.iter().collect();
        let mods = RustModules::build(&refs);
        let x = mods.pos("cli/run/x.rs").unwrap().clone();
        let path = |segs: &[&str], from: &ModPos| mods.resolve_module(from, segs).map(|r| r.path);
        assert_eq!(path(&["super"], &x).as_deref(), Some("cli::run"));
        assert_eq!(path(&["super", "super"], &x).as_deref(), Some("cli"));
        assert_eq!(path(&["crate", "cli"], &x).as_deref(), Some("cli"));
        assert_eq!(path(&["serde_json"], &x), None);
        let run = mods.pos("cli/run/mod.rs").unwrap().clone();
        assert_eq!(path(&["x"], &run).as_deref(), Some("cli::run::x"));
        // A separate target names the library by its crate name.
        let bin = mods.pos("bin/tool.rs").unwrap().clone();
        let r = mods.resolve_module(&bin, &["my_crate", "cli"]).unwrap();
        assert_eq!((r.path.as_str(), r.guessed), ("cli", true));
        assert!(mods.resolve_module(&bin, &["std", "cli"]).is_none());
    }
}
