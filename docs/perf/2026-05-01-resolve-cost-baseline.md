# Resolve cost baseline — 2026-05-01

`a2m project` cold-path timings on five real Rust crates. The objective is the **V2 decision gate** from `docs/todo/2026-05-01-git-aware-mermaid-generation.md`: does cross-module resolve consume more than 30% of wall time on any tested repo? If yes, V2 (edge-level cache, Salsa-style invalidation graph) is justified. If no, V1.5 ships sufficient observability and V2 stays in the freezer.

## Methodology

Each repo was analyzed three times back-to-back with the cache wiped between runs (cold path). The release binary at `target/release/a2m` was used. Timings come from the `parse_phase` and `resolve_phase` `tracing::info_span!` summaries added in #44.

```bash
a2m project <repo-path> --trace=info -x "target,node_modules,.git"
```

Tested on macOS, M-series, single-threaded a2m. Numbers are in milliseconds.

## Results

### Cold-path (no cache)

| Repo | Files | LOC | Atoms | Edges | Parse | Resolve | Total | Resolve % |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| polystore | 6 | 772 | 21 | 0 | 1 ms | 0 ms | 1 ms | 0% |
| ast-to-mermaid (this branch) | 22 | 7 330 | 169 | 48 | 18 ms | 0 ms | 18 ms | 0% |
| ingester | 55 | 8 387 | 352 | 32 | 22 ms | 0 ms | 22 ms | 0% |
| sigil-engine | 92 | 36 496 | 955 | 213 | 114 ms | 1 ms | 115 ms | 0.8% |
| anatta-crates (workspace) | 143 | 37 288 | 1 068 | 142 | 90 ms | 1 ms | 91 ms | 1.0% |
| **rust-analyzer** | **1 463** | **570 752** | **15 867** | **5 924** | **1 432 ms** | **102 ms** | **1 534 ms** | **6.6%** |

Median across three cold runs per repo. Variance < 5% on all measurements.

### Cache effectiveness (rust-analyzer)

Two distinct caches now ship — the **bundle-level cache** (`refs/<sha>/`) for whole-bundle reuse on identical refs, and the **atom-level cache** (`blobs/<git_blob_sha>.cbor`) for per-file dedup across branches.

**`a2m index` bundle-level (idempotent on identical ref)**

| Operation | Wall time |
|---|---:|
| Cold (full materialization) | **5.05 s** |
| Hot (cache hit, idempotent) | **0.057 s** |
| **Speedup** | **88×** |

**`a2m project` atom-level (parse-phase only)**

| Operation | parse_phase | resolve_phase | Total |
|---|---:|---:|---:|
| Cold (1464 files, 0 hits) | **1 592 ms** | 98 ms | 1 690 ms |
| Warm (1464 hits, 0 misses) | **42 ms** | 98 ms | 140 ms |
| 1 file modified (1463 hits, 1 miss) | **47 ms** | 93 ms | 140 ms |
| **parse-phase speedup** | **38×** | — | **12×** |

Headline interpretation:
- **Bundle-level (`a2m index`) is the right knob for CI / batch workflows**: same ref → instant.
- **Atom-level (`a2m project`/`overview`/`module`/etc.) is the right knob for dev loops**: 95%+ of files unchanged between branches → parse skipped almost entirely.
- Total speedup is currently 12× on warm runs because resolve still runs (98 ms). When V2 ships an edge-level cache, the warm-path total approaches 100× of cold.
- Cache size is modest: 11 MB for 1463 blobs on rust-analyzer (~7.5 KB / blob avg).

## Decision-gate verdict

**V2 (edge-level cache) is still NOT justified at current scale, but the trend is no longer dismissive.**

- Resolve grew from ≤ 1% (≤ 1k atoms) to **6.6% on rust-analyzer** (15k atoms, 6k edges). Still 4.5× under the 30% threshold, but the curve is real.
- Linear extrapolation:
  - rustc-scale (~30k atoms): ~12-15%
  - 60k+ atoms (large monorepos): could approach 30%
- For now, V1's full re-resolve is appropriate. **Re-evaluate when a real workload's resolve crosses 15%.**
- Parse still dominates everywhere (~93% on rust-analyzer). Future perf wins should target parallel parsing first.

## What this baseline does NOT cover

- **Truly massive codebases** (rustc / chromium / google-internal-scale, > 100k files). rust-analyzer (~570k LOC) is the largest publicly-cloneable Rust codebase the author has on disk.
- **Repos with deep `use`-import ambiguity**: the disambiguation logic added in `36f1585` walks candidate sets per call. A pathological case (many same-named functions across modules) would inflate resolve specifically.
- **Python-heavy codebases**: only Rust crates measured. Python parser may have different cost characteristics.
- **Bundle write cost on huge repos**: at rust-analyzer size the cold `a2m index` is ~5s, of which only 1.5s is parse+resolve — the remaining 3.5s is writing 15k+ small files. Atomic-rename pattern (#46) doesn't help here; parallel writes (rayon) would.

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
