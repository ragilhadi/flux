#!/bin/bash
# Build script for Flux Docker image

set -e

echo "🔨 Building Flux Docker image..."
docker build -t flux:latest .

echo "✅ Build complete!"
echo ""
echo "To run Flux, use:"
echo "  docker run --rm \\"
echo "    -v \$(pwd)/config.yaml:/app/config.yaml \\"
echo "    -v \$(pwd)/data:/app/data \\"
echo "    -v \$(pwd)/results:/app/results \\"
echo "    flux:latest"
