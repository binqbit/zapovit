# Running Zapovit

The executable is `zapovit`. It runs a Telegram adapter, application commands, scheduler and delivery workers in one Rust process. PostgreSQL stores durable state; Garage stores encrypted file objects. No message broker is required.

## Admission

This build accepts **synthetic test data only**. `serve` requires `DATA_MODE=synthetic`; the default production mode fails closed. This implements the admission requirement in the technical specification: an independent review covering the selected threshold implementation and the integrated service is still required before real secrets are accepted. Passing tests or RustSec checks does not substitute for that review.

Use a separate test bot. Messages to bots pass through Telegram and through the service in plaintext before encryption. Removing a chat message does not remove screenshots, forwarded copies, device caches or older backups.

## Docker Compose

Requirements: Docker Engine, Docker Compose, and a Telegram test bot token. The checked-in images and Rust dependencies are pinned. No PostgreSQL, S3, admin or health port is published to the host by default.

The application image `zapovit:local` is built from this repository. Its `pull_policy: build` makes Compose build it locally, including on `up -d`, using cached layers when available. PostgreSQL and Garage are pulled from their pinned upstream images. [Docker pull policies](https://docs.docker.com/reference/compose-file/services/#pull_policy).

```sh
docker build -t zapovit:local .
docker run --rm --user "$(id -u):$(id -g)" \
  --mount "type=bind,src=$PWD,dst=/setup" \
  zapovit:local generate-env --output /setup/.env
```

Set `TELEGRAM_BOT_TOKEN` in `.env` to your test bot token using an editor. The generator creates the other credentials and keyrings automatically, writes `.env` with mode `0600`, and refuses to overwrite an existing file. All deployment settings live in `.env`; no host secrets directory is needed. A native build can run `./target/debug/zapovit generate-env --output .env` instead.

For an existing installation, transfer its existing keyrings and credentials into `.env`; generating replacement encryption keys does not migrate stored data.

Start the stack; initialization runs automatically:

```sh
docker compose up -d
docker compose ps
docker compose logs --tail 50 app migrate storage-init
```

The one-shot migration and storage bootstrap must succeed before the application starts. Storage bootstrap creates one node layout and a private bucket, then imports the configured S3 key with read/write permission and no bucket-owner permission. Garage admin credentials are passed only to Garage and bootstrap, never to the bot.

The application acquires database leadership and creates the journal automatically when its directory is empty and the database is unused: no bot binding, application records or maintenance hold. Existing journals are verified and reused on every startup. No manual `journal-init` step is required. The journal has a separate persistent volume; a missing journal on a used database or an incomplete/damaged journal stops startup without replacing history. Preserve its current authenticated history and anchor independently of database snapshots.

If startup fails, inspect `docker compose logs --tail 50 app`. `startup_failed` identifies the stage, such as `journal_open`, `database_schema` or `telegram_get_me`. Invalid settings report their variable name and a static reason through `configuration_invalid`; values and underlying credential-bearing errors are omitted. `journal_initialized` confirms automatic creation. A missing journal that cannot be initialized reports `journal_unavailable`; restore the current journal for an existing installation and check volume access.

The default `APP_UID`/`APP_GID` is 10001. If overriding them in `.env`, also arrange ownership of the journal volume. The deployment is a single node and does not provide high availability.

## Configuration

`generate-env` creates `.env` with credentials and deployment defaults; `.env.example` documents the settings. Docker Compose loads `.env` and forwards the settings required by each container. `APP_UID` and `APP_GID` configure the application container identity. Internal service addresses and the journal path are defined by Compose and application defaults; adding other variables to `.env` alone does not forward them into a container.

The Rust binary reads the supported environment variables below directly and does not load `.env` files. Export them when running it outside Compose. Unrelated variables are ignored; supported settings still undergo validation, including value bounds. `check-config` does not print secret values.

| Variable | Default |
| --- | --- |
| `DATA_MODE` | `production` — rejected until admission is implemented; Compose explicitly uses `synthetic` |
| `DATABASE_URL` | Required; generated for `db:5432/zapovit` |
| `TELEGRAM_BOT_TOKEN` | Required for the bot; set your test bot token |
| `KEK_KEYRING`, `VERIFIER_KEYRING`, `JOURNAL_KEYRING` | Required JSON keyrings; generated automatically |
| `JOURNAL_DIR` | `/var/lib/zapovit/journal` |
| `S3_ACCESS_KEY_ID`, `S3_SECRET_ACCESS_KEY` | Required S3 credentials; generated automatically |
| `S3_ENDPOINT`, `S3_REGION`, `S3_BUCKET` | `http://object-storage:3900`, `garage`, `zapovit` |
| `TELEGRAM_API_BASE` | `https://api.telegram.org`; loopback HTTP is supported for tests |
| `HEALTH_BIND` | `0.0.0.0:8080` on the private container network |
| `WORKERS`, `DATABASE_POOL` | `4` (range 1–4), `10` (range 2–20) |

Compose also uses the generated `DATABASE_PASSWORD` for PostgreSQL, and `GARAGE_RPC_SECRET` and `GARAGE_ADMIN_TOKEN` for Garage and storage bootstrap. `DATABASE_PASSWORD` must match the password in `DATABASE_URL`.

Keyring variables contain JSON with `active` and a map of key IDs to base64url-encoded 32-byte keys. Keep old key IDs while dependent envelopes or guardian verifiers exist. Merely changing an active key ID neither re-encrypts old data nor repairs a compromised key.

`/live`, `/ready` and `/metrics` are internal endpoints. Metrics expose aggregate queue counts, unknown/failed sends, poll age, hold time, pending object bytes and pool usage. They contain no account IDs or secret names. Readiness depends on working polling and an empty inbox; it does not claim that an operator's maintenance hold has been lifted.

## Bot flow

The main menu contains My plan, Guardian requests and Settings. Owners also have direct activity-confirmation and STOP buttons. My plan groups secrets, trusted people and plan activation, and shows Continue draft while a live draft exists. Settings contains Language, Access and recovery, and permanent plan deletion for the owner. Language selection marks the current choice and supports Ukrainian and English. `/settings` opens settings directly; `/start` returns to the main menu; `/help` explains the main actions. Startup registers localized Telegram command menus; a registration failure is logged without preventing emergency commands from being processed. Submenus and draft steps provide navigation back. Menu navigation and person selection update the existing message using [Telegram message editing](https://core.telegram.org/bots/api#editmessagetext); previews, lists, input replies and confirmations may send separate messages.

1. `/start` → create a plan → save and acknowledge the recovery key.
2. Invite people. They open the invitation; the owner confirms their numeric Telegram IDs.
3. Add text, copyable text, hidden text and/or documents with captions. Choose guardians, recipients, threshold and timing. Use the 7 / 28 / 7 day preset or enter custom intervals; invalid intervals can be corrected in the same step. Review the selected IDs before Save. Back and Continue draft preserve the selections. Buttons belong to the draft, revision and step shown when they were created; old Save or selection buttons cannot act on another draft or another step.
4. Save seals the content. The owner cannot read it again. Every selected guardian must acknowledge storing their code. Activate the plan explicitly.
5. `/checkin` cancels pending release and refreshes activity. `/stop` pauses it; `/resume` explicitly arms it. A check-in does not lift a STOP.
6. Missing activity confirmations within the configured period starts a review case. Guardian notices identify the owner's Telegram ID and the secret reference. Submit code opens Telegram's reply interface, and the response must reply to that specific prompt. Guardians reply with their codes only if, based on what they know, the owner is unable to manage their data themselves and they agree to release the secret. Inactivity alone does not establish this. Quorum starts the waiting period; it does not immediately send the data.
7. Delivery rechecks authorization just before dispatch. Text and files follow the saved order. An unknown result is not automatically retried; the recipient can request a retry through `/guardians` after accepting a duplicate warning.

Leaving a draft for a menu preserves it while its storage lifetime remains valid. Drafts expire after 15 minutes without added content, with a maximum lifetime of one hour. Unfinished drafts are also discarded when the service restarts. The owner is notified and can create a new draft; sealed secrets remain stored. This prevents restoring a pre-Save database snapshot from making sealed content readable again through an old draft.

`/recover` accepts a recovery token from a new account, pauses the plan and issues a replacement token. Acknowledging the replacement completes ownership transfer. `/recoverstop` only stops the plan. Both actions are also available under Settings → Access and recovery. Leaving the recovery prompt through Back cancels its input step. The old owner cannot arm it while a transfer is pending. An account already receiving or guarding that plan cannot become its owner through recovery. Owners can replace their recovery key under Settings → Access and recovery.

Guardians can request cancellation and vote through `/guardians`; unanimity is required. Separate labels identify cancellation of one secret and of the whole plan. A secret-specific cancellation does not cancel siblings. Permanent plan deletion is available in Settings and requires an explicit confirmation. Individual secrets can be deleted from My plan → Secrets and status with a confirmation that identifies the smaller scope. Activity dates are displayed in UTC. A late delivery result remains recorded but cannot lift a secret STOP or change the state of a newer release case.

## Backup and restore

Daily encrypted backups, seven-day retention, RPO 24 hours and RTO four hours are operational targets, not guarantees. Keep backups on separate storage and periodically perform a restore drill. Use [age](https://github.com/FiloSottile/age) public recipients; do not keep its private recovery identity on the application host.

The backup command requires Python 3 and the standard Compose database (`db:5432/zapovit`) and journal path. It rejects a mismatched database URL or password before exporting data.

```sh
bash scripts/backup.sh /secure/public-recipients.txt /separate-backup-disk/zapovit
```

The script enables persisted maintenance, waits for bounded I/O to drain, exports PostgreSQL, encrypted objects, effective runtime credentials and a verified journal snapshot, then encrypts the archive. It captures settings from the running containers in `runtime.env`, rather than copying a possibly edited host `.env`. STOP and check-in remain available. New content writes, release and object deletion are held. A failure leaves maintenance enabled and preserves the private staging directory for investigation. After successful backup, release remains on an operational hold.

Backup helper containers use `--pull never` to reuse the local application image without pulling or rebuilding it during the backup.

The daily backup target is not ready for unattended deployment: every successful run currently applies a new 24-hour release hold, so running it daily can keep transmission held continuously. Separate normal backup completion from restore recovery before enabling that schedule; see the [improvement roadmap](roadmap.md). Delete archives older than seven days only after checking that a newer complete, decryptable backup exists. Never treat a PostgreSQL dump alone as a recoverable backup.

Restore into an isolated installation first:

1. Stop the application. Verify the archive checksum, decrypt it with the age identity, and extract into a private directory.
2. Restore PostgreSQL using `pg_restore` from the same supported PostgreSQL release. The restored database retains maintenance mode.
3. Restore `runtime.env` as the isolated installation's `.env` with mode `0600`. Preserve its keyrings and adjust connection settings for the restore environment. Bootstrap Garage to import the configured S3 credentials before restoring objects.
4. Run `zapovit import-objects --directory /backup/objects` with the backup directory mounted read-only and the restored database/S3 configured. Every object is checked against its manifest digest.
5. Restore the **latest independently preserved control journal**, not an older journal simply because it accompanied the database dump. Verify it with `verify-journal`. If freshness cannot be established, keep maintenance enabled.
6. Start the application in the isolated environment. Startup replays control operations before polling or jobs. Confirm current owners, deleted scopes and STOP states; inspect missing-object errors and unknown deliveries.
7. Only after a successful drill, repeat the verified procedure for the intended environment. Lift maintenance explicitly with `zapovit maintenance`; this retains an operational hold. Owners can confirm activity while held.

Deleting a secret, plan or profile removes its live content, policy/contact bindings and scoped credentials. Contact-free tombstones prevent old scope IDs from being recreated; the encrypted control journal remains required for restore. Independent account records can remain for participation in other plans. Cleanup jobs and file-object records survive only until cleanup completes.

Physical deletion of files is asynchronous. The object ledger retains keys after the first DELETE to catch bounded late uploads. A STOP cannot recall a send already marked Dispatching, and deleting live rows cannot erase WAL, old backups or recipients' copies.

## Development and verification

```sh
cargo build --locked
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo audit
cargo deny check
docker compose config --quiet
```

The initial V1 schema is in `migrations/0001_core.sql`. Once an installation has applied it, preserve that file and add numbered migrations for subsequent schema changes. SQLx checks migration checksums; modifying an applied migration prevents startup. Tests use fresh disposable schemas. The application never resets an existing database automatically.

Full-record administrative queries fail closed above 10,000 matching records; they never silently truncate backup exports. This implementation needs paginated administrative scans before operating beyond that bound.

PostgreSQL tests create isolated schemas and require a database name ending in `_test`:

```sh
TEST_DATABASE_URL=postgresql://USER:PASSWORD@127.0.0.1:5432/zapovit_test \
  cargo test -p adapters --test pipeline --locked -- --include-ignored --test-threads=1
TEST_DATABASE_URL=postgresql://USER:PASSWORD@127.0.0.1:5432/zapovit_test \
  cargo test -p app --locked -- --include-ignored --test-threads=1
```

Garage integration tests additionally require `TEST_S3_ENDPOINT` pointing to loopback and an **absolute** `TEST_S3_CREDENTIALS_FILE` path. Run `cargo test -p adapters --test transport --locked -- --include-ignored` with those variables.

The earlier running-bot validation used local loopback Telegram and smoke-test helpers that are not included in this repository. The checked-in Rust tests above provide the available automated verification; they do not reproduce that full running-bot scenario.

Development-only helpers, synthetic database clusters and test reports belong in `.agent-workspace/`, ignored by Git and excluded from Docker build contexts. A local helper can provision its own PostgreSQL, run the commands above and stop that cluster afterward; it must not load deployment credentials or target an existing database. Maintained tests and operational backup scripts remain tracked in `crates/*/tests` and `scripts/`. Neither the build nor CI depends on `.agent-workspace/`.

See [verification results](verification.md) for evidence and environment limits.
