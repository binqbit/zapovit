#!/usr/bin/env python3
"""Capture effective Compose credentials without printing inspected environments."""

import json
import os
from pathlib import Path
import re
import subprocess
import sys
from urllib.parse import unquote, urlsplit


APP_VARIABLES = (
    "DATA_MODE",
    "DATABASE_URL",
    "TELEGRAM_BOT_TOKEN",
    "KEK_KEYRING",
    "VERIFIER_KEYRING",
    "JOURNAL_KEYRING",
    "JOURNAL_DIR",
    "S3_ENDPOINT",
    "S3_REGION",
    "S3_BUCKET",
    "S3_ACCESS_KEY_ID",
    "S3_SECRET_ACCESS_KEY",
    "HEALTH_BIND",
    "TELEGRAM_API_BASE",
    "WORKERS",
    "DATABASE_POOL",
)
CAPTURED_VARIABLES = APP_VARIABLES + (
    "APP_UID",
    "APP_GID",
    "DATABASE_PASSWORD",
    "GARAGE_RPC_SECRET",
    "GARAGE_ADMIN_TOKEN",
)


class CaptureError(Exception):
    """An error whose message contains no captured configuration values."""


def docker_output(arguments):
    try:
        result = subprocess.run(
            ["docker", *arguments], capture_output=True, check=False, timeout=30
        )
    except (OSError, subprocess.TimeoutExpired):
        raise CaptureError("Could not inspect the running Compose services.") from None
    if result.returncode or len(result.stdout) > 1024 * 1024:
        raise CaptureError("Could not inspect the running Compose services.")
    return result.stdout


def inspect_service(service):
    ids = docker_output(
        ["compose", "ps", "--status", "running", "--quiet", service]
    ).split()
    if len(ids) != 1 or not re.fullmatch(rb"[a-f0-9]{12,64}", ids[0]):
        raise CaptureError(f"Expected exactly one running {service} container.")
    try:
        records = json.loads(docker_output(["inspect", ids[0].decode("ascii")]))
        if len(records) != 1 or records[0]["State"]["Running"] is not True:
            raise ValueError
        return records[0]["Config"]
    except (KeyError, TypeError, ValueError):
        raise CaptureError(f"Invalid inspection result for {service}.") from None


def environment(config):
    result = {}
    for entry in config.get("Env", []):
        if not isinstance(entry, str) or "=" not in entry:
            raise CaptureError("Invalid container environment.")
        name, value = entry.split("=", 1)
        if name in result:
            raise CaptureError("Duplicate variable in container environment.")
        result[name] = value
    return result


def required(values, name):
    value = values.get(name)
    if not isinstance(value, str) or not value or "\0" in value:
        raise CaptureError(f"Running container is missing required {name}.")
    return value


def captured_values(app, db, garage):
    app_env = environment(app)
    result = {name: required(app_env, name) for name in APP_VARIABLES}
    identity = app.get("User", "")
    if not re.fullmatch(r"[0-9]+:[0-9]+", identity):
        raise CaptureError("The app container must use a numeric UID:GID.")
    result["APP_UID"], result["APP_GID"] = identity.split(":", 1)
    db_env = environment(db)
    result["DATABASE_PASSWORD"] = required(db_env, "POSTGRES_PASSWORD")
    validate_topology(result, db_env)
    garage_env = environment(garage)
    for name in ("GARAGE_RPC_SECRET", "GARAGE_ADMIN_TOKEN"):
        result[name] = required(garage_env, name)
    return result


def validate_topology(values, db_env):
    # This backup wrapper dumps the Compose database, not an arbitrary external
    # database that a separately configured app could happen to reference.
    try:
        database = urlsplit(values["DATABASE_URL"])
        valid = (
            database.scheme in ("postgres", "postgresql")
            and database.hostname == "db"
            and database.port in (None, 5432)
            and unquote(database.username or "") == "zapovit"
            and unquote(database.password or "") == values["DATABASE_PASSWORD"]
            and unquote(database.path) == "/zapovit"
            and not database.query
            and not database.fragment
            and db_env.get("POSTGRES_USER") == "zapovit"
            and db_env.get("POSTGRES_DB") == "zapovit"
            and values["JOURNAL_DIR"] == "/var/lib/zapovit/journal"
        )
    except ValueError:
        valid = False
    if not valid:
        raise CaptureError("Backup requires the standard Compose database and journal paths.")


def dotenv(values):
    # Double quotes support escaped backslashes, quotes and newlines. Compose
    # consumes doubled dollars literally instead of interpolating secret text.
    return "# Effective running configuration captured for restore. Keep private.\n" + "".join(
        name + "=" + json.dumps(values[name].replace("$", "$$"), ensure_ascii=False) + "\n"
        for name in CAPTURED_VARIABLES
    )


def capture(destination):
    values = captured_values(
        inspect_service("app"), inspect_service("db"), inspect_service("object-storage")
    )
    # Exclusive creation also refuses a pre-existing symlink. The parent backup
    # stage is private; never overwrite another recovery configuration.
    descriptor = os.open(destination, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(descriptor, "w", encoding="utf-8", newline="\n") as output:
        output.write(dotenv(values))


def run_with_runtime(source, arguments):
    # Shell variables normally override --env-file. Strip only the captured
    # configuration names, preserving Compose project/context selection.
    inherited = os.environ.copy()
    for name in CAPTURED_VARIABLES:
        inherited.pop(name, None)
    inherited.pop("COMPOSE_ENV_FILES", None)
    try:
        return subprocess.run(
            ["docker", "compose", "--env-file", str(Path(source).resolve()), *arguments],
            env=inherited,
            check=False,
        ).returncode
    except OSError:
        raise CaptureError("Could not run Docker Compose with the captured configuration.") from None


def main(arguments):
    if len(arguments) == 2 and arguments[0] == "capture":
        capture(arguments[1])
        return 0
    if len(arguments) >= 3 and arguments[0] == "run":
        return run_with_runtime(arguments[1], arguments[2:])
    raise CaptureError("usage: backup_env.py capture PATH | run PATH COMPOSE_ARGUMENTS...")


if __name__ == "__main__":
    try:
        sys.exit(main(sys.argv[1:]))
    except CaptureError as error:
        print(f"Backup configuration step failed: {error}", file=sys.stderr)
        sys.exit(1)
    except (OSError, ValueError, TypeError):
        # Do not echo subprocess stderr, inspected JSON or exception arguments:
        # any of them could contain a credential.
        print("Backup configuration step failed; verify running services and private output path.", file=sys.stderr)
        sys.exit(1)
