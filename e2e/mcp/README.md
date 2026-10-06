# MCP full-scale smoke test

Exercises every CMS MCP tool end-to-end against a running `cms_demo`
tenant, using a raw `prefix.secret` key directly as the Bearer token
(the copy-paste path). 30 checks: discovery, fail-closed scope, page
create at scale with nested stream bodies, child placement + validation,
search/read-back, update/publish, translations (apply/delete/default-
reject), media upload + dedupe, snippets, collections.

## Run

```bash
# 1. Provision a demo tenant with a few locales:
cd e2e/.state   # or any scratch dir
export DATABASE_URL="sqlite:$PWD/registry.db?mode=rwc"
BIN=../../target/debug/examples/cms_demo
$BIN migrate-registry
$BIN create-tenant demo --mode database \
    --database-url "sqlite:$PWD/demo.db?mode=rwc" --host-pattern demo.localhost
$BIN create-user demo admin --password 'TestPw123!' --superuser
$BIN migrate-tenants
# optional extra locales for the translation checks:
python3 - <<'PY'
import sqlite3; c=sqlite3.connect("demo.db")
for code,name,o in [("fr","Français",1),("es","Español",2),("de","Deutsch",3)]:
    c.execute("insert into cms_locale (code,name,is_default,active,sort_order,created_at)"
              " values (?,?,0,1,?,datetime('now'))",(code,name,o)); c.commit()
PY

# 2. Boot the server (a stable session secret so keys survive), which
#    seeds the MCP skills:
export RUSTANGO_SESSION_SECRET="demo-mcp-secret-at-least-32-bytes-long!!"
RUSTANGO_BIND=127.0.0.1:8210 RUSTANGO_APEX_DOMAIN=localhost $BIN runserver &

# 3. Mint the demo keys the smoke test reads:
$BIN create-user-key demo admin --label "Demo full-scale key" \
    | grep -oE '[0-9a-f]{8}\.[0-9a-f]{32}' > /tmp/mcp_full_key
$BIN create-user-key demo admin --label "Demo read-only key" --skill cms-read \
    | grep -oE '[0-9a-f]{8}\.[0-9a-f]{32}' > /tmp/mcp_read_key

# 4. Run it:
python3 ../mcp/fullscale_smoke.py
```
