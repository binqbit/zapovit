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
| `JOURNAL_REPLICA_DIR` | Optional native path; every accepted control append also fsyncs this replica |
| `S3_ACCESS_KEY_ID`, `S3_SECRET_ACCESS_KEY` | Required S3 credentials; generated automatically |
| `S3_ENDPOINT`, `S3_REGION`, `S3_BUCKET` | `http://object-storage:3900`, `garage`, `zapovit` |
| `TELEGRAM_API_BASE` | `https://api.telegram.org`; loopback HTTP is supported for tests |
| `HEALTH_BIND` | `0.0.0.0:8080` on the private container network |
| `WORKERS`, `DATABASE_POOL` | `4` (range 1–4), `10` (range 2–20) |

Compose also uses the generated `DATABASE_PASSWORD` for PostgreSQL, and `GARAGE_RPC_SECRET` and `GARAGE_ADMIN_TOKEN` for Garage and storage bootstrap. `DATABASE_PASSWORD` must match the password in `DATABASE_URL`.

Keyring variables contain JSON with `active` and a map of key IDs to base64url-encoded 32-byte keys. Keep old key IDs while dependent envelopes or guardian verifiers exist. Merely changing an active key ID neither re-encrypts old data nor repairs a compromised key.

`/live`, `/ready` and `/metrics` are internal endpoints. `/ready` rechecks poll and scheduler freshness, integrity and maintenance from the database on every request; a cached healthy boolean cannot hide a stalled loop. Scoped pending/quarantined controls and release holds additionally block dispatch for the affected plans and appear in the bot's effective state. A healthy HTTP probe alone does not authorize delivery. Metrics contain aggregate counts and ages, with no account IDs, private names or payloads.

Monitor `zapovit_poll_age`, `zapovit_scheduler_age`, `zapovit_scheduler_lag_seconds`, `zapovit_scheduler_failed_plans`, `zapovit_inbox_oldest_seconds`, `zapovit_inbox_quarantined`, `zapovit_control_backlog`, `zapovit_integrity_hold`, `zapovit_reserved_blob_bytes`, `zapovit_pending_bytes` and `zapovit_backup_age_seconds` (`-1` means no completed backup). Alerts, paging destination and an incident owner must be configured by the deployment operator; exposing metrics does not deliver alerts by itself.

## Bot flow

Home shows the owned plan's readiness, effective pauses, deadlines and one next step. Secrets and People appear together; Inbox and Settings occupy another row. STOP is always available to an owner. Inbox separates guardian requests from received transmissions. Settings contains language, fixed UTC offset, recovery, recent operation results, help/privacy/service status and profile deletion. Additional controls appear inside the relevant card. Buttons use at most two columns, descriptive text and optional Telegram styles; color is never the only indication of meaning.

`/start` opens Home, `/settings` opens Settings and `/help` explains the flow. Startup registers Ukrainian and English command menus. Navigation edits the current message; a confirmed unavailable edit falls back to a new message. An uncertain API response is not blindly resent.

1. Create a plan and save/acknowledge its recovery key. The readiness checklist explains the next missing step.
2. Invite people. Opening a link previews its owner and expiry; the invitee explicitly accepts or declines. The owner confirms the numeric Telegram identity. Private contact labels do not replace this check. Pending invitations can be revoked and unused contacts archived; sealed policies never silently change.
3. Choose guardians, recipients, quorum and intervals before entering content. The default is 7 / 28 / 7 days. Custom timing asks one interval at a time. A quorum of one has an explicit explanation; **all** selected guardians must store their individual codes regardless of quorum.
4. Add text, copyable/hidden text or files, name the draft, and reorder/remove blocks. Review, then explicitly confirm irreversible encryption. The owner can no longer read or edit sealed content or policy. All guardians must acknowledge codes within 24 hours; failed setup requires a new secret from the owner's original. Activation remains explicit.
5. `/checkin` cancels pending release and refreshes activity without lifting STOP. `/stop` pauses the whole plan. A secret card can pause only that secret. Enabling a plan does not undo an individual secret pause; enable it separately after reviewing readiness. An empty or unready plan cannot be enabled.
6. Missing activity confirmations opens a review. The guardian card identifies the owner and secret, code readiness and request deadline. Submit code uses Telegram ForceReply and validates the specific prompt. Guardians act only when they agree that release is appropriate; inactivity alone does not establish incapacity. Quorum starts the waiting interval.
7. Recipients use Inbox → Received transmissions to see part status and retry eligibility. Sent means accepted by Telegram, not read. An unknown send is not automatically repeated; a recipient can request a retry within its allowed window after a duplicate warning.

Draft and sensitive-input sessions are separate. Moving between menus/roles preserves draft choices; a completed file upload is recorded even if another prompt is open. Drafts expire 15 minutes after the last content block, at most one hour, and are discarded on restart to prevent an old pre-Save snapshot from reopening sealed content. Explicit discard schedules temporary-object cleanup. Stale draft buttons cannot change a newer revision or cancel an unrelated prompt. Late recovery/guardian keys are rejected outside their intended input step and scheduled for chat-message removal.

`/recover` prompts for a key, pauses the plan and issues a replacement key. Acknowledgment completes ownership transfer. `/recoverstop` only stops the plan. A single message `/recover KEY` or `/recoverstop KEY` also works; verified existing credentials have reserved admission when ordinary registration is full. The old owner cannot enable the plan during recovery. An existing guardian/recipient of that plan cannot become its owner. Leaving a prompt cancels that input step. Recovery secrets are never appropriate support attachments.

Secret and contact names are encrypted private owner metadata. Other roles see an opaque reference and the owner's Telegram identity. Dates default to UTC. The optional offset changes display only; it is fixed and must be adjusted manually for daylight saving time.

Guardians find unanimous cancellation under More details in the relevant card. Deletion first pauses the selected scope, then requires a five-minute confirmation bound to current ownership and control epochs. Going back leaves it paused. Settings deletion removes the owner profile, its secrets and recovery credential; participation in other people's plans stays. Delivery already in flight and previously received copies cannot be recalled. Recent operation history displays bounded, actor-scoped outcomes and support references without content or codes.

## Runtime ordering and limits

Ingress encrypts and deduplicates updates, recording actor/plan routing beside the envelope. Leases, bounded backoff and quarantine prevent a failed event from occupying a worker forever. Independent actors and plans make progress separately. A quarantined trusted control holds its affected plan; untrusted malformed traffic cannot impose a global pause. Investigate the static failure class and scope before `zapovit inbox-retry --bot BOT_ID --update UPDATE_ID`; retry does not authorize the action or remove Engine checks.

Ingress takes the cursor write lock while committing its routed batch. Final dispatch takes the corresponding shared lock, then rechecks scoped controls, journal intents, epochs, state and lease before committing Dispatching. An accepted control therefore precedes dispatch or waits behind an already-started dispatch. The boundary is database acceptance, not Telegram wall-clock arrival; a started HTTP send cannot be recalled. STOP/check-in outcomes are durably recorded before their bounded, asynchronous UI acknowledgment.

New accounts, drafts, invitations, ordinary queue work and encrypted storage have global admission limits as well as per-plan quotas. Atomic storage reservations include pending and GC objects. At 50,000 unprocessed inbox rows, ordinary input receives bounded rejection feedback while protective traffic retains admission. The hard limit is 100,000 unprocessed rows, including quarantine. Deduplication and safe coalescing happen before this limit. If a protective event cannot fit, ingress commits only the preceding accepted/rejected prefix and keeps the remaining encrypted poll batch for retry without advancing past it. Retry revalidates authority and does not refresh poll freshness; prolonged saturation therefore also invokes the stale-poll hold. Accepted controls and quarantined evidence are never evicted to make room.

Verified recovery has a separate bounded account reserve. SQL and lock waits have deadlines, scheduler failures are isolated per plan, and runtime/history scans use bounded keyset pages. These are operating limits, not evidence of a particular public traffic capacity. Identical unclaimed tail STOP/check-in events coalesce only when no actor/plan event intervenes. Distinct protective operations preserve their ordering and outcomes. Public-load tests must include adversarial authorized traffic, durable journal growth and the cursor lock shared by UI readiness reads.

Expired UI actions and seven-day ephemeral inbox/notice/receipt history are removed in bounded batches. Actor history is capped at 100 results. Control intents, authenticated journal, tombstones and delivery proofs are retained for supported restores; do not prune them merely because UI history expired. Audit their disk usage and establish a backup-retention/compaction policy before expanding deployment scale.

## Backup and restore

Daily encrypted backups, seven-day retention, RPO 24 hours and RTO four hours are operational targets, not guarantees. Keep backups on separate storage and periodically perform a restore drill. Use [age](https://github.com/FiloSottile/age) public recipients; do not keep its private recovery identity on the application host.

The backup command requires Python 3 and the standard Compose database (`db:5432/zapovit`) and journal path. It rejects a mismatched database URL or password before exporting data.

```sh
bash scripts/backup.sh /secure/public-recipients.txt /separate-backup-disk/zapovit
```

The script starts a persisted UUID-owned backup session, waits for bounded I/O and lease drain, exports PostgreSQL and encrypted objects, takes a consistent authenticated journal prefix, and encrypts the archive. `runtime.env` captures effective running configuration; the manifest records actual container image IDs. Helpers use these image IDs with `--pull never --no-build`. A mutable local tag cannot silently select a different backup binary.

STOP/check-in remain available while content writes, release and object deletion are held. Success calls `backup-complete --session ID`, ending its own maintenance session while preserving all current operational holds, including any added during the backup. Normal daily backups do **not** introduce a fresh 24-hour hold. Failure leaves maintenance enabled and retains private staging evidence; another session or generic maintenance command cannot impersonate successful completion. Delete old archives only after verifying a newer complete, decryptable archive. A PostgreSQL dump alone is not a backup.

The control journal authenticates its prefix once, then appends/fsyncs on a blocking worker with a response deadline. An uncertain or timed-out write never acknowledges success. For protection from whole-host loss, configure independently durable mounted storage, not another directory on the same disk:

```sh
JOURNAL_REPLICA_PATH=/mounted/independent/zapovit \
  docker compose -f compose.yaml -f compose.replica.yaml up -d
```

Both configured copies must be durably updated before a protective action is acknowledged. Filesystem/mount durability and failure independence are deployment responsibilities. Preserve a current authenticated replica anchor outside the failed host; a backup's own anchor does not prove that later STOPs were preserved.

Restore into an isolated installation first:

1. Stop the application; verify/decrypt the archive with age into a private directory. Restore PostgreSQL with the supported `pg_restore` version. The dump retains maintenance and an explicit restore-verification gate.
2. Restore `runtime.env` as private configuration, preserving keyrings and adapting isolated connection paths. Bootstrap Garage with the restored credentials. Run `zapovit restore-require` to fence a manually restored installation as well.
3. Run `zapovit import-objects --directory /backup/objects`. Versioned manifest pages and every object digest are verified; older bounded manifests remain supported.
4. Install the **latest independently preserved journal** at the configured `JOURNAL_DIR`, together with its matching anchor. Preserve any configured replica consistently. Stop if freshness cannot be established.
5. Run `zapovit restore-verify --directory /var/lib/zapovit/journal --witness /independent/current-anchor`. The directory must be the live configured journal, and its authenticated anchor must match the independent current witness. An older backup anchor is insufficient.
6. Start the isolated app. Startup replays controls before normal work. Confirm ownership, STOP and deleted scopes, unknown deliveries and object availability. Do not invent a new empty journal for an existing database.
7. After the drill, repeat the verified procedure for the intended environment. Explicit `zapovit maintenance` leaves a **24-hour release hold** after restore; it refuses to bypass missing restore verification. Owners may confirm activity while held.

The lower-level backup sequence is `backup-begin` → `backup-drain --session ID --timeout-seconds 180` → exports → `backup-snapshot --session ID --directory PATH` → archive encryption → `backup-complete --session ID`. Use the maintained wrapper for a complete archive. Export refuses an absent, mismatched or undrained session. The journal snapshot follows the database dump. SQL intent staging precedes journal append, so a dumped intent may still be awaiting append; startup also journals and replays unapplied SQL intents before normal work.

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

Unbounded administrative list helpers still fail closed above 10,000 matches. Startup replay, cleanup, scheduler and object export use bounded pages instead; the export regression covers more than 10,000 ledger rows. Preserve this distinction when adding new global queries.

PostgreSQL tests create isolated schemas and require a database name ending in `_test`:

```sh
TEST_DATABASE_URL=postgresql://USER:PASSWORD@127.0.0.1:5432/zapovit_test \
  cargo test -p adapters --test pipeline --locked -- --include-ignored --test-threads=1
TEST_DATABASE_URL=postgresql://USER:PASSWORD@127.0.0.1:5432/zapovit_test \
  cargo test -p app --locked -- --include-ignored --test-threads=1
TEST_DATABASE_URL=postgresql://USER:PASSWORD@127.0.0.1:5432/zapovit_test \
  cargo test -p adapters --test backup_protocol --locked -- --ignored --skip garage_ --test-threads=1
```

Garage integration tests additionally require `TEST_S3_ENDPOINT` pointing to loopback and an **absolute** `TEST_S3_CREDENTIALS_FILE` path. Run `cargo test -p adapters --test transport --locked -- --include-ignored` with those variables.

The maintained container integration entry point is `python3 scripts/integration/compose_smoke.py`. It creates uniquely named source/restore stacks, generates synthetic configuration without reading deployment `.env`, tests the running bot against a local API fixture, exercises real Garage and encrypted backup/restore, and tears down its own stacks. It needs Docker access, Rust 1.98.1 and age. Private artifacts go to `.agent-workspace/`; CI publishes only selected non-secret evidence. The separate CI job also records SBOMs and rejects high/critical image vulnerabilities. Configuration of this job is not proof it has passed in the current environment.

Development-only helpers, synthetic database clusters and test reports belong in `.agent-workspace/`, ignored by Git and excluded from Docker build contexts. A local helper can provision its own PostgreSQL, run the commands above and stop that cluster afterward; it must not load deployment credentials or target an existing database. Maintained tests and operational backup scripts remain tracked in `crates/*/tests` and `scripts/`. Neither the build nor CI depends on `.agent-workspace/`.

See [verification results](verification.md) for evidence and environment limits.
