DEDUP_BIN := dedup/target/release/dedup

.PHONY: build test pipeline

build:
	cd dedup && cargo build --release --quiet

test: build
	python3 normalizer/tests/test_normalize.py
	bash dedup/tests/cli_test.sh
	bash report/tests/test_report.sh

# Usage: make pipeline RAW=<dir with *.csv> OUT=<output dir>
pipeline: build
	@test -n "$(RAW)" || { echo "set RAW=<dir of csv files>" >&2; exit 2; }
	@test -n "$(OUT)" || { echo "set OUT=<output dir>" >&2; exit 2; }
	mkdir -p $(OUT)/normalized
	set -e; for f in $(RAW)/*.csv; do \
		base=$$(basename $$f .csv); \
		python3 normalizer/normalize.py $$f $(OUT)/normalized/$$base.jsonl; \
	done
	$(DEDUP_BIN) $(OUT)/normalized/*.jsonl > $(OUT)/merged.jsonl
	bash report/report.sh $(OUT)/merged.jsonl > $(OUT)/report.json
