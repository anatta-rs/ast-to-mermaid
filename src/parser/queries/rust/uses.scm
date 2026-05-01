; Every Rust `use` declaration reachable from the file root, including
; ones nested inside inline `mod { … }` blocks.
;
; The parser's `flatten_use` then recurses into each declaration's
; argument to handle group forms (`use a::{b, c}`), aliases
; (`use a as b`), and wildcards (`use a::*`). That recursion is
; structural over arbitrary nesting depths and doesn't simplify under
; a flat query — the value of using a query here is just removing the
; manual tree walk that finds the use_declaration nodes.

(use_declaration) @use
