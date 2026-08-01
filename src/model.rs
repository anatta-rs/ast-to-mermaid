//! Domain model: code atoms, edges, ids — the public types renderers and
//! the pipeline consume.

use serde::{Deserialize, Serialize};

// ── Entity identity ──────────────────────────────────────────────────────────

/// Opaque identifier for a code atom (function, module, struct, …).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EntityId(pub(crate) String);

impl EntityId {
    /// Construct from any string-like value.
    #[must_use]
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// Return the raw string representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for EntityId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// ── Atom kind ────────────────────────────────────────────────────────────────

/// Coarse-grained category of a code entity.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum AtomKind {
    /// A function or method.
    Function,
    /// A module (file-level in Rust, package in Python).
    Module,
    /// A `struct` or dataclass.
    Struct,
    /// A `trait` or ABC.
    Trait,
    /// An `impl` block.
    Impl,
    /// An `enum`.
    Enum,
    /// A `type` alias.
    TypeAlias,
    /// A `const`.
    Const,
    /// A `static`.
    Static,
    /// A macro definition.
    Macro,
    /// Any kind not listed above.
    Other(String),
}

impl AtomKind {
    /// Canonical lower-case string used in IDs and metadata.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Function => "function",
            Self::Module => "module",
            Self::Struct => "struct",
            Self::Trait => "trait",
            Self::Impl => "impl",
            Self::Enum => "enum",
            Self::TypeAlias => "type_alias",
            Self::Const => "const",
            Self::Static => "static",
            Self::Macro => "macro",
            Self::Other(s) => s.as_str(),
        }
    }

    /// Reconstruct from a lower-case string.
    #[must_use]
    pub fn parse(s: &str) -> Self {
        match s {
            "function" => Self::Function,
            "module" => Self::Module,
            "struct" => Self::Struct,
            "trait" => Self::Trait,
            "impl" => Self::Impl,
            "enum" => Self::Enum,
            "type_alias" => Self::TypeAlias,
            "const" => Self::Const,
            "static" => Self::Static,
            "macro" => Self::Macro,
            other => Self::Other(other.to_owned()),
        }
    }
}

impl std::fmt::Display for AtomKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ── Call site ────────────────────────────────────────────────────────────────

/// Control-flow context a call site sits in, as a bitfield.
///
/// Detected by walking the callee's ancestors up to the enclosing
/// function. Only the *presence* of a control node matters — no field is
/// read — so one set of kinds covers all three grammars. That is the
/// boundary drawn in `sequence::lang`: structural dispatch is shared,
/// field reads are per-language.
pub mod call_flags {
    /// The call is `await`ed — Rust postfix `.await`, Python and Dart
    /// prefix `await`.
    pub const AWAIT: u8 = 1 << 0;
    /// The call sits under an `if` / `match` / `switch`, so it may not run
    /// at all. Without this, a rank alone implies a sequence that the code
    /// does not guarantee.
    pub const CONDITIONAL: u8 = 1 << 1;
    /// The call sits in a loop and may run many times.
    pub const REPEATED: u8 = 1 << 2;
}

/// One call site in an atom's body: what it calls, where, and under what
/// control flow.
///
/// Replaces the bare `String` the parser used to emit. `name` is what the
/// resolver consumes and is unchanged; `rank` and `flags` are what lets a
/// view state *when* a call happens rather than merely *that* it happens.
///
/// `rank` was already implicit — the parser's dedup uses `retain`, which
/// preserves first-appearance order — but nothing named it, so no view
/// could use it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallSite {
    /// Resolver-eligible name: a bare identifier, or a qualified
    /// `module::foo` / `Owner::method` path.
    pub name: String,
    /// Position in the enclosing body, in source order, 0-based.
    #[serde(default)]
    pub rank: u16,
    /// Control-flow context — see [`call_flags`].
    #[serde(default, skip_serializing_if = "is_zero")]
    pub flags: u8,
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's skip_serializing_if requires a by-ref predicate"
)]
fn is_zero(v: &u8) -> bool {
    *v == 0
}

impl CallSite {
    /// A call site with no recorded context — used by tests and by
    /// callers that only care about the name.
    #[must_use]
    pub fn bare(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            rank: 0,
            flags: 0,
        }
    }

    /// Whether this site carries `flag` (one of [`call_flags`]).
    #[must_use]
    pub fn has(&self, flag: u8) -> bool {
        self.flags & flag != 0
    }
}

// ── Code atom ────────────────────────────────────────────────────────────────

/// A single named code entity parsed from a source file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeAtom {
    /// Globally unique identifier (e.g. `code:src/lib.rs::function::foo`).
    pub id: EntityId,
    /// Coarse kind.
    pub kind: String,
    /// Short display name (e.g. `foo`).
    pub name: String,
    /// Source file path relative to the analyzed root.
    pub file_path: String,
    /// First line of the definition (1-based).
    pub line_start: u32,
    /// Last line of the definition (1-based).
    pub line_end: u32,
    /// Doc comment / docstring (may be empty).
    pub doc: String,
    /// Declaration signature (e.g. `pub fn foo(x: u32) -> bool`).
    pub signature: String,
    /// Git blob SHA-1 hex hash of the atom's source text (`SHA1("blob N\0" + content)`).
    /// Same value as `git hash-object` on the enclosing source slice.
    pub content_hash: String,
    /// Call sites in this atom's body, in source order. Each carries the
    /// resolver-eligible name plus its rank and control-flow context —
    /// see [`CallSite`].
    ///
    /// Every occurrence is kept, including repeats: calling `read()`
    /// twice yields two sites. Collapsing them here would erase the
    /// second call from every consumer, and the graph's own edge
    /// de-duplication already keeps that from multiplying edges.
    pub calls: Vec<CallSite>,
    /// Names captured from receiver-style call sites — `obj.method()` in
    /// Rust (`field_expression`) or `obj.method()` in Python (`attribute`
    /// access). These carry no usable receiver type, so the resolver
    /// never matches them across modules; they are kept only for
    /// intra-container direct linking (e.g. `self.method()` inside an
    /// impl/class). Splitting them out is what prevents a stray
    /// `client.build()` from ghost-binding to an unrelated free fn
    /// named `build` somewhere else in the graph.
    ///
    /// Ranks share one sequence with [`CodeAtom::calls`], so the two
    /// lists interleave back into source order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub method_calls: Vec<CallSite>,
    /// Type owner for methods (Rust impl owner type, Python class). `None`
    /// for free functions and non-method items. The resolver uses this to
    /// keep bare-name calls (`obj.method()`) from binding to lifted methods
    /// of unrelated types — only qualified `Owner::method` calls do.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
}

// ── Edges ────────────────────────────────────────────────────────────────────

/// What a directed edge means.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum EdgeKind {
    /// `from` calls `to`.
    Calls,
    /// `from` uses the type/value `to`.
    Uses,
    /// `from` implements `to`.
    Implements,
    /// `from` contains `to` (module → item).
    Contains,
}

impl EdgeKind {
    /// Lower-case canonical string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Calls => "calls",
            Self::Uses => "uses",
            Self::Implements => "implements",
            Self::Contains => "contains",
        }
    }

    /// Reconstruct from a lower-case string. Unknown kinds map to `Uses`.
    #[must_use]
    pub fn parse(s: &str) -> Self {
        match s {
            "calls" => Self::Calls,
            "implements" => Self::Implements,
            "contains" => Self::Contains,
            _ => Self::Uses,
        }
    }
}

impl std::fmt::Display for EdgeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A directed edge between two atoms.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    /// Source atom.
    pub from: EntityId,
    /// Target atom.
    pub to: EntityId,
    /// Relationship kind.
    pub kind: EdgeKind,
    /// Optional role label (e.g. field name for a `Uses` edge).
    pub role: Option<String>,
    /// For a `Calls` edge: the ranks of `from`'s call sites that produced
    /// it, ascending. Empty for every other kind.
    ///
    /// The resolver collapses repeated calls to one target into a single
    /// edge, so this is a `Vec`: it is what lets a reader recover *which*
    /// sites an edge stands for, and how many. Without it the only way
    /// back to a site is matching on the name, which confuses homonyms —
    /// see [`crate::render::flow`].
    ///
    /// Left empty by [`Edge::new`] and by bundles written before the
    /// field existed; consumers must treat "empty" as "unknown", never as
    /// "no site".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sites: Vec<u16>,
}

impl Edge {
    /// Convenience constructor. Leaves [`Edge::sites`] empty.
    #[must_use]
    pub fn new(from: EntityId, to: EntityId, kind: EdgeKind) -> Self {
        Self {
            from,
            to,
            kind,
            role: None,
            sites: Vec::new(),
        }
    }

    /// A `Calls` edge that remembers the call sites it came from.
    #[must_use]
    pub fn calls_from_sites(from: EntityId, to: EntityId, sites: Vec<u16>) -> Self {
        Self {
            from,
            to,
            kind: EdgeKind::Calls,
            role: None,
            sites,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_id_roundtrip_and_display() {
        let id = EntityId::new("code:src/lib.rs::function::foo");
        assert_eq!(id.as_str(), "code:src/lib.rs::function::foo");
        assert_eq!(id.to_string(), "code:src/lib.rs::function::foo");
    }

    #[test]
    fn atom_kind_as_str_covers_all_variants() {
        assert_eq!(AtomKind::Function.as_str(), "function");
        assert_eq!(AtomKind::Module.as_str(), "module");
        assert_eq!(AtomKind::Struct.as_str(), "struct");
        assert_eq!(AtomKind::Trait.as_str(), "trait");
        assert_eq!(AtomKind::Impl.as_str(), "impl");
        assert_eq!(AtomKind::Enum.as_str(), "enum");
        assert_eq!(AtomKind::TypeAlias.as_str(), "type_alias");
        assert_eq!(AtomKind::Const.as_str(), "const");
        assert_eq!(AtomKind::Static.as_str(), "static");
        assert_eq!(AtomKind::Macro.as_str(), "macro");
        assert_eq!(AtomKind::Other("custom".to_owned()).as_str(), "custom");
    }

    #[test]
    fn atom_kind_parse_roundtrips() {
        for s in [
            "function",
            "module",
            "struct",
            "trait",
            "impl",
            "enum",
            "type_alias",
            "const",
            "static",
            "macro",
        ] {
            assert_eq!(AtomKind::parse(s).as_str(), s, "roundtrip failed for {s}");
        }
        // unknown maps to Other
        assert_eq!(AtomKind::parse("unknown").as_str(), "unknown");
    }

    #[test]
    fn atom_kind_display() {
        assert_eq!(AtomKind::Function.to_string(), "function");
        assert_eq!(AtomKind::Other("xyz".to_owned()).to_string(), "xyz");
    }

    #[test]
    fn edge_kind_as_str_and_parse_roundtrip() {
        for (s, v) in [
            ("calls", EdgeKind::Calls),
            ("uses", EdgeKind::Uses),
            ("implements", EdgeKind::Implements),
            ("contains", EdgeKind::Contains),
        ] {
            assert_eq!(v.as_str(), s);
            assert_eq!(EdgeKind::parse(s), v);
        }
        // unknown maps to Uses
        assert_eq!(EdgeKind::parse("bogus"), EdgeKind::Uses);
    }

    #[test]
    fn edge_kind_display() {
        assert_eq!(EdgeKind::Calls.to_string(), "calls");
        assert_eq!(EdgeKind::Implements.to_string(), "implements");
    }

    #[test]
    fn edge_new_sets_role_none() {
        let a = EntityId::new("a");
        let b = EntityId::new("b");
        let e = Edge::new(a.clone(), b.clone(), EdgeKind::Calls);
        assert_eq!(e.from, a);
        assert_eq!(e.to, b);
        assert_eq!(e.kind, EdgeKind::Calls);
        assert!(e.role.is_none());
    }
}
