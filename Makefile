.PHONY: fmt fmt-check lint test coverage coverage-summary coverage-gate check ci clean

# Exclude `main.rs` files of bin crates from coverage — they're 3-line wrappers
# that delegate to the core lib (which IS tested).
COVERAGE_IGNORE := 'main\.rs$$'

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

# Fail if line coverage is below 95%.
coverage-gate:
	@PCT=$$(cargo llvm-cov --all-features --workspace --ignore-filename-regex $(COVERAGE_IGNORE) --json --summary-only 2>/dev/null \
		| python3 -c 'import json,sys; print(json.load(sys.stdin)["data"][0]["totals"]["lines"]["percent"])'); \
	echo "Line coverage: $${PCT}%"; \
	python3 -c "import sys; sys.exit(0 if float('$$PCT') >= 95.0 else 1)" \
		|| { echo "FAIL: coverage $${PCT}% < 95%"; exit 1; }

check: fmt-check lint test

ci: check coverage-gate

clean:
	cargo clean
	rm -rf coverage/ lcov.info *.profraw
