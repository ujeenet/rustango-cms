# Contributing

Thanks for helping. This file covers what you need to build, test and send
a change. Security problems go through [SECURITY.md](SECURITY.md), not an
issue.

## Build

rustango-cms builds against the published `rustango` crate. To work on
both at once, check `rustango` out next to this repository and follow
[`.cargo/config.toml.example`](.cargo/config.toml.example) to patch it in.

```sh
cargo build                          # PostgreSQL (the default feature)
cargo build --no-default-features --features sqlite
```

Every feature must build on SQLite, PostgreSQL **and** MySQL: use the
dialect-neutral ORM and `_pool` helpers, not backend-only SQL.

## Test

Run both suites — most database-backed tests are SQLite-gated, so the
default run alone skips them:

```sh
cargo test --workspace
cargo test --workspace --features sqlite
```

The end-to-end suite boots the `cms_demo` example on SQLite (Node ≥ 22.13):

```sh
cd e2e && npm ci && npx playwright test
```

Tests that need a live MySQL or PostgreSQL are `#[ignore]`d and document
their environment variable at the top of the file (for example
`RCMS_TEST_MYSQL_URL`).

Before a pull request, also run what CI runs:

```sh
cargo clippy --workspace --all-targets -- -D clippy::correctness -D clippy::suspicious
cargo audit
```

## Pull requests

- One logical change per commit, with a message that says why.
- Reference the issue (`Closes #123`).
- A change that breaks host applications — a public signature, a CSS class,
  a template, a config variable — gets an entry in
  [CHANGELOG.md](CHANGELOG.md) and [UPGRADING.md](UPGRADING.md).

## Licensing

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed under the MIT and Apache-2.0 licenses, without
any additional terms or conditions.

Everyone taking part is expected to follow the
[code of conduct](CODE_OF_CONDUCT.md).
