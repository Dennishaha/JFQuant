"""Independent numerical, constraint and time-boundary acceptance."""
import threading
import unittest
import numpy as np
from allocation_fixture import config
from jfquant_research.allocation import solve, StockHistory, independent_checks


class AllocationTests(unittest.TestCase):
    def test_equal_and_exiting_holdings_turnover(self):
        w,r=solve(config(),['a','b'],[],[.2,.3])
        np.testing.assert_allclose(w,[.49,.49],atol=1e-12)
        self.assertAlmostEqual(r['constraints']['turnover'],.48)
        with self.assertRaisesRegex(ValueError,'turnover'):
            solve(config(max_turnover=.1),['a','b'],[],[.2,.3])
        with self.assertRaisesRegex(ValueError,'candidate count'):
            solve(config(max_stock_weight=.1),['a','b'],[],[0.,0.])

    def test_covariance_and_risk_budgets_against_closed_form(self):
        # Independent covariance oracle; shrinkage is part of the intended contract.
        from sklearn.covariance import LedoitWolf
        x=np.random.default_rng(52).normal(size=(100,2))*[.01,.03]
        cov=LedoitWolf().fit(x).covariance_
        expected=(cov[1,1]-cov[0,1])/(cov[0,0]+cov[1,1]-2*cov[0,1])
        w,_=solve(config(method='minimum_variance',risk_window=100),['a','b'],x,[0.,0.])
        self.assertAlmostEqual(w[0]/.98,expected,places=4)
        w,r=solve(config(method='risk_budgeting',risk_window=100),['a','b'],x,[0.,0.])
        np.testing.assert_allclose(r['risk_contributions'],[.5,.5],atol=1e-4)
        with self.assertRaisesRegex(ValueError,'incomplete'):
            solve(config(method='minimum_variance',risk_window=100),['a','b'],x[:-1],[0.,0.])

    def test_negative_expected_returns_preserve_exposure_and_cancellation(self):
        x=np.random.default_rng(5).normal(size=(40,2))*.01
        w,_=solve(config(method='mean_variance',risk_window=40,risk_aversion=10.),['a','b'],x,[0.,0.],[-.01,-.02])
        self.assertAlmostEqual(w.sum(),.98,places=6)
        self.assertGreater(w[0],w[1])
        flag=threading.Event();flag.set()
        with self.assertRaises(InterruptedError):solve(config(),['a'],[],[0.],cancelled=flag)

    def test_mean_variance_matches_constrained_two_asset_oracle(self):
        from sklearn.covariance import LedoitWolf
        x=np.random.default_rng(31).normal(size=(100,2))*[.1,.2]
        cov=LedoitWolf().fit(x).covariance_
        mu=np.array([.001,.002]); lam=10.
        # d/dw1 of mu1*w1 + mu2*(.98-w1) - lam*w'Cov*w = 0.
        oracle=(mu[0]-mu[1]+2*lam*.98*(cov[1,1]-cov[0,1]))/(2*lam*(cov[0,0]+cov[1,1]-2*cov[0,1]))
        w,_=solve(config(method='mean_variance',risk_window=100,risk_aversion=lam),['a','b'],x,[0.,0.],mu)
        np.testing.assert_allclose(w,[oracle,.98-oracle],atol=1e-6)
        from skfolio.exceptions import NonPositiveVarianceError
        with self.assertRaises(NonPositiveVarianceError):
            solve(config(method='minimum_variance',risk_window=100),['a','b'],np.zeros((100,2)),[0.,0.])

    def test_future_data_cannot_change_risk_or_calibration(self):
        calendar=[f'{i:04}' for i in range(40)]
        prices={s:{d:float(10+i*k+.01*i*i*k) for i,d in enumerate(calendar)} for s,k in [('a',.1),('b',.2),('c',.3)]}
        signals={d:dict(a=1.,b=2.,c=3.) for d in calendar}
        c=config(risk_window=5,calibration_window=10,calibration_min_days=3,calibration_min_pairs=3)
        h=StockHistory(calendar,prices,signals,3,c)
        before=h.risk(calendar[20],['a','b']);mu,receipt=h.calibrate(calendar[20],['a','b'],signals[calendar[20]])
        for s in prices:
            for d in calendar[21:]: prices[s][d]*=100
        self.assertEqual(before,h.risk(calendar[20],['a','b']))
        for d in calendar[21:]:signals[d]['a']=1000.
        self.assertEqual((mu,receipt),h.calibrate(calendar[20],['a','b'],signals[calendar[20]]))
        self.assertEqual(receipt['last_return_date'],calendar[20])
        prices['a'].pop(calendar[18])
        with self.assertRaisesRegex(ValueError,'gap'):h.risk(calendar[20],['a','b'])
        with self.assertRaisesRegex(ValueError,'label_gap'):h.calibrate(calendar[20],['a','b'],signals[calendar[20]])

    def test_combined_constraints_apply_to_underlying_stocks(self):
        mapping=np.array([[.98,.98],[0.,0.]])
        with self.assertRaisesRegex(ValueError,'underlying_stock_cap'):
            solve(config(max_stock_weight=.1),['one','two'],[],[0.,0.],mapping=mapping)
        with self.assertRaisesRegex(ValueError,'budget/weights'):
            independent_checks([np.nan],.98,1.,[0.],None)


class CombinedTests(unittest.TestCase):
    def fixture(self):
        from jfquant_research.combined import CombinedAllocator
        calendar=[f'{i:04}' for i in range(40)]
        sources=[]
        for i,symbol in enumerate(['SH600000','SZ000001']):
            rows=[dict(date=d,nav=1.+j*(.01 if i==0 else .002),open_nav=1.+j*(.01 if i==0 else .002),weights={symbol:.98},changed=False) for j,d in enumerate(calendar)]
            targets=[dict(date=calendar[22],effective_date=calendar[23],weights={symbol:.98})]
            sources.append(dict(id=str(i),reference=dict(rows=rows),targets=targets))
        plan=dict(execution=dict(rebalance_days=5,allocation=config()))
        return CombinedAllocator(plan,sources,calendar,threading.Event()),calendar

    def test_no_daily_rebalance_and_capital_drift_is_self_financing(self):
        model,calendar=self.fixture()
        weights,r=model.allocate(calendar[20],{})
        np.testing.assert_allclose(r['weights'],[.5,.5],atol=1e-12)
        self.assertAlmostEqual(sum(weights.values()),.98)
        self.assertEqual(model.allocate(calendar[21],weights),(None,None))
        changed,r=model.allocate(calendar[22],weights)
        self.assertEqual(r['capital_kind'],'drifted')
        self.assertGreater(r['weights'][0],.5)
        self.assertAlmostEqual(sum(r['weights']),1.)
        self.assertAlmostEqual(sum(changed.values()),.98)

    def test_risk_inputs_do_not_read_future_reference_nav(self):
        model,calendar=self.fixture()
        model.plan['execution']['allocation']=config(method='minimum_variance',risk_window=5)
        first=model.history(calendar[20])
        for rows in model.rows:
            for d in calendar[21:]:rows[d]['open_nav']*=1000
        later=model.history(calendar[20])
        np.testing.assert_equal(first[0],later[0])
        self.assertEqual(first[1],later[1])

class NativeDecisionTests(unittest.TestCase):
    def test_cancel_releases_pending_projection_before_replacement(self):
        import inspect
        import akquant as aq
        order=aq.Order('old','SH600000',aq.OrderSide.Buy,aq.OrderType.Limit,100.,price=10.)
        values={key:None for key in inspect.signature(aq.StrategyContext).parameters}
        values.update(cash=1000.,previous_cash=1000.,positions={},available_positions={},position_entry_prices={},current_time=0,active_orders=[order])
        context=aq.StrategyContext(**values)
        self.assertEqual(context.get_projected_position('SH600000'),100.)
        context.cancel_order('old')
        self.assertEqual(context.get_projected_position('SH600000'),0.)
        state=__import__('json').loads(context.jfquant_decision_snapshot())
        self.assertEqual(state['orders'],[])
        self.assertEqual(state['cancel_intents'],['old'])
        self.assertGreater(state['projected_buying_power'],999.)

    def test_pool_exit_is_an_explicit_target_instruction(self):
        import json,tempfile
        from pathlib import Path
        from unittest.mock import patch
        import test_runtime
        request={}
        original=test_runtime.backtest
        def capture(value,*args):
            request.update(value)
            return original(value,*args)
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp)
            with patch.object(test_runtime,'backtest',capture):
                test_runtime.RuntimeTests().run_backtest(root)
            dates=request['signal_dates']
            request['pool_exit_policy']='next_tradable_day'
            request['pool']={'intervals':{'SH600000':[dict(start=dates[0],end=dates[2])], 'SZ000001':[dict(start=dates[0],end=dates[-1])]}}
            original(request,threading.Event(),lambda *args,**kwargs:None)
            report=json.loads((root/'allocation_report.json').read_text())
            exits=[d for d in report['decisions'] if d.get('instruction_kind')=='pool_exit']
            self.assertTrue(exits)
            self.assertEqual(exits[0]['date'],dates[3])
            self.assertEqual(exits[0]['names'],[])
            self.assertGreater(exits[0]['constraints']['turnover'],0.)
            self.assertIn('submission_snapshot',exits[0])
