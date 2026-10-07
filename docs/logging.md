# Logging in rustango-cms

`#[rustango::main]` auto-installs a `tracing_subscriber::fmt` subscriber
with `EnvFilter` defaulting to `info,sqlx=warn`. This page covers what
that means for a CMS host, the targets the CMS emits, and the
recommended dev / prod patterns.

## What's wired by default

| Surface | Behaviour |
|---|---|
| `#[rustango::main]` attribute | Calls `tracing_subscriber::fmt().with_env_filter(EnvFilter::try_from_default_env().unwrap_or("info,sqlx=warn")).try_init()` before the tokio runtime starts. `try_init` is no-op if a subscriber is already installed (tests). |
| `rustango::access_log::AccessLogLayer` | One INFO line per HTTP request — method, path, status, duration. Wired in tang-cms's `main.rs` via `.access_log(AccessLogLayer::default())`. |
| `tracing::warn!` / `error!` / `info!` call sites in rustango-cms | Emit through the auto-installed subscriber. Each carries a `target = "rustango_cms::<module>"` so per-module filtering works (catalog below). |

## Setting the filter

The default `info,sqlx=warn` ships with the macro. Override via the
`RUST_LOG` env var — standard `EnvFilter` syntax:

```sh
# Pretty-fmt, info everywhere, debug for one module
RUST_LOG=info,rustango_cms::render=debug cargo run

# Quiet down a noisy upstream
RUST_LOG=info,sqlx=warn,hyper=warn,h2=warn cargo run

# Trace HTTP routing into the CMS admin
RUST_LOG=info,rustango_cms::admin=trace cargo run

# Production — INFO base, drop sqlx noise, JSON output (see below)
RUST_LOG=info,sqlx=warn
```

## Tracing targets the CMS emits

Every target below corresponds to a Rust module in
`rustango_cms`. Use them in the filter string to scope verbosity.

| Target | What it covers |
|---|---|
| `rustango_cms::admin` | Top-level admin handler errors (page edit, media, etc.) |
| `rustango_cms::admin::account` | Account-preferences submit — failed email sends, password rotations |
| `rustango_cms::api` | Public JSON API errors (pages / images / documents) |
| `rustango_cms::auto_menu` | Auto-menu builder fan-out — schema drift, missing snippet warnings |
| `rustango_cms::block` | StreamField block render failures |
| `rustango_cms::cache_invalidate` | Page cache purge failures (Cloudflare, Varnish and the other purge backends) |
| `rustango_cms::children_filtered` | Tera `children_filtered()` Tera fn — bad arg shapes |
| `rustango_cms::forms` | `register_form_field_kind!` + form submission failures |
| `rustango_cms::hooks` | Plugin hooks — registration drift, panicked callbacks |
| `rustango_cms::migrations` | Per-tenant migration runs |
| `rustango_cms::page_log` | Audit-log write failures |
| `rustango_cms::page_subscription` | Publish-notify email send failures |
| `rustango_cms::page_url` | URL reverse / build helpers — passed bad ids |
| `rustango_cms::reference_index` | Reference-index rebuild failures + diffing |
| `rustango_cms::render` | Public page render — template errors, alias resolution warnings, snippet drift |
| `rustango_cms::rendition` | Raster resize / re-encode — `image` crate failures |
| `rustango_cms::rendition_route` | `/__media__/<filter>/<id>` — collection-restriction lookups, missing bytes |
| `rustango_cms::revision` | Per-page revision capture (`crate::revision::capture`) |
| `rustango_cms::search` | Public + admin search — query log + promotion lookup |
| `rustango_cms::snippet_render` | Tera `cms_snippet()` fn — drift / missing snippet ids |
| `rustango_cms::task_kind` | Workflow task registry lookups |
| `rustango_cms::theme_seed` | Theme seeding on tenant boot |
| `rustango_cms::view_restriction` | Per-page view restriction lookups |
| `rustango_cms::widget` | Widget-kind dispatch — unknown kinds |
| `rustango_cms::workflow_mail` | Workflow notification email send failures |

A quick recipe for "show me what the CMS thinks went wrong on this
request":

```sh
RUST_LOG=warn,rustango_cms=debug
```

…surfaces every CMS-emitted warning + every internal debug line
without drowning in framework or sqlx output.

## Production setup

`#[rustango::main]` is fine in dev but for prod you typically want:

- **JSON** output for log shippers (Loki / Datadog / CloudWatch)
- **A specific filter** that matches your alert rules
- **Per-tenant or per-request span context** so a deploy can grep one
  customer's traffic

The macro is opt-out: install your own subscriber **before** any
rustango code runs and the macro's `try_init` becomes a no-op.

```rust
// src/main.rs — replace `#[rustango::main]` with hand-rolled init
// when SOMETHING in the environment marks this as a production
// build. Keep the macro for dev so the pretty-print fmt is the
// default.

use tracing_subscriber::{fmt, prelude::*, EnvFilter};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,sqlx=warn"));
    if std::env::var("RUSTANGO_LOG_FORMAT").as_deref() == Ok("json") {
        tracing_subscriber::registry()
            .with(filter)
            .with(fmt::layer().json().with_current_span(true).with_span_list(false))
            .init();
    } else {
        tracing_subscriber::registry()
            .with(filter)
            .with(fmt::layer().pretty())
            .init();
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async {
            // body that was inside the macro-wrapped main goes here
            rustango::manage::Cli::new()
                .tenancy()
                .api(my_api())
                .run()
                .await
        })
}
```

Set `RUSTANGO_LOG_FORMAT=json` in your prod env and the
`tracing_subscriber::fmt::layer().json()` formatter takes over —
each record becomes a single JSON object suitable for ingestion.

## OpenTelemetry / external traces

`tracing_opentelemetry` plugs into the same `tracing` subscriber.
For a production CMS that wants distributed-trace context to flow
from edge cache → CMS → DB, drop the OTel layer alongside the fmt
layer in the manual-init pattern above:

```rust
let tracer = opentelemetry_otlp::new_pipeline()
    .tracing()
    .with_exporter(opentelemetry_otlp::new_exporter().tonic())
    .install_batch(opentelemetry_sdk::runtime::Tokio)?;
let otel = tracing_opentelemetry::layer().with_tracer(tracer);
tracing_subscriber::registry()
    .with(filter)
    .with(fmt::layer())
    .with(otel)
    .init();
```

Targets that benefit most: `rustango_cms::render`,
`rustango_cms::rendition_route`, `rustango_cms::api` — span data
shows the request path through resolve → load extension → render →
post-process.

## Common filter recipes

| Goal | `RUST_LOG=` |
|---|---|
| Default dev (what the macro picks) | `info,sqlx=warn` |
| Debug one page's render path | `info,rustango_cms::render=debug,rustango_cms::page_url=debug` |
| Audit log drift only | `warn,rustango_cms::page_log=info` |
| Quiet sqlx + hyper noise in prod | `info,sqlx=warn,hyper=warn,h2=warn` |
| Trace the rendition cache | `info,rustango_cms::rendition_route=trace,rustango_cms::rendition=trace` |
| Workflow email troubleshooting | `info,rustango_cms::workflow_mail=trace,rustango_cms::page_subscription=trace` |
| Everything CMS, quiet upstream | `warn,rustango_cms=debug` |

## See also

- `rustango-macros/src/lib.rs:226-310` — the `#[rustango::main]`
  expansion (auto-init source)
- `rustango/src/access_log.rs` — request-line logging layer
- [tracing-subscriber docs](https://docs.rs/tracing-subscriber/) —
  full `EnvFilter` syntax + layer composition
- [tracing_opentelemetry](https://docs.rs/tracing-opentelemetry/) —
  external-trace bridging
