# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).


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
