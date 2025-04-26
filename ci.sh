#!/usr/bin/env bash

set -euxo pipefail

# Format check first
cargo fmt --all -- --check

# Test all crates
cargo test --all

# Run configuration file checks
cd ./source/stryvo
cargo run -p stryvo -- --config-toml ./assets/example-config.toml --validate-configs
cargo run -p stryvo -- --config-toml ./assets/test-config.toml --validate-configs
cargo run -p stryvo -- --config-kdl ./assets/test-config.kdl --validate-configs
cd ../../

# ensure the user manual can be built
cd user-manual
mdbook build
cd ../

# Docker-related checks
echo "Building Docker image..."
docker build -t stryvo:test .

# Validate Docker image works with test configuration
echo "Testing Docker image with test configuration..."
docker run --rm stryvo:test --config-toml /app/assets/test-config.toml --validate-configs
docker run --rm stryvo:test --config-kdl /app/assets/test-config.kdl --validate-configs

# Test docker-compose
echo "Testing docker-compose setup..."
docker-compose config --quiet
docker-compose up -d
docker-compose ps
docker-compose down
