# Git hooks

Mirror the CI checks locally so you don't push broken code.

## Install (one time, after clone)

```bash
make hooks
```

This sets `core.hooksPath` to `.githooks/` and chmods the scripts.

## What runs

| Hook | Command | Speed |
|---|---|---|
| `pre-commit` | `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test --workspace` | ~5–15 s |
| `pre-push` | `make ci` (above + coverage gate ≥ 95%) | ~30–60 s |

## Bypass

If you absolutely need to skip:

```bash
git commit --no-verify    # skip pre-commit
git push --no-verify      # skip pre-push
```

CI is the source of truth — bypassing locally only delays the failure to the PR.
