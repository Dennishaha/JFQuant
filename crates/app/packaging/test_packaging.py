"""Load-command fixtures test failures that clean-environment import cannot expose."""

import importlib.util
import tempfile
import unittest
from pathlib import Path

spec = importlib.util.spec_from_file_location(
    "verify_python", Path(__file__).with_name("verify-python.py")
)
verifier = importlib.util.module_from_spec(spec)
spec.loader.exec_module(verifier)


class NativeAudit(unittest.TestCase):
    def test_exact_minimum_os_and_legacy_commands(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            file = root / "bin/python"
            for version in ("14.1", "14.0.1", "15.0"):
                for load in (
                    f"cmd LC_BUILD_VERSION\ncmdsize 32\nminos {version}",
                    f"cmd LC_VERSION_MIN_MACOSX\ncmdsize 16\nversion {version}",
                ):
                    with self.assertRaisesRegex(RuntimeError, "exceeds"):
                        verifier.verify_load_commands(file, root, load, [])
            with self.assertRaisesRegex(RuntimeError, "missing minimum"):
                verifier.verify_load_commands(file, root, "", [])
            verifier.verify_load_commands(
                file, root, "cmd LC_BUILD_VERSION\nminos 14.0", []
            )

    def test_rpath_must_resolve_and_cannot_escape_bundle(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            (root / "lib").mkdir()
            file = root / "bin/python"
            load = "cmd LC_BUILD_VERSION\nminos 11.0\ncmd LC_RPATH\ncmdsize 40\npath @executable_path/../lib (offset 12)"
            with self.assertRaisesRegex(RuntimeError, "missing package"):
                verifier.verify_load_commands(
                    file, root, load, ["@rpath/libtest.dylib"]
                )
            (root / "lib/libtest.dylib").touch()
            verifier.verify_load_commands(file, root, load, ["@rpath/libtest.dylib"])
            with self.assertRaisesRegex(RuntimeError, "escapes"):
                verifier.verify_load_commands(
                    file, root, load, ["@loader_path/../../external.dylib"]
                )
            with self.assertRaisesRegex(RuntimeError, "unsupported"):
                verifier.verify_load_commands(
                    file, root, load, ["/opt/homebrew/lib/test.dylib"]
                )

    def test_library_identity_is_not_an_external_dependency(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            verifier.verify_load_commands(
                root / "lib/test.dylib",
                root,
                "cmd LC_ID_DYLIB\ncmd LC_BUILD_VERSION\nminos 14.0",
                ["libtest.dylib", "/usr/lib/libSystem.B.dylib"],
            )


if __name__ == "__main__":
    unittest.main()
