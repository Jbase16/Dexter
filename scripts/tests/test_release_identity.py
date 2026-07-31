from __future__ import annotations

import stat
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from scripts.release_identity import (
    ModelIdentity,
    ReleaseIdentity,
    RuntimeIdentity,
    SourceIdentityError,
    SourceTreeIdentity,
    _ollama_app_version,
    _ollama_client_version,
    atomic_write_text,
    hash_identity_inputs,
    hash_source_tree,
    probe_ollama,
    render_identity_markdown,
)


class SourceTreeIdentityTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary_directory = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary_directory.name)

    def tearDown(self) -> None:
        self.temporary_directory.cleanup()

    def write(self, relative_path: str, content: str) -> Path:
        path = self.root / relative_path
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8")
        return path

    def identity(self, *include_paths: str):
        return hash_source_tree(self.root, include_paths=include_paths)

    def test_enumeration_order_does_not_change_identity(self) -> None:
        self.write("src/zeta.txt", "z")
        self.write("src/alpha.txt", "a")
        first = self.identity("src")
        original_iterdir = Path.iterdir

        def reversed_iterdir(path: Path):
            return iter(reversed(list(original_iterdir(path))))

        with mock.patch.object(Path, "iterdir", reversed_iterdir):
            second = self.identity("src")
        self.assertEqual(first, second)

    def test_one_byte_change_changes_identity(self) -> None:
        source = self.write("src/value.txt", "a")
        before = self.identity("src")
        source.write_text("b", encoding="utf-8")
        self.assertNotEqual(before.sha256, self.identity("src").sha256)

    def test_executable_bit_changes_identity(self) -> None:
        source = self.write("scripts/tool.py", "pass\n")
        before = self.identity("scripts")
        source.chmod(source.stat().st_mode | stat.S_IXUSR)
        self.assertNotEqual(before.sha256, self.identity("scripts").sha256)

    def test_symlink_target_changes_identity_without_following_target(self) -> None:
        self.write("src/first.txt", "same")
        self.write("src/second.txt", "same")
        link = self.root / "src/current.txt"
        link.symlink_to("first.txt")
        before = self.identity("src")
        link.unlink()
        link.symlink_to("second.txt")
        self.assertNotEqual(before.sha256, self.identity("src").sha256)

    def test_ignored_outputs_do_not_change_identity(self) -> None:
        self.write("src/main.py", "pass\n")
        before = self.identity(".")
        self.write("src/target/debug/output", "generated")
        self.write("src/runtime.log", "operator content")
        self.write("docs/live-smoke-results/release/latest.json", "{}")
        self.assertEqual(before, self.identity("."))

    def test_spaces_and_unicode_hash_deterministically(self) -> None:
        self.write("src/space name/éclair.txt", "content")
        first = self.identity("src")
        second = self.identity("src")
        self.assertEqual(first.sha256, second.sha256)

    def test_missing_required_path_fails(self) -> None:
        with self.assertRaisesRegex(SourceIdentityError, "required source path"):
            self.identity("missing")

    def test_disappearing_file_fails_instead_of_hashing_partial_tree(self) -> None:
        source = self.write("src/value.txt", "secret").resolve()
        original_open = Path.open

        def remove_before_open(path: Path, *args, **kwargs):
            if path == source:
                source.unlink()
            return original_open(path, *args, **kwargs)

        with mock.patch.object(Path, "open", remove_before_open):
            with self.assertRaises(SourceIdentityError):
                self.identity("src")

    def test_config_change_changes_identity_without_exposing_content(self) -> None:
        secret = "private-token-value"
        config = self.write("config/default.yaml", secret)
        before = hash_source_tree(
            self.root,
            include_paths=("config",),
            include_file_manifest=True,
        )
        config.write_text("replacement", encoding="utf-8")
        after = hash_source_tree(
            self.root,
            include_paths=("config",),
            include_file_manifest=True,
        )
        self.assertNotEqual(before.sha256, after.sha256)
        self.assertNotIn(secret, str(before.to_dict()))


class RuntimeIdentityTests(unittest.TestCase):
    def test_dependency_and_proto_inputs_are_hashed_separately(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            dependency = root / "Cargo.lock"
            proto = root / "dexter.proto"
            dependency.write_text("dependency", encoding="utf-8")
            proto.write_text("syntax = \"proto3\";", encoding="utf-8")
            hashes = hash_identity_inputs(
                root,
                input_paths={"rust_lock": "Cargo.lock", "shared_proto": "dexter.proto"},
            )
        self.assertEqual(set(hashes), {"rust_lock", "shared_proto"})
        self.assertNotIn("dependency", str(hashes))

    def test_ollama_client_version_ignores_connection_warning(self) -> None:
        completed = mock.Mock(
            returncode=0,
            stdout=(
                "Warning: could not connect to a running Ollama instance\n"
                "Warning: client version is 0.24.0\n"
            ),
            stderr="",
        )
        with mock.patch(
            "scripts.release_identity.subprocess.run", return_value=completed
        ):
            self.assertEqual(_ollama_client_version(), "0.24.0")

    def test_ollama_app_version_is_collected_separately(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            info_plist = Path(directory) / "Info.plist"
            info_plist.write_bytes(
                b'<?xml version="1.0" encoding="UTF-8"?>'
                b'<plist version="1.0"><dict>'
                b"<key>CFBundleShortVersionString</key><string>0.32.5</string>"
                b"</dict></plist>"
            )
            self.assertEqual(_ollama_app_version(info_plist), "0.32.5")

    def test_ollama_probe_records_daemon_and_configured_model_digests(self) -> None:
        responses = (
            {"version": "0.32.4"},
            {
                "models": [
                    {"name": "model-a:latest", "digest": "sha256:a"},
                    {"name": "model-b:latest", "digest": "sha256:b"},
                ]
            },
        )
        with mock.patch(
            "scripts.release_identity._get_json", side_effect=responses
        ):
            daemon, compatibility, models = probe_ollama(
                {"fast": "model-a:latest", "primary": "model-b:latest"},
                base_url="http://localhost:11434/path?token=secret",
            )
        self.assertEqual(daemon, "0.32.4")
        self.assertEqual(compatibility, "PASS")
        self.assertEqual(models[0].digest, "sha256:a")

    def test_ollama_probe_fails_closed_when_a_configured_model_is_missing(self) -> None:
        responses = ({"version": "1.0"}, {"models": []})
        with mock.patch(
            "scripts.release_identity._get_json", side_effect=responses
        ):
            _, compatibility, models = probe_ollama(
                {"fast": "missing:latest"},
                base_url="http://localhost:11434",
            )
        self.assertEqual(compatibility, "FAIL")
        self.assertFalse(models[0].available)

    def test_atomic_write_failure_preserves_prior_destination(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            destination = Path(directory) / "latest.json"
            destination.write_text("prior", encoding="utf-8")
            with mock.patch(
                "scripts.release_identity.os.replace",
                side_effect=OSError("interrupted"),
            ):
                with self.assertRaises(OSError):
                    atomic_write_text(destination, "replacement")
            self.assertEqual(destination.read_text(encoding="utf-8"), "prior")
            self.assertEqual(list(Path(directory).glob(".*.tmp")), [])

    def test_markdown_render_contains_hashes_but_no_config_content(self) -> None:
        secret = "private-token-value"
        identity = ReleaseIdentity(
            captured_at="2026-07-30T00:00:00+00:00",
            source=SourceTreeIdentity("source-hash", 2),
            config_sha256="config-hash",
            config_path="/tmp/config.toml",
            personality_sha256="personality-hash",
            personality_path="config/personality/default.yaml",
            input_sha256={"rust_lock": "lock-hash"},
            runtime=RuntimeIdentity(
                macos="26.3",
                architecture="arm64",
                rustc="rustc 1.0",
                cargo="cargo 1.0",
                swift="Swift 6.2",
                python="3.13",
                pytest="pytest 9",
                ollama_client="0.24.0",
                ollama_client_path="/opt/homebrew/bin/ollama",
                ollama_app="0.32.5",
                ollama_daemon="0.32.4",
                ollama_api_compatibility="PASS",
                ollama_base_url="http://localhost:11434",
                ollama_models_path="/Users/operator/ollama-models",
                proto_generators={
                    "protoc": "libprotoc 32",
                    "protoc_gen_swift": "1.0",
                    "protoc_gen_grpc_swift_2": "2.0",
                },
                models=(ModelIdentity("model:latest", "digest", True),),
            ),
        )
        markdown = render_identity_markdown(identity)
        self.assertIn("source-hash", markdown)
        self.assertIn("digest", markdown)
        self.assertNotIn(secret, markdown)


if __name__ == "__main__":
    unittest.main()
