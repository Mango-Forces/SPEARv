"""Build a private Bookworm runtime; never install into the OCI host."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
from typing import TypedDict, cast

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
OUT = ROOT / ".build" / "runtime"
PREFIX = "/opt/spearv/python"
PYTHON = f"{PREFIX}/bin/python3.14"
SITE = f"{PREFIX}/lib/python3.14/site-packages"
IMAGE = "localhost/spearv-library-runtime:qualification"


class Archive(TypedDict):
    """The fields needed to verify a pinned source or wheel archive."""

    filename: str
    bytes: int
    sha256: str


class Manifest(TypedDict):
    """Validated inputs to the private runtime build."""

    builder_image: str
    runtime_image: str
    python: Archive
    packages: list[Archive]


def read_manifest(path: Path = HERE / "manifest.json") -> Manifest:
    """Validate the small, repository-owned build manifest at its boundary."""
    manifest = json.loads(path.read_text())
    if not isinstance(manifest, dict) or manifest.get("format") != 1 or manifest.get("prefix") != PREFIX:
        raise ValueError("unsupported runtime manifest")
    for key in ("builder_image", "runtime_image"):
        if not isinstance(manifest.get(key), str) or "@sha256:" not in manifest[key]:
            raise ValueError("runtime images must have immutable digests")
    if not isinstance(manifest.get("python"), dict) or not isinstance(manifest.get("packages"), list):
        raise ValueError("invalid runtime archive list")
    for item in [manifest["python"], *manifest["packages"]]:
        if not isinstance(item, dict) or not isinstance(item.get("filename"), str):
            raise ValueError("invalid archive record")
        if Path(item["filename"]).name != item["filename"] or item["filename"] in (".", ".."):
            raise ValueError("archive filename must be a basename")
        if not isinstance(item.get("sha256"), str) or len(item["sha256"]) != 64:
            raise ValueError("invalid archive hash")
        if not isinstance(item.get("bytes"), int) or item["bytes"] <= 0:
            raise ValueError("invalid archive identity")
    return cast(Manifest, manifest)


def check_archive(path: Path, size: int, digest: str) -> None:
    """Reject incomplete or changed archive bytes before installation."""
    if path.stat().st_size != size:
        raise ValueError(f"archive size differs: {path.name}")
    with path.open("rb") as stream:
        if hashlib.file_digest(stream, "sha256").hexdigest() != digest:
            raise ValueError(f"archive hash differs: {path.name}")


def run(args: list[str], name: str) -> None:
    """Run one recorded build step, preserving diagnostic output."""
    OUT.mkdir(parents=True, exist_ok=True)
    with (OUT / f"{name}.log").open("a") as log:
        log.write(json.dumps(args) + "\n")
        log.flush()
        subprocess.run(args, check=True, stdout=log, stderr=subprocess.STDOUT)


def docker(image: str, mounts: list[str], command: str) -> list[str]:
    """Run a compiler in a bounded container with no network or host installs."""
    return [
        "docker", "run", "--rm", "--pull=never", "--network=none",
        "--memory=3g", "--cpus=2", "--cpuset-cpus=16-19", "--pids-limit=128",
        "--user", f"{os.getuid()}:{os.getgid()}",
        "-e", "OMP_NUM_THREADS=1", "-e", "OPENBLAS_NUM_THREADS=1",
        "-e", "MKL_NUM_THREADS=1", "-e", f"LD_LIBRARY_PATH={PREFIX}/lib",
        *mounts, image, "bash", "-ceu", command,
    ]


def prepare(manifest: Manifest) -> None:
    """Check all payloads and extract only this build's Python source."""
    if shutil.disk_usage(ROOT).free < 10 * 1024**3:
        raise RuntimeError("less than 10 GiB disk headroom; no cleanup attempted")
    for item in [manifest["python"], *manifest["packages"]]:
        archive = OUT / "archives" / item["filename"]
        check_archive(archive, item["bytes"], item["sha256"])
    source = OUT / "source" / "Python-3.14.4"
    marker = OUT / "source" / "archive.sha256"
    if source.exists():
        if not marker.exists() or marker.read_text().strip() != manifest["python"]["sha256"]:
            raise RuntimeError("refusing an unidentified existing Python source tree")
        return
    source.parent.mkdir(parents=True, exist_ok=True)
    with tarfile.open(OUT / "archives" / manifest["python"]["filename"]) as archive:
        archive.extractall(source.parent, filter="data")
    if '#define PY_VERSION              "3.14.4"' not in (source / "Include" / "patchlevel.h").read_text():
        raise ValueError("Python source version differs")
    marker.write_text(manifest["python"]["sha256"] + "\n")


def compile_runtime(manifest: Manifest) -> None:
    """Build and install only beneath the staged private prefix."""
    for name in ("build", "install"):
        (OUT / name).mkdir(exist_ok=True)
    mounts = [
        "-v", f"{OUT / 'source'}:/source:ro",
        "-v", f"{OUT / 'build'}:/build",
        "-v", f"{OUT / 'install'}:/install",
    ]
    command = (
        "cd /build; /source/Python-3.14.4/configure "
        f"--prefix={PREFIX} --enable-shared --with-ensurepip=install; "
        "make -j2; make -j2 altinstall DESTDIR=/install"
    )
    run(docker(manifest["builder_image"], mounts, command), "compile-python")
    prefix = OUT / "install" / PREFIX.lstrip("/")
    mounts = [
        "-v", f"{prefix}:{PREFIX}",
        "-v", f"{OUT / 'archives'}:/wheels:ro",
        "-v", f"{HERE}:/recipe:ro",
        "-v", f"{OUT}:/evidence",
    ]
    command = (
        f"{PYTHON} -I -B -m pip install --no-index --no-cache-dir "
        "--disable-pip-version-check --require-hashes "
        "--find-links=/wheels -r /recipe/requirements.lock; "
        f"cc /recipe/embed_probe.c -o /evidence/embed-probe "
        f"$({PREFIX}/bin/python3.14-config --embed --cflags --ldflags) "
        f"-Wl,-rpath,{PREFIX}/lib; "
        f"{PYTHON} -I -B /recipe/preflight.py /recipe/manifest.json"
    )
    run(docker(manifest["builder_image"], mounts, command), "install-and-embed")


def build_image() -> None:
    """Stage only private runtime artifacts, not the entire source checkout."""
    context = OUT / "context"
    if context.exists():
        raise RuntimeError("runtime image context already exists; inspect before reuse")
    context.mkdir()
    shutil.copytree(OUT / "install" / PREFIX.lstrip("/"), context / "python")
    for name in ("Dockerfile", "preflight.py", "manifest.json"):
        shutil.copy2(HERE / name, context / name)
    shutil.copy2(OUT / "embed-probe", context / "embed-probe")
    run([
        "docker", "build", "--network=default", "-t", IMAGE,
        "--label", "org.spearv.purpose=library-runtime-qualification",
        str(context),
    ], "build-image")


def qualify() -> None:
    """Exercise the new ABI and deterministic runtime failure cases."""
    common = [
        "docker", "run", "--rm", "--pull=never", "--network=none",
        "--read-only", "--memory=512m", "--cpus=2",
        "--cpuset-cpus=16-19", "--pids-limit=64",
        "--tmpfs", "/tmp:rw,size=64m",
    ]
    cases = [
        ("private-runtime", [], [], None),
        ("private-embedding", [], ["/app/embed-probe"], None),
        ("missing-metadata", ["--tmpfs", f"{SITE}/numpy-2.5.4.dist-info:rw,size=1m"], [], "private dependency versions differ"),
        ("missing-data", ["--tmpfs", f"{SITE}/astropy_iers_data/data:rw,size=1m"], [], "finals2000A"),
        ("missing-package-libraries", ["--tmpfs", f"{SITE}/numpy.libs:rw,size=1m"], [], "ImportError"),
        ("missing-interpreter-libraries", ["--tmpfs", f"{PREFIX}/lib:rw,size=1m"], [], "libpython3.14"),
    ]
    results = []
    for name, extra, command, expected_error in cases:
        result = subprocess.run(
            [*common, *extra, IMAGE, *command],
            text=True, capture_output=True,
        )
        (OUT / f"{name}.stdout").write_text(result.stdout)
        (OUT / f"{name}.stderr").write_text(result.stderr)
        valid = result.returncode == 0 if expected_error is None else (
            result.returncode != 0 and expected_error in result.stderr
        )
        results.append({"name": name, "returncode": result.returncode, "expected_outcome": valid})
        (OUT / "qualification.json").write_text(json.dumps(results, indent=2) + "\n")
        if not valid:
            raise RuntimeError(f"runtime qualification failed: {name}")
    print(json.dumps(results, indent=2))


def main() -> None:
    """Run a bounded runtime stage; caller holds the shared measurement lock."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("stage", choices=("prepare", "compile", "image", "test", "all"))
    stage = parser.parse_args().stage
    manifest = read_manifest()
    if stage in ("prepare", "all"):
        prepare(manifest)
    if stage in ("compile", "all"):
        compile_runtime(manifest)
    if stage in ("image", "all"):
        build_image()
    if stage in ("test", "all"):
        qualify()


if __name__ == "__main__":
    main()
