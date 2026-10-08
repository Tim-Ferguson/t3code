#!/usr/bin/env python3
"""Prepare the instrumented committed snapshot only; never build or launch it.

Python >=3.12. Cached dioxus-desktop0.7.10 is copied, then its original hashes
are checked before applying the small patch. Source and cargo registry stay intact.
"""
import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile

parser = argparse.ArgumentParser()
parser.add_argument("snapshot", type=Path)
parser.add_argument("patched_dependency", type=Path)
parser.add_argument("--repository", type=Path, default=Path(__file__).resolve().parents[4])
parser.add_argument("--bundle-identifier", required=True)
args = parser.parse_args()
here = Path(__file__).resolve().parent
commit = "fcd48c83a4aa9fa7d79ccd3336f43256c8eb4fa7"
for directory in [args.snapshot, args.patched_dependency]:
    if not str(directory.resolve()).startswith("/private/tmp/t3port-bench-") or directory.exists():
        raise SystemExit("Both destinations must be new /private/tmp/t3port-bench-* directories")
if not args.bundle_identifier.startswith("org.t3port.benchmark."):
    raise SystemExit("An explicit unique benchmark bundle identifier is required")
cargo_home = Path(os.environ.get("CARGO_HOME", str(Path.home() / ".cargo")))
candidates = sorted((cargo_home / "registry/src").glob("*/dioxus-desktop-0.7.10"))
manifest = json.loads((here / "dioxus-benchmark-patch-manifest.json").read_text())
def sha(file):
    return hashlib.sha256(file.read_bytes()).hexdigest()
source = next((candidate for candidate in candidates if all(
    sha(candidate / file) == digest
    for file, digest in manifest["originalChangedFiles"].items()
)), None)
if source is None:
    raise SystemExit("Exact pinned dioxus-desktop0.7.10 cache is required; no automatic download")
if sha(here / "dioxus-desktop-0.7.10-benchmark.patch") != manifest["patchSha256"]:
    raise SystemExit("Benchmark patch hash mismatch")
archive = subprocess.run(["git", "archive", commit, "rust"], cwd=args.repository,
                         stdout=subprocess.PIPE, check=True).stdout
args.snapshot.mkdir(parents=True)
with tarfile.open(fileobj=io.BytesIO(archive)) as tar:
    tar.extractall(args.snapshot, filter="data")
shutil.copytree(source, args.patched_dependency)
subprocess.run(["/usr/bin/patch", "-p1", "--batch", "-i",
                str(here / "dioxus-desktop-0.7.10-benchmark.patch")],
               cwd=args.patched_dependency, check=True)
root = args.snapshot / "rust"
shutil.copyfile(here / "rust-desktop-main.rs", root / "crates/ui/src/main.rs")
cargo = root / "Cargo.toml"
cargo.write_text(cargo.read_text() + "\n[patch.crates-io]\ndioxus-desktop = { path = " +
                 json.dumps(str(args.patched_dependency.resolve())) + " }\n")
config = root / "crates/ui/Dioxus.toml"
config.write_text(config.read_text() + "\n[bundle]\nidentifier = " +
                  json.dumps(args.bundle_identifier) + "\n")
print(json.dumps({"sourceCommit": commit, "snapshot": str(args.snapshot.resolve()),
                  "manifest": str(cargo.resolve()), "bundleIdentifier": args.bundle_identifier,
                  "next": "Generate terminal surface assets, resolve offline lock, then explicitly build; nothing launched"}))
