#!/usr/bin/env python3
"""Step 2: stage the build context, build the linux/amd64 image, and record what was built.

    packaging/.venv/bin/python packaging/tools/build.py [--version V] [--rebuild]
    packaging/.venv/bin/python packaging/tools/build.py --rebuild --adopt TAG --adopt-context SHA256

PGO training runs threads whose timing varies, so two builds of one context give different
binaries (about 1% apart in speed). `--adopt` records a candidate that was built from this release's
context with the same `docker build` command plus `--no-cache` (see CANDIDATE_COMMAND), instead of
building again: the freshly staged context must have the digest the candidate was built from.

The context is a fresh copy of fastsim's source, PGO builder, public scenario inputs and Dockerfile.
It stays in releases/<v>/context/ as the release's source of record, so later edits to fastsim/
never change what a validated release was built from. The image is one single-platform manifest
(no provenance or SBOM attestations), so its local manifest digest is the digest the registry
serves after the push.

After the build, the image config's WorkingDir is set to CONFIG_WORKDIR without adding a layer
(see set_config_workdir); the image is saved, patched and loaded back under the same tag.
"""
from __future__ import annotations

import argparse
import hashlib
import io
import json
import os
import shutil
import struct
import subprocess
import sys
import tarfile
import tempfile
from pathlib import Path

sys.dont_write_bytecode = True
from common import (  # noqa: E402
    CONTEXT_DIRS, CONTEXT_FILES, DOCKERFILE, SOURCE, SUB, UNITS, ToolError, git_state, image_identity,
    load_release, manifest_digest, now, run, sha256_file, tree_digest, tree_files, write_json,
)


def stage_context(context: Path) -> dict[str, str]:
    context.mkdir(parents=True)
    shutil.copyfile(DOCKERFILE, context / "Dockerfile")
    for name in CONTEXT_FILES:
        source = SOURCE / name
        if source.is_symlink() or not source.is_file():
            raise ToolError(f"fastsim/{name} must be a regular file")
        (context / name).parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source, context / name)
    for name in CONTEXT_DIRS:
        # Links are copied as links so that tree_files() refuses them instead of following them.
        shutil.copytree(SOURCE / name, context / name, symlinks=True)
    # Only configuration files enter the build stage; never reference outputs, keys or logs.
    for unit in sorted(UNITS.iterdir()):
        if not (unit / "card.toml").is_file():
            continue
        inputs = [unit / "card.toml"]
        inputs += ([unit / "batch.json", *sorted((unit / "scenarios").glob("*.json"))]
                   if (unit / "batch.json").is_file() else [unit / "scenario.json"])
        for source in inputs:
            if source.is_symlink() or not source.is_file():
                raise ToolError(f"PGO input must be a regular file: {source.name}")
            dest = context / "training" / source.relative_to(UNITS)
            dest.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(source, dest)
    return tree_files(context)


def static_x86_64_elf(data: bytes) -> bool:
    """A 64-bit little-endian x86-64 ELF with no PT_INTERP program header (no dynamic loader)."""
    if data[:4] != b"\x7fELF" or data[4] != 2 or data[5] != 1:
        return False
    if struct.unpack_from("<H", data, 18)[0] != 62:
        return False
    phoff = struct.unpack_from("<Q", data, 32)[0]
    phentsize, phnum = struct.unpack_from("<HH", data, 54)
    return all(struct.unpack_from("<I", data, phoff + i * phentsize)[0] != 3 for i in range(phnum))


#: Where the Dockerfile installs the binary. The image holds no /usr: the GPU injection writes into
#: /usr when a container starts, and an image directory that gets a new entry then is copied up by
#: overlayfs with an fsync, inside the Final's StartedAt-FinishedAt window.
BINARY_DIR = "/bin"

#: The image's working directory, set in its config only. Docker creates a missing working
#: directory when it creates the container (and again, as a no-op, before the start is timed), so
#: /etc is copied up into the container's writable layer outside the timed window, instead of
#: during the start, when the NVIDIA hook and ldconfig write /etc. A Dockerfile WORKDIR would
#: create the directory in the image and defeat this.
CONFIG_WORKDIR = "/etc/fastsim"

#: Every entry the single layer may hold. Anything else is a directory the start could copy up.
RUNTIME_ROOT = {"bin": "dir", "bin/simulate": "file", "bin/simulate-batch": "link", "etc": "dir", "tmp": "dir"}

OCI_MANIFEST = "application/vnd.oci.image.manifest.v1+json"


def inspect_binary(tag: str) -> dict:
    container = run(["docker", "create", "--platform", "linux/amd64", tag, "simulate"]).stdout.strip()
    try:
        with tempfile.TemporaryDirectory(prefix="t3-binary-") as tmp:
            for name in ("simulate", "simulate-batch"):
                run(["docker", "cp", f"{container}:{BINARY_DIR}/{name}", str(Path(tmp) / name)])
            binary, alias = Path(tmp) / "simulate", Path(tmp) / "simulate-batch"
            data = binary.read_bytes()
            if binary.is_symlink() or not static_x86_64_elf(data):
                raise ToolError(f"{BINARY_DIR}/simulate is not a static x86-64 executable")
            if not alias.is_symlink() or os.readlink(alias) != "simulate":
                raise ToolError(f"{BINARY_DIR}/simulate-batch must be a symlink to simulate")
            return {"path": f"{BINARY_DIR}/simulate", "sha256": sha256_file(binary), "bytes": len(data),
                    "static_x86_64": True, "simulate_batch": "symlink -> simulate"}
    finally:
        run(["docker", "rm", "-f", container], check=False)


def layer_root(layer: bytes) -> dict[str, str]:
    """The entries of a layer tarball (any compression): path without ./ -> dir, file or link."""
    entries = {}
    with tarfile.open(fileobj=io.BytesIO(layer), mode="r:*") as tar:
        for member in tar.getmembers():
            name = member.name.removeprefix("./").rstrip("/")
            if not name or name == ".":
                continue
            if member.isdir():
                kind = "dir"
            elif member.issym():
                kind = "link"
            elif member.isfile():
                kind = "file"
            else:
                raise ToolError(f"the image layer holds {name}, which is neither a file, a directory nor a link")
            if (name == "tmp" and member.mode & 0o7777 != 0o1777) or (name == "bin/simulate" and member.mode & 0o111 == 0) \
                    or (name == "bin/simulate-batch" and member.linkname != "simulate"):
                raise ToolError(f"the image layer's {name} has the wrong mode or target")
            entries[name] = kind
    return entries


def patch_workdir(archive: bytes, workdir: str) -> tuple[bytes, dict]:
    """A `docker save` OCI archive of one single-layer image, with the config's WorkingDir set.

    The layer is untouched; the config, the manifest that names it and the index change. The layer
    must hold exactly RUNTIME_ROOT, so nothing exists at `workdir` or under /usr.
    """
    with tarfile.open(fileobj=io.BytesIO(archive)) as tar:
        files = {member.name: tar.extractfile(member).read() for member in tar.getmembers() if member.isfile()}

    def blob(digest: str) -> bytes:
        data = files.get("blobs/sha256/" + digest.removeprefix("sha256:"))
        if data is None or "sha256:" + hashlib.sha256(data).hexdigest() != digest:
            raise ToolError(f"the saved image lacks blob {digest} or it does not match its digest")
        return data

    def put(data: bytes) -> tuple[str, int]:
        digest = "sha256:" + hashlib.sha256(data).hexdigest()
        files["blobs/sha256/" + digest.removeprefix("sha256:")] = data
        return digest, len(data)

    index = json.loads(files["index.json"])
    if len(index.get("manifests") or []) != 1 or index["manifests"][0].get("mediaType") != OCI_MANIFEST:
        raise ToolError("the saved image is not one single-platform OCI manifest")
    descriptor = index["manifests"][0]
    built_manifest = descriptor["digest"]
    manifest = json.loads(blob(built_manifest))
    if len(manifest.get("layers") or []) != 1:
        raise ToolError("the image must have exactly one layer")
    root = layer_root(blob(manifest["layers"][0]["digest"]))
    if root != RUNTIME_ROOT:
        extra, missing = sorted(set(root) - set(RUNTIME_ROOT)), sorted(set(RUNTIME_ROOT) - set(root))
        raise ToolError(f"the image layer must hold exactly {sorted(RUNTIME_ROOT)}; extra {extra}, missing {missing}, "
                        f"or a kind differs: {root}")
    config = json.loads(blob(manifest["config"]["digest"]))
    if (config.get("config") or {}).get("WorkingDir") not in (None, "", "/"):
        raise ToolError("the Dockerfile must not set WORKDIR; build.py sets it in the config")
    config["config"]["WorkingDir"] = workdir
    entry = {"created_by": f"WORKDIR {workdir} (image config only, set by packaging/tools/build.py)", "empty_layer": True}
    if config.get("created"):
        entry = {"created": config["created"], **entry}
    config.setdefault("history", []).append(entry)

    for old in (manifest["config"]["digest"], built_manifest):
        files.pop("blobs/sha256/" + old.removeprefix("sha256:"))
    config_digest, config_size = put(json.dumps(config).encode())
    manifest["config"]["digest"], manifest["config"]["size"] = config_digest, config_size
    manifest_digest_, manifest_size = put(json.dumps(manifest, indent=2).encode())
    descriptor["digest"], descriptor["size"] = manifest_digest_, manifest_size
    if "config.digest" in (descriptor.get("annotations") or {}):
        descriptor["annotations"]["config.digest"] = config_digest
    files["index.json"] = json.dumps(index, separators=(",", ":")).encode()
    if "manifest.json" in files:
        legacy = json.loads(files["manifest.json"])
        legacy[0]["Config"] = "blobs/sha256/" + config_digest.removeprefix("sha256:")
        files["manifest.json"] = json.dumps(legacy, separators=(",", ":")).encode()

    out = io.BytesIO()
    with tarfile.open(fileobj=out, mode="w", format=tarfile.PAX_FORMAT) as tar:
        for name in ("blobs", "blobs/sha256"):
            member = tarfile.TarInfo(name)
            member.type, member.mode = tarfile.DIRTYPE, 0o755
            tar.addfile(member)
        for name in sorted(files):
            member = tarfile.TarInfo(name)
            member.size, member.mode = len(files[name]), 0o644
            tar.addfile(member, io.BytesIO(files[name]))
    return out.getvalue(), {"workdir": workdir, "built_manifest_digest": built_manifest,
                            "manifest_digest": manifest_digest_, "config_digest": config_digest,
                            "layer_digest": manifest["layers"][0]["digest"], "layer_root": root}


def set_config_workdir(tag: str, workdir: str) -> dict:
    """Set the local image's config WorkingDir in place: docker save, patch_workdir, docker load."""
    saved = subprocess.run(["docker", "save", "--platform", "linux/amd64", tag], capture_output=True, timeout=600)
    if saved.returncode != 0:
        raise ToolError(f"docker save exited {saved.returncode}: {saved.stderr.decode(errors='replace')[-800:]}")
    patched, record = patch_workdir(saved.stdout, workdir)
    loaded = subprocess.run(["docker", "load"], input=patched, capture_output=True, timeout=600)
    if loaded.returncode != 0:
        raise ToolError(f"docker load exited {loaded.returncode}: {loaded.stderr.decode(errors='replace')[-800:]}")
    return record


def check_verbs(tag: str) -> dict:
    """Both verbs resolve on PATH (not 126/127) and reach our own argument check."""
    expected = {"simulate": "--config is required", "simulate-batch": "--batch-dir is required"}
    seen = {}
    for verb, message in expected.items():
        result = run(["docker", "run", "--rm", "--platform", "linux/amd64", "--network=none", tag, verb], check=False)
        if result.returncode != 1 or message not in result.stderr:
            raise ToolError(f"`{verb}` with no arguments exited {result.returncode}: {result.stderr[-300:]}")
        seen[verb] = f"exit 1: {message}"
    return seen


# How a candidate for `--adopt` is built: the release build's command (below) with --no-cache, so
# that its PGO training runs again, and its own tag (`releases/<v>/context` as the context).
CANDIDATE_COMMAND = ["docker", "build", "--no-cache", "--platform", "linux/amd64", "--provenance=false", "--sbom=false",
                     "--progress", "plain", "--build-arg", "VERSION=<version>", "--tag", "<candidate>",
                     "releases/<version>/context"]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--version", help="release version (default: release.toml)")
    parser.add_argument("--rebuild", action="store_true", help="replace an unpublished release's build and records")
    parser.add_argument("--adopt", metavar="TAG", help="record this candidate image instead of building (with --rebuild)")
    parser.add_argument("--adopt-context", metavar="SHA256", help="the context digest the candidate was built from")
    parser.add_argument("--adopt-log", metavar="PATH", type=Path, help="the candidate's docker build log")
    parser.add_argument("--adopt-note", metavar="TEXT", help="where and how the candidate was built, if not by CANDIDATE_COMMAND here")
    args = parser.parse_args()
    if args.adopt and not (args.rebuild and args.adopt_context and args.adopt_log):
        parser.error("--adopt needs --rebuild, --adopt-context and --adopt-log")
    try:
        release = load_release(args.version)
        directory = release.dir
        if directory.exists():
            if (directory / "published.json").exists():
                raise ToolError(f"release {release.version} is published; bump the version in release.toml")
            if not args.rebuild:
                raise ToolError(f"releases/{release.version}/ exists; bump the version or pass --rebuild")
            shutil.rmtree(directory)
        directory.mkdir(parents=True)
        write_json(directory / "release.json", {**release.to_dict(), "created": now()})
        files = stage_context(directory / "context")
        source = {
            "context_sha256": tree_digest(files),
            "files": files,
            "fastsim_git": git_state(SOURCE),
            "staged": now(),
        }
        write_json(directory / "source.json", source)
        print(f"Staged {len(files)} files, context {source['context_sha256'][:16]}; building {release.local_tag} (linux/amd64)…", flush=True)

        iidfile = directory / "image-id.txt"
        command = ["docker", "build", "--platform", "linux/amd64", "--provenance=false", "--sbom=false",
                   "--progress", "plain", "--build-arg", f"VERSION={release.version}",
                   "--tag", release.local_tag, "--iidfile", str(iidfile), str(directory / "context")]
        if args.adopt:
            if source["context_sha256"] != args.adopt_context:
                raise ToolError(f"the staged context is {source['context_sha256']}, not the candidate's {args.adopt_context}")
            candidate = image_identity(args.adopt)
            run(["docker", "tag", candidate["id"], release.local_tag])
            iidfile.write_text(candidate["id"] + "\n")
            how = args.adopt_note or " ".join(CANDIDATE_COMMAND)
            note = (f"Adopted {args.adopt} ({candidate['id']}), built from context {args.adopt_context} with\n"
                    f"  {how}\n"
                    f"Its build log follows.\n\n")
            (directory / "build.log").write_bytes(note.encode() + args.adopt_log.read_bytes())
            write_json(directory / "adopted.json", {"candidate": args.adopt, "image_id": candidate["id"],
                                                    "context_sha256": args.adopt_context,
                                                    "command": CANDIDATE_COMMAND, "note": args.adopt_note,
                                                    "recorded": now()})
            print(f"Adopted {args.adopt} as {release.local_tag}", flush=True)
        else:
            with open(directory / "build.log", "wb") as log:
                status = subprocess.run(command, stdout=log, stderr=subprocess.STDOUT, timeout=7200).returncode
            if status != 0:
                tail = (directory / "build.log").read_text(errors="replace")[-3000:]
                raise ToolError(f"docker build exited {status}; see releases/{release.version}/build.log\n{tail}")

        built = image_identity(release.local_tag)
        if iidfile.read_text().strip() != built["id"]:
            raise ToolError("docker build's --iidfile disagrees with docker image inspect")
        workdir = set_config_workdir(release.local_tag, CONFIG_WORKDIR)
        identity = image_identity(release.local_tag)
        config = identity["config"]
        if (identity["architecture"], identity["os"]) != ("amd64", "linux"):
            raise ToolError(f"built {identity['os']}/{identity['architecture']}, not linux/amd64")
        if (config.get("Labels") or {}).get("qfbench2.interface_version") != "2.0":
            raise ToolError("the image lacks LABEL qfbench2.interface_version=\"2.0\"")
        if config.get("Entrypoint"):
            raise ToolError("the image must not set an ENTRYPOINT")
        if config.get("User") != "65534:65534" or "HOME=/tmp" not in (config.get("Env") or []):
            raise ToolError("the image must run as 65534:65534 with HOME=/tmp")
        if config.get("WorkingDir") != CONFIG_WORKDIR or identity["layers"] != built["layers"]:
            raise ToolError("setting the WorkingDir changed more than the config, or did not take")
        digest = manifest_digest(identity)
        if digest != workdir["manifest_digest"]:
            raise ToolError(f"{release.local_tag} is not the image with the patched config after docker load")
        record = {
            "version": release.version,
            "local_tag": release.local_tag,
            "image_id": identity["id"],
            "manifest_digest": digest,
            "built_image_id": built["id"],
            "config_workdir": workdir,
            **{key: identity[key] for key in ("descriptor", "architecture", "os", "config", "layers", "size")},
            "binary": inspect_binary(release.local_tag),
            "verbs": check_verbs(release.local_tag),
            "docker_server": run(["docker", "version", "--format", "{{.Server.Version}}"]).stdout.strip(),
            "built": now(),
        }
        write_json(directory / "image.json", record)
    except ToolError as error:
        print(f"Build stopped: {error}", file=sys.stderr)
        return 1
    print(f"Built {release.local_tag}\n  manifest digest {digest}\n  binary sha256 {record['binary']['sha256']} "
          f"({record['binary']['bytes']:,} bytes, static)\n  records: {directory.relative_to(SUB)}/")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
