#!/bin/bash
# Example script to run Flux with a sample configuration

set -e

# Create directories if they don't exist
mkdir -p data results

# Copy sample file to data directory
cp samples/sample.txt data/ 2>/dev/null || true

# Check if config is provided
CONFIG=${1:-samples/simple-get.yaml}

if [ ! -f "$CONFIG" ]; then
    echo "❌ Configuration file not found: $CONFIG"
    echo "Usage: $0 [config-file]"
    echo "Example: $0 samples/simple-get.yaml"
    exit 1
fi

echo "⚡ Running Flux with configuration: $CONFIG"
echo ""

# Run Flux
docker run --rm \
  -v "$(pwd)/$CONFIG:/app/config.yaml" \
  -v "$(pwd)/data:/app/data" \
  -v "$(pwd)/results:/app/results" \
  flux:latest

echo ""
echo "✅ Load test complete!"
echo "📊 Reports saved to ./results/"
