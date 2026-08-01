.PHONY: fmt fmt-check lint test coverage coverage-summary coverage-gate check ci clean hooks

# Coverage exclusions:
#  - main.rs / src/bin/*.rs : thin clap-parse + delegate-to-lib wrappers.
#                             The lib IS tested.
#  - sibling-repo paths     : when [patch] points at workspaces/ingester or
#                             workspaces/polystore in local dev, those files
#                             show up in the report and drag the gate down.
#                             They have their own coverage gates in their own
#                             repos. CI doesn't have those paths, so this is
#                             purely a local-dev safeguard.
COVERAGE_IGNORE := '(main|bin/[^/]+)\.rs$$|workspaces/(ingester|polystore|dork|pm)/'

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all --check

lint:
	cargo clippy --all-targets --all-features -- -D warnings

test:
	cargo test --all-features --workspace

coverage:
	cargo llvm-cov --all-features --workspace --ignore-filename-regex $(COVERAGE_IGNORE) --html --output-dir coverage/
	@echo "→ coverage/html/index.html"

coverage-summary:
	cargo llvm-cov --all-features --workspace --ignore-filename-regex $(COVERAGE_IGNORE) --summary-only

# Fail if line coverage is below 90% — the same threshold CI enforces, so
# a green local run means a green CI run. Was 95%, which no longer held
# after the v0.2 git-aware modules landed: the gate failed on every run
# and became noise developers learned to skip.
coverage-gate:
	@PCT=$$(cargo llvm-cov --all-features --workspace --ignore-filename-regex $(COVERAGE_IGNORE) --json --summary-only 2>/dev/null \
		| python3 -c 'import json,sys; print(json.load(sys.stdin)["data"][0]["totals"]["lines"]["percent"])'); \
	echo "Line coverage: $${PCT}%"; \
	python3 -c "import sys; sys.exit(0 if float('$$PCT') >= 90.0 else 1)" \
		|| { echo "FAIL: coverage $${PCT}% < 90%"; exit 1; }

check: fmt-check lint test

ci: check coverage-gate

clean:
	cargo clean
	rm -rf coverage/ lcov.info *.profraw

# Install local git hooks (pre-commit + pre-push). Run once after clone.
hooks:
	git config --local core.hooksPath .githooks
	chmod +x .githooks/pre-commit .githooks/pre-push
	@echo "✓ git hooks installed → .githooks/"
	@echo "  pre-commit: fmt + clippy + test"
	@echo "  pre-push:   make ci (+ coverage gate)"
