# Implementation verification

## Conversational creation follow-up on 30 September 2026

Creation now uses one question at a time: recovery-key acknowledgment and explicit contact confirmation, followed by six numbered draft stages (guardians, recipients, direct content input, quorum, timing, review). Text and documents are accepted directly at the content question. Each stage transition, Back and explicit resume sends a new message; selection changes stay within the same question. Previous inline keyboards are retired using Telegram's [editMessageReplyMarkup](https://core.telegram.org/bots/api#editmessagereplymarkup). Draft mutations also validate the current question message ID, so retiring markup is not the authority check. Home and advanced management remain available separately.

The integration preserves the existing architecture and Engine validation. `adapters/bot/setup.rs` derives preparation from authorized account/plan/contact state; `drafts.rs` uses the existing DraftSession to retain answers and the current question ID. `bot.rs` routes direct content through the existing encrypted append/upload path, while `menus.rs` and `telegram.rs` handle fresh questions and button retirement. No migration, new plaintext content store, changed deadline, or automatic plan activation was introduced. Readiness notifications link to the next relevant question; a plan stopped by its owner stays stopped until explicitly enabled.

Public surfaces changed: `/start` resumes Setup plans, `/continue` asks the current question, creation/invitation/confirmation/content/readiness messages form one guided journey, and the core draft stages have a different order. [README](../README.md), [operations](operations.md#bot-flow), [production plan](production-plan.md) and both locales were synchronized. The earlier descriptions of every transition editing in place, choosing a content type first, and timing before content were corrected against the implemented handlers.

Validation used Rust 1.98.1, isolated PostgreSQL 17.11 and loopback Telegram with synthetic data.

| Check | Result |
| --- | --- |
| Default workspace | 51 tests passed |
| PostgreSQL pipeline | All 98 cases passed together, including six conversational regressions, draft/contact navigation, source cleanup and STOP under saturated ordinary admission |
| App database tests | Both leadership-loss and journal initialization cases passed |
| Static and documentation checks | Formatting, locked all-target Clippy with warnings denied, diff whitespace, 304-key locale parity and local documentation targets passed; the documentation drift helper's 20 unmatched tokens were reviewed as internal SQL/constants |

The final combined run covers **151 distinct Rust tests**, with an unchanged source fingerprint. Commands were `cargo test --workspace --locked`, `cargo test -p adapters --test pipeline --locked -- --ignored --test-threads=1`, `cargo test -p app --locked -- --ignored --test-threads=1`, `cargo fmt --all -- --check`, and `cargo clippy --workspace --all-targets --locked -- -D warnings`. Final evidence is in ignored `.agent-workspace/artifacts/pg-l9e2cyom/report.json` and `conversation-clippy-final.log`. Focused navigation (`pg-ffkgfav4`, 22 cases), transport (`pg-oy4pvq0r`, four cases), readiness (`pg-1kh70uoh`, two cases) and admission-fixture (`pg-hh99ngi7`, one case) runs also passed. Test services were stopped; successful disposable databases were removed and first-failure evidence retained. Backup/Garage contracts were not rerun for this adapter change; the existing external release gates below still apply.

The new tests use distinct Telegram message IDs and assert new messages, stale-question rejection, direct text/document capture, source cleanup, pre-key/wrong-stage guidance and explicit activation. Independent review also identified draft input remaining active after Home/Settings, misleading readiness when secrets have different states, and a used confirmation preserving a separate input prompt. Navigation now suspends capture, readiness identifies the pending secret, and replay clears the input prompt. Completion copy requires the latest displayed secret to be Armed without blockers; otherwise the actual secret states are shown. A regression reproduced the older-ready/newer-paused bug before this fix, then passed for five latest-secret states. Other regressions cover suspended input and an older pending secret alongside a newer ready one.

First-failure evidence is retained locally. An initial document test used the final file ciphertext estimate instead of the actual serialized draft-object size; it now checks the object ledger and decrypts the stored content. The first full pipeline run passed 94 cases and failed one recovery-admission fixture assertion. Its retained database shows the requests crossed a minute boundary: the fixture exhausted the previous minute's counter, then the real limiter correctly reset it. The fixture now pins that exhausted-quota precondition for its routing/cleanup assertion; production rate-limit behavior is unchanged. No real Telegram client or deployment was contacted; actual client rendering remains unverified.

## Telegram UX correction on 30 September 2026

This follow-up implements the user's revised interaction requirements: named contacts, contextual confirmation, resumable setup, a hideable Home/Continue keyboard, formatted menu text, prompt cleanup of saved source messages, and explicit confirmation before ordinary STOP. The earlier public-service matrix below remains a separate historical checkpoint.

The existing domain → application/ports → adapters → runtime boundaries remain unchanged. `application/account_names.rs` owns encrypted presentation metadata and authorized projections; application draft transactions own cleanup obligations. `adapters/bot` owns navigation, compact labels and confirmation screens, while `adapters/telegram.rs` owns native UTF-16 message entities and keyboard payloads. PostgreSQL routing preserves reserved admission for the new STOP confirmation/cancel actions. Stored names do not grant authority. Existing migrations remain unchanged; optional JSON fields support older accounts and drafts.

The public surfaces and documentation changed together: [README](../README.md) summarizes setup; [operations](operations.md#bot-flow) specifies navigation, names, source cleanup and confirmation; the [production plan](production-plan.md) records the later STOP decision. Earlier immediate-STOP/no-dialog descriptions were replaced. Verified recovery-key stop retains its credential-validated behavior. Telegram supports [native text entities](https://core.telegram.org/bots/api#messageentity), [hideable reply keyboards](https://core.telegram.org/bots/api#replykeyboardmarkup), and [private-chat source-message deletion with limits](https://core.telegram.org/bots/api#deletemessage). Names containing markup characters remain literal text.

Validation used Rust 1.98.1, isolated PostgreSQL 17.11 and loopback Telegram with synthetic data.

| Check | Result |
| --- | --- |
| Default workspace | 51 tests passed, including encrypted-account backward compatibility, UTF-16 formatting and bounded Unicode labels |
| PostgreSQL pipeline | All 91 cases passed together, including eight source-cleanup regressions, named-contact/resume navigation and STOP confirmation under saturated ordinary admission |
| App database tests | Both leadership-loss and automatic journal initialization tests passed |
| Final copy follow-up | After the last English/Ukrainian wording corrections, all 51 default tests plus three people-navigation and one menu-navigation cases passed again |
| Static and documentation checks | Formatting, locked all-target Clippy with warnings denied, diff whitespace, local documentation file targets and the documentation drift helper passed; the helper's five unmatched tokens are internal SQL/constants |

This follow-up exercised **144 distinct Rust tests**. Commands were `cargo test --workspace --locked`, `cargo test -p adapters --test pipeline --locked -- --ignored --test-threads=1`, `cargo test -p app --locked -- --ignored --test-threads=1`, `cargo fmt --all -- --check`, and `cargo clippy --workspace --all-targets --locked -- -D warnings`. Local reports: `pg-77ge2rzq` (combined matrix), `pg-nyto6wua` (final contact/STOP navigation), `pg-atplqglx` (final localized menus), `ux-final-copy-workspace.log` and `ux-correction-clippy-final.log`. The combined matrix's source fingerprint changed only for the final locale wording; those changes have separate passing checks with stable fingerprints. Existing backup/Garage contracts were not rerun for this UX change.

All test services were stopped. Disposable databases from successful runs and three intermediate compile-only failures were removed; their logs and the original behavioral failure evidence were retained. Auxiliary resources remain Git-ignored in `.agent-workspace/`.

The original cleanup regression run failed four of six cases: cleanup waited for Save and expiry could leave messages without feedback. The expanded eight-case cleanup suite then passed after the transactional fix. A later navigation test reproduced the repeated-confirmation dead end; replay now shows the current contact card after authority checks, without confirming again. First-failure logs and subsequent results remain in ignored `.agent-workspace/artifacts/`; maintained regressions live under `crates/adapters/tests/pipeline_cases/`.

No real Telegram account or deployment was contacted. Loopback payload/state tests validate behavior and entity offsets, not actual Android/iOS/Desktop rendering. The public-release gates listed below remain in effect.

## Public-service implementation on 30 September 2026

Implemented the accepted [production plan](production-plan.md) within the existing domain → application/ports → adapters → runtime structure. The Telegram adapter projects Engine readiness and uses Engine-authorized operations; button visibility never grants authority. This iteration adds private metadata, contact/invitation lifecycle, independent draft/prompt sessions, block editing, explicit sealing/deletion, scoped resume, recipient views and operation receipts. Runtime work covers ordering, leases, quarantine, resource reservations, bounded scans, freshness, backup sessions and mirrored journal durability. Migrations `0002`–`0004` preserve the original `0001` checksum.

The current validation uses Rust 1.98.1, PostgreSQL 17.11, native Garage 2.4.0, loopback Telegram and synthetic fixtures. No real Telegram recipient or deployment credentials were used.

| Check | Evidence and scope |
| --- | --- |
| Default workspace | 48 tests passed, including cryptography, localization, journal fault cases, shared I/O deadlines and Telegram contracts |
| PostgreSQL integration | All 75 cases passed together, including menu guidance, recovery admission, full queue backpressure and cached-authority revalidation |
| App database tests | Leadership-loss and automatic journal initialization tests passed |
| Backup protocol | Five PostgreSQL tests passed, including original-schema upgrade, session/restore fencing and 10,002-row export with paged and legacy manifest import |
| Real Garage | Two additional Rust contracts passed: 1 MiB PUT/HEAD/GET/bounded GET/DELETE, and exact-byte backup/import with tampered-manifest rejection; storage bootstrap and its idempotent repeat also passed |
| Static/dependency checks | Formatting, all-target locked Clippy with warnings denied, cargo-audit and cargo-deny passed; deny retains permitted duplicate-version/unmatched-license warnings |
| Operational scripts/docs | Ten Python backup tests, shell/Python syntax, merged Compose configuration, diff whitespace and local documentation links passed |

This covers **133 distinct Rust tests and ten Python tests**, counting the separately executed backup, Garage and journal-scale cases once. The final workspace + 75-case pipeline + two app cases ran together with an unchanged source fingerprint; the other contracts have their own retained reports. No named Rust test remains covered only by its default ignored status.

Pre-commit review then corrected only English/Ukrainian copy: the missing contact-details fallback and references to the renamed People menu. Locale validation passed again, and all 132 literal Telegram translation calls were checked for missing keys. These copy corrections do not imply another complete database test run.

The initial integrated run passed 55/58 tests. One failure exposed a missing inactivity deadline on the secret card and was fixed in the UI. Two historical control fixtures assumed that an empty plan or an individually stopped secret could be resumed through plan activation; the fixtures now prepare a real ready secret or explicitly resume that secret, preserving their safety assertions. Separate review found and fixed stale draft callbacks cancelling unrelated prompts, late credentials being interpreted as draft content, non-atomic upload enqueue/state transitions, completed mutation results hidden by expired UI state, and explicit recipient retries blocked by their own routed inbox barrier. Original failures and subsequent passing evidence are retained locally.

A fresh advisory scan found [RUSTSEC-2026-0285 / the upstream rustls advisory](https://github.com/rustls/rustls/security/advisories/GHSA-2mjx-qc3c-rqvc). The lockfile now uses rustls **0.23.45**; the subsequent scan reports no vulnerabilities or advisory warnings. This is a dependency correction, not an independent security assessment of Zapovit.

The journal scale probe appended and replayed **10,000 records / 6,278,830 bytes** using two fsynced local copies. Append phase: 119.11 seconds. First/last 100 appends had p95 **9.714 / 31.452 ms** (p50 7.270 / 26.011 ms). This was a debug build on a concurrently used filesystem, with both copies on one host. It demonstrates the bounded incremental append path at this workload, not a production latency SLO or independent-host durability.

Local evidence lives in ignored `.agent-workspace/artifacts/`: the final full matrix (`pg-k0q7svzj`), earlier integration checkpoint (`pg-bmjca9b7`), final navigation (`pg-gqcw_o2y`), native Garage (`pg-v9_w7eg2`), backup sessions (`pg-_c5_vmii`, `pg-crxeppby`), runtime regressions (`pg-qewgc6je`, `pg-1nm6vog_`), journal probe and static/dependency logs. These are private local artifacts, not prerequisites for CI. Maintained tests and the disposable Compose runner are tracked. The native Garage executable was byte-for-byte verified against the binary extracted from the repository's digest-pinned OCI image; provenance is retained with the local binary. All temporary database/storage services were stopped after their checks.

**Still required before a public release:** the configured Docker/Compose/age recovery job and image scans must actually pass; Docker daemon access here was denied. The real Garage contracts do not replace container UID/network/mount or complete encrypted-wrapper testing. PostgreSQL 18.6 remains the CI target. Real Telegram Android/iOS/Desktop usability, novice-user trials, independent threshold/security review, an incident owner/support contact/alert routing, independently durable journal storage with a current witness, host-loss recovery and measured RPO/RTO/load remain release gates. Advisory UI status still shares a cursor lock with dispatch readiness; its polling impact must be included in public-load measurements. `DATA_MODE=synthetic` remains enforced.

## Earlier repository and Telegram UX review on 30 September 2026

Verified with Rust 1.98.1, native PostgreSQL 17.11, loopback Telegram mocks and synthetic fixtures. The pinned toolchain was selected explicitly because the host's default Cargo/Rust binaries were 1.97.1.

| Check | Result |
| --- | --- |
| Formatting and lint | `cargo fmt --all -- --check` and all-target locked Clippy with warnings denied pass |
| Default workspace suite | 42 passing tests; 42 external-service tests ignored by this command |
| PostgreSQL pipeline | All 39 ignored cases pass across the full run and focused follow-ups, including three draft UX and four late-delivery regressions |
| PostgreSQL application cases | Both leadership-loss and automatic journal startup tests pass |
| Backup helpers and deployment syntax | Nine Python tests, `docker compose config --quiet`, and `bash -n scripts/backup.sh` pass |
| Repository hygiene | Diff whitespace and local documentation links checked; `.agent-workspace/` is ignored by Git and excluded from Docker build contexts |

This covers **83 distinct Rust tests** and **nine Python tests**. The full PostgreSQL pipeline initially passed 37 cases and failed two existing assertions that still expected the old generic error text/silent rate limit. Updated expectations preserve the authorization/state assertions; all four transport-validation cases and the menu-navigation case then passed on the final source. The initial failures and follow-up logs are retained in the local workspace.

The late-delivery defect was reproduced before its fix: two of four new lifecycle regressions failed. After the fix all four pass. A late Unknown/Permanent/Sent result remains recorded without lifting a secret STOP, mutating a newer case, or reviving a deleted scope.

Telegram verification covers draft/step/revision binding, preserving selections after Settings, correction of invalid timing, explicit cancellation/deletion scope, UTC status dates, bounded rate-limit feedback, ForceReply payloads and localized command registration. It does not demonstrate rendering in actual Telegram clients.

Local helpers create and stop their own private PostgreSQL clusters; logs and reports live in `.agent-workspace/`. Maintained tests remain tracked and runnable without that directory. No real Telegram account was contacted. Garage integration, the Docker image/Compose runtime and a complete backup/restore were not executed in this review; Docker daemon access was denied. PostgreSQL 17 checks do not replace the pinned PostgreSQL 18.6 CI target. Newly identified operational work is recorded in the [roadmap](roadmap.md).

## Earlier validation

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
