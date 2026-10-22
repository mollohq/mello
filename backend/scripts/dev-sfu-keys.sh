#!/usr/bin/env bash
# Make the SFU token key pair for the local stack.
#
# Nakama signs SFU tokens with the private key (SFU_JWT_PRIVATE_KEY in .env).
# The local SFU (docker compose --profile sfu) checks them with the public key.
# The files are backend/sfu_private.pem and backend/sfu_public.pem (both in
# .gitignore). An existing pair is kept.
#
# Usage: ./scripts/dev-sfu-keys.sh
set -euo pipefail
cd "$(dirname "$0")/.."

if [[ ! -f sfu_private.pem ]]; then
	openssl genrsa -out sfu_private.pem 2048 2>/dev/null
	echo "Made sfu_private.pem"
fi
openssl rsa -in sfu_private.pem -pubout -out sfu_public.pem 2>/dev/null
echo "Wrote sfu_public.pem"

# One .env line: compose turns \n in a double-quoted value into a newline.
key_line=$(tr -d '\r' <sfu_private.pem | sed -e ':a' -e 'N' -e '$!ba' -e 's/\n/\\n/g')
cat <<MSG

Put these lines in backend/.env (replace the empty ones):

SFU_JWT_PRIVATE_KEY="${key_line}"
SFU_ENDPOINT_EU=ws://localhost:8443/ws

Then start the stack with the local SFU:

docker compose --profile sfu up -d --build
MSG
