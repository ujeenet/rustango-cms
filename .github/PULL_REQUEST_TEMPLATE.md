## What and why

<!-- What this changes and the problem it solves. Link the issue: Closes #123 -->

## Checklist

- [ ] `cargo test --workspace` and `cargo test --workspace --features sqlite` pass
- [ ] `cargo clippy --workspace --all-targets -- -D clippy::correctness -D clippy::suspicious` is clean
- [ ] Admin or public-site changes were checked in a browser (e2e: `cd e2e && npx playwright test`)
- [ ] A breaking change has an entry in CHANGELOG.md and UPGRADING.md
- [ ] New admin strings are in all six catalogs under `src/admin/locales/`
