; `import` / `export` directives anywhere in the file.
;
; Dart URIs come in three shapes, all handled by `dart.rs`:
;   - `dart:async`                     — SDK, no module to link
;   - `package:app/models/user.dart`   — another package
;   - `../models/user.dart`            — path-relative
;
; The optional `alias:` is the `as x` prefix, and `combinator` carries
; `show` / `hide`. `part` / `part_of` are NOT matched here: they merge a
; file into its parent library rather than importing it, so treating them
; as imports would duplicate edges. 52 of 52 `part of` occurrences in the
; reference corpus sit in generated code, which is filtered out upstream.

(library_import) @import
(library_export) @import
