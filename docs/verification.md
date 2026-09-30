# Implementation verification

Full local validation was reported on 7 September 2026 with synthetic data. The results are summarized below; the separate validation and security report is not included in this repository.

## Environment configuration follow-up

Deployment now uses plain environment names and one `.env` file instead of mounted secret files. `generate-env` creates private, non-overwriting configuration with independent random keyrings and matching database/storage credentials. Backup captures effective running values into `runtime.env`.

Follow-up validation passed 31 default workspace tests and nine backup configuration tests, including Compose parsing of quoted values and rejection of mismatched database credentials. The generated configuration was parsed through Compose, checked by the Rust binary, captured for backup and parsed again with identical application values. Native Garage 2.4.0 passed environment-secret startup, repeated key import, wrong-key rejection, permission checks and S3 operations. The running bot passed the complete loopback Telegram onboarding, encrypted text/file, release and STOP scenario using only environment credentials.

Formatting, all-target Clippy, the locked build and shell syntax checks pass. The Dockerfile includes the public `.env.example` template while real `.env` files remain excluded. Actual Docker daemon execution remains unavailable. The 60-test matrix below records the earlier full audit; its secret-file deployment and backup setup have been superseded by this configuration follow-up.

## Executed checks

| Check | Result and scope |
| --- | --- |
| Rust build and static gates | Rust 1.98.1; locked workspace build, formatting, all-target Clippy with warnings denied |
| Default workspace suite | 27 passing tests; external-service tests run explicitly below |
| PostgreSQL pipeline | 32 passing tests, including the journal case; fresh isolated schemas from `0001_core.sql` |
| Telegram and Garage contracts | 5 passing tests: Telegram response classification, copy/file preservation, cleanup retries, shared throttling and actual Garage PUT/GET/HEAD/DELETE |
| Leadership failure | Original PostgreSQL leader session terminated in isolation; supervisor fails without reconnecting |
| Crypto properties | 48 threshold/subset cases and 48 malformed-input cases; fixed input seed `0x5a41504f564954`, plus published/fixed vectors, tampering and retained-key tests |
| Running bot | Loopback Telegram API with native PostgreSQL and Garage; onboarding, recovery ACK, invitations, text/file builder, code ACK, inactivity, quorum, delay, assigned delivery and STOP |
| Backup and restore | Native PostgreSQL dump/restore; encrypted objects imported into another Garage bucket and compared; journal verified; age round trip |
| Dependency policy | cargo-audit 0.22.2 and cargo-deny 0.20.2 pass; no known advisories or advisory warnings; duplicate dependency versions are allowed warnings |
| Deployment artifacts | Compose configuration and script syntax pass; all four pinned Docker Hub manifests resolve to their specified digests |
| Source indicators | Source-only scan finds no matching credential, unsafe-block or unfinished-implementation indicators; this heuristic is not comprehensive SAST |

The commands above exercise **60 distinct named tests**; overlapping invocations are not counted twice. The generated property cases are additional iterations inside two of those tests.

Coverage includes wrong-owner access, immutable policies, threshold/grace checks, source deduplication, deterministic upload/delete races, terminal send monotonicity, stale cancellation replacement, final credential authorization, recovery replay, missing-parent cleanup, full deletion with minimal tombstones, snapshot restoration, migration checksum rejection, quotas, priority authorization and unavailable Telegram recipients.

## Verified lifecycle decisions

- Unfinished drafts are discarded at startup. Restoring a snapshot taken before Save must not reopen saved content through an old draft. Owners receive a notice; sealed secrets remain stored.
- Deletion removes live profile/plan/secret rows, policy/contact bindings and scoped credentials. Minimal tombstones contain only scope IDs, scope kind and journal operation ID. The encrypted journal remains necessary for restore. Independent account and operational abuse records have separate lifetimes; cleanup jobs/object records remain only through cleanup.
- A retained profile can create a fresh plan after plan deletion. Its valid recovery credential can also initiate ownership transfer to a new account through a fresh empty paused plan; deleted IDs and data are not revived.
- Unknown Telegram send results are not automatically retried. Confirmed successful sends cannot be downgraded by a racing worker-loss result.
- The journal has one writer enforced by an OS lock. Capacity checks fail before an append can make future replay unreadable.

## Environment and remaining admission work

Local PostgreSQL was **17.11** and Garage **2.4.0**. Compose and CI target PostgreSQL **18.6**. Docker daemon access was denied, so the actual image build, Compose startup, container UID/network behavior and complete Docker backup wrapper remain unexecuted. Resolving a pinned manifest does not validate the image contents or replace an image vulnerability scan. The configured GitHub Actions workflow was not run remotely.

No real Telegram account, token or recipient was contacted. A dedicated synthetic test bot is still needed to verify Telegram-side copy, save, message-removal and uncertain-delivery behavior.

Production mode remains disabled. The selected threshold implementation and integrated cryptographic lifecycle still require the independent review specified in the technical design. This code review, negative testing and advisory scan do not supply that certification. `synthetic` is an admission rule, not a detector for real secrets.

The validation adds deterministic concurrency and restore cases, but does not kill the process at every I/O instruction, simulate all filesystem failures, measure production load or establish RPO/RTO guarantees. Leader monitoring detects loss within a bounded interval and terminates the service; it is not a database generation fence. Runtime/migration SQL role separation and an image scan remain deployment admission work.

Inbox persistence is deduplicated and critical commands have semantic retry checks. The handled-event marker follows command execution; it is not one transaction enclosing every dialog effect. Do not infer universal exactly-once behavior after a crash. Deletion cannot recall recipients' copies or erase old backups/WAL. Zeroizing owned buffers does not guarantee that all HTTP/serialization/framework copies are erased.

Full-record administrative scans fail above 10,000 matches instead of returning partial exports. Paginated scans and measured operating budgets are required before increasing deployment scale. Low-level object export requires maintenance and completed I/O drain; the backup wrapper includes a 120-second drain. A live control journal must be preserved independently of older database snapshots.

See [operations](operations.md#development-and-verification) for reproducible commands. Generated credentials, database files and native binaries remain in ignored local directories. Temporary test services are stopped after validation.
