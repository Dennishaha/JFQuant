"""AKQuant owns matching, fees and the continuous portfolio; JFQuant supplies signals."""
import datetime as dt
import json
import resource
import time
from pathlib import Path

import akquant as aq
import pandas as pd
import pyarrow.parquet as pq


def backtest(request, cancelled, emit):
    bars = pq.read_table(request["bars"]).to_pandas()
    bars["date"] = pd.to_datetime(bars["date"]).dt.tz_localize("Asia/Shanghai") + pd.Timedelta(hours=15)
    signals = pq.read_table(request["signals"]).to_pandas()
    by_date = {str(date): dict(zip(frame["symbol"], frame["score"])) for date, frame in signals.groupby("date", sort=True)}
    for date in request.get("signal_dates", []):
        by_date.setdefault(str(date), {})
    pool = request.get("pool", {}).get("intervals", {})
    actions = request["actions"]
    initial = request["initial_cash"]
    events = []
    terminal = {}
    trades_by_day = {}

    # Register native actions before engine.run through the public analyzer lifecycle.
    # Calling Engine from inside a strategy callback would reborrow the running engine.
    native_actions = [aq.CorporateAction(event["symbol"], dt.date.fromisoformat(event["date"]), getattr(aq.CorporateActionType, event["kind"]), event["value"]) for event in actions]
    class ActionInput(aq.AnalyzerTemplate):
        name = "jfquant_action_input"
        loaded = False

        def on_start(self, context):
            self.engine = context["engine"]
            for action in native_actions:
                context["engine"].add_corporate_action(action)
            if request.get("rights") is not None:
                if not hasattr(self.engine, "configure_jfquant_rights"):
                    raise ValueError("patched AKQuant rights capability unavailable")
                self.engine.configure_jfquant_rights(json.dumps(request["rights"], allow_nan=False))
            self.loaded = True

    action_input = ActionInput()

    class ResearchStrategy(aq.Strategy):
        def __init__(self):
            super().__init__()
            self.clock = 0

        def on_cross_section(self, trading_date, timestamp):
            if cancelled.is_set():
                raise InterruptedError("backtest cancelled")
            date = str(trading_date)
            members = {s for s, ranges in pool.items() if any(r["start"] <= date <= r["end"] for r in ranges)} if pool else set(request["lot_sizes"])
            if request.get("pool_exit_policy") == "next_tradable_day":
                for symbol, quantity in self.positions.items():
                    if quantity > 0 and symbol not in members:
                        self.order_target_percent(symbol, 0.0)
            if date not in by_date:
                return
            if self.clock % request["rebalance_days"] == 0:
                self.cancel_all_orders()
                # Model scores may all be negative: ranking, not sign, selects long holdings.
                ranking = sorted(((s,v) for s,v in by_date[date].items() if s in members), key=lambda item: (-item[1], item[0]))[:request["top_n"]]
                weights = {symbol: 0.98 / len(ranking) for symbol, _ in ranking} if ranking else {}
                self.rebalance_weights(weights, liquidate_unmentioned=True)
            self.clock += 1
            emit("progress", message=f"AKQuant {date}")

        def on_trade(self, trade):
            date = str(pd.Timestamp(trade.timestamp, unit="ns", tz="UTC").tz_convert("Asia/Shanghai").date())
            totals = trades_by_day.setdefault(date, {"notional": 0.0, "fee": 0.0, "blocked": 0})
            totals["notional"] += float(trade.quantity) * float(trade.price)
            totals["fee"] += float(trade.commission)
            events.append({"date": date, "instrument": trade.symbol, "side": str(trade.side).split(".")[-1].lower(), "status": "filled", "filled_quantity": float(trade.quantity), "price": float(trade.price), "fee": float(trade.commission), "reason": ""})

        def on_reject(self, order):
            date = str(pd.Timestamp(order.updated_at, unit="ns", tz="UTC").tz_convert("Asia/Shanghai").date())
            totals = trades_by_day.setdefault(date, {"notional": 0.0, "fee": 0.0, "blocked": 0})
            totals["blocked"] += 1
            events.append({"date": date, "instrument": order.symbol, "side": str(order.side).split(".")[-1].lower(), "status": "rejected", "filled_quantity": 0.0, "price": None, "fee": 0.0, "reason": str(order.reject_reason)})

        def on_after_trading(self, trading_date, timestamp):
            if cancelled.is_set():
                raise InterruptedError("backtest cancelled")
            # The callback can precede final deferred fills or be dispatched lazily
            # on the next session. It is not an authoritative closing snapshot.

        def on_stop(self):
            terminal.update({"positions":dict(self.positions),"cash":float(self.cash),"equity":float(self.equity)})

    configs = [aq.InstrumentConfig(symbol=symbol, lot_size=int(lot), sellable_after_days=1, tick_size=0.01) for symbol, lot in request["lot_sizes"].items()]
    config = aq.BacktestConfig(strategy_config=aq.StrategyConfig(initial_cash=initial, commission_rate=request["commission_rate"], min_commission=request["minimum_commission"], stamp_tax_rate=request["stamp_tax"], transfer_fee_rate=request["transfer_fee"], exit_on_last_bar=False, enable_fractional_shares=False, volume_limit_pct=0.1), instruments_config=configs, show_progress=False, timezone="Asia/Shanghai", bootstrap_samples=0)
    engine_started = time.perf_counter()
    try:
        result = aq.run_backtest(data=bars, strategy=ResearchStrategy, config=config, t_plus_one=True, fill_policy=aq.NextOpen(), show_progress=False, analyzer_plugins=[action_input])
    except Exception:
        if request.get("rights") is not None and hasattr(action_input, "engine"):
            (Path(request["output"]) / "rights_failed_state.json").write_text(action_input.engine.jfquant_rights_state())
        raise
    if not action_input.loaded:
        raise ValueError("AKQuant corporate-action input hook failed")
    if cancelled.is_set():
        raise InterruptedError("backtest cancelled")
    output = Path(request["output"])
    (output / "backtest_timing.json").write_text(json.dumps({"engine_seconds": time.perf_counter() - engine_started, "peak_rss_bytes": resource.getrusage(resource.RUSAGE_SELF).ru_maxrss}))
    if request.get("rights") is not None:
        (output / "rights_state.json").write_text(action_input.engine.jfquant_rights_state())
    (output / "order_events.jsonl").write_text("".join(json.dumps(event, allow_nan=False) + "\n" for event in events))
    curve = result.equity_curve_daily
    cash = {str(pd.Timestamp(t).date()): float(v) for t, v in result.cash_curve_daily.items()}
    native_days = {}
    for timestamp, equity in curve.items():
        date = str(pd.Timestamp(timestamp).date())
        if date not in cash:
            raise ValueError(f"execution_cash_snapshot_missing: {date}")
        native_days[date] = {"cash":cash[date],"equity":float(equity)}
    (output / "native_account_daily.json").write_text(json.dumps({"schema_version":1,"days":native_days,"terminal":terminal},allow_nan=False))
    previous = initial
    days = []
    benchmark = request["benchmark_returns"]
    for timestamp, equity in curve.items():
        date = str(pd.Timestamp(timestamp).date())
        totals = trades_by_day.get(date, {"notional": 0.0, "fee": 0.0, "blocked": 0})
        days.append({"date": date, "net_return": float(equity) / previous - 1, "benchmark_return": benchmark.get(date, 0.0), "turnover": totals["notional"] / previous, "cost": totals["fee"] / previous, "execution_fee_cny": totals["fee"], "blocked_orders": totals["blocked"]})
        previous = float(equity)
    if not days:
        raise ValueError("AKQuant produced no daily equity observations")
    return {"days": days, "engine": "akquant", "version": aq.__version__, "event_policy": "ex_date"}
