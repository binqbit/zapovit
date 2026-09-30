#!/usr/bin/env bash
set -euo pipefail
set -o noclobber
umask 077
# Pass an age recipients file (public keys) and a destination on a separate backup disk.
if [[ $# != 2 ]]; then echo 'usage: scripts/backup.sh AGE_RECIPIENTS BACKUP_DIRECTORY' >&2; exit 2; fi
command -v age >/dev/null
command -v python3 >/dev/null
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
recipients=$(realpath "$1")
destination=$(realpath "$2")
stage=$(mktemp -d "$destination/.zapovit-backup.XXXXXXXX")
chmod 700 "$stage"
trap 'status=$?; if (( status != 0 )); then echo "Backup failed; maintenance is retained. Private evidence: $stage" >&2; fi' EXIT
stamp=$(date -u +%Y%m%dT%H%M%SZ)
archive="$destination/zapovit-$stamp.tar.age"
[[ ! -e "$archive" && ! -e "$archive.partial" && ! -e "$archive.sha256" ]] || {
  echo 'Backup destination already exists.' >&2
  exit 1
}
python3 "$script_dir/backup_env.py" capture "$stage/runtime.env" "$stage/deployment.json"
runtime_compose() {
  python3 "$script_dir/backup_env.py" run "$stage/runtime.env" "$@"
}
# A failure deliberately leaves maintenance enabled. Inspect the failed stage, then resume explicitly.
session=$(runtime_compose exec -T app /usr/local/bin/zapovit backup-begin)
[[ "$session" =~ ^[a-f0-9]{8}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{12}$ ]] || {
  echo 'Backup session identifier was not returned; maintenance remains enabled.' >&2
  exit 1
}
printf '%s\n' "$session" > "$stage/backup-session"
runtime_compose exec -T app /usr/local/bin/zapovit backup-drain --session "$session" --timeout-seconds 180
runtime_compose exec -T db pg_dump -U zapovit -d zapovit -Fc > "$stage/postgres.dump"
runtime_compose run --pull never --no-build --rm --no-deps --user 0:0 --volume "$stage:/backup" app export-objects --directory /backup/objects
# Capture the journal after the dump: its controls may be newer and are replayed
# idempotently after restore. The dumped maintenance row has no completion checkpoint.
runtime_compose run --pull never --no-build --rm --no-deps --user 0:0 --volume "$stage:/backup" app backup-snapshot --session "$session" --directory /backup/journal
runtime_compose run --pull never --no-build --rm --no-deps --user 0:0 --entrypoint /bin/chown --volume "$stage:/backup" app -R "$(id -u):$(id -g)" /backup
tar -C "$stage" -cf - . | age -R "$recipients" > "$archive.partial"
ln "$archive.partial" "$archive"
rm -- "$archive.partial"
sha256sum "$archive" > "$archive.sha256"
python3 - "$archive" "$archive.sha256" "$destination" <<'PY'
import os
import sys
for path in sys.argv[1:]:
    descriptor = os.open(path, os.O_RDONLY)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)
PY
runtime_compose exec -T app /usr/local/bin/zapovit backup-complete --session "$session"
rm -rf -- "$stage"
echo "Encrypted backup written: $archive"
# Retention is explicit: schedule daily, retain 7 days, and verify a newer backup before deleting older ones.
