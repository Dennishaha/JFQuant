"""Deterministic collector contracts. No public network is used by these tests."""

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import MagicMock, patch

import pandas as pd

from jfquant_collectors.providers import canonical_akshare, canonical_yfinance, market, nullable


class ProviderContracts(unittest.TestCase):
    def test_units_adjustment_and_nulls(self):
        raw = pd.DataFrame(
            [
                {
                    "日期": "2020-01-02",
                    "股票代码": "600000",
                    "开盘": 10,
                    "最高": 11,
                    "最低": 9,
                    "收盘": 10,
                    "成交量": 12,
                    "成交额": 12000,
                }
            ]
        )
        adjusted = raw.copy()
        for key in ("开盘", "最高", "最低", "收盘"):
            adjusted[key] *= 2
        bar = canonical_akshare("SH600000", raw, adjusted)[0]
        self.assertEqual(bar["volume"], 1200)
        self.assertEqual(bar["amount"], 12000)
        self.assertEqual(bar["open"], 10)
        self.assertEqual(bar["adj_open"], 20)
        self.assertIsNone(nullable(float("nan")))
        self.assertIsNone(nullable(float("inf")))
        self.assertIsNone(nullable("not a number"))
        raw.loc[0, "开盘"] = float("nan")
        self.assertIsNone(canonical_akshare("SH600000", raw, adjusted)[0]["open"])

    def test_duplicate_dates_and_security_mismatch_are_rejected(self):
        frame = pd.DataFrame(
            [
                {
                    "日期": "2020-01-02",
                    "股票代码": "600000",
                    "开盘": 10,
                    "最高": 11,
                    "最低": 9,
                    "收盘": 10,
                    "成交量": 10,
                    "成交额": 10000,
                }
            ]
            * 2
        )
        with self.assertRaises(ValueError):
            canonical_akshare("SH600000", frame, frame)
        with self.assertRaises(ValueError):
            canonical_akshare("SZ000001", frame.iloc[:1], frame.iloc[:1])

    def test_adjusted_identity_and_duplicate_dates_are_rejected(self):
        frame = pd.DataFrame(
            [
                {
                    "日期": "2020-01-02",
                    "股票代码": "600000",
                    "开盘": 10,
                    "最高": 11,
                    "最低": 9,
                    "收盘": 10,
                    "成交量": 10,
                    "成交额": 10000,
                }
            ]
        )
        with self.assertRaisesRegex(ValueError, "duplicate adjusted"):
            canonical_akshare("SH600000", frame, pd.concat([frame, frame]))
        other = frame.copy()
        other.loc[0, "股票代码"] = "000001"
        with self.assertRaisesRegex(ValueError, "adjusted security mismatch"):
            canonical_akshare("SH600000", frame, other)

    def test_mismatched_code_identity_fails_before_provider_or_cache(self):
        with tempfile.TemporaryDirectory() as directory:
            cache = Path(directory) / "cache"
            result = subprocess.run(
                [sys.executable, "-I", "-B", "-m", "jfquant_collectors"],
                input=json.dumps(
                    {
                        "version": 7,
                        "operation": "market",
                        "expected_collector_signature": "wrong",
                        "cache_dir": str(cache),
                    }
                )
                + "\n",
                capture_output=True,
                text=True,
                check=False,
                timeout=10,
            )
            self.assertEqual(result.returncode, 1)
            event = json.loads(result.stdout)
            self.assertEqual(event["event"], "error")
            self.assertIn("signature mismatch", event["message"])
            self.assertFalse(cache.exists())

    def test_yahoo_is_never_claimed_raw(self):
        frame = pd.DataFrame(
            {
                "Open": [10.0],
                "High": [11.0],
                "Low": [9.0],
                "Close": [10.0],
                "Volume": [float("nan")],
            },
            index=pd.DatetimeIndex(["2020-01-02"], tz="Asia/Shanghai"),
        )
        bar = canonical_yfinance("SH600000", frame)[0]
        self.assertIsNone(bar["open"])
        self.assertEqual(bar["adj_open"], 10)
        self.assertIsNone(bar["volume"])
        self.assertIsNone(bar["amount"])
        self.assertEqual(bar["trade_date"], "2020-01-02")

    def test_yahoo_explicit_parameters_and_exclusive_end(self):
        frame = pd.DataFrame(
            {"Close": [10]}, index=pd.DatetimeIndex(["2020-01-02"], tz="Asia/Shanghai")
        )
        ticker = MagicMock()
        ticker.history.return_value = frame
        ticker.get_history_metadata.return_value = {
            "symbol": "600000.SS",
            "currency": "CNY",
            "exchangeTimezoneName": "Asia/Shanghai",
        }
        request = {
            "provider": "yfinance",
            "adjustment_source": "none",
            "start": "2020-01-02",
            "end": "2020-01-03",
            "instruments": ["SH600000"],
            "benchmarks": [],
            "cache_dir": "/tmp/jfquant-test-cache",
            "universes": [],
        }
        calendar = pd.DataFrame({"trade_date": ["2020-01-02", "2020-01-03"]})
        with (
            patch("yfinance.Ticker", return_value=ticker),
            patch("yfinance.set_tz_cache_location"),
            patch("akshare.tool_trade_date_hist_sina", return_value=calendar),
        ):
            events = []
            market(request, lambda event, **value: events.append((event, value)))
        kwargs = ticker.history.call_args.kwargs
        self.assertEqual(kwargs["end"], "2020-01-04")
        for field in ("auto_adjust", "back_adjust", "repair"):
            self.assertIs(kwargs[field], False)
        self.assertIs(kwargs["keepna"], True)
        self.assertIs(kwargs["actions"], True)
        self.assertEqual(events[-1][0], "complete")

    def test_independent_coefficients_do_not_call_quotes_or_actions(self):
        from jfquant_collectors.providers import coefficients

        request = {
            "provider": "akshare",
            "adjustment_source": "sina_hfq",
            "instruments": ["SH600000"],
            "benchmarks": [],
            "end": "2020-01-03",
        }
        events = []
        with (
            patch(
                "jfquant_collectors.providers.fetch_adjustment", return_value={"fixture": True}
            ) as factor,
            patch("akshare.stock_zh_a_hist", side_effect=AssertionError("quote dependency")),
            patch(
                "akshare.stock_history_dividend_detail",
                side_effect=AssertionError("action dependency"),
            ),
        ):
            coefficients(request, lambda event, **value: events.append((event, value)))
        factor.assert_called_once()
        self.assertEqual(events[0][0], "adjustment_shard")
        self.assertEqual(events[-1][0], "complete")

    def test_quotes_only_does_not_depend_on_factor_endpoint(self):
        raw = pd.DataFrame(
            [
                {
                    "日期": "2020-01-02",
                    "股票代码": "600000",
                    "开盘": 10,
                    "最高": 11,
                    "最低": 9,
                    "收盘": 10,
                    "成交量": 12,
                    "成交额": 12000,
                }
            ]
        )
        request = {
            "operation": "quotes",
            "provider": "akshare",
            "adjustment_source": "sina_hfq",
            "start": "2020-01-02",
            "end": "2020-01-03",
            "instruments": ["SH600000"],
            "benchmarks": [],
            "universes": [],
        }
        events = []
        with (
            patch("akshare.stock_zh_a_hist", return_value=raw),
            patch(
                "akshare.tool_trade_date_hist_sina",
                return_value=pd.DataFrame({"trade_date": ["2020-01-02", "2020-01-03"]}),
            ),
            patch(
                "jfquant_collectors.providers.fetch_adjustment",
                side_effect=AssertionError("factor dependency"),
            ),
        ):
            market(request, lambda event, **value: events.append((event, value)))
        shard = next(v for e, v in events if e == "shard")
        self.assertEqual(shard["adjustment"]["basis"]["kind"], "unavailable")
        self.assertEqual(shard["bars"][0]["volume"], 1200)

    def test_empty_yahoo_response_fails(self):
        ticker = MagicMock()
        ticker.history.return_value = pd.DataFrame()
        request = {
            "provider": "yfinance",
            "adjustment_source": "none",
            "start": "2020-01-02",
            "end": "2020-01-03",
            "instruments": ["SH600000"],
            "benchmarks": [],
            "cache_dir": "/tmp/jfquant-test-cache",
            "universes": [],
        }
        with (
            patch("yfinance.Ticker", return_value=ticker),
            patch("yfinance.set_tz_cache_location"),
            self.assertRaisesRegex(ValueError, "empty provider"),
        ):
            market(request, lambda *a, **kw: None)


if __name__ == "__main__":
    unittest.main()


class ActionContracts(unittest.TestCase):
    def test_sina_per_ten_units_preserve_missing_cash_date(self):
        from jfquant_collectors.providers import corporate_actions

        table = pd.DataFrame(
            [
                {
                    "公告日期": "2020-01-01",
                    "股权登记日": "2020-01-03",
                    "除权除息日": "2020-01-06",
                    "红股上市日": "2020-01-07",
                    "进度": "实施",
                    "送股": 1,
                    "转增": 2,
                    "派息": 2.3,
                }
            ]
        )
        request = {
            "provider": "akshare",
            "start": "2020-01-01",
            "end": "2020-12-31",
            "instruments": ["SH600000"],
        }

        def source(**kwargs):
            return table if kwargs["indicator"] == "分红" else pd.DataFrame()

        events = []
        with (
            patch("akshare.stock_history_dividend_detail", side_effect=source),
            patch("jfquant_collectors.providers.time.sleep"),
        ):
            corporate_actions(request, lambda event, **value: events.append((event, value)))
        record = events[1][1]["records"][0]
        self.assertAlmostEqual(record["cash_per_share"], 0.23)
        self.assertAlmostEqual(record["shares_per_share"], 0.3)
        self.assertIsNone(record["pay_date"])
        self.assertEqual(record["listing_date"], "2020-01-07")
        self.assertEqual(events[1][1]["evidence"]["distribution_unit"], "per_ten_shares")

    def test_unknown_empty_action_history_is_not_declared_ready(self):
        from jfquant_collectors.providers import corporate_actions

        request = {"provider": "akshare", "instruments": ["SH600000"]}
        with (
            patch("akshare.stock_history_dividend_detail", return_value=pd.DataFrame()),
            patch("jfquant_collectors.providers.time.sleep"),
            self.assertRaisesRegex(ValueError, "cannot prove"),
        ):
            corporate_actions(request, lambda *args, **kwargs: None)
