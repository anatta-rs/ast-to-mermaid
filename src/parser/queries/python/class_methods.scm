; Methods inside a `class` body.
;
; Captures each `function_definition` (or `decorated_definition`) whose
; direct parent is the class's `block` body. Used to lift methods to
; first-class function atoms with id
; `code:{file}::function::{class_name}::{method_name}` and
; `parent = Some(class_name)` so the resolver can match qualified
; `Class::method` calls without colliding with same-named methods of
; other classes.

(class_definition
  body: (block
    (function_definition) @method))

(class_definition
  body: (block
    (decorated_definition) @method))
