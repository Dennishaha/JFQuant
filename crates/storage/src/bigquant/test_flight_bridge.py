"""Offline boundary checks: denied permission must never become empty successful data."""
import datetime
import decimal
import importlib.util
import pathlib
import unittest

spec = importlib.util.spec_from_file_location('bridge', pathlib.Path(__file__).with_name('flight_bridge.py'))
bridge = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bridge)


class BridgeTests(unittest.TestCase):
    def test_quota_guard_includes_extra_row_and_checks_table_entitlement(self):
        quota = {'datasources': ['allowed'], 'weekly_quota': 500, 'used_quota': 491}
        self.assertEqual(bridge.read_allowance(quota, 'allowed', 3, 3)['requested_max_cells'], 9)
        with self.assertRaises(RuntimeError):
            bridge.read_allowance(quota, 'allowed', 3, 4)
        with self.assertRaises(PermissionError) as caught:
            bridge.read_allowance(quota, 'not-allowed', 1, 1)
        self.assertEqual(bridge.error_code(caught.exception), 'bigquant_table_not_entitled')
        with self.assertRaises(RuntimeError):
            bridge.read_allowance({'datasources': ['allowed']}, 'allowed', 1, 1)

    def test_provider_denials_and_secret_redaction(self):
        for message, expected in [
            ('get user quota failed: 请先申请SDK使用权限', 'bigquant_sdk_permission_required'),
            ('permission denied fixture-secret', 'bigquant_permission_denied'),
            ('quota exhausted fixture-secret', 'bigquant_quota_denied'),
            ('unhandled fixture-secret', 'bigquant_flight_failed'),
        ]:
            result = bridge.error_code(RuntimeError(message))
            self.assertEqual(result, expected)
            self.assertNotIn('fixture-secret', result)
        self.assertEqual(bridge.error_code(TimeoutError()), 'bigquant_flight_timeout')

    def test_financial_missing_values_and_decimal_precision(self):
        self.assertEqual(bridge.clean({'unknown_equity': None, 'bad': float('nan'),
                                      'amount': decimal.Decimal('123456789012345.67'),
                                      'day': datetime.date(2024, 12, 31)}),
                         {'unknown_equity': None, 'bad': None,
                          'amount': '123456789012345.67', 'day': '2024-12-31'})


if __name__ == '__main__':
    unittest.main()
