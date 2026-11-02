#!/usr/bin/env python3
"""Portable, offline framework identity check; copied into each Skill by sync.py."""
import argparse
import hashlib
import json
import pathlib
import os
import shutil
import subprocess
import sys


def target_triple():
    host = next(line.split(": ", 1)[1] for line in subprocess.check_output(["rustc", "-vV"], text=True).splitlines() if line.startswith("host: "))
    return os.environ.get("CARGO_BUILD_TARGET", host)


def digest(files):
    """Hash UTF-8 path, NUL, lowercase file SHA-256, LF in sorted path order."""
    return hashlib.sha256(
        "".join(f"{name}\0{value}\n" for name, value in sorted(files.items())).encode()
    ).hexdigest()


FRAMEWORK_PACKAGES = {"modorch", "modorch-core", "modorch-registry", "modorch-runtime",
                      "modorch-eventhub", "modorch-scheduler", "modorch-config", "modorch-observe"}


def identity(packages):
    files = {}
    for package in packages:
        name = package["name"]
        if name not in FRAMEWORK_PACKAGES:
            continue
        base = pathlib.Path(package["manifest_path"]).parent
        for path in [base / "Cargo.toml", *sorted((base / "src").rglob("*.rs"))]:
            if path.is_symlink() or not path.is_file():
                raise ValueError(f"framework source is missing or a symlink: {path.name}")
            files[f"{name}/{path.relative_to(base).as_posix()}"] = hashlib.sha256(
                path.read_bytes()
            ).hexdigest()
    if "modorch/Cargo.toml" not in files:
        raise ValueError("resolved dependency graph has no modorch Facade")
    return {"files": files, "sha256": digest(files)}


def evaluate(metadata, expected):
    packages = metadata["packages"]
    facades = [p for p in packages if p["name"] == "modorch"]
    if len(facades) != 1:
        raise ValueError("expected exactly one resolved modorch dependency")
    facade = facades[0]
    if facade["version"] not in expected["compatible_versions"]:
        raise ValueError(f"unsupported modorch version: {facade['version']}")
    nodes = metadata.get("resolve", {}).get("nodes", [])
    node = next((n for n in nodes if n["id"] == facade["id"]), None)
    if node is None:
        raise ValueError("Cargo metadata did not resolve Facade features")
    features = sorted(node.get("features", []))
    if not set(expected.get("required_features", [])) <= set(features):
        raise ValueError("the selected Skill needs runtime-enabled Facade APIs")
    if set(features) - set(expected["features"]):
        raise ValueError("unknown Facade features")
    # A feature subset resolves fewer framework crates. Every resolved crate must match,
    # and its exact files must match the expected subset; extra/missing files are rejected.
    actual = identity(packages)
    names = {p["name"] for p in packages if p["name"] in FRAMEWORK_PACKAGES}
    subset = {k: v for k, v in expected["identity"]["files"].items() if k.split("/")[0] in names}
    if actual["files"] != subset or not subset:
        raise ValueError("framework fingerprint mismatch; install a matching Bundle or revalidate the change")
    return {
        "status": "Passed", "framework_version": facade["version"],
        "features": features, "framework_sha256": actual["sha256"],
        "skill_revision": expected["skill_revision"],
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest-path", type=pathlib.Path, required=True)
    parser.add_argument("--identity", type=pathlib.Path,
                        default=pathlib.Path(__file__).resolve().parent.parent / "references/framework-identity.json")
    args = parser.parse_args()
    report = {"status": "Blocked", "validation_status": "NotRun"}
    if not shutil.which("cargo") or not shutil.which("rustc"):
        report["reason"] = "Rust/Cargo toolchain is unavailable; tests NotRun"
    else:
        try:
            target = target_triple()
            result = subprocess.run(
                ["cargo", "metadata", "--format-version=1", "--locked", "--offline",
                 "--manifest-path", str(args.manifest_path), "--filter-platform", target],
                capture_output=True, text=True, timeout=60, check=False,
            )
            if result.returncode:
                report["reason"] = "Cargo metadata failed (lockfile/cache/version); API calls blocked"
                report["diagnostic"] = result.stderr.strip()
            else:
                report = evaluate(json.loads(result.stdout), json.loads(args.identity.read_text()))
        except (ValueError, OSError, subprocess.TimeoutExpired, KeyError) as error:
            report["reason"] = str(error)
    print(json.dumps(report, ensure_ascii=False, indent=2))
    return 0 if report["status"] == "Passed" else 2


if __name__ == "__main__":
    sys.exit(main())
