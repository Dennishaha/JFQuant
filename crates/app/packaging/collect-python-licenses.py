"""Copy installed-package notices and license metadata without writing to the runtime."""

import json
import shutil
import sys
from importlib import metadata
from pathlib import Path

output = Path(sys.argv[1])
output.mkdir(parents=True, exist_ok=True)
manifest = []
for dist in metadata.distributions():
    name = dist.metadata["Name"]
    target = output / name
    target.mkdir(exist_ok=True)
    notices = []
    for file in dist.files or []:
        if any(
            part.lower().startswith(("license", "licence", "copying", "notice"))
            for part in file.parts
        ):
            source = Path(dist.locate_file(file))
            if source.is_file():
                relative = "_".join(file.parts)
                shutil.copyfile(source, target / relative)
                notices.append(relative)
    (target / "METADATA.txt").write_text(
        dist.read_text("METADATA")
        or json.dumps(list(dist.metadata.items()), ensure_ascii=False),
        encoding="utf-8",
    )
    manifest.append(
        {
            "name": name,
            "version": dist.version,
            "license": dist.metadata.get("License-Expression")
            or dist.metadata.get("License"),
            "notices": notices,
        }
    )
(output / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
