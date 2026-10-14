"""Relocate installed wheel library identities/references before auditing and signing."""

import os
import subprocess
import sys
from pathlib import Path

root = Path(sys.argv[1]).resolve()
objects = []
identities = {}
for path in root.rglob("*"):
    if not path.is_file() or path.is_symlink():
        continue
    with path.open("rb") as stream:
        if stream.read(4) not in (
            b"\xcf\xfa\xed\xfe",
            b"\xfe\xed\xfa\xcf",
            b"\xca\xfe\xba\xbe",
            b"\xca\xfe\xba\xbf",
        ):
            continue
    load = subprocess.check_output(
        ["/usr/bin/otool", "-arch", "arm64", "-l", str(path)], text=True
    )
    dependencies = [
        line.strip().split(" (", 1)[0]
        for line in subprocess.check_output(
            ["/usr/bin/otool", "-arch", "arm64", "-L", str(path)], text=True
        ).splitlines()[1:]
    ]
    if "cmd LC_ID_DYLIB" in load and dependencies:
        identities[dependencies[0]] = path
        if dependencies[0] == "@loader_path/" + path.name:
            objects.append((path, dependencies))
            continue
        subprocess.run(
            [
                "/usr/bin/install_name_tool",
                "-id",
                "@loader_path/" + path.name,
                str(path),
            ],
            check=True,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
        )
    objects.append((path, dependencies))
for path, dependencies in objects:
    for old in dependencies:
        if (
            old.startswith("@loader_path/")
            and (path.parent / old.removeprefix("@loader_path/")).exists()
        ):
            continue
        target = identities.get(old)
        if target is not None and target != path:
            new = "@loader_path/" + os.path.relpath(target, path.parent)
            subprocess.run(
                ["/usr/bin/install_name_tool", "-change", old, new, str(path)],
                check=True,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE,
            )
