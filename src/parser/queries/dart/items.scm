; Top-level Dart items lifted to atoms.
;
; Unlike Rust (`impl`) and Python (`class`), Dart has four distinct
; container kinds — `class`, `mixin`, `extension`, `enum` — all of which
; hold methods. They are captured here and grouped by `render/module.rs`.
;
; `function_declaration` is a top-level function: it wraps `signature:`
; and `body:`, so the whole declaration is the atom (a bare
; `function_signature` is an abstract member, handled in class_methods).

(source_file (class_declaration)     @item)
(source_file (mixin_declaration)     @item)
(source_file (extension_declaration) @item)
(source_file (enum_declaration)      @item)
(source_file (function_declaration)  @item)
(source_file (type_alias)            @item)
