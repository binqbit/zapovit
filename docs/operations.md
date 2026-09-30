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

Home shows the owned plan's readiness, effective pauses, deadlines and one next step. Setup screens focus on the current step, while additional controls appear under More options or inside the relevant card. STOP is available to an owner and opens a confirmation. Inbox separates guardian requests from received transmissions. Settings contains language, fixed UTC offset, recovery, recent operation results, help/privacy/service status and profile deletion. Buttons use at most two columns, descriptive text and optional Telegram styles; color is never the only indication of meaning. Menu headings and short field labels use native Telegram entities, with offsets and lengths measured in UTF-16 code units; names containing HTML or Markdown characters remain literal text. [Telegram MessageEntity](https://core.telegram.org/bots/api#messageentity).

`/start` shows the welcome/Create screen when there is no owned profile, continues preparation for a plan in Setup, and opens Home for an established active or paused plan. It installs hideable Home / Continue buttons below the input field; Home always opens the overview. `/continue` and Continue return to the active draft or the next incomplete preparation question, preserving draft choices and content. Moving to another question posts a fresh message. Selections within the same question edit that message, as do management-card navigation and lists. A confirmed unavailable edit falls back to a new message; an uncertain API response is not blindly resent. Navigating away cancels a separate sensitive-input prompt. `/settings` opens Settings and `/help` explains the flow. Startup registers Ukrainian and English command menus.

1. Create a plan. The first question asks whether its recovery key has been saved outside Telegram. The key arrives in a separate message with its own Saved — delete this message button. Use Check this step after acknowledging it, or Can't find my key to open recovery options. Do not send content during preparation.
2. Answer who you trust by creating a private invitation link for one person. That person must open the bot and explicitly accept or decline. Check who joined, then verify their name and username with them before confirming; names are not proof of identity. The confirmation question offers Confirm and Reject. Continue moves on after someone is confirmed. Contact management retains numeric Telegram IDs, private labels and archive options; pending invitations can be revoked. Sealed policies never silently change.
3. The draft has six numbered questions: guardians → recipients → content → required confirmations → timing → review and sealing. At question 3, send text or attach a document directly; additional messages/files add blocks until Continue. Other questions expect their displayed buttons, except explicit custom-timing input prompts. Text/files sent at a button-only preparation question are rejected with an explanation and are not saved as secret content. More options exposes copyable/hidden text, draft naming, block ordering/removal and delivery-rule changes. The default timing is 7 / 28 / 7 days; custom timing asks one interval at a time. A quorum of one has an explicit explanation.
4. Review, then explicitly confirm irreversible sealing. Draft content is already encrypted at rest; sealing makes content and policy unavailable for further owner reading or editing. **All** selected guardians must acknowledge their individual codes within 24 hours, regardless of quorum. The finishing question checks pending codes for new secrets before reporting readiness and offers explicit activation when allowed. Failed setup requires a new secret from an original the owner kept safely elsewhere. An already active plan reports when its new secret is ready. Invitation-accepted and codes-ready notices link back into preparation and readiness respectively.
5. `/checkin` cancels pending release and refreshes activity without lifting STOP. `/stop` and the Home STOP button open a whole-plan confirmation; a secret card opens a confirmation for that secret only. The confirmation lasts five minutes. The plan and secret states do not change until Yes, stop is accepted. Cancel leaves them unchanged and invalidates that confirmation, so a delayed click on its old button cannot stop the scope. Enabling a plan does not undo an individual secret pause; enable it separately after reviewing readiness. An empty or unready plan cannot be enabled.
6. Missing activity confirmations opens a review. The guardian card identifies the owner and secret, code readiness and request deadline. Submit code uses Telegram ForceReply and validates the specific prompt. Guardians act only when they agree that release is appropriate; inactivity alone does not establish incapacity. Quorum starts the waiting interval.
7. Recipients use Inbox → Received transmissions to see part status and retry eligibility. Sent means accepted by Telegram, not read. An unknown send is not automatically repeated; a recipient can request a retry within its allowed window after a duplicate warning.

Recovery-key and contact preparation progress is derived from Engine's persisted profile, recovery acknowledgment and confirmed contacts; it needs no additional wizard session. Waiting for invitations happens before a secret draft is created. Draft and sensitive-input sessions remain separate. Moving between menus/roles preserves draft choices and suspends draft input. An ordinary message sent after leaving the question is neither captured nor deleted; the bot first presents the current question again. A completed file upload is recorded even if another prompt is open, without reopening a suspended question. Drafts expire 15 minutes after the last content block, at most one hour, and are discarded on restart to prevent an old pre-Save snapshot from reopening sealed content. The conversational flow does not extend these limits. Explicit discard schedules temporary-object cleanup. Stale draft buttons cannot change a newer revision or cancel an unrelated prompt. Late recovery/guardian keys are rejected outside their intended input step and scheduled for chat-message removal.

Original secret-input messages are queued for removal before Save: the encrypted text append and its cleanup job commit in one transaction; file cleanup commits only after successful encrypted upload and durable draft attachment. Rejected input, failed uploads and uploads finishing after a draft was cancelled do not erase unpersisted input. Cleanup has priority over ordinary notices and uploads, retries idempotently, and survives draft cancellation. Save and cancellation do not recreate completed cleanup; preview messages and older drafts retain their cleanup fallback. A storage acknowledgment is not a deletion acknowledgment. Permanent deletion failure or expiry of the 24-hour cleanup job atomically queues a manual-deletion notice for an existing account. Telegram only allows bot deletion of messages less than 48 hours old; a delayed or refused deletion may require the user to remove the original themselves. [Telegram deletion limits](https://core.telegram.org/bots/api#deletemessage).

`/recover` prompts for a key; a valid key pauses the plan and issues a replacement key. Acknowledgment completes ownership transfer. `/recoverstop` stops the plan after validating the recovery key, without transferring ownership or adding the ordinary STOP confirmation dialog. A single message `/recover KEY` or `/recoverstop KEY` also works; verified existing credentials have reserved admission when ordinary registration is full. The old owner cannot enable the plan during recovery. An existing guardian/recipient of that plan cannot become its owner. Leaving a prompt cancels that input step. Recovery secrets are never appropriate support attachments.

Private secret and contact labels are encrypted owner metadata. A private contact label takes precedence over a cached Telegram display name; numeric IDs remain available in contact details and as a fallback. First/last names and usernames from authenticated Telegram updates are cached encrypted at rest. Showing legacy contacts during setup or opening a contact card with no cached name may make a bounded `getChat` lookup: a two-second timeout, at most one attempt per account per day and 30 globally per minute. Lookup failure leaves the contact usable. Other roles see the owner's Telegram identity and an opaque secret reference, never the owner's private labels. Names are presentation data; authorization always uses numeric account identities. Dates default to UTC. The optional offset changes display only; it is fixed and must be adjusted manually for daylight saving time.

Guardians find unanimous cancellation under More details in the relevant card. Deletion first pauses the selected scope, then requires a five-minute confirmation bound to current ownership and control epochs. Going back leaves it paused. Settings deletion removes the owner profile, its secrets and recovery credential; participation in other people's plans stays. Delivery already in flight and previously received copies cannot be recalled. Recent operation history displays bounded, actor-scoped outcomes and support references without content or codes.

## Runtime ordering and limits

Ingress encrypts and deduplicates updates, recording actor/plan routing beside the envelope. Leases, bounded backoff and quarantine prevent a failed event from occupying a worker forever. Independent actors and plans make progress separately. A quarantined trusted control holds its affected plan; untrusted malformed traffic cannot impose a global pause. Investigate the static failure class and scope before `zapovit inbox-retry --bot BOT_ID --update UPDATE_ID`; retry does not authorize the action or remove Engine checks.

Ingress takes the cursor write lock while committing its routed batch. Final dispatch takes the corresponding shared lock, then rechecks scoped controls, journal intents, epochs, state and lease before committing Dispatching. An accepted control therefore precedes dispatch or waits behind an already-started dispatch. The boundary is database acceptance, not Telegram wall-clock arrival; a started HTTP send cannot be recalled. Opening the ordinary STOP dialog does not execute a stop. After its confirmation, STOP outcomes are durably recorded before their bounded, asynchronous UI acknowledgment, as are direct check-in and verified recovery-key stop outcomes.

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
