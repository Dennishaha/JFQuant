"""Independent NumPy arithmetic oracle for the exported Rust AST, not a
certification of ambiguous original formulas. Synthetic fixture only; no Qlib.
Run: python reference_ops.py CATALOG_JSON OUTPUT_JSON
"""
import json
import sys

import numpy as np

DAYS, STOCKS = 320, 6
rng = np.random.default_rng(191)
shape=(DAYS,STOCKS)
close=20+np.arange(STOCKS)[None,:]*.5+np.arange(DAYS)[:,None]*.01+rng.uniform(-3,3,shape)
open_=close+rng.uniform(-1,1,shape)
high=np.maximum(close,open_)+rng.uniform(.1,2,shape)
low=np.minimum(close,open_)-rng.uniform(.1,2,shape)
volume=rng.uniform(1000,15000,shape)
vwap=(open_+close+high+low)/4
bclose=100+np.arange(DAYS)*.02+rng.uniform(-3,3,DAYS)
bopen=bclose+rng.uniform(-1,1,DAYS)
def delay(x,n=1):
    out=np.full(shape,np.nan);out[n:]=x[:-n];return out
previous=delay(close);previous_open=delay(open_)
inputs={'open': open_,'close': close,'high': high,'low': low,'volume': volume,'vwap': vwap,'amount': volume*vwap,
    'ret': close/previous-1,'hd': high-delay(high),'ld': delay(low)-low,
    'tr': np.maximum.reduce([high-low,abs(high-previous),abs(low-previous)]),
    'dtm': np.where(open_<=previous_open,0,np.maximum(high-open_,open_-previous_open)),
    'dbm': np.where(open_>=previous_open,0,np.maximum(open_-low,open_-previous_open)),
    'banchmarkindexclose': np.repeat(bclose[:,None],STOCKS,axis=1),
    'banchmarkindexopen': np.repeat(bopen[:,None],STOCKS,axis=1),
    'mkt': rng.normal(0,.01,shape),'smb': rng.normal(0,.01,shape),'hml': rng.normal(0,.01,shape)}
inputs['dtm'][0]=np.nan;inputs['dbm'][0]=np.nan

def rolling(x,n,fn):
    out=np.full(shape,np.nan)
    for t in range(n-1,DAYS):
        for s in range(STOCKS):
            a=x[t+1-n:t+1,s]
            if np.isfinite(a).all():out[t,s]=fn(a)
    return out

def rank(x):
    out=np.full(shape,np.nan)
    for t in range(DAYS):
        valid=x[t,np.isfinite(x[t])]
        for s in range(STOCKS):
            if np.isfinite(x[t,s]):out[t,s]=np.count_nonzero((valid<=x[t,s])|(abs(valid-x[t,s])<=1e-12*np.maximum(1,np.maximum(abs(valid),abs(x[t,s])))))/len(valid)
    return out

def evaluate(ast):
    name,arg=next(iter(ast.items()))
    if name=='Column': return inputs[arg].copy()
    if name=='Const':return np.full(shape,arg)
    if name=='Sequence':return np.repeat(np.arange(1,DAYS+1)[:,None],STOCKS,axis=1)
    if name=='Unsupported':raise ValueError('quarantined')
    if name in ('Neg','Abs','Log','Sign','Rank','CumulativeSum'):
        x=evaluate(arg)
        if name=='Neg':return -x
        if name=='Abs':return abs(x)
        if name=='Log':return np.log(x)
        if name=='Sign':return np.sign(x)
        if name=='Rank':return rank(x)
        if name=='CumulativeSum':return np.where(np.isfinite(x),np.nancumsum(x,axis=0),np.nan)
    if name=='If':
        c,a,b=map(evaluate,arg);return np.where(np.isfinite(c),np.where(c!=0,a,b),np.nan)
    if name in ('Delay','Delta'):
        x=evaluate(arg[0]);lag=delay(x,arg[1]);return lag if name=='Delay' else x-lag
    if name=='Pow':return evaluate(arg[0])**arg[1]
    if name in ('Add','Sub','Mul','Div','PowExpr','Eq','Gt','Lt','Min','Max','And','Or','Filter'):
        a,b=map(evaluate,arg)
        if name=='Filter':return np.where(np.isfinite(b)&(b!=0),a,np.nan)
        operations={'Add':lambda:a+b,'Sub':lambda:a-b,'Mul':lambda:a*b,'Div':lambda:a/b,'PowExpr':lambda:a**b,
            'Eq':lambda:(a==b).astype(float),'Gt':lambda:(a>b).astype(float),'Lt':lambda:(a<b).astype(float),
            'Min':lambda:np.minimum(a,b),'Max':lambda:np.maximum(a,b),'And':lambda:((a!=0)&(b!=0)).astype(float),'Or':lambda:((a!=0)|(b!=0)).astype(float)}
        return np.where(np.isfinite(a)&np.isfinite(b),operations[name](),np.nan)
    if name=='Ema':
        x=evaluate(arg[0]);alpha=arg[2]/arg[1];out=np.full(shape,np.nan)
        for s in range(STOCKS):
            state=None
            for t in range(DAYS):
                if np.isfinite(x[t,s]):
                    state=x[t,s] if state is None else alpha*x[t,s]+(1-alpha)*state
                    out[t,s]=state
        return out
    if name in ('Corr','Cov','RegBeta','RegResi','SumIf'):
        a,b=map(evaluate,arg[:2]);n=arg[2];out=np.full(shape,np.nan)
        filtered=name=='RegBeta' and 'Filter' in arg[0] and 'Filter' in arg[1]
        for t in range(n-1,DAYS):
            for s in range(STOCKS):
                x=a[t+1-n:t+1,s];y=b[t+1-n:t+1,s];valid=np.isfinite(x)&np.isfinite(y)
                if not filtered and not valid.all():continue
                x=x[valid];y=y[valid]
                if name=='SumIf':out[t,s]=x[y!=0].sum();continue
                if len(x)<2:continue
                if name=='Corr':
                    if len(x)==2 and x[0]!=x[1] and y[0]!=y[1]:out[t,s]=1.0 if np.sign(x[1]-x[0])==np.sign(y[1]-y[0]) else -1.0
                    else:out[t,s]=np.corrcoef(x,y)[0,1]
                else:
                    c=np.cov(x,y,ddof=1)[0,1];v=np.var(y,ddof=1)
                    if name=='Cov':out[t,s]=c
                    elif v>0:
                        beta=c/v
                        out[t,s]=beta if name=='RegBeta' else x[-1]-(x.mean()+beta*(y[-1]-y.mean()))
        return out
    if name=='RegResiMulti':
        y=evaluate(arg[0]);xs=[evaluate(v) for v in arg[1]];n=arg[2];out=np.full(shape,np.nan)
        for t in range(n-1,DAYS):
            for s in range(STOCKS):
                x=np.column_stack([np.ones(n)]+[v[t+1-n:t+1,s] for v in xs]);v=y[t+1-n:t+1,s]
                if not np.isfinite(x).all() or not np.isfinite(v).all():continue
                coef,_,r,_=np.linalg.lstsq(x,v,rcond=None)
                if r==x.shape[1]:out[t,s]=v[-1]-x[-1]@coef
        return out
    x=evaluate(arg[0]);n=arg[1]
    functions={'TsSum':lambda a:a.sum(),'TsMean':lambda a:a.mean(),'TsStd':lambda a:a.std(ddof=1),
        'TsMin':lambda a:a.min(),'TsMax':lambda a:a.max(),'TsRank':lambda a:np.count_nonzero((a<=a[-1])|(abs(a-a[-1])<=1e-12*np.maximum(1,np.maximum(abs(a),abs(a[-1])))))/len(a),
        'DecayLinear':lambda a:np.average(a,weights=np.arange(1,len(a)+1)),
        'Count':lambda a:np.count_nonzero(a),'Product':lambda a:a.prod(),
        'WeightedMean':lambda a:np.average(a,weights=.9**np.arange(len(a)-1,-1,-1)),
        'HighDay':lambda a:np.argmax(a[::-1]),'LowDay':lambda a:np.argmin(a[::-1])}
    return rolling(x,n,functions[name])

def clean(x):return [float(v) if np.isfinite(v) else None for v in x.ravel()]
if __name__=='__main__':
    with open(sys.argv[1]) as catalog_file:
        catalog=json.load(catalog_file)
    expected={}
    np.seterr(all='ignore')
    for spec in catalog['specs']:
        if 'Unsupported' not in spec['expression']:
            result=evaluate(spec['expression']);expected[spec['id']]=clean(result[-3:])
    with open(sys.argv[2],'w') as f:
        json.dump({'formula_version': catalog['specs'][0]['version'],'origin': 'Independent NumPy operator oracle over exported AST; not original-formula certification','days': DAYS,'stocks': STOCKS,'tail_days': 3,'columns': {k:clean(v) for k,v in inputs.items()},'expected': expected},f,separators=(',',':'),allow_nan=False)
    print('goldens',len(expected),'all-null tails',[k for k,v in expected.items() if all(x is None for x in v)])
