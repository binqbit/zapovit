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
stamp=$(date -u +%Y%m%dT%H%M%SZ)
archive="$destination/zapovit-$stamp.tar.age"
[[ ! -e "$archive" && ! -e "$archive.partial" && ! -e "$archive.sha256" ]] || {
  echo 'Backup destination already exists.' >&2
  exit 1
}
python3 "$script_dir/backup_env.py" capture "$stage/runtime.env"
runtime_compose() {
  python3 "$script_dir/backup_env.py" run "$stage/runtime.env" "$@"
}
# A failure deliberately leaves maintenance enabled. Inspect the failed stage, then resume explicitly.
runtime_compose exec -T app /usr/local/bin/zapovit maintenance --enabled
sleep 120
runtime_compose exec -T db pg_dump -U zapovit -d zapovit -Fc > "$stage/postgres.dump"
mkdir "$stage/journal"
runtime_compose cp app:/var/lib/zapovit/journal/control.jsonl "$stage/journal/control.jsonl"
runtime_compose cp app:/var/lib/zapovit/journal/anchor.json "$stage/journal/anchor.json"
runtime_compose run --pull never --rm --no-deps --user 0:0 --volume "$stage:/backup" app export-objects --directory /backup/objects
runtime_compose run --pull never --rm --no-deps --user 0:0 --volume "$stage:/backup:ro" app verify-journal --directory /backup/journal
runtime_compose run --pull never --rm --no-deps --user 0:0 --entrypoint /bin/chown --volume "$stage:/backup" app -R "$(id -u):$(id -g)" /backup
tar -C "$stage" -cf - . | age -R "$recipients" > "$archive.partial"
ln "$archive.partial" "$archive"
rm -- "$archive.partial"
sha256sum "$archive" > "$archive.sha256"
runtime_compose exec -T app /usr/local/bin/zapovit maintenance
rm -rf -- "$stage"
echo "Encrypted backup written: $archive"
# Retention is explicit: schedule daily, retain 7 days, and verify a newer backup before deleting older ones.
