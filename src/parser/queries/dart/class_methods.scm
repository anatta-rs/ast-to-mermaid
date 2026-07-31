; Methods inside a Dart container body.
;
; 90% of Dart functions are methods (9585 `method_declaration` against
; 1057 `function_declaration` over a 756-file corpus), so this is the main
; path, not a secondary case.
;
; `method_declaration` wraps `method_signature` + `function_body` and is
; the concrete method. A bare `function_signature` under `declaration` is
; an abstract member (interface / abstract class) — captured too, so that
; abstract contracts still show up as atoms.
;
; All four container kinds are covered; `render/module.rs` groups the
; captured methods under their owner.

(class_declaration
  body: (class_body
    (class_member (method_declaration) @method)))

(class_declaration
  body: (class_body
    (class_member (declaration (function_signature) @method))))

(mixin_declaration
  (class_body
    (class_member (method_declaration) @method)))

(extension_declaration
  (extension_body
    (class_member (method_declaration) @method)))

(enum_declaration
  (enum_body
    (class_member (method_declaration) @method)))
