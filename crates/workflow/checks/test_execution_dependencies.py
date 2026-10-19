"""Checks the reproducibility and scope of the dependency receipt."""
import copy
from pathlib import Path
import tempfile
import unittest

from execution_dependencies import receipt


def package(root, name, dependencies):
    return {'name': name, 'version': '0.1.0', 'edition': '2024', 'rust_version': None,
            'manifest_path': str(root / name / 'Cargo.toml'), 'features': {},
            'dependencies': dependencies}


def dependency(name, path=None, req='^1.0'):
    return {'name': name, 'source': None if path else 'registry+https://example.test',
            'req': req, 'rename': None, 'optional': False, 'uses_default_features': True,
            'target': None, 'registry': None, 'path': str(path) if path else None,
            'kind': None, 'features': []}


class ExecutionDependencies(unittest.TestCase):
    def test_receipt_is_portable_and_ignores_unrelated_app_contracts(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'Cargo.lock').write_bytes(b'locked dependencies')
            def metadata(base):
                return {'packages': [
                    package(base, 'analytics-workflow', [dependency('analytics-core', base / 'analytics-core')]),
                    package(base, 'analytics-core', [dependency('serde')]),
                    package(base, 'analytics-desktop', [dependency('egui')])]}
            expected = receipt(root, metadata(root))
            relocated = metadata(Path('/relocated'))
            relocated['packages'][2]['dependencies'][0]['req'] = '^99.0'
            self.assertEqual(expected, receipt(root, relocated))
            self.assertEqual(set(expected['dependency_contracts_sha256']),
                             {'analytics-workflow', 'analytics-core'})

    def test_effective_dependency_features_and_lock_changes_invalidate_receipt(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            lock = root / 'Cargo.lock'
            lock.write_bytes(b'first')
            metadata = {'packages': [package(root, 'analytics-workflow', [dependency('serde')])]}
            expected = receipt(root, metadata)
            for key, value in [('req', '^2.0'), ('features', ['derive'])]:
                changed = copy.deepcopy(metadata)
                changed['packages'][0]['dependencies'][0][key] = value
                self.assertNotEqual(expected, receipt(root, changed))
            lock.write_bytes(b'second')
            self.assertNotEqual(expected, receipt(root, metadata))


if __name__ == '__main__':
    unittest.main()
