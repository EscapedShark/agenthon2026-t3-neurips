"""Check that PGO staging copies configuration inputs without references or private files, and that
the config-only WorkingDir patch keeps the layer and refuses a root it does not expect."""
import gzip
import hashlib
import io
import json
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import build
from common import ToolError


def layer(entries):
    """A gzip layer tarball from (name, kind, mode, link) tuples."""
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w") as tar:
        for name, kind, mode, link in entries:
            member = tarfile.TarInfo(name)
            member.mode = mode
            data = b""
            if kind == "dir":
                member.type = tarfile.DIRTYPE
            elif kind == "link":
                member.type, member.linkname = tarfile.SYMTYPE, link
            else:
                data = b"\x7fELF static"
                member.size = len(data)
            tar.addfile(member, io.BytesIO(data))
    return gzip.compress(raw.getvalue())


GOOD_ROOT = [("bin/", "dir", 0o755, ""), ("bin/simulate", "file", 0o755, ""),
             ("bin/simulate-batch", "link", 0o777, "simulate"), ("etc/", "dir", 0o755, ""),
             ("tmp/", "dir", 0o1777, "")]


def saved_image(entries, workdir="/"):
    """An OCI archive shaped like `docker save` of a single-platform, single-layer image."""
    blobs = {}

    def put(data):
        digest = "sha256:" + hashlib.sha256(data).hexdigest()
        blobs["blobs/sha256/" + digest[7:]] = data
        return digest, len(data)

    layer_digest, layer_size = put(layer(entries))
    config = {"architecture": "amd64", "os": "linux", "created": "2026-10-09T00:00:00Z",
              "config": {"User": "65534:65534", "WorkingDir": workdir},
              "rootfs": {"type": "layers", "diff_ids": ["sha256:" + "0" * 64]},
              "history": [{"created_by": "COPY /rootfs/ / # buildkit"}]}
    config_digest, config_size = put(json.dumps(config).encode())
    manifest = {"schemaVersion": 2, "mediaType": build.OCI_MANIFEST,
                "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": config_digest,
                           "size": config_size},
                "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip", "digest": layer_digest,
                            "size": layer_size}]}
    manifest_digest, manifest_size = put(json.dumps(manifest, indent=2).encode())
    index = {"schemaVersion": 2, "mediaType": "application/vnd.oci.image.index.v1+json",
             "manifests": [{"mediaType": build.OCI_MANIFEST, "digest": manifest_digest, "size": manifest_size,
                            "annotations": {"config.digest": config_digest,
                                            "io.containerd.image.name": "docker.io/library/fastsim-t3:9.9",
                                            "org.opencontainers.image.ref.name": "9.9"}}]}
    files = {**blobs, "index.json": json.dumps(index).encode(), "oci-layout": b'{"imageLayoutVersion":"1.0.0"}',
             "manifest.json": json.dumps([{"Config": "blobs/sha256/" + config_digest[7:], "RepoTags": ["fastsim-t3:9.9"],
                                           "Layers": ["blobs/sha256/" + layer_digest[7:]]}]).encode()}
    out = io.BytesIO()
    with tarfile.open(fileobj=out, mode="w") as tar:
        for name, data in files.items():
            member = tarfile.TarInfo(name)
            member.size = len(data)
            tar.addfile(member, io.BytesIO(data))
    return out.getvalue(), layer_digest, manifest_digest


def read_archive(archive):
    with tarfile.open(fileobj=io.BytesIO(archive)) as tar:
        return {m.name: tar.extractfile(m).read() for m in tar.getmembers() if m.isfile()}


class Workdir(unittest.TestCase):
    def test_sets_the_config_workdir_and_keeps_the_layer(self):
        archive, layer_digest, built = saved_image(GOOD_ROOT)
        patched, record = build.patch_workdir(archive, "/etc/fastsim")
        files = read_archive(patched)
        index = json.loads(files["index.json"])
        descriptor = index["manifests"][0]
        manifest_bytes = files["blobs/sha256/" + descriptor["digest"][7:]]
        self.assertEqual("sha256:" + hashlib.sha256(manifest_bytes).hexdigest(), descriptor["digest"])
        manifest = json.loads(manifest_bytes)
        config_bytes = files["blobs/sha256/" + manifest["config"]["digest"][7:]]
        self.assertEqual(len(config_bytes), manifest["config"]["size"])
        config = json.loads(config_bytes)
        self.assertEqual(config["config"]["WorkingDir"], "/etc/fastsim")
        self.assertTrue(config["history"][-1]["empty_layer"])
        self.assertEqual(manifest["layers"][0]["digest"], layer_digest)
        self.assertIn("blobs/sha256/" + layer_digest[7:], files)
        self.assertNotIn("blobs/sha256/" + built[7:], files)
        self.assertEqual(descriptor["annotations"]["config.digest"], manifest["config"]["digest"])
        self.assertEqual(descriptor["annotations"]["org.opencontainers.image.ref.name"], "9.9")
        self.assertEqual(json.loads(files["manifest.json"])[0]["Config"], "blobs/sha256/" + manifest["config"]["digest"][7:])
        self.assertEqual((record["built_manifest_digest"], record["manifest_digest"]), (built, descriptor["digest"]))

    def test_refuses_a_root_the_start_would_copy_up(self):
        for extra in ([("usr/", "dir", 0o755, "")], [("etc/fastsim/", "dir", 0o755, "")],
                      [("lib/", "dir", 0o755, "")]):
            with self.subTest(extra=extra[0][0]), self.assertRaises(ToolError):
                build.patch_workdir(saved_image(GOOD_ROOT + extra)[0], "/etc/fastsim")

    def test_refuses_wrong_modes_links_and_a_dockerfile_workdir(self):
        cases = {"tmp not 1777": [e if e[0] != "tmp/" else ("tmp/", "dir", 0o755, "") for e in GOOD_ROOT],
                 "batch link": [e if e[0] != "bin/simulate-batch" else ("bin/simulate-batch", "link", 0o777, "/bin/simulate")
                                for e in GOOD_ROOT],
                 "no binary": [e for e in GOOD_ROOT if e[0] != "bin/simulate"]}
        for name, entries in cases.items():
            with self.subTest(case=name), self.assertRaises(ToolError):
                build.patch_workdir(saved_image(entries)[0], "/etc/fastsim")
        with self.assertRaises(ToolError):
            build.patch_workdir(saved_image(GOOD_ROOT, workdir="/app")[0], "/etc/fastsim")


class Context(unittest.TestCase):
    def test_pgo_inputs_and_nested_builder_are_staged_without_unrelated_files(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            source, units = root / "source", root / "units"
            entries = {
                "source/Cargo.toml": "[package]", "source/Cargo.lock": "lock",
                "source/src/lib.rs": "", "source/tools/pgo_build.py": "# trainer",
                "source/tools/unrelated.txt": "excluded", "source/.env": "synthetic-only",
                "units/single/card.toml": "card", "units/single/scenario.json": "{}",
                "units/single/trace.parquet": "excluded",
                "units/batch/card.toml": "card", "units/batch/batch.json": "{}",
                "units/batch/scenarios/sub_00.json": "{}",
                "units/batch/checks/reference_data/trace.parquet": "excluded",
                "Dockerfile": "FROM scratch",
            }
            for name, text in entries.items():
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(text)
            with patch.multiple(build, SOURCE=source, UNITS=units, DOCKERFILE=root / "Dockerfile"):
                files = build.stage_context(root / "context")
            self.assertEqual(set(files), {
                "Dockerfile", "Cargo.toml", "Cargo.lock", "src/lib.rs", "tools/pgo_build.py",
                "training/single/card.toml", "training/single/scenario.json",
                "training/batch/card.toml", "training/batch/batch.json", "training/batch/scenarios/sub_00.json",
            })


if __name__ == "__main__":
    unittest.main()
