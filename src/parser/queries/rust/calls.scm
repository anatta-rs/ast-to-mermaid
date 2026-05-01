; Call sites inside a Rust function body.
;
; tree-sitter-rust does NOT have a `method_call_expression` node — method
; invocations like `obj.method()` parse as a `call_expression` whose
; function field is a `field_expression`. So we capture two shapes:
;
; - `call_expression` with bare or path-qualified callee: `foo()`,
;   `module::foo()`, `crate::path::foo()`.
;
; - `field_expression` to record the trailing identifier in `obj.method`,
;   `result.unwrap`, etc. The resolver and SKIP_CALLS decide which of
;   these are interesting; we surface them all so nothing is filtered
;   away too early.

(call_expression
  function: [(identifier) (scoped_identifier)] @call.fn)

(field_expression
  field: (_) @call.field)
