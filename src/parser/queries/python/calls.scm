; Call sites inside a Python function body.
;
; `call` matches both `foo()` and `obj.method()`. The parser strips any
; receiver (everything before the last `.`) so only the bare callee
; name remains.

(call function: (_) @call.fn)
