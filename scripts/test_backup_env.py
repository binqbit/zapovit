#!/usr/bin/env python3
"""Synthetic, daemon-free checks for effective backup configuration capture."""

import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import backup_env


def fixtures():
    values = {name: "synthetic" for name in backup_env.APP_VARIABLES}
    values["DATABASE_URL"] = "postgresql://zapovit:synthetic@db:5432/zapovit"
    values["JOURNAL_DIR"] = "/var/lib/zapovit/journal"
    app = {
        "User": "10001:10002",
        "Env": [f"{name}={value}" for name, value in values.items()]
        + ["UNRELATED_SECRET=must-not-be-captured"],
    }
    db = {
        "Env": [
            "POSTGRES_USER=zapovit",
            "POSTGRES_DB=zapovit",
            "POSTGRES_PASSWORD=synthetic",
        ]
    }
    garage = {"Env": ["GARAGE_RPC_SECRET=rpc", "GARAGE_ADMIN_TOKEN=admin"]}
    return app, db, garage


class CaptureTests(unittest.TestCase):
    def test_captures_effective_values_and_only_allowlisted_names(self):
        values = backup_env.captured_values(*fixtures())
        self.assertEqual(set(values), set(backup_env.CAPTURED_VARIABLES))
        self.assertEqual(values["APP_UID"], "10001")
        self.assertEqual(values["APP_GID"], "10002")
        self.assertEqual(values["DATABASE_PASSWORD"], "synthetic")
        self.assertEqual(values["GARAGE_ADMIN_TOKEN"], "admin")

    def test_missing_required_app_db_and_garage_values_fail_closed(self):
        for index, name in (
            (0, "KEK_KEYRING"),
            (1, "POSTGRES_PASSWORD"),
            (2, "GARAGE_ADMIN_TOKEN"),
        ):
            with self.subTest(name=name):
                records = fixtures()
                records[index]["Env"] = [
                    row for row in records[index]["Env"] if not row.startswith(name + "=")
                ]
                with self.assertRaises(backup_env.CaptureError):
                    backup_env.captured_values(*records)

    def test_rejects_wrong_database_before_backup(self):
        for url in (
            "postgresql://zapovit:wrong@db:5432/zapovit",
            "postgresql://zapovit:synthetic@external:5432/zapovit",
            "postgresql://another:synthetic@db:5432/zapovit",
            "postgresql://zapovit:synthetic@db:5432/another",
            "postgresql://zapovit:synthetic@db:5432/zapovit?host=external",
        ):
            with self.subTest(url=url):
                app, db, garage = fixtures()
                app["Env"] = [row for row in app["Env"] if not row.startswith("DATABASE_URL=")]
                app["Env"].append("DATABASE_URL=" + url)
                with self.assertRaises(backup_env.CaptureError):
                    backup_env.captured_values(app, db, garage)

    def test_database_password_decodes_percent_escapes(self):
        app, db, garage = fixtures()
        app["Env"] = [row for row in app["Env"] if not row.startswith("DATABASE_URL=")]
        app["Env"].append("DATABASE_URL=postgresql://zapovit:a%24%27%40%25@db:5432/zapovit")
        db["Env"] = [row for row in db["Env"] if not row.startswith("POSTGRES_PASSWORD=")]
        db["Env"].append("POSTGRES_PASSWORD=a$'@%")
        self.assertEqual(backup_env.captured_values(app, db, garage)["DATABASE_PASSWORD"], "a$'@%")

    def test_rejects_ambiguous_environment_and_container_identity(self):
        app, db, garage = fixtures()
        app["Env"].append("KEK_KEYRING=duplicate")
        with self.assertRaises(backup_env.CaptureError):
            backup_env.captured_values(app, db, garage)
        app, db, garage = fixtures()
        app["User"] = "named-user"
        with self.assertRaises(backup_env.CaptureError):
            backup_env.captured_values(app, db, garage)

    def test_capture_is_private_exclusive_and_refuses_symlink(self):
        with tempfile.TemporaryDirectory() as directory:
            destination = Path(directory) / "runtime.env"
            records = dict(zip(("app", "db", "object-storage"), fixtures()))
            with patch.object(backup_env, "inspect_service", side_effect=records.__getitem__):
                backup_env.capture(destination)
                self.assertEqual(stat.S_IMODE(destination.stat().st_mode), 0o600)
                contents = destination.read_bytes()
                with self.assertRaises(FileExistsError):
                    backup_env.capture(destination)
                self.assertEqual(destination.read_bytes(), contents)
                symlink = Path(directory) / "link.env"
                symlink.symlink_to(destination)
                with self.assertRaises(FileExistsError):
                    backup_env.capture(symlink)
                self.assertEqual(destination.read_bytes(), contents)

    def test_runner_prevents_shell_configuration_drift(self):
        inherited = {name: "host-drift" for name in backup_env.CAPTURED_VARIABLES}
        inherited.update(COMPOSE_PROJECT_NAME="chosen-project", PATH="path", COMPOSE_ENV_FILES="other.env")
        with patch.dict(os.environ, inherited, clear=True):
            with patch.object(backup_env.subprocess, "run", return_value=subprocess.CompletedProcess([], 0)) as run:
                self.assertEqual(backup_env.run_with_runtime("runtime.env", ["run", "app", "check-config"]), 0)
                effective = run.call_args.kwargs["env"]
                self.assertEqual(effective, {"COMPOSE_PROJECT_NAME": "chosen-project", "PATH": "path"})
                self.assertIn("--env-file", run.call_args.args[0])

    def test_docker_errors_do_not_echo_captured_stderr(self):
        result = subprocess.CompletedProcess([], 1, stdout=b"", stderr=b"secret-content")
        with patch.object(backup_env.subprocess, "run", return_value=result):
            with self.assertRaises(backup_env.CaptureError) as error:
                backup_env.docker_output(["inspect", "synthetic"])
        self.assertNotIn("secret-content", str(error.exception))

    def test_metadata_pins_helpers_to_the_running_image(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            records = dict(zip(("app", "db", "object-storage"), fixtures()))
            for index, record in enumerate(records.values()):
                record["_ImageID"] = "sha256:" + str(index + 1) * 64
            with patch.object(backup_env, "inspect_service", side_effect=records.__getitem__):
                backup_env.capture(root / "runtime.env", root / "deployment.json")
            with patch.object(backup_env.subprocess, "run", return_value=subprocess.CompletedProcess([], 0)) as run:
                backup_env.run_with_runtime(root / "runtime.env", ["run", "--no-build", "app", "check-config"])
                effective = run.call_args.kwargs["env"]
                self.assertEqual(effective["APP_IMAGE"], records["app"]["_ImageID"])
                self.assertEqual(effective["APP_PULL_POLICY"], "never")
            self.assertNotIn("TELEGRAM_BOT_TOKEN", (root / "deployment.json").read_text())

    @unittest.skipUnless(shutil.which("docker"), "Docker Compose CLI unavailable")
    def test_compose_round_trips_special_characters_without_a_daemon(self):
        probe = subprocess.run(["docker", "compose", "version"], capture_output=True, check=False)
        if probe.returncode:
            self.skipTest("Docker Compose CLI unavailable")
        samples = (
            "literal $TOKEN ${OTHER:-value} $$ dollars",
            "both 'single' and \"double\" quotes",
            "slash\\then'quote and final\\",
            "first line\nsecond\rline\ttab",
            "юнікод # = text",
            "\\n and \\r must stay literal",
        )
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            compose = root / "compose.yaml"
            compose.write_text(
                "services:\n  probe:\n    image: busybox\n    environment:\n"
                + "".join(f"      {name}: ${{{name}}}\n" for name in backup_env.CAPTURED_VARIABLES)
            )
            values = {
                name: samples[index % len(samples)]
                for index, name in enumerate(backup_env.CAPTURED_VARIABLES)
            }
            env_file = root / "runtime.env"
            env_file.write_text(backup_env.dotenv(values), encoding="utf-8")
            inherited = {name: value for name, value in os.environ.items() if name not in backup_env.CAPTURED_VARIABLES}
            result = subprocess.run(
                ["docker", "compose", "--env-file", str(env_file), "-f", str(compose), "config", "--format", "json"],
                capture_output=True,
                text=True,
                env=inherited,
                check=True,
                timeout=20,
            )
            actual = json.loads(result.stdout)["services"]["probe"]["environment"]
            # Canonical Compose config escapes literal dollars for re-parsing.
            expected = {name: value.replace("$", "$$") for name, value in values.items()}
            self.assertEqual(actual, expected)


if __name__ == "__main__":
    unittest.main()
