.PHONY: build run test clean dev migrate

# Build the proxy
build:
	cargo build --release

# Run in development mode
dev:
	RUST_LOG=marlobu_proxy=debug,info cargo run

# Run release build
run:
	./target/release/marlobu-proxy

# Run tests
test:
	cargo test

# Clean build artifacts
clean:
	cargo clean

# Format code
fmt:
	cargo fmt

# Lint code
lint:
	cargo clippy -- -D warnings

# Run database migrations
migrate:
	@echo "Running migrations..."
	psql $(DATABASE_URL) -f migrations/001_marlobu_schema.sql

# Run with sample data
migrate-with-sample:
	@echo "Running migrations with sample data..."
	@sed 's/^\/\*$$//' migrations/001_marlobu_schema.sql | sed 's/^\*\/$$//' | psql $(DATABASE_URL)

# Start local Postgres (for development)
postgres:
	docker run -d --name marlobu-postgres \
		-e POSTGRES_PASSWORD=postgres \
		-e POSTGRES_DB=marlobu \
		-p 5432:5432 \
		postgres:16-alpine

# Stop local Postgres
postgres-stop:
	docker stop marlobu-postgres && docker rm marlobu-postgres

# Check if everything compiles
check:
	cargo check

# Build documentation
doc:
	cargo doc --open

# Full CI check
ci: fmt lint test build
	@echo "CI checks passed!"
