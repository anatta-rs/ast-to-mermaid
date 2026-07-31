; Call sites inside a Dart function body.
;
; Dart's `call_expression` exposes the same `function:` + `arguments:`
; field layout as Rust's, so the shared handler in `sequence/visit.rs`
; serves it unchanged. The callee is either a bare `identifier` (`log()`)
; or a `member_expression` (`obj.method()`); the parser strips the
; receiver so only the bare name remains.
;
; `cascade_call_expression` (`obj..a()..b()`) is deliberately NOT matched
; here: it carries `property:` but no `function:` field, so it needs its
; own handler rather than silently resolving to nothing.

(call_expression function: (_) @call.fn)
