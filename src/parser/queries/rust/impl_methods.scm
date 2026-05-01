; Methods inside an `impl` block.
;
; Captures each `function_item` whose direct parent is the impl's
; `declaration_list` body. Used to lift methods to first-class function
; atoms with id `code:{file}::function::{impl_owner}::{method_name}`.

(impl_item
  body: (declaration_list
    (function_item) @method))
