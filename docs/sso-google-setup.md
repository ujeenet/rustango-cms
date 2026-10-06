# Setting up Google as an SSO provider

## How it works here (read first)

- SSO is **per-tenant (per-`Org`)** and served by the framework's tenancy admin
  at `/login/sso` (start) and `/login/sso/callback` (finish).
- It's **link-to-existing only**: Google returns a verified email, and it must
  already match a `rustango_users.email` on that tenant. SSO **never
  auto-provisions** a user — create the admin user first with the same email
  they'll use at Google.
- The redirect URI is derived per-host as `{scheme}://{host}/login/sso/callback`
  (scheme honors `X-Forwarded-Proto`, else `https`). **Each tenant hostname
  needs its own redirect URI** registered in Google.

## 1. Create the OAuth client in Google Cloud

1. Go to [console.cloud.google.com](https://console.cloud.google.com) → create
   or pick a project.
2. **APIs & Services → OAuth consent screen**
   - User type: **Internal** (if everyone is in your Google Workspace) or
     **External**.
   - Fill app name, support email, developer email. Save.
   - Scopes: the framework requests **`openid`, `email`, `profile`** — these are
     the default non-sensitive scopes, so you don't need to add anything special
     or submit for verification.
3. **APIs & Services → Credentials → Create Credentials → OAuth client ID**
   - Application type: **Web application**.
   - **Authorized redirect URIs** — add one per tenant host, e.g.:
     ```
     https://demo.example.com/login/sso/callback
     https://acme.example.com/login/sso/callback
     ```
     For local testing (HTTP is allowed on localhost):
     `http://demo.localhost:PORT/login/sso/callback`
   - Authorized JavaScript origins: not required (this is a server-side redirect
     flow).
4. Click **Create** and copy the **Client ID** and **Client secret**.

> ⚠️ The path must be exactly `/login/sso/callback`. Google requires an **exact
> string match** — no trailing slash, right scheme, right host. A mismatch shows
> Google's `redirect_uri_mismatch` error.

## 2. Provide the secret to the app

The framework stores a **secret reference**, never the raw secret. Give the
client secret to the process via an env var:

```bash
export ORG_GOOGLE_CLIENT_SECRET="<the client secret from Google>"
```

The Org's `sso_secret_ref` will point at it as `env://ORG_GOOGLE_CLIENT_SECRET`
(`vault://…` is also supported).

## 3. Configure the tenant's Org

Set these fields on the tenant `Org` (via the tenancy admin):

| Field             | Value                                             |
| ----------------- | ------------------------------------------------- |
| `sso_enabled`     | `true`                                            |
| `sso_provider`    | `google`                                          |
| `sso_client_id`   | _the Client ID from Google_                       |
| `sso_secret_ref`  | `env://ORG_GOOGLE_CLIENT_SECRET`                  |
| `sso_issuer_url`  | _(leave empty — only needed for generic `oidc`)_  |

The Google preset already knows the endpoints
(`accounts.google.com/o/oauth2/v2/auth`, token, and
`openidconnect.googleapis.com/v1/userinfo`) and requests `access_type=offline` +
`prompt=consent`.

> The `sso_*` columns are behind the `admin-sso` cargo feature. Make sure the
> CMS binary is built with that feature on, and the AddColumn migration has been
> applied, or the fields won't exist.

## 4. Test

1. Ensure a `rustango_users` row exists on that tenant whose `email` matches your
   Google account.
2. Visit the tenant admin `/login` — a **"Sign in with google"** button appears
   when `sso_enabled` + provider is set.
3. Click it → Google consent → bounce back to `/login/sso/callback` → logged in.
4. Failures come back as `/login?sso_error=<code>` (`disabled`, `config`, etc.) —
   `config` usually means the secret didn't resolve or the redirect URI couldn't
   be derived.
