; Rust items lifted to atoms in our graph.
;
; Each item is captured as `@item`; the parser dispatches on the
; captured node's kind() because each kind has different name-extraction
; logic (impl_item combines type+trait, others use the `name` field).
;
; The patterns are not anchored to `source_file`: items declared inside
; an inline `mod x { … }` are atoms too. The parser keeps a capture only
; when every enclosing node up to the file root is a `mod_item` (or an
; `extern` block), which records the inline module path as the atom's
; scope and drops what the graph does not model — functions nested in a
; function body, and methods, which `impl_methods.scm` and
; `trait_methods.scm` pick up separately.

(function_item)    @item
(struct_item)      @item
(union_item)       @item
(trait_item)       @item
(impl_item)        @item
(enum_item)        @item
(type_item)        @item
(const_item)       @item
(static_item)      @item
(macro_definition) @item

; Functions declared in an `extern "C" { … }` block have no body.
(foreign_mod_item
  body: (declaration_list (function_signature_item) @item))
