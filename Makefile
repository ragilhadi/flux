# Makefile for Flux - High-Performance Load Testing Tool
# This file provides convenient shortcuts for common development tasks

.PHONY: help dev build release test test-verbose clean fmt lint check docker-build docker-run docker-stop docker-test clean-all ci

# Default target - show help
help:
	@echo "Flux Project - Available Commands"
	@echo "=================================="
	@echo ""
	@echo "Development:"
	@echo "  make dev          - Run development build with debug logging"
	@echo "  make check        - Quick compile check without building"
	@echo ""
	@echo "Building:"
	@echo "  make build        - Build debug binary"
	@echo "  make release      - Build optimized release binary"
	@echo ""
	@echo "Testing:"
	@echo "  make test         - Run all tests"
	@echo "  make test-verbose - Run tests with output"
	@echo ""
	@echo "Code Quality:"
	@echo "  make fmt          - Format code with rustfmt"
	@echo "  make lint         - Run clippy linter"
	@echo "  make ci           - Run all CI checks (fmt + lint + test)"
	@echo ""
	@echo "Docker:"
	@echo "  make docker-build - Build Docker image"
	@echo "  make docker-run   - Run example test in Docker"
	@echo "  make docker-test  - Run sample test with Docker"
	@echo "  make docker-stop  - Stop running Docker container"
	@echo ""
	@echo "Cleanup:"
	@echo "  make clean        - Remove build artifacts"
	@echo "  make clean-all    - Deep clean including dependencies"

# Development
dev:
	@echo "Running Flux in development mode..."
	RUST_LOG=debug cargo run -- config.yaml

check:
	@echo "Running quick compile check..."
	cargo check

# Building
build:
	@echo "Building debug binary..."
	cargo build

release:
	@echo "Building release binary (optimized)..."
	cargo build --release
	@echo "Binary location: target/release/flux"

# Testing
test:
	@echo "Running tests..."
	cargo test

test-verbose:
	@echo "Running tests with output..."
	cargo test -- --nocapture --test-threads=1

# Code Quality
fmt:
	@echo "Formatting code..."
	cargo fmt

lint:
	@echo "Running clippy linter..."
	cargo clippy -- -D warnings

ci: fmt lint test
	@echo "All CI checks passed!"

# Docker
docker-build:
	@echo "Building Docker image..."
	docker build -t flux:latest .
	@echo "Image built: flux:latest"

docker-run: docker-build
	@echo "Running example test in Docker..."
	@mkdir -p data results
	docker run --rm \
	  -v $$(pwd)/samples/simple-get.yaml:/app/config.yaml \
	  -v $$(pwd)/data:/app/data \
	  -v $$(pwd)/results:/app/results \
	  flux:latest
	@echo "Results saved to ./results/"

docker-test: docker-build
	@echo "Running sample test with Docker..."
	@mkdir -p data results
	@echo "Sample file content" > data/sample.txt
	docker run --rm \
	  -v $$(pwd)/config.yaml:/app/config.yaml \
	  -v $$(pwd)/data:/app/data \
	  -v $$(pwd)/results:/app/results \
	  flux:latest
	@echo "Results saved to ./results/"

docker-stop:
	@echo "Stopping any running Flux containers..."
	docker ps -a | grep flux | awk '{print $$1}' | xargs -r docker stop
	docker ps -a | grep flux | awk '{print $$1}' | xargs -r docker rm

# Cleanup
clean:
	@echo "Cleaning build artifacts..."
	cargo clean

clean-all: clean
	@echo "Deep cleaning (including dependencies cache)..."
	rm -rf target/
	rm -rf Cargo.lock