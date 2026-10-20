"""Pinned official SDK bridge: no credentials in argv, files or error output."""
import contextlib
import datetime
import decimal
import io
import json
import math
import re
import signal
import sys


# Includes authentication and Flight read; parent also supervises task cancellation.
def deadline_expired(*_):
    raise TimeoutError()

signal.signal(signal.SIGALRM, deadline_expired)
if __name__ == '__main__':
    signal.alarm(90)


def clean(value):
    if isinstance(value, dict):
        return {k: clean(v) for k, v in value.items()}
    if isinstance(value, (list, tuple)):
        return [clean(v) for v in value]
    if isinstance(value, (datetime.datetime, datetime.date)):
        return value.isoformat()
    if isinstance(value, decimal.Decimal):
        return str(value)
    if isinstance(value, float) and not math.isfinite(value):
        return None
    return value


def read_allowance(quota, table, columns, max_rows):
    """Check the current SDK allowlist and worst-case cells before downloading."""
    if not isinstance(quota, dict) or not isinstance(quota.get('datasources'), list):
        raise TypeError('quota response invalid')
    if table not in quota['datasources']:
        raise PermissionError('table not in SDK allowlist')
    weekly = quota.get('weekly_quota')
    used = quota.get('used_quota')
    if (type(weekly) is not int or type(used) is not int
            or weekly < 0 or used < 0):
        raise RuntimeError('quota response invalid')
    bound = columns * max_rows
    if bound > max(0, weekly - used):
        raise RuntimeError('quota insufficient for requested upper bound')
    return {'weekly_quota': weekly, 'used_quota': used,
            'requested_max_cells': bound}


def main():
    request = json.load(sys.stdin)
    # The SDK may log authentication diagnostics. Never relay its raw exceptions/logs.
    with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
        import importlib.metadata
        if importlib.metadata.version('bigquant') != '0.1.11' or importlib.metadata.version('bigquant-core') != '0.1.14':
            return {'ok': False, 'error': 'bigquant_sdk_version_mismatch'}
        import bigquant
        ak, sk = request.pop('key').split('.', 1)
        bigquant.init(ak=ak, sk=sk)
        from bigquant import dai
        if request['operation'] == 'quota':
            quota = dai.get_data_quota()
            if not isinstance(quota, (dict, list)):
                raise RuntimeError(str(quota))
            result = {'quota': clean(quota)}
        else:
            quota = dai.get_data_quota()
            tables = request.get('tables', [request['table']])
            if not tables or request['table'] not in tables:
                raise ValueError('invalid query table scope')
            for table_id in tables:
                read_allowance(quota, table_id, request['column_count'], request['max_rows'])
            allowance = read_allowance(quota, request['table'],
                                       request['column_count'], request['max_rows'])
            table = dai.query(request['sql'], filters=request['filters']).arrow()
            result = {'rows': clean(table.to_pylist()), 'arrow_schema': str(table.schema),
                      'allowance': allowance}
        bigquant.close()
        return {'ok': True, **result}


def error_code(error):
    # Never return provider exception text, which can contain authentication data.
    msg = str(error).lower()
    if 'binder error' in msg:
        headline = msg.split('binder error:', 1)[1].splitlines()[0]
        # Only short quoted SQL identifiers; never exception text, query text or credentials.
        hints = re.findall(r'["`\']([a-z_][a-z0-9_]{0,30})["`\']', headline)[:3]
        return 'bigquant_sql_binding' + (('_' + '_'.join(hints)) if hints else '')
    for marker, code in (
        ('ambiguous', 'bigquant_sql_ambiguous'),
        ('binder error', 'bigquant_sql_binding'),
        ('parser error', 'bigquant_sql_parse'),
        ('maximum expression depth', 'bigquant_sql_expression_limit'),
        ('too large', 'bigquant_query_too_large'),
        ('out of memory', 'bigquant_query_memory_limit'),
    ):
        if marker in msg:
            return code
    if 'table not in sdk allowlist' in msg:
        return 'bigquant_table_not_entitled'
    if 'sdk' in msg and any(x in msg for x in ('申请', 'permission', '权限')):
        return 'bigquant_sdk_permission_required'
    if isinstance(error, ModuleNotFoundError):
        return 'bigquant_sdk_missing'
    if isinstance(error, TimeoutError):
        return 'bigquant_flight_timeout'
    if any(x in msg for x in ('permission', 'forbidden', 'not authorized', '权限', '无权')):
        return 'bigquant_permission_denied'
    if any(x in msg for x in ('quota', '额度')):
        return 'bigquant_quota_denied'
    if any(x in msg for x in ('unauthenticated', 'authentication', 'credential', '401')):
        return 'bigquant_authentication_failed'
    return 'bigquant_flight_failed'


if __name__ == '__main__':
    try:
        result = main()
    except (OSError, ValueError, TypeError, PermissionError, KeyError) as error:
        result = {'ok': False, 'error': error_code(error)}
    print(json.dumps(result, ensure_ascii=False, allow_nan=False, separators=(',', ':')))
