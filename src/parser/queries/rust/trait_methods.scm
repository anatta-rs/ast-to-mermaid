; Methods declared inside a `trait` block: required ones
; (`fn f(&self);`, a `function_signature_item`) and provided ones with a
; default body (`function_item`).
;
; Lifted like impl methods, with the trait as owner
; (`code:{file}::function::{Trait}::{method}`), so that a call through a
; trait object or a trait bound (`s: &dyn Store` → `s.get()`) has a
; method atom to resolve to.

(trait_item
  body: (declaration_list
    [(function_item) (function_signature_item)] @method))
