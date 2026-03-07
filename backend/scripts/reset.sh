#!/bin/bash
# Reset backend (delete all data)
cd "$(dirname "$0")/.."
docker compose down -v
docker compose up -d
echo "Backend reset complete!"
