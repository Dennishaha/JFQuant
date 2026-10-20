#!/usr/bin/env python3
"""Repository-local architecture acceptance; no third-party Python dependencies."""

import pathlib
import re
import subprocess
import sys

from execution_dependencies import RECEIPT, generate

ROOT = pathlib.Path(__file__).resolve().parents[3]
ALLOWED = {
    "core": set(),
    "factor": {"core"},
    "engine": {"core"},
    "storage": {"core"},
    "workflow": {"core", "factor", "engine", "storage"},
    "cli": {"core", "factor", "storage", "workflow"},
    "app": {"core", "factor", "engine", "storage", "workflow"},
}
errors = []
try:
    if (ROOT / RECEIPT).read_text() != generate(ROOT):
        errors.append(
            "workflow: execution dependency receipt is stale; run checks/execution_dependencies.py --write"
        )
except (OSError, ValueError, KeyError, subprocess.CalledProcessError) as error:
    errors.append(f"workflow: cannot validate execution dependency receipt: {error}")
for component, allowed in ALLOWED.items():
    manifest = (ROOT / "crates" / component / "Cargo.toml").read_text()
    deps = set(re.findall(r'path\s*=\s*"\.\./([^"]+)"', manifest))
    if deps - allowed:
        errors.append(f"{component}: forbidden dependencies {sorted(deps - allowed)}")
    for file in (ROOT / "crates" / component / "src").rglob("*.rs"):
        text = file.read_text()
        for path in re.findall(r'include_(?:str|bytes)!\(\s*"([^"]+)"', text):
            target = (file.parent / path).resolve()
            if not target.is_relative_to(ROOT / "crates" / component):
                errors.append(
                    f"{file.relative_to(ROOT)}: cross-component embedded resource {path}"
                )
        if component in ("app", "cli") and re.search(
            r"\bbackend::Worker\b|\bWorker\s*\{", text
        ):
            errors.append(f"{file.relative_to(ROOT)}: client accesses worker internals")
root = (ROOT / "crates/workflow/src/lib.rs").read_text()
if re.search(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s", root, re.MULTILINE):
    errors.append("workflow/lib.rs: business functions belong in a component module")
if "Box<Worker>" in (ROOT / "crates/workflow/src/backend.rs").read_text():
    errors.append("query sessions must not embed a task worker")


# Warn on new/growing long functions. This heuristic is a review signal, not a correctness gate.
def long_functions(text):
    text = re.split(r"#\[cfg\(test\)\]\s*mod\s", text)[0]
    starts = list(
        re.finditer(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+(\w+)", text, re.MULTILINE)
    )
    return {
        m[1]: text[
            m.start() : starts[i + 1].start() if i + 1 < len(starts) else len(text)
        ].count("\n")
        for i, m in enumerate(starts)
    }


for file in (ROOT / "crates").rglob("*.rs"):
    if "src" not in file.parts or "tests" in file.stem:
        continue
    rel = file.relative_to(ROOT)
    old = subprocess.run(
        ["git", "show", f"HEAD:{rel}"],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=False,
    )
    before = long_functions(old.stdout)
    for name, size in long_functions(file.read_text()).items():
        if size > 200 and size > before.get(name, 0) + 30:
            print(
                f"REVIEW {rel}:{name}: approximately {size} lines; check responsibilities"
            )
for error in errors:
    print("ERROR", error, file=sys.stderr)
print(f"Architecture checks: {len(errors)} violation(s)")
sys.exit(bool(errors))
