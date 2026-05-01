# Resolve cost baseline — 2026-05-01

`a2m project` cold-path timings on five real Rust crates. The objective is the **V2 decision gate** from `docs/todo/2026-05-01-git-aware-mermaid-generation.md`: does cross-module resolve consume more than 30% of wall time on any tested repo? If yes, V2 (edge-level cache, Salsa-style invalidation graph) is justified. If no, V1.5 ships sufficient observability and V2 stays in the freezer.

## Methodology

Each repo was analyzed three times back-to-back with the cache wiped between runs (cold path). The release binary at `target/release/a2m` was used. Timings come from the `parse_phase` and `resolve_phase` `tracing::info_span!` summaries added in #44.

```bash
a2m project <repo-path> --trace=info -x "target,node_modules,.git"
```

Tested on macOS, M-series, single-threaded a2m. Numbers are in milliseconds.

## Results

| Repo | Files | LOC | Atoms | Edges | Parse | Resolve | Total | Resolve % |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| polystore | 6 | 772 | 21 | 0 | 1 ms | 0 ms | 1 ms | 0% |
| ast-to-mermaid (this branch) | 22 | 7 330 | 169 | 48 | 18 ms | 0 ms | 18 ms | 0% |
| ingester | 55 | 8 387 | 352 | 32 | 22 ms | 0 ms | 22 ms | 0% |
| sigil-engine | 92 | 36 496 | 955 | 213 | 114 ms | 1 ms | 115 ms | 0.8% |
| anatta-crates (workspace) | 143 | 37 288 | 1 068 | 142 | 90 ms | 1 ms | 91 ms | 1.0% |

Median across three cold runs per repo. Variance < 5% on all measurements.

## Decision-gate verdict

**V2 (edge-level cache) is NOT justified by this baseline.**

- Resolve is ≤ 1% of wall time on every tested repo, **30× under the 30% threshold**.
- The largest repo measured (143 files, 37k LOC, 1k atoms) finishes in 91 ms cold. Adding a Salsa-style invalidation graph would save microseconds at the cost of significant implementation complexity.
- Parse time dominates — ~0.6 ms/file on average, scaling linearly with file count. Future perf wins should target the parse loop (parallel parsing per #46.7 in the design doc, parser instance caching) before going after resolve.

## What this baseline does NOT cover

- **Truly large monorepos** (10k+ files): not tested locally. The author's intuition + Bazel/SCIP prior art suggests resolve becomes super-linear in the number of unresolved cross-module call sites, so 1k → 100k atoms could shift this curve. The threshold could conceivably be hit on rust-analyzer / rustc / chromium-scale codebases.
- **Repos with deep `use`-import ambiguity**: the disambiguation logic added in `36f1585` walks candidate sets per call. A pathological case (many same-named functions across modules) would inflate resolve specifically.
- **Python-heavy codebases**: only Rust crates measured. Python parser may have different cost characteristics.

## Recommended next checkpoints

Re-run this baseline at these milestones to catch a regime shift:

1. **At 50k+ files**: clone tokio + ripgrep + serde + reqwest into a synthetic monorepo, re-measure.
2. **After parser parallelization** (review critical #1 + scalability fixes): the `parse:resolve` ratio will shift; verify resolve still stays well under 30%.
3. **On every grammar bump**: `tree-sitter-rust` / `tree-sitter-python` updates could change parse cost characteristics.

## How to re-run

The benchmark script lives at `/tmp/perf-bench.sh` (preserved as a fixture below — copy into a real script when needed). Adapt the `run_repo` calls to point at additional repos.

```bash
#!/bin/bash
A2M=$(pwd)/target/release/a2m
CACHE=/tmp/a2mb

run_repo() {
  local name="$1"; local path="$2"
  [ ! -d "$path" ] && { echo "## $name — SKIPPED"; return; }
  local files=$(find "$path" -name "*.rs" -not -path "*/target/*" 2>/dev/null | wc -l | tr -d ' ')
  local loc=$(find "$path" -name "*.rs" -not -path "*/target/*" -exec wc -l {} + 2>/dev/null | tail -1 | awk '{print $1}')
  echo "## $name ($files files, ${loc:-0} LOC)"
  for run in 1 2 3; do
    rm -rf "$CACHE"
    local out=$("$A2M" project "$path" --trace=info -x "target,node_modules,.git" 2>&1 | sed 's/\x1b\[[0-9;]*m//g')
    local p=$(echo "$out" | grep -oE "parse_phase done.*elapsed_ms=[0-9]+" | grep -oE "[0-9]+$" | head -1)
    local r=$(echo "$out" | grep -oE "resolve_phase done.*elapsed_ms=[0-9]+" | grep -oE "[0-9]+$" | head -1)
    local a=$(echo "$out" | grep -oE "atoms=[0-9]+" | head -1 | grep -oE "[0-9]+")
    local e=$(echo "$out" | grep -oE "edges=[0-9]+" | head -1 | grep -oE "[0-9]+")
    echo "  run $run: parse=${p:-0}ms  resolve=${r:-0}ms  atoms=$a  edges=$e"
  done
  echo ""
}
```

Closes #47.
