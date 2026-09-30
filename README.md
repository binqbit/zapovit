# Zapovit

Zapovit is a digital inheritance project for passing sensitive information and access to digital assets to trusted people when the owner is unable to manage their data themselves.

Users interact with Zapovit through a Telegram bot.

## What it covers

- Passwords, account credentials, and recovery information.
- Private documents, instructions, and other sensitive records.
- Cryptocurrency wallet recovery information and access to digital assets.

## How it works

The owner decides what to pass on, who should receive it, and under which conditions. Activity confirmations and review by trusted contacts can help determine when to begin the transfer process. Recipients receive their assigned information once the configured conditions are met.

Missed activity confirmations start a review by trusted contacts. They do not, on their own, establish that the owner is unable to manage their data.

## Documentation

- [Project concept](docs/concept.md) — a discussion of possible features, inheritance scenarios, trust relationships, and design approaches.
- [Version 1 scope](docs/v1.md) — the initial feature set, activity checks, guardian confirmations, and delivery of assigned secrets.
- [Version 1 technical specification](docs/v1-technical-spec.md) — architecture, technology stack, encryption, storage, delivery workflows, and deployment.
- [Running and operating Zapovit](docs/operations.md) — setup, configuration, bot usage, backups, and verification commands.
