.DEFAULT_GOAL := help

.PHONY: .cargo
.cargo: ## Check that cargo is installed
	@cargo --version || echo 'Please install cargo: https://github.com/rust-lang/cargo'

.PHONY: .uv
.uv: ## Check that uv is installed
	@uv --version || echo 'Please install uv: https://docs.astral.sh/uv/getting-started/installation/'

.PHONY: install-dev
install-dev: .cargo .uv ## Install dependencies and set up pre-commit hooks
	cargo build
	uvx prek install --install-hooks

.PHONY: install-cli
install-cli: .cargo ## Install the speedtest CLI
	cargo install --path . --locked

.PHONY: format-rs
format-rs: .cargo ## Format Rust code with rustfmt
	cargo fmt --all

# tracked and untracked markdown; style is in .mdformat.toml
MD_FILES := $(shell git ls-files -co --exclude-standard '*.md')
MDFORMAT := uvx --with mdformat-tables mdformat

.PHONY: format-md
format-md: .uv ## Format markdown with mdformat
	$(MDFORMAT) $(MD_FILES)

.PHONY: format
format: format-rs format-md ## Format Rust and markdown

.PHONY: check
check: .cargo ## Check Rust code with clippy
	cargo clippy --all-targets -- -D warnings

.PHONY: lint-md
lint-md: .uv ## Check markdown formatting with mdformat
	$(MDFORMAT) --check $(MD_FILES)

.PHONY: lint
lint: check lint-md ## Lint Rust and markdown

.PHONY: test
test: .cargo ## Run Rust unit tests
	cargo test

.PHONY: run
run: .cargo ## Run the speed test
	cargo run --release

# (must stay last!)
.PHONY: help
help: ## Show this help (usage: make help)
	@echo "Usage: make [recipe]"
	@echo "Recipes:"
	@awk '/^[a-zA-Z0-9_-]+:.*?##/ { \
	    helpMessage = match($$0, /## (.*)/); \
	        if (helpMessage) { \
	            recipe = $$1; \
	            sub(/:/, "", recipe); \
	            printf "  \033[36mmake %-20s\033[0m %s\n", recipe, substr($$0, RSTART + 3, RLENGTH); \
	    } \
	}' $(MAKEFILE_LIST)
