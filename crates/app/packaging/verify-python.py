"""Audit every bundled Python Mach-O object before signing the distribution."""

import json
import re
import subprocess
import sys
from pathlib import Path

MACHO_MAGIC = (
    b"\xcf\xfa\xed\xfe",
    b"\xfe\xed\xfa\xcf",
    b"\xca\xfe\xba\xbe",
    b"\xca\xfe\xba\xbf",
)


def inside(path, root):
    resolved = path.resolve()
    if not resolved.is_relative_to(root):
        raise RuntimeError(f"dependency escapes Python bundle: {path}")
    return resolved


def verify_load_commands(file, root, load, dependencies):
    """Validate the arm64 slice, including legacy OS commands and actual dependency targets."""
    versions = re.findall(r"\bminos\s+([\d.]+)", load)
    versions += re.findall(
        r"cmd LC_VERSION_MIN_MACOSX\s+cmdsize \d+\s+version ([\d.]+)", load
    )
    if not versions:
        raise RuntimeError(f"missing minimum OS version: {file}")
    for version in versions:
        parsed = tuple(map(int, version.split(".")))
        if (parsed + (0, 0, 0))[:3] > (14, 0, 0):
            raise RuntimeError(f"minimum OS exceeds macOS14.0: {file}: {version}")
    rpaths = re.findall(r"cmd LC_RPATH\s+cmdsize \d+\s+path (.*?) \(offset", load)

    def relative(name):
        if name.startswith("@loader_path/"):
            return inside(file.parent / name.removeprefix("@loader_path/"), root)
        if name.startswith("@executable_path/"):
            return inside(root / "bin" / name.removeprefix("@executable_path/"), root)
        raise RuntimeError(f"unsupported Python runtime path: {file}: {name}")

    search_paths = [relative(path if path.endswith("/") else path + "/") for path in rpaths]
    for index, name in enumerate(dependencies):
        # otool lists LC_ID_DYLIB first; its spelling is not a dependency target.
        if index == 0 and "cmd LC_ID_DYLIB" in load:
            continue
        if name.startswith(("/usr/lib/", "/System/Library/")):
            continue
        if name.startswith("@rpath/"):
            candidates = [
                inside(p / name.removeprefix("@rpath/"), root) for p in search_paths
            ]
        else:
            candidates = [relative(name)]
        if not any(path.is_file() for path in candidates):
            raise RuntimeError(f"missing package dependency: {file}: {name}")


def audit(root):
    root = root.resolve()
    machos = []
    for file in root.rglob("*"):
        if file.is_symlink():
            inside(file, root)
            if not file.exists():
                raise RuntimeError(f"broken Python symlink: {file}")
            continue
        if not file.is_file():
            continue
        with file.open("rb") as stream:
            magic = stream.read(4)
        if magic not in MACHO_MAGIC:
            continue
        subprocess.run(
            ["/usr/bin/lipo", "-verify_arch", "arm64", str(file)], check=True
        )
        load = subprocess.check_output(
            ["/usr/bin/otool", "-arch", "arm64", "-l", str(file)], text=True
        )
        dependencies = [
            line.strip().split(" (", 1)[0]
            for line in subprocess.check_output(
                ["/usr/bin/otool", "-arch", "arm64", "-L", str(file)], text=True
            ).splitlines()[1:]
        ]
        verify_load_commands(file, root, load, dependencies)
        machos.append(str(file.relative_to(root)))
    if not machos:
        raise RuntimeError("no Python Mach-O objects found")
    return machos


if __name__ == "__main__":
    objects = audit(Path(sys.argv[1]))
    if len(sys.argv) > 2:
        Path(sys.argv[2]).write_text(
            json.dumps({"arm64": True, "macos14": True, "objects": objects}, indent=2)
            + "\n"
        )
    print(json.dumps({"arm64": True, "macos14": True, "native_objects": len(objects)}))
