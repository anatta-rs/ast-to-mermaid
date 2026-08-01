# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).


## [Unreleased]

## [0.9.1](https://github.com/anatta-rs/ast-to-mermaid/compare/v0.9.0...v0.9.1) - 2026-08-01

### Bug fixes

- *(parser)* Record every call occurrence, in source order ([#196](https://github.com/anatta-rs/ast-to-mermaid/pull/196))

## [0.9.0](https://github.com/anatta-rs/ast-to-mermaid/compare/v0.8.0...v0.9.0) - 2026-08-01

### Bug fixes

- *(dart)* Top-level functions never had their calls extracted ([#187](https://github.com/anatta-rs/ast-to-mermaid/pull/187))
- *(dart)* ClassName.method() is a qualified call, not an unknown receiver ([#185](https://github.com/anatta-rs/ast-to-mermaid/pull/185))
- *(resolve)* One pub package is one crate, not one per directory ([#183](https://github.com/anatta-rs/ast-to-mermaid/pull/183))
- *(repolith)* Select the package for cargo-llvm-cov ([#179](https://github.com/anatta-rs/ast-to-mermaid/pull/179))

### Documentation

- README status says v0.8.0 ([#177](https://github.com/anatta-rs/ast-to-mermaid/pull/177))

### Features

- *(flow)* Account for every recorded call, resolved or not ([#195](https://github.com/anatta-rs/ast-to-mermaid/pull/195))
- *(cli)* A2m flow — forward call graph annotated with order and control flow ([#193](https://github.com/anatta-rs/ast-to-mermaid/pull/193))
- *(dart)* Infer receiver types from file-scope declarations ([#189](https://github.com/anatta-rs/ast-to-mermaid/pull/189))
- *(cli)* --include-generated, and bump GRAMMAR_VERSION for Dart ([#180](https://github.com/anatta-rs/ast-to-mermaid/pull/180))

### Refactor

- *(model)* Call sites carry rank and control-flow flags ([#191](https://github.com/anatta-rs/ast-to-mermaid/pull/191))

## [0.8.0](https://github.com/anatta-rs/ast-to-mermaid/compare/v0.7.1...v0.8.0) - 2026-07-31

### Bug fixes

- *(sequence)* Classify Dart null-aware `obj?.m()` receivers ([#174](https://github.com/anatta-rs/ast-to-mermaid/pull/174))

### Features

- *(resolve)* Cross-module Dart — package layout + `as` aliases ([#175](https://github.com/anatta-rs/ast-to-mermaid/pull/175))
- *(sequence)* Dart semantics — receivers, cascades, switch, closures ([#173](https://github.com/anatta-rs/ast-to-mermaid/pull/173))
- *(dart)* Parse Dart, render its module view, filter generated code ([#171](https://github.com/anatta-rs/ast-to-mermaid/pull/171))

### Refactor

- *(sequence)* Split the walker by operation, not by language pair ([#172](https://github.com/anatta-rs/ast-to-mermaid/pull/172))

## [0.7.1](https://github.com/anatta-rs/ast-to-mermaid/compare/v0.7.0...v0.7.1) - 2026-07-25

### Bug fixes

- *(sequence)* Emit lexer-safe labels for older Mermaid parsers: `&` → `&amp;` in sequence labels (symmetry with the existing `<`/`>` escapes) and ASCII `...` instead of the Unicode ellipsis in truncated alt/loop headers and the depth-limit marker. Not reproducible with mermaid ≥ 11.16, kept as defense for embedded/older renderers ([#156](https://github.com/anatta-rs/ast-to-mermaid/issues/156))

## [0.7.0](https://github.com/anatta-rs/ast-to-mermaid/compare/v0.6.0...v0.7.0) - 2026-07-24

### Features

- *(tests)* Golden harness over on-disk fixture corpora (`tests/fixtures/mini-rust`, `mini-python`): all 7 views asserted by set equality on full node/edge/participant/counter sets, so a silently missing edge fails CI exactly like an extra one. On the pre-fix tree, 9 of its 13 tests fail — the review's whole bug surface ([#167](https://github.com/anatta-rs/ast-to-mermaid/issues/167))

### Bug fixes

- *(diff)* Node labels use `fn name (file)` instead of the raw `code:<path>::<kind>::<name>` entity id; legacy bundles without a `name` field still fall back to the id ([#166](https://github.com/anatta-rs/ast-to-mermaid/issues/166))
- *(dot)* `rankdir` emitted exactly once (the header default duplicated the per-graph directive, and preceded it stale for `BT`/`LR`/`RL` inputs) ([#166](https://github.com/anatta-rs/ast-to-mermaid/issues/166))
- *(cli)* `a2m function --help` now documents that direct callees render alongside the reverse call chain ([#166](https://github.com/anatta-rs/ast-to-mermaid/issues/166))
- *(overview)* Count impl-block and class methods in the per-module `fn` counters — modules whose functions all live in `impl` blocks reported a misleading "0 fn" ([#165](https://github.com/anatta-rs/ast-to-mermaid/issues/165))
- *(sequence)* Literal receivers (`"msg".to_string()`, `", ".join(xs)`) stay on the `self` lifeline instead of minting a participant; double quotes in labels become single quotes so truncation can no longer leave an unbalanced `"` that breaks the Mermaid parser ([#164](https://github.com/anatta-rs/ast-to-mermaid/issues/164))
- *(module)* Render intra-module call edges inside the subgraph — the help always promised "intra/cross-module calls" but only cross-module arrows were drawn ([#163](https://github.com/anatta-rs/ast-to-mermaid/issues/163))
- *(impact)* [**breaking output**] Emit forward (callee) edges alongside the backward walk — the help always promised "forward + backward, 3 hops" but only callers were rendered; header goes `graph BT` → `graph TD` ([#162](https://github.com/anatta-rs/ast-to-mermaid/issues/162))

## [0.6.0](https://github.com/anatta-rs/ast-to-mermaid/compare/v0.5.1...v0.6.0) - 2026-05-05

### Bug fixes

- *(security)* Defense-in-depth pass on symlink, blob-sha, GIT_* env, decorator loop, log injection (C42) ([#153](https://github.com/anatta-rs/ast-to-mermaid/pull/153))
- *(cache)* Dedup gc_at to_remove + partial GcReport on disk-full + tmp sweep on Cache::open (C40) ([#151](https://github.com/anatta-rs/ast-to-mermaid/pull/151))
- *(pipeline)* Error on canonicalize-strip-prefix failure + refuse prune on empty bundle (C39) ([#150](https://github.com/anatta-rs/ast-to-mermaid/pull/150))
- *(cli/format)* Strip_suffix + checked_mul in parse_size/parse_duration (C38) ([#149](https://github.com/anatta-rs/ast-to-mermaid/pull/149))
- *(cache)* Append ThreadId + counter to atomic_write tmp suffix (C26) ([#137](https://github.com/anatta-rs/ast-to-mermaid/pull/137))
- *(artifacts)* Filename collision-safe IDs on case-insensitive filesystems ([#108](https://github.com/anatta-rs/ast-to-mermaid/pull/108))
- *(parser)* Handle BOM, CR-only line endings, and non-UTF-8 git paths ([#107](https://github.com/anatta-rs/ast-to-mermaid/pull/107))
- *(graph)* Recover gracefully on poisoned RwLock in Store accessors ([#106](https://github.com/anatta-rs/ast-to-mermaid/pull/106))
- *(security)* Refuse to follow symlinks on cache writes and wipes ([#78](https://github.com/anatta-rs/ast-to-mermaid/pull/78)) ([#104](https://github.com/anatta-rs/ast-to-mermaid/pull/104))
- *(security)* Validate git refs to block flag-injection (--upload-pack=...) in --ref ([#103](https://github.com/anatta-rs/ast-to-mermaid/pull/103))
- *(render)* Unify mermaid_id and sequence::sanitize_id (single sanitizer contract) ([#102](https://github.com/anatta-rs/ast-to-mermaid/pull/102))
- *(security)* Recursion guards on AST visitors (depth limit, no stack overflow on adversarial input) ([#101](https://github.com/anatta-rs/ast-to-mermaid/pull/101))
- *(parser)* Parse_phase skips+warns on per-file failure (no halt) ([#99](https://github.com/anatta-rs/ast-to-mermaid/pull/99))

### Features

- *(cache)* Bounded GC with symlink-loop guard and high-water-mark auto-trigger ([#105](https://github.com/anatta-rs/ast-to-mermaid/pull/105))
- *(graph)* Add forward + reverse edge adjacency index to Store ([#90](https://github.com/anatta-rs/ast-to-mermaid/pull/90))

### Performance

- *(parser,cache)* Consume ParseUnit by value in apply_to + put_unit (C37) ([#148](https://github.com/anatta-rs/ast-to-mermaid/pull/148))
- *(sequence)* Parallel build_sequences + extract_all O(N) + cache max_depth + has_visible memo (C36) ([#147](https://github.com/anatta-rs/ast-to-mermaid/pull/147))
- *(render)* Snapshot atom HashMap once per render to eliminate per-child RwLock reads (C35) ([#146](https://github.com/anatta-rs/ast-to-mermaid/pull/146))
- *(artifacts)* Reuse AdjMaps across bundle phases + intern EntityIds (C34) ([#145](https://github.com/anatta-rs/ast-to-mermaid/pull/145))
- *(pipeline)* Stream parse_phase results to avoid O(N) memory peak (C33) ([#144](https://github.com/anatta-rs/ast-to-mermaid/pull/144))
- *(graph,resolve)* Borrow-not-clone APIs for Store and resolver (C28) ([#139](https://github.com/anatta-rs/ast-to-mermaid/pull/139))
- *(pipeline)* Drop bundle() input.content clone via Arc<[u8]> (C27) ([#138](https://github.com/anatta-rs/ast-to-mermaid/pull/138))
- *(cli/sequence)* Propagate v0.6.0 BatchReader + extract_all to sequence-CLI branch ([#136](https://github.com/anatta-rs/ast-to-mermaid/pull/136))
- *(pipeline)* Parallelize parse_phase per-file with rayon ([#100](https://github.com/anatta-rs/ast-to-mermaid/pull/100))
- *(sequence)* Parse each file once for build_sequences (no per-function reparse) ([#98](https://github.com/anatta-rs/ast-to-mermaid/pull/98))
- *(git)* Use git cat-file --batch over a persistent piped child ([#97](https://github.com/anatta-rs/ast-to-mermaid/pull/97))
- *(graph)* Predecessor-map BFS for reverse_call_paths (no path cloning) ([#96](https://github.com/anatta-rs/ast-to-mermaid/pull/96))
- *(artifacts)* Build adjacency maps once for emit_artifacts + entity_meta ([#94](https://github.com/anatta-rs/ast-to-mermaid/pull/94))
- *(render/overview)* Switch to forward edge adjacency ([#93](https://github.com/anatta-rs/ast-to-mermaid/pull/93))
- *(render)* Switch project loop to forward edge adjacency ([#92](https://github.com/anatta-rs/ast-to-mermaid/pull/92))

### Refactor

- *(cli/sequence)* Funnel sequence_filename through artifacts::filename_id (C41) ([#152](https://github.com/anatta-rs/ast-to-mermaid/pull/152))
- *(pipeline)* Expose language_for and DRY ext->Language mapping (C32) ([#143](https://github.com/anatta-rs/ast-to-mermaid/pull/143))
- *(parser)* Move MAX_AST_DEPTH out of sequence/ to break parser→sequence import (C31) ([#142](https://github.com/anatta-rs/ast-to-mermaid/pull/142))
- *(render)* Collapse 3 ID/label sanitizers into render::util ([#141](https://github.com/anatta-rs/ast-to-mermaid/pull/141))
- *(cli)* Split cli/run.rs (1451L) into per-subcommand modules (C29) ([#140](https://github.com/anatta-rs/ast-to-mermaid/pull/140))
- Remove dead code (deserialize_out_edges, parser PartialEq, capture_index, parse_into) ([#114](https://github.com/anatta-rs/ast-to-mermaid/pull/114))
- *(parser)* Split parser/mod.rs per language (rust, python, typescript) ([#112](https://github.com/anatta-rs/ast-to-mermaid/pull/112))
- *(cli)* Split cli_support.rs into cli/{flags,run,format} ([#111](https://github.com/anatta-rs/ast-to-mermaid/pull/111))
- *(cli)* DRY the CSV exclude parser across 4 subcommands ([#110](https://github.com/anatta-rs/ast-to-mermaid/pull/110))
- *(error)* Unify parse_size and parse_duration on AstToMermaidError ([#109](https://github.com/anatta-rs/ast-to-mermaid/pull/109))

<!--
This section is regenerated by release-plz from conventional commit
messages on the next release PR. Do not hand-curate entries here —
they will be overwritten. See CONTRIBUTING.md § "Changelog convention".
-->

## [0.5.1](https://github.com/anatta-rs/ast-to-mermaid/compare/v0.5.0...v0.5.1) - 2026-05-04

### Features

- *(bundle)* Reconcile-on-write in write_artifacts ([#60](https://github.com/anatta-rs/ast-to-mermaid/pull/60))

## [0.5.0](https://github.com/anatta-rs/ast-to-mermaid/compare/v0.4.2...v0.5.0) - 2026-05-02

### Features

- *(bundle)* Emit per-function sequenceDiagram as 5th layer (--with-sequences) ([#58](https://github.com/anatta-rs/ast-to-mermaid/pull/58))

## [0.4.2](https://github.com/anatta-rs/ast-to-mermaid/compare/v0.4.1...v0.4.2) - 2026-05-02

### Features

- *(sequence)* A2m sequence — Mermaid sequenceDiagram from one fn body ([#56](https://github.com/anatta-rs/ast-to-mermaid/pull/56))

## [0.4.1](https://github.com/anatta-rs/ast-to-mermaid/compare/v0.4.0...v0.4.1) - 2026-05-02

### Tests

- *(cli_support)* Cover walk-ref / index / diff / gc / resolve paths ([#55](https://github.com/anatta-rs/ast-to-mermaid/pull/55))

### Rf

- Add internal ignored artifacts

## [0.4.0](https://github.com/anatta-rs/ast-to-mermaid/compare/v0.3.0...v0.4.0) - 2026-05-02

### Bug fixes

- *(resolve)* Eliminate ghost cross-module edges + isolate git_source tests ([#52](https://github.com/anatta-rs/ast-to-mermaid/pull/52))

## [0.3.0](https://github.com/anatta-rs/ast-to-mermaid/compare/v0.2.0...v0.3.0) - 2026-05-02

### Features

- --format=dot for graphs too big for Mermaid ([#51](https://github.com/anatta-rs/ast-to-mermaid/pull/51))

## [0.2.0](https://github.com/anatta-rs/ast-to-mermaid/compare/v0.1.0...v0.2.0) - 2026-05-01

### Features

- Git-aware mermaid generation (V1+V1.5, content-addressed cache, diff) ([#48](https://github.com/anatta-rs/ast-to-mermaid/pull/48))

## [0.1.0](https://github.com/anatta-rs/ast-to-mermaid/releases/tag/v0.1.0) - 2026-05-01

### Bug fixes

- *(render)* Escape Mermaid reserved keywords in node ids ([#35](https://github.com/anatta-rs/ast-to-mermaid/pull/35))
- Publish-readiness — review blockers, default ignores, release-plz unblock ([#30](https://github.com/anatta-rs/ast-to-mermaid/pull/30))
- *(clippy)* Backtick `node_modules` and friends in a2m-bundle doc ([#27](https://github.com/anatta-rs/ast-to-mermaid/pull/27))
- *(resolve)* Disambiguate cross-module calls via use imports + qualified paths ([#24](https://github.com/anatta-rs/ast-to-mermaid/pull/24))

### Documentation

- Refresh README + regenerate stale docs/ diagrams ([#36](https://github.com/anatta-rs/ast-to-mermaid/pull/36))
- Showcase real diagrams in README + tighten the deps pitch ([#28](https://github.com/anatta-rs/ast-to-mermaid/pull/28))
- Rewrite README to match current shape ([#26](https://github.com/anatta-rs/ast-to-mermaid/pull/26))
- Rewrite README — accurate capabilities (5 levels, 6 bins, no MCP) ([#18](https://github.com/anatta-rs/ast-to-mermaid/pull/18))
- Add architecture.mmd + overview.mmd (self-bootstrap) ([#13](https://github.com/anatta-rs/ast-to-mermaid/pull/13))

### Features

- *(render)* Type::method target shorthand + drill into impl methods ([#33](https://github.com/anatta-rs/ast-to-mermaid/pull/33))
- *(parser+resolve)* Impl methods, Implements edges, extern atoms, robust use extractor ([#32](https://github.com/anatta-rs/ast-to-mermaid/pull/32))
- Collapse 7 binaries into one `a2m` with subcommands; prep crates.io ([#29](https://github.com/anatta-rs/ast-to-mermaid/pull/29))
- Add a2m-bundle binary ([#25](https://github.com/anatta-rs/ast-to-mermaid/pull/25))
- Tour 2 — drop graph deps + emit 4-layer artifacts ([#21](https://github.com/anatta-rs/ast-to-mermaid/pull/21))
- *(cli)* Split ast-to-mermaid-cli into 6 verb-named binaries ([#17](https://github.com/anatta-rs/ast-to-mermaid/pull/17))
- *(cli)* --exclude flag to skip extra directories during walk ([#15](https://github.com/anatta-rs/ast-to-mermaid/pull/15))
- *(render)* Module / function / impact zoom levels ([#14](https://github.com/anatta-rs/ast-to-mermaid/pull/14))
- *(cli)* Analyze command — closes #5 ([#12](https://github.com/anatta-rs/ast-to-mermaid/pull/12))
- *(render)* Mermaid renderers (project + overview levels) — closes #3 ([#11](https://github.com/anatta-rs/ast-to-mermaid/pull/11))
- *(resolve)* Cross-module call resolver (closes #4) ([#10](https://github.com/anatta-rs/ast-to-mermaid/pull/10))
- *(store)* Add InMemoryStore impl GraphStore<Atom, Relation> ([#8](https://github.com/anatta-rs/ast-to-mermaid/pull/8))
- Initial scaffold — workspace + 3 crates + CI ≥95% coverage ([#1](https://github.com/anatta-rs/ast-to-mermaid/pull/1))

### Performance

- *(render)* Use polystore bulk methods to collapse N+1 round-trips ([#16](https://github.com/anatta-rs/ast-to-mermaid/pull/16))

### Refactor

- *(parser)* Drive item/method/call/use extraction from tree-sitter queries ([#34](https://github.com/anatta-rs/ast-to-mermaid/pull/34))
- Flatten ast-to-mermaid — merge core+cli into single crate, drop MCP ([#20](https://github.com/anatta-rs/ast-to-mermaid/pull/20))
