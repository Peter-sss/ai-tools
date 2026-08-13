#!/usr/bin/env bash
# Upload local Tauri updater signing secrets to Peter-sss/ai-tools.
# Requires: gh auth login  (or GH_TOKEN)
set -euo pipefail

REPO="${1:-Peter-sss/ai-tools}"
KEY_PATH="${TAURI_SIGNING_PRIVATE_KEY_FILE:-$HOME/.tauri/ai-tools.key}"
PASSWORD="${TAURI_SIGNING_PRIVATE_KEY_PASSWORD:-ai-tools-release}"

if [[ ! -f "$KEY_PATH" ]]; then
  echo "Missing private key file: $KEY_PATH" >&2
  echo "Generate with: npx tauri signer generate -w \"$HOME/.tauri/ai-tools.key\" -p '...' --ci" >&2
  exit 1
fi

if ! command -v gh >/dev/null 2>&1; then
  echo "GitHub CLI (gh) is required." >&2
  exit 1
fi

gh auth status >/dev/null

gh secret set TAURI_SIGNING_PRIVATE_KEY -R "$REPO" < "$KEY_PATH"
gh secret set TAURI_SIGNING_PRIVATE_KEY_PASSWORD -R "$REPO" --body "$PASSWORD"
echo "Uploaded TAURI_SIGNING_PRIVATE_KEY and TAURI_SIGNING_PRIVATE_KEY_PASSWORD to $REPO"
gh secret list -R "$REPO"
