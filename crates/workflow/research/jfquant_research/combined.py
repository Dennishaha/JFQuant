"""Frozen strategy targets -> capital allocation -> one net stock instruction."""
import numpy as np
from .allocation import solve, independent_checks, TOL


class CombinedAllocator:
    def __init__(self, plan, sources, calendar, cancelled):
        self.plan, self.sources, self.calendar, self.cancelled = plan, sources, calendar, cancelled
        self.indices = {d:i for i,d in enumerate(calendar)}
        self.names = [s['id'] for s in sources]
        self.rows = [{r['date']:r for r in s['reference']['rows']} for s in sources]
        self.targets = [{r['date']:r for r in s['targets']} for s in sources]
        self.capital = None
        self.last_nav = None
        self.clock = 0

    def history(self, date):
        h, c = self.plan['execution']['rebalance_days'], self.plan['execution']['allocation']
        end = self.indices[date]-1-h
        start = end-c['risk_window']+1
        if start < 0:
            raise ValueError('combined_risk_warmup_insufficient')
        result=[]
        for i in range(start,end+1):
            a,b=self.calendar[i+1],self.calendar[i+1+h]
            row=[]
            for rows in self.rows:
                if a not in rows or b not in rows:
                    raise ValueError(f'combined_reference_history_gap:{a}:{b}')
                row.append(rows[b]['open_nav']/rows[a]['open_nav']-1.)
            result.append(row)
        return np.asarray(result),dict(first_signal_date=self.calendar[start],last_signal_date=self.calendar[end],last_return_date=self.calendar[end+1+h],horizon=h)

    def allocate(self, date, current):
        c=self.plan['execution']['allocation']
        try:
            row=[rows[date] for rows in self.rows]
        except KeyError as error:
            raise ValueError(f'combined_reference_date_missing:{date}') from error
        nav=np.array([r['nav'] for r in row])
        if not np.isfinite(nav).all() or (nav<=0).any():
            raise ValueError('combined_reference_nav_invalid')
        if self.capital is not None:
            self.capital=self.capital*nav/self.last_nav
            self.capital=self.capital/self.capital.sum()  # Explicit self-financing capital drift.
        self.last_nav=nav
        scheduled=self.clock % self.plan['execution']['rebalance_days']==0
        recipes=[targets[date]['weights'] if date in targets else r['weights'] for targets,r in zip(self.targets,row)]
        changed=scheduled or any(date in t or r['changed'] for t,r in zip(self.targets,row))
        self.clock+=1
        if not changed:
            return None,None
        symbols=sorted(set(current).union(*(r.keys() for r in recipes)))
        mapping=np.array([[r.get(s,0.) for r in recipes] for s in symbols])
        previous=np.array([current.get(s,0.) for s in symbols])
        bounds=None
        if scheduled:
            returns=[];expected=None
            if c['method']!='equal':
                returns,bounds=self.history(date)
                if c['method']=='mean_variance':expected=np.mean(returns,axis=0)
            self.capital,receipt=solve(c,self.names,returns,previous,expected,mapping,self.cancelled)
            capital_kind='optimized'
        else:
            receipt=dict(schema_version=1,method=c['method'],names=self.names,weights=self.capital.tolist(),status='capital_drift',constraints=independent_checks(self.capital,1.,1.,previous,c['max_turnover'],mapping,c['max_stock_weight']))
            capital_kind='drifted'
        weights=mapping@self.capital
        receipt.update(capital_kind=capital_kind,risk_history=bounds,underlying_symbols=symbols,mapping=mapping.tolist(),underlying_weights=weights.tolist(),reference_nav=nav.tolist())
        return dict(zip(symbols,weights.tolist())),receipt
