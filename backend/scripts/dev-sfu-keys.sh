#!/usr/bin/env bash
# Make the SFU token key pair for the local stack.
#
# Nakama signs SFU tokens with the private key (SFU_JWT_PRIVATE_KEY in .env).
# The local SFU (the compose in mello-sfu) checks them with the public key.
# The files are backend/sfu_private.pem and backend/sfu_public.pem (both in
# .gitignore). An existing pair is kept. The public key is copied to
# mello-sfu/local/jwt_pub.pem, which the mello-sfu compose mounts.
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

sfu_dir="${MELLO_SFU_DIR:-../../mello-sfu}"
if [[ -d "$sfu_dir" ]]; then
	mkdir -p "$sfu_dir/local"
	cp sfu_public.pem "$sfu_dir/local/jwt_pub.pem"
	echo "Copied it to $sfu_dir/local/jwt_pub.pem"
else
	echo "No mello-sfu checkout at $sfu_dir. Copy sfu_public.pem to mello-sfu/local/jwt_pub.pem."
fi

# One .env line: compose turns \n in a double-quoted value into a newline.
key_line=$(tr -d '\r' <sfu_private.pem | sed -e ':a' -e 'N' -e '$!ba' -e 's/\n/\\n/g')
cat <<MSG

Put these lines in backend/.env (replace the empty ones):

SFU_JWT_PRIVATE_KEY="${key_line}"
SFU_ENDPOINT_EU=ws://localhost:8443/ws

Then start the backend, and the local SFU from the mello-sfu compose:

docker compose up -d
(cd "$sfu_dir" && docker compose up -d --build)
MSG
