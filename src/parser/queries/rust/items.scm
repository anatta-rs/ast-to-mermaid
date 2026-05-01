; Top-level Rust items lifted to atoms in our graph.
;
; Each item is captured as `@item`; the parser dispatches on the
; captured node's kind() because each kind has different name-extraction
; logic (impl_item combines type+trait, others use the `name` field).
;
; Wrapping in `(source_file ...)` restricts matches to direct children
; of the file root — items nested inside impl bodies or function bodies
; are NOT matched here. Methods inside impl blocks are picked up
; separately by `impl_methods.scm`.

(source_file (function_item)   @item)
(source_file (struct_item)     @item)
(source_file (trait_item)      @item)
(source_file (impl_item)       @item)
(source_file (enum_item)       @item)
(source_file (type_item)       @item)
(source_file (const_item)      @item)
(source_file (static_item)     @item)
(source_file (macro_definition) @item)
