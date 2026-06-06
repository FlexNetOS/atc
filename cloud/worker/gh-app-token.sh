#!/usr/bin/env bash
#
# Mint a short-lived GitHub App installation token ([[tasks/harmony-844]]).
#
# No PAT is baked into the image: the worker holds only the GitHub App private
# key (as a Fly secret) and exchanges a 10-minute JWT for an installation token
# scoped to the target repo. Prints the token on stdout.
#
# Required env (Fly app secrets):
#   GH_APP_ID                 GitHub App id (numeric)
#   GH_APP_INSTALLATION_ID    Installation id on the gitkb org
#   GH_APP_PRIVATE_KEY        PEM private key (full contents)
set -euo pipefail

: "${GH_APP_ID:?missing GH_APP_ID}"
: "${GH_APP_INSTALLATION_ID:?missing GH_APP_INSTALLATION_ID}"
: "${GH_APP_PRIVATE_KEY:?missing GH_APP_PRIVATE_KEY}"

b64url() { openssl base64 -A | tr '+/' '-_' | tr -d '='; }

now=$(date +%s)
iat=$((now - 60))      # allow for clock skew
exp=$((now + 540))     # 9 minutes (max 10)

header='{"alg":"RS256","typ":"JWT"}'
payload="{\"iat\":${iat},\"exp\":${exp},\"iss\":\"${GH_APP_ID}\"}"

header_b64=$(printf '%s' "$header" | b64url)
payload_b64=$(printf '%s' "$payload" | b64url)
unsigned="${header_b64}.${payload_b64}"

key_file=$(mktemp)
trap 'rm -f "$key_file"' EXIT
printf '%s' "$GH_APP_PRIVATE_KEY" > "$key_file"

signature=$(printf '%s' "$unsigned" \
  | openssl dgst -sha256 -sign "$key_file" \
  | b64url)
jwt="${unsigned}.${signature}"

# Exchange the JWT for an installation token.
curl -fsS -X POST \
  -H "Authorization: Bearer ${jwt}" \
  -H "Accept: application/vnd.github+json" \
  "https://api.github.com/app/installations/${GH_APP_INSTALLATION_ID}/access_tokens" \
  | jq -r '.token'
