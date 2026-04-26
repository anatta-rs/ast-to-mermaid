# Contributing to ast-to-mermaid

## Commit messages — Conventional Commits

This project uses [Conventional Commits](https://www.conventionalcommits.org)
so that [release-plz](https://release-plz.dev) can drive versioning automatically.

| Prefix | Effect on version (pre-1.0) | Notes |
|---|---|---|
| `feat:` | patch bump | new feature |
| `feat!:` or `BREAKING CHANGE:` in body | **minor** bump | breaking change |
| `fix:` | patch bump | bug fix |
| `perf:` / `refactor:` | patch bump | included in CHANGELOG |
| `docs:` / `test:` | patch bump | included in CHANGELOG |
| `chore:` / `style:` / `ci:` | **no bump** | excluded from CHANGELOG |
| `chore(deps):` | patch bump | dependency updates appear in CHANGELOG |

After we hit `1.0`, `feat!` will bump major and `feat` will bump minor —
standard semver.

## Release flow (automated)

1. Open a PR against `main`. Use a conventional commit message.
2. CI runs (`fmt`, `clippy`, `test`, `coverage ≥ 95%`).
3. Merge the PR (squash-merge, the commit subject becomes the conventional
   commit consumed by release-plz).
4. On every push to `main`, the `Release-plz` workflow opens (or updates) a
   **Release PR** that bumps `Cargo.toml` and updates `CHANGELOG.md`.
5. Merging the Release PR creates a git tag (`vX.Y.Z`) and a GitHub Release.

You never edit `Cargo.toml.version` or `CHANGELOG.md` by hand.

## Local checks

```bash
make check          # fmt + clippy + test
make coverage       # HTML report under coverage/html/
make coverage-gate  # fails if line coverage < 95%
make ci             # everything
```

## Local git hooks

Mirror CI checks before they hit the server:

```bash
make hooks    # one-time setup after clone
```

This sets `core.hooksPath` to `.githooks/`, installing:
- **pre-commit**: `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test --workspace` (~5–15 s)
- **pre-push**: `make ci` (above + coverage gate ≥ 95%) (~30–60 s)

Bypass with `git commit --no-verify` / `git push --no-verify` only if absolutely needed.

## Branch policy

- `main` is always shippable. CI must be green.
- Feature work in `feat/<short-name>` branches.
- Direct commits to `main` are forbidden (use PRs).

## Dependency on polystore

This workspace depends on [polystore](https://github.com/anatta-rs/polystore)
via `branch = "main"` (declared in workspace `Cargo.toml`). Cargo resolves the
latest commit each time we update `Cargo.lock` locally; downstream consumers
that vendor `ast-to-mermaid` will pin via the lockfile they create.

When polystore ships a breaking change, we update accordingly here and bump
`ast-to-mermaid` minor (pre-1.0).

## License

By contributing you agree that your contributions are licensed under
[Apache-2.0](./LICENSE).
