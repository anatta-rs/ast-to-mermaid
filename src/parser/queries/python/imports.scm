; Python import statements, captured wholesale and unfolded in Rust
; (`python::extract_imports`) into two maps:
;   - symbol imports (`from m import s` / `from m import s as a`) →
;     local name → `<module_last>::s` (rewrites bare calls)
;   - module imports (`import m` / `import m as x` / `from . import sub`) →
;     local alias → `<module_last>` (rewrites `alias.fn()` calls)
;
; Unfolding in Rust mirrors the Rust `flatten_use` approach: the grammar
; nests aliases / dotted names / relative prefixes in ways a flat query
; can't cleanly destructure.

(import_statement) @import
(import_from_statement) @from
