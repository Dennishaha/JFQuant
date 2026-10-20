#!/usr/bin/env python3
"""Freeze workflow's effective Rust dependency contracts without embedding other crates."""

import argparse
import hashlib
import json
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
RECEIPT = Path("crates/workflow/research/rust-execution-dependencies.json")


def digest(data):
    return "sha256:" + hashlib.sha256(data).hexdigest()


def receipt(root, metadata):
    packages = {p["name"]: p for p in metadata["packages"]}
    local = {str(Path(p["manifest_path"]).parent): p["name"] for p in packages.values()}
    contracts = {}
    pending = ["analytics-workflow"]
    while pending:
        name = pending.pop()
        if name in contracts:
            continue
        package = packages[name]
        dependencies = []
        for dependency in package["dependencies"]:
            if dependency["kind"] is not None:
                continue
            entry = {
                k: dependency[k]
                for k in (
                    "name",
                    "source",
                    "req",
                    "rename",
                    "optional",
                    "uses_default_features",
                    "target",
                    "registry",
                )
            }
            entry["features"] = sorted(dependency["features"])
            if dependency.get("path") is not None:
                entry["component"] = local[dependency["path"]]
                pending.append(entry["component"])
            dependencies.append(entry)
        contract = {
            k: package[k] for k in ("name", "version", "edition", "rust_version")
        }
        contract["features"] = {k: sorted(v) for k, v in package["features"].items()}
        contract["dependencies"] = sorted(
            dependencies, key=lambda d: json.dumps(d, sort_keys=True)
        )
        contracts[name] = digest(
            json.dumps(contract, sort_keys=True, separators=(",", ":")).encode()
        )
    return {
        "schema_version": 1,
        "cargo_lock_sha256": digest((root / "Cargo.lock").read_bytes()),
        "dependency_contracts_sha256": dict(sorted(contracts.items())),
    }


def generate(root=ROOT):
    metadata = subprocess.run(
        [
            "cargo",
            "metadata",
            "--locked",
            "--offline",
            "--no-deps",
            "--format-version",
            "1",
        ],
        cwd=root,
        check=True,
        capture_output=True,
        text=True,
    )
    return (
        json.dumps(receipt(root, json.loads(metadata.stdout)), indent=2, sort_keys=True)
        + "\n"
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--write", action="store_true", help="refresh the component-owned receipt"
    )
    args = parser.parse_args()
    expected = generate()
    path = ROOT / RECEIPT
    if args.write:
        path.write_text(expected)
        print("Updated", RECEIPT)
    elif not path.is_file() or path.read_text() != expected:
        parser.exit(
            1,
            "Execution dependency receipt is stale; run: "
            "python3 crates/workflow/checks/execution_dependencies.py --write\n",
        )
    else:
        print("Execution dependency receipt: current")


if __name__ == "__main__":
    main()
