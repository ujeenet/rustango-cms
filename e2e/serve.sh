#!/usr/bin/env bash
# Boot the cms_demo example on sqlite for the Playwright E2E suite.
#
# Fresh state on every run: the registry + tenant DBs and the media dir
# live under e2e/.state (gitignored, wiped here), so specs can assume an
# empty library and exactly one tenant/user:
#
#   tenant  demo   → http://demo.localhost:${E2E_PORT}
#   user    admin  / TestPw123!  (superuser)
#
# Invoked by playwright.config.ts's webServer; run manually for debugging:
#   bash e2e/serve.sh
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
STATE="$REPO/e2e/.state"
PORT="${E2E_PORT:-8210}"
ARGS=(--example cms_demo --no-default-features --features sqlite)

cd "$REPO"
export PATH="$HOME/.cargo/bin:$PATH"

echo "[e2e] building cms_demo…" >&2
cargo build "${ARGS[@]}" >&2

BIN="$REPO/target/debug/examples/cms_demo"
rm -rf "$STATE"
mkdir -p "$STATE"
cd "$STATE"

export DATABASE_URL="sqlite:$STATE/registry.db?mode=rwc"

echo "[e2e] provisioning tenant + user…" >&2
"$BIN" migrate-registry >&2
"$BIN" create-tenant demo \
    --mode database \
    --database-url "sqlite:$STATE/demo.db?mode=rwc" \
    --host-pattern demo.localhost >&2
"$BIN" create-user demo admin --password 'TestPw123!' --superuser >&2
"$BIN" migrate-tenants >&2

echo "[e2e] serving on 127.0.0.1:$PORT (Host demo.localhost)" >&2
export RUSTANGO_BIND="127.0.0.1:$PORT"
export RUSTANGO_APEX_DOMAIN=localhost
# Preview tokens are HMAC-signed and disabled outright without a key, so
# the headless-preview specs need one. Fixed rather than random: a token
# minted before a restart should still verify after it.
export RCMS_SECRET_KEY="${RCMS_SECRET_KEY:-e2e-preview-signing-key-not-a-secret}"
export RUST_LOG=warn
exec "$BIN" runserver
