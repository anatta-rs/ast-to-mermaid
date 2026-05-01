; Top-level Python items lifted to atoms.
;
; Decorated definitions (`@decorator\ndef foo():`) are captured as a
; whole; the parser unwraps the inner definition.

(module (function_definition)  @item)
(module (class_definition)     @item)
(module (decorated_definition) @item)
