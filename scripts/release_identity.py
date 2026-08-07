#!/usr/bin/env python3
"""Deterministic, secret-safe source identity for Dexter release evidence."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import plistlib
import re
import shutil
import stat
import subprocess
import tempfile
import tomllib
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import asdict, dataclass
from datetime import UTC, datetime
from pathlib import Path
from typing import Any, Iterable, Mapping, Sequence


DEFAULT_INCLUDE_PATHS = (
    "Makefile",
    "src",
    "config",
    "scripts",
    "docs/DEXTER_PRODUCTION_BLOCKERS_IMPLEMENTATION_SPEC.md",
    "docs/PHASE_72_DAILY_DRIVER_RELEASE_GATE.md",
    "docs/DEXTER_OPERATOR_CONTROLS.md",
)

IGNORED_DIRECTORY_NAMES = frozenset(
    {
        ".build",
        ".git",
        ".mypy_cache",
        ".pytest_cache",
        ".ruff_cache",
        ".tox",
        ".venv",
        "__pycache__",
        "target",
        "venv",
    }
)

IGNORED_FILE_SUFFIXES = frozenset(
    {
        ".db",
        ".log",
        ".pyc",
        ".pyo",
        ".sock",
        ".sqlite",
        ".sqlite3",
        ".tmp",
    }
)

DEFAULT_MODELS = {
    "fast": "qwen3:8b",
    "primary": "gemma4:26b",
    "heavy": "deepseek-r1:32b",
    "code": "deepseek-coder-v2:16b",
    "vision": "gemma4:26b",
    "embed": "mxbai-embed-large",
}

DEFAULT_OLLAMA_BASE_URL = "http://localhost:11434"
DEFAULT_PERSONALITY_PATH = "config/personality/default.yaml"

IDENTITY_INPUT_PATHS = {
    "rust_lock": "src/rust-core/Cargo.lock",
    "python_manifest": "src/python-workers/pyproject.toml",
    "python_lock": "src/python-workers/uv.lock",
    "swift_manifest": "src/swift/Package.swift",
    "swift_resolved": "src/swift/Package.resolved",
    "shared_proto": "src/shared/proto/dexter.proto",
    "swift_proto_messages": (
        "src/swift/Sources/Dexter/Bridge/generated/dexter.pb.swift"
    ),
    "swift_proto_services": (
        "src/swift/Sources/Dexter/Bridge/generated/dexter.grpc.swift"
    ),
    "gate_makefile": "Makefile",
    "gate_identity_helper": "scripts/release_identity.py",
    "gate_orchestrator": "scripts/release_gate.py",
    "gate_attestation": "scripts/release_attestation.py",
    "gate_release_checks": "scripts/release_checks.py",
    "gate_status_consumer": "scripts/release_status.py",
    "gate_proto_check": "scripts/proto-check.sh",
    "gate_smoke_evidence": "scripts/smoke_evidence.py",
    "gate_acceptance_status": "scripts/acceptance-status.sh",
    "gate_smoke_summary": "scripts/live-smoke-summary.sh",
}


class SourceIdentityError(RuntimeError):
    """Raised when a complete, stable source identity cannot be produced."""


@dataclass(frozen=True)
class FileIdentity:
    path: str
    kind: str
    executable: bool
    sha256: str


@dataclass(frozen=True)
class SourceTreeIdentity:
    sha256: str
    file_count: int
    files: tuple[FileIdentity, ...] | None = None

    def to_dict(self) -> dict[str, object]:
        result: dict[str, object] = {
            "sha256": self.sha256,
            "file_count": self.file_count,
        }
        if self.files is not None:
            result["files"] = [asdict(entry) for entry in self.files]
        return result


@dataclass(frozen=True)
class ModelIdentity:
    tag: str
    digest: str | None
    available: bool


@dataclass(frozen=True)
class RuntimeIdentity:
    macos: str
    architecture: str
    rustc: str
    cargo: str
    swift: str
    python: str
    pytest: str
    ollama_client: str
    ollama_client_path: str
    ollama_app: str
    ollama_daemon: str
    ollama_api_compatibility: str
    ollama_base_url: str
    ollama_models_path: str
    proto_generators: Mapping[str, str]
    models: tuple[ModelIdentity, ...]

    def to_dict(self) -> dict[str, object]:
        result = asdict(self)
        result["models"] = [asdict(model) for model in self.models]
        return result


@dataclass(frozen=True)
class ReleaseIdentity:
    captured_at: str
    source: SourceTreeIdentity
    config_sha256: str
    config_path: str
    personality_sha256: str
    personality_path: str
    input_sha256: Mapping[str, str]
    runtime: RuntimeIdentity

    def to_dict(self) -> dict[str, object]:
        return {
            "captured_at": self.captured_at,
            "source": self.source.to_dict(),
            "config_sha256": self.config_sha256,
            "config_path": self.config_path,
            "personality_sha256": self.personality_sha256,
            "personality_path": self.personality_path,
            "input_sha256": dict(self.input_sha256),
            "runtime": self.runtime.to_dict(),
        }


@dataclass(frozen=True)
class _EntrySnapshot:
    device: int
    inode: int
    mode: int
    size: int
    modified_ns: int
    changed_ns: int


def _snapshot(path: Path) -> _EntrySnapshot:
    try:
        status = path.lstat()
    except OSError as error:
        raise SourceIdentityError(f"source entry unavailable: {path}") from error
    return _EntrySnapshot(
        device=status.st_dev,
        inode=status.st_ino,
        mode=status.st_mode,
        size=status.st_size,
        modified_ns=status.st_mtime_ns,
        changed_ns=status.st_ctime_ns,
    )


def _is_ignored(relative_path: Path) -> bool:
    if "docs/live-smoke-results" in relative_path.as_posix():
        return True
    if any(part in IGNORED_DIRECTORY_NAMES for part in relative_path.parts[:-1]):
        return True
    return relative_path.suffix.lower() in IGNORED_FILE_SUFFIXES


def _collect_entries(root: Path, include_paths: Sequence[str]) -> list[Path]:
    entries: set[Path] = set()
    for include_path in include_paths:
        candidate = root / include_path
        if not candidate.exists() and not candidate.is_symlink():
            raise SourceIdentityError(f"required source path is missing: {include_path}")
        if candidate.is_symlink() or candidate.is_file():
            relative = candidate.relative_to(root)
            if not _is_ignored(relative):
                entries.add(relative)
            continue

        pending = [candidate]
        while pending:
            directory = pending.pop()
            try:
                children = list(directory.iterdir())
            except OSError as error:
                relative = directory.relative_to(root)
                raise SourceIdentityError(
                    f"cannot enumerate source directory: {relative}"
                ) from error
            for child in children:
                relative = child.relative_to(root)
                if _is_ignored(relative):
                    continue
                if child.is_symlink() or child.is_file():
                    entries.add(relative)
                elif child.is_dir():
                    pending.append(child)

    return sorted(entries, key=lambda path: path.as_posix().encode("utf-8"))


def _hash_file(path: Path) -> str:
    digest = hashlib.sha256()
    try:
        with path.open("rb") as source:
            for chunk in iter(lambda: source.read(1024 * 1024), b""):
                digest.update(chunk)
    except OSError as error:
        raise SourceIdentityError(f"cannot read source file: {path}") from error
    return digest.hexdigest()


def _identity_for_entry(root: Path, relative_path: Path) -> FileIdentity:
    path = root / relative_path
    before = _snapshot(path)
    executable = bool(before.mode & (stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH))

    if stat.S_ISLNK(before.mode):
        try:
            target = os.readlink(path)
        except OSError as error:
            raise SourceIdentityError(f"cannot read source symlink: {path}") from error
        content_sha256 = hashlib.sha256(os.fsencode(target)).hexdigest()
        kind = "symlink"
    elif stat.S_ISREG(before.mode):
        content_sha256 = _hash_file(path)
        kind = "file"
    else:
        raise SourceIdentityError(f"unsupported source entry type: {path}")

    after = _snapshot(path)
    if after != before:
        raise SourceIdentityError(f"source entry changed while hashing: {relative_path}")

    return FileIdentity(
        path=relative_path.as_posix(),
        kind=kind,
        executable=executable,
        sha256=content_sha256,
    )


def _tree_digest(entries: Iterable[FileIdentity]) -> str:
    digest = hashlib.sha256()
    for entry in entries:
        for field in (
            entry.path,
            entry.kind,
            "1" if entry.executable else "0",
            entry.sha256,
        ):
            digest.update(field.encode("utf-8"))
            digest.update(b"\0")
    return digest.hexdigest()


def hash_source_tree(
    root: Path,
    *,
    include_paths: Sequence[str] = DEFAULT_INCLUDE_PATHS,
    include_file_manifest: bool = False,
) -> SourceTreeIdentity:
    """Hash a stable snapshot of the selected source tree without file contents."""

    resolved_root = root.resolve()
    if not resolved_root.is_dir():
        raise SourceIdentityError(f"source root is not a directory: {root}")

    relative_paths = _collect_entries(resolved_root, include_paths)
    identities = tuple(
        _identity_for_entry(resolved_root, relative_path)
        for relative_path in relative_paths
    )
    if _collect_entries(resolved_root, include_paths) != relative_paths:
        raise SourceIdentityError("source tree changed while hashing")

    return SourceTreeIdentity(
        sha256=_tree_digest(identities),
        file_count=len(identities),
        files=identities if include_file_manifest else None,
    )


def _sha256_bytes(content: bytes) -> str:
    return hashlib.sha256(content).hexdigest()


def _hash_optional_file(path: Path, *, absent_marker: bytes) -> str:
    if not path.exists():
        return _sha256_bytes(absent_marker)
    before = _snapshot(path)
    digest = _hash_file(path)
    if _snapshot(path) != before:
        raise SourceIdentityError(f"identity file changed while hashing: {path}")
    return digest


def hash_identity_inputs(
    root: Path,
    *,
    input_paths: Mapping[str, str] = IDENTITY_INPUT_PATHS,
) -> dict[str, str]:
    resolved_root = root.resolve()
    hashes: dict[str, str] = {}
    for name, relative_path in sorted(input_paths.items()):
        path = resolved_root / relative_path
        if not path.is_file():
            raise SourceIdentityError(f"required identity input is missing: {relative_path}")
        hashes[name] = _hash_optional_file(path, absent_marker=b"")
    return hashes


def _read_effective_settings(
    root: Path, config_path: Path
) -> tuple[dict[str, str], str, str]:
    models = dict(DEFAULT_MODELS)
    personality_path = DEFAULT_PERSONALITY_PATH
    ollama_base_url = DEFAULT_OLLAMA_BASE_URL
    if not config_path.exists():
        return models, personality_path, ollama_base_url

    try:
        with config_path.open("rb") as config_file:
            config = tomllib.load(config_file)
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise SourceIdentityError(f"cannot load effective Dexter config: {config_path}") from error

    configured_models = config.get("models", {})
    if isinstance(configured_models, dict):
        for tier in DEFAULT_MODELS:
            value = configured_models.get(tier)
            if isinstance(value, str) and value:
                models[tier] = value

    configured_core = config.get("core", {})
    if isinstance(configured_core, dict):
        value = configured_core.get("personality_path")
        if isinstance(value, str) and value:
            personality_path = value

    configured_inference = config.get("inference", {})
    if isinstance(configured_inference, dict):
        value = configured_inference.get("ollama_base_url")
        if isinstance(value, str) and value:
            ollama_base_url = value

    personality = Path(os.path.expanduser(personality_path))
    if not personality.is_absolute():
        personality = root / personality
    try:
        personality_path = personality.resolve().relative_to(root.resolve()).as_posix()
    except ValueError:
        personality_path = str(personality.resolve())

    return models, personality_path, ollama_base_url


def _command_version(argv: Sequence[str]) -> str:
    try:
        result = subprocess.run(
            argv,
            check=False,
            capture_output=True,
            text=True,
            timeout=10,
        )
    except (OSError, subprocess.SubprocessError):
        return "unavailable"
    output = (result.stdout or result.stderr).strip().splitlines()
    return output[0] if result.returncode == 0 and output else "unavailable"


def _ollama_client_version(executable: str = "ollama") -> str:
    try:
        result = subprocess.run(
            (executable, "--version"),
            check=False,
            capture_output=True,
            text=True,
            timeout=10,
        )
    except (OSError, subprocess.SubprocessError):
        return "unavailable"
    output = "\n".join((result.stdout, result.stderr))
    match = re.search(
        r"(?:client|ollama)\s+version(?:\s+is)?\s+([^\s]+)",
        output,
        flags=re.IGNORECASE,
    )
    return match.group(1) if result.returncode == 0 and match else "unavailable"


def _ollama_app_version(
    info_plist: Path = Path("/Applications/Ollama.app/Contents/Info.plist"),
) -> str:
    try:
        with info_plist.open("rb") as plist_file:
            metadata = plistlib.load(plist_file)
    except (OSError, plistlib.InvalidFileException):
        return "unavailable"
    version = metadata.get("CFBundleShortVersionString")
    return version if isinstance(version, str) and version else "unavailable"


def _macos_identity() -> str:
    product = _command_version(("sw_vers", "-productVersion"))
    build = _command_version(("sw_vers", "-buildVersion"))
    if product == "unavailable":
        return platform.platform()
    return f"{product} ({build})" if build != "unavailable" else product


def _proto_generator_versions() -> dict[str, str]:
    grpc_candidates = sorted(
        Path("/opt/homebrew/Cellar/protoc-gen-grpc-swift").glob(
            "*/bin/protoc-gen-grpc-swift-2"
        )
    )
    grpc_generator = (
        os.fspath(grpc_candidates[-1])
        if grpc_candidates
        else "protoc-gen-grpc-swift-2"
    )
    grpc_version = _command_version((grpc_generator, "--version"))
    if grpc_candidates and grpc_version == "protoc-gen-grpc-swift-2":
        grpc_version = grpc_candidates[-1].parents[1].name
    return {
        "protoc": _command_version(("protoc", "--version")),
        "protoc_gen_swift": _command_version(("protoc-gen-swift", "--version")),
        "protoc_gen_grpc_swift_2": grpc_version,
    }


def _safe_ollama_url(base_url: str) -> str:
    parsed = urllib.parse.urlsplit(base_url)
    if parsed.scheme not in {"http", "https"} or not parsed.hostname:
        raise SourceIdentityError("configured Ollama URL is invalid")
    host = parsed.hostname
    if ":" in host:
        host = f"[{host}]"
    port = f":{parsed.port}" if parsed.port is not None else ""
    return f"{parsed.scheme}://{host}{port}"


def _get_json(url: str) -> Mapping[str, Any]:
    request = urllib.request.Request(url, headers={"Accept": "application/json"})
    try:
        with urllib.request.urlopen(request, timeout=5) as response:
            payload = json.load(response)
    except (OSError, ValueError, urllib.error.URLError) as error:
        raise SourceIdentityError("Ollama API probe failed") from error
    if not isinstance(payload, dict):
        raise SourceIdentityError("Ollama API returned a non-object response")
    return payload


def _canonical_ollama_tag(tag: str) -> str:
    final_segment = tag.rsplit("/", 1)[-1]
    return tag if ":" in final_segment else f"{tag}:latest"


def probe_ollama(
    configured_models: Mapping[str, str],
    *,
    base_url: str,
) -> tuple[str, str, tuple[ModelIdentity, ...]]:
    safe_base_url = _safe_ollama_url(base_url)
    try:
        version_payload = _get_json(f"{safe_base_url}/api/version")
        tags_payload = _get_json(f"{safe_base_url}/api/tags")
    except SourceIdentityError:
        models = tuple(
            ModelIdentity(tag=tag, digest=None, available=False)
            for tag in dict.fromkeys(configured_models.values())
        )
        return "unavailable", "FAIL", models

    daemon_version = version_payload.get("version")
    raw_models = tags_payload.get("models")
    if not isinstance(daemon_version, str) or not isinstance(raw_models, list):
        models = tuple(
            ModelIdentity(tag=tag, digest=None, available=False)
            for tag in dict.fromkeys(configured_models.values())
        )
        return "invalid", "FAIL", models

    available: dict[str, str | None] = {}
    for raw_model in raw_models:
        if not isinstance(raw_model, dict):
            continue
        tag = raw_model.get("name") or raw_model.get("model")
        digest = raw_model.get("digest")
        if isinstance(tag, str):
            available[_canonical_ollama_tag(tag)] = (
                digest if isinstance(digest, str) else None
            )

    models = tuple(
        ModelIdentity(
            tag=tag,
            digest=available.get(_canonical_ollama_tag(tag)),
            available=_canonical_ollama_tag(tag) in available,
        )
        for tag in dict.fromkeys(configured_models.values())
    )
    compatibility = "PASS" if all(model.available for model in models) else "FAIL"
    return daemon_version, compatibility, models


def collect_release_identity(
    root: Path,
    *,
    config_path: Path | None = None,
    include_file_manifest: bool = False,
) -> ReleaseIdentity:
    resolved_root = root.resolve()
    effective_config_path = (
        config_path.expanduser()
        if config_path is not None
        else Path.home() / ".dexter" / "config.toml"
    )
    models, personality_path, ollama_base_url = _read_effective_settings(
        resolved_root, effective_config_path
    )
    personality = Path(personality_path)
    if not personality.is_absolute():
        personality = resolved_root / personality

    daemon_version, compatibility, model_identities = probe_ollama(
        models, base_url=ollama_base_url
    )
    ollama_client_path = shutil.which("ollama")
    pytest_version = _command_version(
        (os.fspath(Path(os.sys.executable)), "-m", "pytest", "--version")
    )
    return ReleaseIdentity(
        captured_at=datetime.now(UTC).isoformat(),
        source=hash_source_tree(
            resolved_root,
            include_file_manifest=include_file_manifest,
        ),
        config_sha256=_hash_optional_file(
            effective_config_path,
            absent_marker=b"dexter-config:built-in-defaults",
        ),
        config_path=os.fspath(effective_config_path),
        personality_sha256=_hash_optional_file(
            personality,
            absent_marker=b"dexter-personality:built-in-defaults",
        ),
        personality_path=os.fspath(personality),
        input_sha256=hash_identity_inputs(resolved_root),
        runtime=RuntimeIdentity(
            macos=_macos_identity(),
            architecture=platform.machine(),
            rustc=_command_version(("rustc", "--version")),
            cargo=_command_version(("cargo", "--version")),
            swift=_command_version(("swift", "--version")),
            python=platform.python_version(),
            pytest=pytest_version,
            ollama_client=_ollama_client_version(
                ollama_client_path or "ollama"
            ),
            ollama_client_path=ollama_client_path or "unavailable",
            ollama_app=_ollama_app_version(),
            ollama_daemon=daemon_version,
            ollama_api_compatibility=compatibility,
            ollama_base_url=_safe_ollama_url(ollama_base_url),
            ollama_models_path=os.path.normpath(
                os.environ.get("OLLAMA_MODELS", "/Users/jason/ollama-models")
            ),
            proto_generators=_proto_generator_versions(),
            models=model_identities,
        ),
    )


def atomic_write_text(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary_path: Path | None = None
    try:
        with tempfile.NamedTemporaryFile(
            mode="w",
            encoding="utf-8",
            dir=path.parent,
            prefix=f".{path.name}.",
            suffix=".tmp",
            delete=False,
        ) as temporary:
            temporary_path = Path(temporary.name)
            temporary.write(content)
            temporary.flush()
            os.fsync(temporary.fileno())
        os.replace(temporary_path, path)
        directory_fd = os.open(path.parent, os.O_RDONLY)
        try:
            os.fsync(directory_fd)
        finally:
            os.close(directory_fd)
    finally:
        if temporary_path is not None:
            temporary_path.unlink(missing_ok=True)


def write_json_atomic(path: Path, payload: Mapping[str, object]) -> None:
    atomic_write_text(path, f"{json.dumps(payload, indent=2, sort_keys=True)}\n")


def render_identity_markdown(identity: ReleaseIdentity) -> str:
    runtime = identity.runtime
    model_rows = "\n".join(
        f"| `{model.tag}` | {'yes' if model.available else 'no'} | "
        f"`{model.digest or 'unavailable'}` |"
        for model in runtime.models
    )
    return f"""# Dexter Release Identity

- Captured: `{identity.captured_at}`
- Source SHA-256: `{identity.source.sha256}` ({identity.source.file_count} files)
- Config SHA-256: `{identity.config_sha256}`
- Personality SHA-256: `{identity.personality_sha256}`
- Dependency/proto/gate inputs: `{len(identity.input_sha256)}`
- macOS: `{runtime.macos}`
- Architecture: `{runtime.architecture}`
- Rust: `{runtime.rustc}`
- Swift: `{runtime.swift}`
- Python: `{runtime.python}`
- Ollama client: `{runtime.ollama_client}`
- Ollama client path: `{runtime.ollama_client_path}`
- Ollama app: `{runtime.ollama_app}`
- Ollama daemon: `{runtime.ollama_daemon}`
- Ollama API compatibility: **{runtime.ollama_api_compatibility}**
- protoc: `{runtime.proto_generators['protoc']}`
- protoc-gen-swift: `{runtime.proto_generators['protoc_gen_swift']}`
- protoc-gen-grpc-swift-2: `{runtime.proto_generators['protoc_gen_grpc_swift_2']}`

| Configured model | Available | Digest |
|---|---:|---|
{model_rows}
"""


def write_identity_evidence(
    identity: ReleaseIdentity, *, json_path: Path, markdown_path: Path
) -> None:
    write_json_atomic(json_path, identity.to_dict())
    atomic_write_text(markdown_path, render_identity_markdown(identity))


def _parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Compute Dexter's deterministic release source identity."
    )
    parser.add_argument(
        "--root",
        type=Path,
        default=Path(__file__).resolve().parent.parent,
        help="repository root (defaults to the parent of scripts/)",
    )
    parser.add_argument(
        "--include-files",
        action="store_true",
        help="include paths and per-entry hashes for diagnosis",
    )
    parser.add_argument(
        "--runtime",
        action="store_true",
        help="also collect config, toolchain, Ollama, and model identity",
    )
    parser.add_argument(
        "--config",
        type=Path,
        help="effective Dexter config path (defaults to ~/.dexter/config.toml)",
    )
    return parser.parse_args()


def main() -> int:
    args = _parse_args()
    try:
        if args.runtime:
            payload = collect_release_identity(
                args.root,
                config_path=args.config,
                include_file_manifest=args.include_files,
            ).to_dict()
        else:
            payload = hash_source_tree(
                args.root,
                include_file_manifest=args.include_files,
            ).to_dict()
    except SourceIdentityError as error:
        print(json.dumps({"error": str(error)}, sort_keys=True))
        return 1
    print(json.dumps(payload, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
