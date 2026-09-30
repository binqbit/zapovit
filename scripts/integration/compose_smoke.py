#!/usr/bin/env python3
"""Build and exercise an isolated Compose stack, Garage, encrypted backup and restore.

Requires Docker Compose, Rust 1.98.1, age and age-keygen. Creates only uniquely
named projects and private .agent-workspace artifacts; never reads a project .env.
The fake Bot API is entirely local and accepts only the synthetic fixture token.
"""

import json
import os
from pathlib import Path
import secrets
import subprocess
import sys
import tempfile
import time
from urllib.request import Request, urlopen


ROOT = Path(__file__).resolve().parents[2]


def main():
    os.umask(0o077)
    base = ROOT / ".agent-workspace" / "artifacts"
    base.mkdir(parents=True, exist_ok=True)
    stage = Path(tempfile.mkdtemp(prefix="compose-", dir=base))
    project = "zapovit-ci-" + secrets.token_hex(6)
    restored = project + "-restore"
    app_image = project + ":app"
    fixture_image = project + ":fixture"
    env = os.environ.copy()
    # An exported live configuration must not override this isolated fixture.
    for name in list(env):
        if name.startswith(("COMPOSE_", "APP_", "TELEGRAM_", "DATABASE_", "S3_", "GARAGE_", "JOURNAL_", "KEK_", "VERIFIER_", "TEST_")):
            env.pop(name, None)
    env.update({
        "COMPOSE_FILE": os.pathsep.join(str(ROOT / p) for p in ("compose.yaml", "scripts/integration/compose.yaml")),
        "COMPOSE_PROJECT_NAME": project,
        "APP_IMAGE": app_image,
        "APP_PULL_POLICY": "never",
        "FIXTURE_IMAGE": fixture_image,
    })
    config = stage / "synthetic.env"
    report = {"scope": "Disposable Docker, PostgreSQL, Garage, local Bot API, encrypted backup and restore", "checks": []}
    active_projects = []

    def run(label, arguments, *, input=None, environment=None, allow_failure=False, timeout=900):
        started = time.monotonic()
        result = subprocess.run(arguments, input=input, capture_output=True, cwd=ROOT,
                                env=environment or env, timeout=timeout)
        (stage / (label + ".log")).write_bytes(result.stdout + result.stderr)
        report["checks"].append({"name": label, "exit_code": result.returncode, "seconds": round(time.monotonic() - started, 2)})
        if result.returncode and not allow_failure:
            raise RuntimeError(label + " failed; see its private artifact log")
        return result

    def compose(label, *arguments, **kwargs):
        return run(label, ["docker", "compose", "--env-file", str(config), *arguments], **kwargs)

    def sql(label, statement):
        return compose(label, "exec", "-T", "db", "psql", "-XAt", "-U", "zapovit", "-d", "zapovit", "-v", "ON_ERROR_STOP=1", "-c", statement).stdout.decode().strip()

    def helper(label, *arguments, mounts=(), allow_failure=False):
        return compose(label, "run", "--pull", "never", "--no-build", "--rm", "--no-deps", "--user", "0:0",
                       *[item for mount in mounts for item in ("--volume", mount)], "app", *arguments,
                       allow_failure=allow_failure)

    def ready(label, endpoint="ready"):
        deadline = time.monotonic() + 180
        while time.monotonic() < deadline:
            result = compose(label, "exec", "-T", "app", "/usr/local/bin/zapovit", "healthcheck", "--url", "http://127.0.0.1:8080/" + endpoint, allow_failure=True, timeout=10)
            if result.returncode == 0:
                return
            time.sleep(2)
        raise RuntimeError(label + " did not become ready")

    try:
        # Fail immediately if the daemon is inaccessible; no alternate socket or privilege escalation.
        run("docker-access", ["docker", "info", "--format", "{{.ServerVersion}}"], timeout=30)
        run("build-app", ["docker", "build", "--target", "release", "-t", app_image, "."], timeout=1800)
        if env.get("GITHUB_OUTPUT"):
            with open(env["GITHUB_OUTPUT"], "a", encoding="utf-8") as output:
                output.write("image=" + app_image + "\n")
        run("build-fixture", ["docker", "build", "--target", "integration-fixture", "-t", fixture_image, "."], timeout=1800)
        run("generate-env", ["docker", "run", "--rm", "--user", f"{os.getuid()}:{os.getgid()}", "--volume", f"{stage}:/output", app_image,
                              "generate-env", "--output", "/output/synthetic.env"])
        text = config.read_text().replace("TELEGRAM_BOT_TOKEN=", "TELEGRAM_BOT_TOKEN=1:synthetic", 1)
        text = text.replace("TELEGRAM_API_BASE=https://api.telegram.org", "TELEGRAM_API_BASE=http://fake-telegram:8081")
        config.write_text(text)
        compose("pull-dependencies", "pull", "db", "object-storage")
        active_projects.append(project)
        compose("start-source", "up", "-d", "--no-build", "--pull", "never")
        ready("source-ready")
        port = compose("fixture-port", "port", "fake-telegram", "8081").stdout.decode().strip()
        update = {"update_id": 1, "message": {"message_id": 1, "date": int(time.time()), "chat": {"id": 701, "type": "private", "first_name": "Synthetic"},
                  "from": {"id": 701, "is_bot": False, "first_name": "Synthetic", "language_code": "en"}, "text": "/start"}}
        with urlopen(Request("http://" + port + "/test/update", data=json.dumps(update).encode(), headers={"Content-Type": "application/json"}), timeout=5) as response:
            assert response.status == 200
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            with urlopen("http://" + port + "/test/message-count", timeout=5) as response:
                if json.load(response)["count"]:
                    break
            time.sleep(0.5)
        else:
            raise RuntimeError("Running bot did not answer a synthetic /start update")
        report["checks"].append({"name": "running-bot-start", "exit_code": 0})

        # Separate schema-scoped integration fixtures use a clearly named disposable database.
        sql("create-contract-db", "CREATE DATABASE zapovit_test")
        db_port = compose("database-port", "port", "db", "5432").stdout.decode().strip()
        s3_port = compose("garage-port", "port", "object-storage", "3900").stdout.decode().strip()
        values = dict(line.split("=", 1) for line in config.read_text().splitlines() if line and not line.startswith("#") and "=" in line)
        values = {key: value.strip("'") for key, value in values.items()}
        credentials = stage / "s3-credentials.json"
        credentials.write_text(json.dumps({"access_key_id": values["S3_ACCESS_KEY_ID"], "secret_access_key": values["S3_SECRET_ACCESS_KEY"]}))
        tests_env = env | {"TEST_DATABASE_URL": f"postgresql://zapovit:{values['DATABASE_PASSWORD']}@{db_port}/zapovit_test",
                           "TEST_S3_ENDPOINT": "http://" + s3_port, "TEST_S3_CREDENTIALS_FILE": str(credentials)}
        run("garage-and-backup-contracts", ["cargo", "test", "-p", "adapters", "--test", "backup_protocol", "--locked", "--", "--ignored", "--test-threads=1"], environment=tests_env)

        identity = stage / "age-identity"
        run("age-keygen", ["age-keygen", "-o", str(identity)])
        recipient = run("age-recipient", ["age-keygen", "-y", str(identity)]).stdout
        recipients = stage / "age-recipients"
        recipients.write_bytes(recipient)
        backup_dir = stage / "backups"
        backup_dir.mkdir()
        before = sql("hold-before-backup", "SELECT hold_until FROM telegram_cursor WHERE singleton")
        # backup_env inspects precisely this unique project, and backup.sh pins helpers to its running image.
        run("encrypted-backup", ["bash", "scripts/backup.sh", str(recipients), str(backup_dir)])
        after = sql("hold-after-backup", "SELECT hold_until FROM telegram_cursor WHERE singleton")
        if before != after or sql("backup-completed", "SELECT enabled OR restore_required FROM maintenance WHERE singleton") != "f":
            raise RuntimeError("Routine backup changed a hold or retained its maintenance gate")
        archive = next(backup_dir.glob("*.tar.age"))
        plain = run("age-decrypt", ["age", "-d", "-i", str(identity), str(archive)]).stdout
        # The archive was just created by this fixture; extract using system tar into its private destination.
        extracted = stage / "extracted"
        extracted.mkdir()
        run("extract-backup", ["tar", "-xf", "-", "-C", str(extracted)], input=plain)
        images = json.loads((extracted / "deployment.json").read_text())["images"]
        actual = run("running-image", ["docker", "image", "inspect", "--format", "{{.Id}}", app_image]).stdout.decode().strip()
        if images["app"] != actual:
            raise RuntimeError("Backup did not capture the running application image")
        # Freeze the source before reading its CURRENT witness. It is deliberately
        # copied separately from the archive; production needs an independent failure domain.
        helper("source-held", "maintenance", "--enabled")
        compose("stop-source-app", "stop", "app")
        witness = stage / "current-journal"
        witness.mkdir()
        compose("current-journal", "cp", "app:/var/lib/zapovit/journal/.", str(witness))

        env["COMPOSE_PROJECT_NAME"] = restored
        env["APP_IMAGE"] = images["app"]
        active_projects.append(restored)
        compose("start-restore-dependencies", "up", "-d", "--no-build", "--pull", "never", "db", "object-storage", "storage-init", "fake-telegram")
        compose("restore-storage-ready", "wait", "storage-init", timeout=180)
        # Wait for database health without starting migrate: pg_restore owns this empty schema.
        deadline = time.monotonic() + 90
        while time.monotonic() < deadline:
            if compose("restore-db-ready", "exec", "-T", "db", "pg_isready", "-U", "zapovit", allow_failure=True).returncode == 0:
                break
            time.sleep(1)
        compose("restore-database", "exec", "-T", "db", "pg_restore", "--exit-on-error", "--no-owner", "-U", "zapovit", "-d", "zapovit", input=(extracted / "postgres.dump").read_bytes())
        helper("restore-require", "restore-require")
        rejected = helper("unverified-restore-rejected", "maintenance", allow_failure=True)
        if rejected.returncode == 0:
            raise RuntimeError("Unverified restore was allowed to leave maintenance")
        # Seed the empty volume using a pinned helper; its initial directory is owned by app UID.
        compose("restore-journal", "run", "--pull", "never", "--no-build", "--rm", "--no-deps", "--user", "0:0", "--entrypoint", "/bin/sh",
                "--volume", f"{witness}:/witness:ro", "app", "-ec", "cp /witness/control.jsonl /witness/anchor.json /var/lib/zapovit/journal/; chown 10001:10001 /var/lib/zapovit/journal/*")
        helper("restore-objects", "import-objects", "--directory", "/backup/objects", mounts=(f"{extracted}:/backup:ro",))
        helper("verify-restored-journal", "restore-verify", "--directory", "/var/lib/zapovit/journal", "--witness", "/witness/anchor.json", mounts=(f"{witness}:/witness:ro",))
        helper("restore-exit", "maintenance")
        remaining = int(sql("restore-hold", "SELECT hold_until-floor(extract(epoch FROM clock_timestamp()))::bigint FROM telegram_cursor WHERE singleton"))
        if remaining < 86300:
            raise RuntimeError("Restored state did not retain the 24-hour dispatch hold")
        restored_start_ms = int(sql("restore-start-time", "SELECT floor(extract(epoch FROM clock_timestamp())*1000)::bigint"))
        compose("start-restored-app", "up", "-d", "--no-build", "--pull", "never", "app")
        # A protected restore must be alive and polling while dispatch stays held.
        # /ready may legitimately encode the operational hold; do not equate it
        # with either liveness or permission to discard that hold.
        ready("restored-live", "live")
        deadline = time.monotonic() + 180
        while time.monotonic() < deadline:
            active = sql("restored-heartbeats-and-hold", f"SELECT EXISTS(SELECT 1 FROM telegram_cursor WHERE singleton AND last_poll_at>to_timestamp({restored_start_ms}/1000.0) AND last_scheduler_at>to_timestamp({restored_start_ms}/1000.0) AND hold_until-floor(extract(epoch FROM clock_timestamp()))::bigint>=86300) AND NOT EXISTS(SELECT 1 FROM runtime_health WHERE integrity_failure) AND NOT EXISTS(SELECT 1 FROM maintenance WHERE enabled OR restore_required)")
            if active == "t":
                break
            time.sleep(2)
        else:
            raise RuntimeError("Restored runtime did not advance both heartbeats while retaining its recovery hold")
        report["success"] = True
    except Exception as error:
        report["success"] = False
        report["failure"] = str(error)
    finally:
        for name in reversed(active_projects):
            env["COMPOSE_PROJECT_NAME"] = name
            try:
                compose(name + "-logs", "logs", "--no-color", allow_failure=True, timeout=30)
                cleanup = compose(name + "-cleanup", "down", "--volumes", "--remove-orphans", allow_failure=True, timeout=120)
                if cleanup.returncode:
                    report["success"] = False
            except Exception:
                report["success"] = False
                report.setdefault("cleanup_failed", []).append(name)
        (stage / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    print("Synthetic integration artifacts: " + str(stage))
    return 0 if report.get("success") else 1


if __name__ == "__main__":
    sys.exit(main())
