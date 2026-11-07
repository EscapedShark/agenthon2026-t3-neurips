"""Paths, the release config and Docker helpers shared by the packaging tools."""
from __future__ import annotations

import datetime as _dt
import hashlib
import json
import os
import re
import stat
import subprocess
import tomllib
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any

SUB = Path(__file__).resolve().parents[1]
WORKSPACE = SUB.parent
SOURCE = WORKSPACE / "fastsim"
TRACK_REPO = WORKSPACE / "track3-simulation-public"
UNITS = TRACK_REPO / "units"
DOCKERFILE = SUB / "docker" / "Dockerfile"
RELEASES = SUB / "releases"
TOOLKIT_VERSION = "2.5.1"

# The context adds the Dockerfile and a whitelist of public PGO scenario inputs.
CONTEXT_FILES = ("Cargo.toml", "Cargo.lock", "tools/pgo_build.py")
CONTEXT_DIRS = ("src",)

MANIFEST_TYPES = {
    "application/vnd.oci.image.manifest.v1+json",
    "application/vnd.docker.distribution.manifest.v2+json",
}
DIGEST_RE = re.compile(r"sha256:[0-9a-f]{64}\Z")
_VERSION_RE = re.compile(r"[0-9A-Za-z][0-9A-Za-z._-]{0,63}\Z")
_REPOSITORY_RE = re.compile(r"docker\.io/[a-z0-9]+(?:[._-][a-z0-9]+)*/[a-z0-9]+(?:[._-][a-z0-9]+)*\Z")


class ToolError(RuntimeError):
    """A check failed. Messages name the rule that failed and never contain secret material."""


@dataclass(frozen=True)
class Release:
    version: str
    local_image: str
    repository: str
    license: str

    @property
    def local_tag(self) -> str:
        return f"{self.local_image}:{self.version}"

    @property
    def remote_tag(self) -> str:
        return f"{self.repository}:{self.version}"

    @property
    def dir(self) -> Path:
        return RELEASES / self.version

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)


def load_release(version: str | None = None) -> Release:
    """The release's own snapshot once built (releases/<v>/release.json), else release.toml."""
    if version is not None:
        snapshot = RELEASES / version / "release.json"
        if snapshot.exists():
            return _release(read_json(snapshot))
    config = tomllib.loads((SUB / "release.toml").read_text(encoding="utf-8"))
    if version is not None and version != config["version"]:
        raise ToolError(f"release {version} has no releases/{version}/release.json and is not release.toml's version")
    return _release(config)


def _release(raw: dict[str, Any]) -> Release:
    release = Release(
        version=str(raw["version"]),
        local_image=str(raw["local_image"]),
        repository=str(raw["repository"]),
        license=str(raw["license"]),
    )
    if not _VERSION_RE.fullmatch(release.version):
        raise ToolError(f"version {release.version!r} is not a valid image tag")
    if not _REPOSITORY_RE.fullmatch(release.repository):
        raise ToolError("repository must be docker.io/<user>/<name> (full docker.io/ prefix, lowercase)")
    return release


def now() -> str:
    return _dt.datetime.now(_dt.timezone.utc).replace(microsecond=0).isoformat()


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def tree_files(root: Path) -> dict[str, str]:
    """sha256 of every regular file under `root`, keyed by POSIX relative path; links refused."""
    files: dict[str, str] = {}
    for path in sorted(root.rglob("*")):
        info = path.lstat()
        rel = path.relative_to(root).as_posix()
        if stat.S_ISLNK(info.st_mode):
            raise ToolError(f"symlink in the build context: {rel}")
        if stat.S_ISREG(info.st_mode):
            files[rel] = sha256_file(path)
        elif not stat.S_ISDIR(info.st_mode):
            raise ToolError(f"special file in the build context: {rel}")
    return files


def tree_digest(files: dict[str, str]) -> str:
    return sha256_bytes("".join(f"{sha}  {rel}\n" for rel, sha in sorted(files.items())).encode())


def read_json(path: Path) -> Any:
    return json.loads(path.read_text(encoding="utf-8"))


def write_json(path: Path, value: Any, *, exclusive: bool = False) -> None:
    """Write through a temporary sibling and rename; `exclusive` refuses to replace a file."""
    data = (json.dumps(value, indent=2, sort_keys=False) + "\n").encode("utf-8")
    if exclusive and (path.exists() or path.is_symlink()):
        raise ToolError(f"{path.relative_to(SUB)} already exists and is never overwritten")
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.tmp")
    temporary.write_bytes(data)
    os.replace(temporary, path)


def run(cmd: list[str], *, timeout: float = 600, check: bool = True, **kwargs: Any) -> subprocess.CompletedProcess:
    try:
        result = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout, **kwargs)
    except (OSError, subprocess.SubprocessError) as error:
        raise ToolError(f"{cmd[0]} {cmd[1] if len(cmd) > 1 else ''} failed to run: {type(error).__name__}") from None
    if check and result.returncode != 0:
        tail = (result.stderr or result.stdout or "")[-1500:]
        raise ToolError(f"`{' '.join(cmd[:3])} ...` exited {result.returncode}: {tail}")
    return result


def docker_json(*args: str, timeout: float = 120) -> Any:
    result = run(["docker", *args], timeout=timeout)
    try:
        return json.loads(result.stdout)
    except ValueError:
        raise ToolError(f"docker {args[0]} did not return JSON") from None


def image_identity(reference: str) -> dict[str, Any]:
    """The runtime-relevant identity of a local image, from `docker image inspect`."""
    inspection = docker_json("image", "inspect", reference)[0]
    config = inspection.get("Config") or {}
    return {
        "id": inspection["Id"],
        "descriptor": inspection.get("Descriptor"),
        "architecture": inspection.get("Architecture"),
        "os": inspection.get("Os"),
        "config": {key: config.get(key) for key in ("User", "Env", "Entrypoint", "Cmd", "WorkingDir", "Labels")},
        "layers": (inspection.get("RootFS") or {}).get("Layers"),
        "size": inspection.get("Size"),
    }


def manifest_digest(identity: dict[str, Any]) -> str:
    """The image's own manifest digest: what the registry serves and the descriptor names."""
    descriptor = identity.get("descriptor") or {}
    if descriptor.get("mediaType") not in MANIFEST_TYPES or not DIGEST_RE.fullmatch(descriptor.get("digest", "")):
        raise ToolError(
            "the local image is not a single-platform manifest in the containerd image store; "
            "build it with packaging/tools/build.py (--provenance=false --sbom=false)"
        )
    return descriptor["digest"]


def units() -> list[Path]:
    return sorted(path.parent for path in UNITS.glob("*/card.toml"))


def git(repo: Path, *args: str) -> str:
    return run(["git", "-C", str(repo), *args], timeout=60).stdout.strip()


def git_state(repo: Path) -> dict[str, Any]:
    status = run(["git", "-C", str(repo), "status", "--porcelain"], timeout=60).stdout  # unstripped: "XY path"
    return {
        "head": git(repo, "rev-parse", "HEAD"),
        "dirty_paths": [line[3:] for line in status.splitlines() if line],
    }
