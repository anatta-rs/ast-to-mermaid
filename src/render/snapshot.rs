//! Borrowed view into a [`Store`]'s atom slice.
//!
//! [`AtomSnapshot`] is the once-per-render replacement for the
//! [`Store::get_atom`] hot loop in every renderer. The caller drives a
//! [`Store::with_atoms`] callback, builds the snapshot from the borrowed
//! slice, and threads `&AtomSnapshot` through the render dispatcher; every
//! lookup downstream is then a single `HashMap` probe with no `RwLock`
//! traffic and no [`crate::model::CodeAtom`] clones.
//!
//! Lifetime contract: `AtomSnapshot<'a>` borrows the slice it was built from
//! — typically the slice handed to a [`Store::with_atoms`] closure — so the
//! read guard must outlive the snapshot. In practice that means: build it
//! inside the callback, render inside the callback, drop both as the
//! callback returns.
//!
//! [`Store`]: crate::graph::Store
//! [`Store::with_atoms`]: crate::graph::Store::with_atoms
//! [`Store::get_atom`]: crate::graph::Store::get_atom

use crate::model::{CodeAtom, EntityId};
use std::collections::HashMap;

/// Borrowed `id → &CodeAtom` index over an atom slice.
pub struct AtomSnapshot<'a> {
    by_id: HashMap<&'a EntityId, &'a CodeAtom>,
}

impl<'a> AtomSnapshot<'a> {
    /// Build a snapshot over `atoms`. O(N) — one hashmap insert per atom.
    #[must_use]
    pub fn build(atoms: &'a [CodeAtom]) -> Self {
        let mut by_id = HashMap::with_capacity(atoms.len());
        for atom in atoms {
            by_id.insert(&atom.id, atom);
        }
        Self { by_id }
    }

    /// O(1) lookup of an atom by its id. Returns the borrowed reference;
    /// no clone of the underlying `CodeAtom`.
    #[must_use]
    pub fn get(&self, id: &EntityId) -> Option<&'a CodeAtom> {
        self.by_id.get(id).copied()
    }

    /// Iterate every atom in the snapshot. Order is unspecified —
    /// downstream renderers feed the iterator into deterministic
    /// containers (`BTreeMap` etc.) so the rendered output remains stable.
    pub fn iter(&self) -> impl Iterator<Item = &'a CodeAtom> + '_ {
        self.by_id.values().copied()
    }

    /// Total number of atoms in the snapshot.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    /// Whether the snapshot is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn atom(id: &str, kind: &str) -> CodeAtom {
        CodeAtom {
            id: EntityId::new(id),
            kind: kind.to_owned(),
            name: id.to_owned(),
            file_path: "src/lib.rs".to_owned(),
            line_start: 1,
            line_end: 1,
            doc: String::new(),
            signature: String::new(),
            content_hash: "h".to_owned(),
            calls: Vec::new(),
            method_calls: Vec::new(),
            parent: None,
        }
    }

    #[test]
    fn build_indexes_every_atom_by_id() {
        let atoms = vec![atom("a", "function"), atom("b", "module")];
        let snap = AtomSnapshot::build(&atoms);
        assert_eq!(snap.len(), 2);
        assert!(!snap.is_empty());
        assert_eq!(snap.get(&EntityId::new("a")).expect("a").kind, "function");
        assert_eq!(snap.get(&EntityId::new("b")).expect("b").kind, "module");
        assert!(snap.get(&EntityId::new("missing")).is_none());
    }

    #[test]
    fn empty_input_is_empty() {
        let atoms: Vec<CodeAtom> = Vec::new();
        let snap = AtomSnapshot::build(&atoms);
        assert!(snap.is_empty());
        assert_eq!(snap.len(), 0);
    }

    #[test]
    fn iter_visits_every_atom() {
        let atoms = vec![atom("a", "function"), atom("b", "module")];
        let snap = AtomSnapshot::build(&atoms);
        let mut kinds: Vec<&str> = snap.iter().map(|a| a.kind.as_str()).collect();
        kinds.sort_unstable();
        assert_eq!(kinds, vec!["function", "module"]);
    }
}
