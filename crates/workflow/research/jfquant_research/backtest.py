"""AKQuant owns matching, fees and the continuous portfolio; JFQuant supplies signals."""
import datetime as dt
import json
import resource
import time
from pathlib import Path

from .allocation import StockHistory, solve, digest, validate_config, TOL

import akquant as aq
import pandas as pd
import pyarrow.parquet as pq


def _backtest(request, cancelled, emit):
    allocation = request["allocation"]
    validate_config(allocation)
    output = Path(request["output"])
    allocation_input = json.loads(Path(request["allocation_input"]).read_text())
    required = {"schema_version", "config", "stock_exposure", "calendar", "prices", "missing_prices", "admission", "prediction_provenance", "reference_range"}
    if not required.issubset(allocation_input) or type(allocation_input["schema_version"]) is not int or allocation_input["schema_version"] != 2:
        raise ValueError("unsupported_allocation_input_format")
    if allocation_input["config"] != allocation or not isinstance(allocation_input["admission"], dict) or not isinstance(allocation_input["missing_prices"], dict):
        raise ValueError("allocation_input_contract_mismatch")
    if allocation["method"] == "mean_variance" and not isinstance(allocation_input["prediction_provenance"], dict):
        raise ValueError("allocation_prediction_provenance_required")
    stock_exposure = allocation_input["stock_exposure"]
    decisions, targets = [], []
    journal_path = output / "allocation-decisions.jsonl"
    journal_path.write_text("")
    journal_count = 0
    def save_allocation(status, error=None):
        nonlocal journal_count
        with journal_path.open("a") as journal:
            for decision,target in zip(decisions[journal_count:],targets[journal_count:]):
                journal.write(json.dumps(dict(decision=decision,target=target),allow_nan=False)+"\n")
                journal_count += 1
        artifact = {"schema_version":2,"status":status,"error":error,"config":allocation,"input_sha256":digest(allocation_input),"decision_count":len(decisions),"journal":"allocation-decisions.jsonl","decisions":decisions if status != "running" else [],"targets":targets if status != "running" else []}
        path = output / "allocation_report.json"
        partial = path.with_suffix(".partial")
        partial.write_text(json.dumps(artifact, allow_nan=False))
        partial.replace(path)
    save_allocation("running")
    bars = pq.read_table(request["bars"]).to_pandas()
    bars["date"] = pd.to_datetime(bars["date"]).dt.tz_localize("Asia/Shanghai") + pd.Timedelta(hours=15)
    reference_bars = bars.copy()
    reference_bars["date"] = reference_bars["date"].dt.date.astype(str)
    (output / "reference-input.json").write_text(json.dumps({"schema_version":1,"bars":reference_bars.to_dict("records"),"actions":request["reference_actions"],"rights":request.get("reference_rights")}, allow_nan=False))
    bars = bars[bars["date"].dt.date.astype(str) >= request["formal_start"]].copy()
    signals = pq.read_table(request["signals"]).to_pandas()
    by_date = {str(date): dict(zip(frame["symbol"], frame["score"])) for date, frame in signals.groupby("date", sort=True)}
    for date in request.get("signal_dates", []):
        by_date.setdefault(str(date), {})
    history = StockHistory(allocation_input["calendar"], allocation_input["prices"], by_date, request["rebalance_days"], allocation, allocation_input["prediction_provenance"], allocation_input["missing_prices"], allocation_input["admission"])
    calendar = allocation_input["calendar"]
    combined = None
    if request.get("combined_input"):
        from .combined import CombinedAllocator
        payload = json.loads(Path(request["combined_input"]).read_text())
        combined = CombinedAllocator(payload["plan"],payload["sources"],calendar,cancelled,stock_exposure)
    issues = [] if combined is not None else history.prepare(calendar, allocation_input["reference_range"]["start"], request["top_n"], cancelled, request["formal_start"])
    admission_report = dict(schema_version=1,status="blocked" if issues else "ready",issues=issues,candidate_dates={d:r for d,(_,r) in history.candidate_cache.items()})
    (output / "allocation-admission.json").write_text(json.dumps(admission_report,allow_nan=False))
    if issues:
        save_allocation("failed", "allocation_data_admission_failed")
        raise ValueError(f"allocation_data_admission_failed: {issues[0]['date']}: {issues[0]['error']}; see allocation-admission.json")
    next_date = {a:b for a,b in zip(calendar,calendar[1:])}
    warmup_targets, warmup_decisions = [], []
    pool = request.get("pool", {}).get("intervals", {})
    last_warmup_target = {}
    if combined is None:
        warmup_clock = 0
        reference_start = allocation_input["reference_range"]["start"]
        for date in calendar:
            if date < reference_start or date >= request["formal_start"] or date not in by_date:
                continue
            if warmup_clock % request["rebalance_days"] == 0:
                scores, eligibility = history.candidates(date,by_date[date])
                ranking = sorted(scores, key=lambda s:(-scores[s],s))[:request["top_n"]]
                warmup_config = dict(allocation)
                warmup_config["max_turnover"] = None  # Reference targets have no actual-account turnover contract.
                risk=[];expected=None;bounds=None;calibration=None;receipt=None
                if ranking:
                    if allocation["method"] != "equal":risk,bounds = history.risk(date,ranking)
                    if allocation["method"] == "mean_variance":expected,calibration = history.calibrate(date,ranking,scores)
                    values,receipt = solve(warmup_config,ranking,risk,[0.]*len(ranking),expected,cancelled=cancelled,stock_exposure=stock_exposure)
                    weights=dict(zip(ranking,values.tolist()))
                else:weights={}
                warmup_targets.append(dict(date=date,effective_date=next_date.get(date),weights=weights))
                warmup_decisions.append(dict(date=date,effective_date=next_date.get(date),solver=receipt,risk_history=bounds,calibration=calibration,eligibility=eligibility,kind="reference_warmup"))
                last_warmup_target = weights
            elif request.get("pool_exit_policy") == "next_tradable_day" and pool:
                kept = {s:w for s,w in last_warmup_target.items() if any(r["start"] <= date <= r["end"] for r in pool.get(s,[]))}
                if kept != last_warmup_target:
                    warmup_targets.append(dict(date=date,effective_date=next_date.get(date),weights=kept))
                    warmup_decisions.append(dict(date=date,instruction_kind="pool_exit",kind="reference_warmup"))
                    last_warmup_target = kept
            warmup_clock+=1
    (output / "reference-targets.json").write_text(json.dumps({"schema_version":1,"targets":warmup_targets,"decisions":warmup_decisions},allow_nan=False))
    for date in request.get("signal_dates", []):
        by_date.setdefault(str(date), {})
    pool = request.get("pool", {}).get("intervals", {})
    transitions = (request.get("rights") or {}).get("corporate", {}).get("events", [])
    retirement = {e["instrument"]: (e["date"], e["terms"]["kind"] == "liquidation") for e in transitions}
    actions = request["actions"]
    initial = request["initial_cash"]
    events = []
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
            self.last_target = {}

        def publish_target(self, date, weights, receipt, snapshot):
            if any(weight < 0 for weight in weights.values()):
                raise ValueError("allocation_execution_negative_weight")
            receipt.update(date=date,effective_date=next_date.get(date),actual_snapshot=snapshot)
            if next_date.get(date) is not None:
                self.cancel_all_orders()
                submission = json.loads(self.ctx.jfquant_decision_snapshot())
                for field in ("timestamp", "cash", "equity", "positions", "available_positions", "locked_value"):
                    if submission[field] != snapshot[field]:
                        raise ValueError(f"allocation_account_changed_during_decision: {field}")
                if self.get_open_orders() or submission["cash"] < 0 or any(q < 0 for q in submission["available_positions"].values()):
                    raise ValueError("allocation_submission_account_invalid")
                receipt["submission_snapshot"] = submission
            decisions.append(receipt)
            targets.append({"date":date,"effective_date":next_date.get(date),"weights":weights})
            self.last_target = weights
            save_allocation("running")
            if next_date.get(date) is not None:
                self.rebalance_weights(weights, liquidate_unmentioned=True)

        def on_cross_section(self, trading_date, timestamp):
            if cancelled.is_set():
                raise InterruptedError("backtest cancelled")
            date = str(trading_date)
            members = {s for s, ranges in pool.items() if any(r["start"] <= date <= r["end"] for r in ranges)} if pool else set(request["lot_sizes"])
            members = {s for s in members if s not in retirement or date < retirement[s][0] or (date == retirement[s][0] and retirement[s][1])}
            exits_pool = set(self.last_target).union(s for s,q in self.positions.items() if q > 0) - members
            scheduled = date in by_date and self.clock % request["rebalance_days"] == 0
            if combined is None and exits_pool and request.get("pool_exit_policy") == "next_tradable_day" and not scheduled:
                snapshot = json.loads(self.ctx.jfquant_decision_snapshot())
                weights = {s:w for s,w in self.last_target.items() if s in members}
                current = {s:q*snapshot["marks"][s]/snapshot["equity"] for s,q in snapshot["positions"].items()}
                turnover = sum(abs(weights.get(s,0.)-current.get(s,0.)) for s in set(weights)|set(current))
                if allocation["max_turnover"] is not None and turnover > allocation["max_turnover"] + TOL:
                    raise ValueError("allocation_infeasible: pool exit turnover")
                receipt = dict(schema_version=1,method=allocation["method"],names=list(weights),weights=list(weights.values()),status="analytic",instruction_kind="pool_exit",constraints=dict(turnover=turnover),exited_symbols=sorted(exits_pool))
                self.publish_target(date,weights,receipt,snapshot)
                if date in by_date:self.clock+=1
                return
            if date not in by_date:
                return
            if combined is not None or self.clock % request["rebalance_days"] == 0:
                # Model scores may all be negative: ranking, not sign, selects long holdings.
                scores, eligibility = history.candidates(date,by_date[date]) if combined is None else ({},None)
                ranking = sorted(((s,v) for s,v in scores.items() if s in members), key=lambda item: (-item[1], item[0]))[:request["top_n"]]
                snapshot = json.loads(self.ctx.jfquant_decision_snapshot())
                if snapshot["equity"] <= 0 or any(q < 0 for q in snapshot["positions"].values()):
                    raise ValueError("allocation_account_snapshot_invalid")
                current = {}
                for symbol, quantity in snapshot["positions"].items():
                    mark = snapshot["marks"].get(symbol)
                    if quantity and (mark is None or mark <= 0):
                        raise ValueError(f"allocation_actual_valuation_unknown: {symbol}")
                    current[symbol] = quantity * mark / snapshot["equity"]
                if combined is not None:
                    weights, receipt = combined.allocate(date,current)
                    if weights is not None:
                        self.publish_target(date,weights,receipt,snapshot)
                    self.clock += 1
                    return
                names = [symbol for symbol, _ in ranking]
                exits = sum(v for symbol,v in current.items() if symbol not in names)
                config = dict(allocation)
                if config["max_turnover"] is not None:
                    if exits > config["max_turnover"] + TOL:
                        raise ValueError("allocation_infeasible: exiting holdings exceed turnover cap")
                    config["max_turnover"] -= exits
                bounds, calibration = None, None
                expected = None
                if names:
                    risk = []
                    if config["method"] != "equal":
                        risk, bounds = history.risk(date, names)
                    if config["method"] == "mean_variance":
                        expected, calibration = history.calibrate(date, names, scores)
                    values, receipt = solve(config, names, risk, [current.get(s,0.) for s in names], expected, cancelled=cancelled, stock_exposure=stock_exposure)
                    weights = dict(zip(names, values.tolist()))
                else:
                    weights = {}
                    if config["max_turnover"] is not None and exits > allocation["max_turnover"] + TOL:
                        raise ValueError("allocation_infeasible: liquidation turnover")
                    receipt = {"schema_version":1,"method":config["method"],"names":[],"weights":[],"status":"analytic","constraints":{"turnover":0.}}
                receipt.update(exiting_turnover=exits, risk_history=bounds, calibration=calibration, eligibility=eligibility)
                receipt["constraints"]["turnover"] += exits
                self.publish_target(date,weights,receipt,snapshot)
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


    configs = [aq.InstrumentConfig(symbol=symbol, lot_size=int(lot), sellable_after_days=1, tick_size=0.01) for symbol, lot in request["lot_sizes"].items()]
    config = aq.BacktestConfig(strategy_config=aq.StrategyConfig(initial_cash=initial, commission_rate=request["commission_rate"], min_commission=request["minimum_commission"], stamp_tax_rate=request["stamp_tax"], transfer_fee_rate=request["transfer_fee"], exit_on_last_bar=False, enable_fractional_shares=False, volume_limit_pct=0.1), instruments_config=configs, show_progress=False, timezone="Asia/Shanghai", bootstrap_samples=0)
    engine_started = time.perf_counter()
    try:
        result = aq.run_backtest(data=bars, strategy=ResearchStrategy, config=config, t_plus_one=True, fill_policy=aq.NextOpen(), show_progress=False, analyzer_plugins=[action_input])
    except Exception as error:
        save_allocation("failed", str(error))
        if request.get("rights") is not None and hasattr(action_input, "engine"):
            (Path(request["output"]) / "rights_failed_state.json").write_text(action_input.engine.jfquant_rights_state())
        raise
    if not action_input.loaded:
        raise ValueError("AKQuant corporate-action input hook failed")
    if cancelled.is_set():
        raise InterruptedError("backtest cancelled")
    terminal = json.loads(action_input.engine.jfquant_account_snapshot())
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
    save_allocation("completed")
    return {"days": days, "engine": "akquant", "version": aq.__version__, "event_policy": "ex_date"}


def backtest(request, cancelled, emit):
    """Keep failure evidence even when admission or reference warmup fails before AKQuant."""
    try:
        return _backtest(request, cancelled, emit)
    except Exception as error:
        output = Path(request["output"])
        path = output / "allocation_report.json"
        try:
            artifact = json.loads(path.read_text()) if path.exists() else {
                "schema_version": 2, "config": request["allocation"], "decisions": [], "targets": []}
            artifact.update(status="failed", error=str(error))
            partial = path.with_suffix(".partial")
            partial.write_text(json.dumps(artifact, allow_nan=False))
            partial.replace(path)
        except Exception:
            # The supervised task also records the original error if the filesystem failed.
            pass
        raise
