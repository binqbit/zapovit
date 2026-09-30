# Zapovit

Zapovit is a digital inheritance project for passing sensitive information and access to digital assets to trusted people when the owner is unable to manage their data themselves.

Users interact with Zapovit through a Telegram bot.

**Development prototype — synthetic test data only.** Production mode is disabled. Do not put real passwords, wallet recovery phrases or private documents into this build. See [verification and release limits](docs/verification.md).

## What it covers

- Passwords, account credentials, and recovery information.
- Private documents, instructions, and other sensitive records.
- Cryptocurrency wallet recovery information and access to digital assets.

## How it works

The owner decides what to pass on, who should receive it, and under which conditions. Activity confirmations and review by trusted contacts can help determine when to begin the transfer process. Recipients receive their assigned information once the configured conditions are met.

Missed activity confirmations start a review by trusted contacts. They do not, on their own, establish that the owner is unable to manage their data.

## Development

The workspace contains `domain` (rules), `application` (use cases and ports), `adapters` (Telegram, PostgreSQL, Garage and cryptography), and `app` (CLI and runtime). Start with the [running instructions](docs/operations.md) and use a dedicated synthetic test bot.

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
python3 scripts/test_backup_env.py
docker compose config --quiet
```

Rust 1.98.1 is pinned in `rust-toolchain.toml`. PostgreSQL and Garage tests are explicitly opt-in; a successful default `cargo test` does not run them. See [development and verification](docs/operations.md#development-and-verification) for their commands.

Local diagnostic tools, synthetic databases, transcripts and reports belong in `.agent-workspace/`, excluded from both Git and Docker build contexts. Maintained regression tests stay in `crates/*/tests`; the build and CI do not depend on the local workspace.

## Documentation

- [Project concept](docs/concept.md) — a discussion of possible features, inheritance scenarios, trust relationships, and design approaches.
- [Version 1 scope](docs/v1.md) — the initial feature set, activity checks, guardian confirmations, and delivery of assigned secrets.
- [Version 1 technical specification](docs/v1-technical-spec.md) — architecture, technology stack, encryption, storage, delivery workflows, and deployment.
- [Running and operating Zapovit](docs/operations.md) — setup, configuration, bot usage, backups, and verification commands.
- [Improvement roadmap](docs/roadmap.md) — repository review, priorities, and acceptance criteria for the next development stages.
