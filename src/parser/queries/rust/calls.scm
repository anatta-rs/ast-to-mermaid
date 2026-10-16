; Call sites inside a Rust function body.
;
; tree-sitter-rust does NOT have a `method_call_expression` node — method
; invocations like `obj.method()` parse as a `call_expression` whose
; function field is a `field_expression`. So we capture two shapes:
;
; - `call_expression` with bare or path-qualified callee: `foo()`,
;   `module::foo()`, `crate::path::foo()`.
;
; - `call_expression` whose callee is a `field_expression`, recording the
;   method name in `obj.method()` (also behind a turbofish,
;   `obj.method::<T>()`). Plain field reads (`self.inner` in
;   `self.inner.run()`) are not calls and are not captured; the parser
;   reads the receiver off the `field_expression` to type the call.

(call_expression
  function: [(identifier) (scoped_identifier)] @call.fn)

(call_expression
  function: (field_expression
    field: (_) @call.field))

(call_expression
  function: (generic_function
    function: (field_expression
      field: (_) @call.field)))
