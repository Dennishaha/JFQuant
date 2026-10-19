WITH
calendar AS (
  SELECT date, row_number() OVER (ORDER BY date) AS market_day
  FROM (SELECT DISTINCT date FROM cn_stock_instruments
        WHERE date >= '2005-01-04' AND date < DATE '{{end}}' + INTERVAL 1 DAY)
),
u AS (
  SELECT date,instrument,count(*) AS universe_rows FROM cn_stock_instruments
  WHERE date >= '2005-01-04' AND date < DATE '{{end}}' + INTERVAL 1 DAY
  GROUP BY date,instrument
),
p AS (
  SELECT date,instrument,count(*) AS price_rows,min(close) AS close,min(adjust_factor) AS factor,
         min(pre_close) AS pre_close,min(change_ratio) AS change_ratio
  FROM cn_stock_real_bar1d
  WHERE date >= '2005-01-04' AND date < DATE '{{end}}' + INTERVAL 1 DAY
  GROUP BY date,instrument
),
s AS (
  SELECT date,instrument,count(*) AS share_rows,min(total_shares) AS shares
  FROM cn_stock_shares
  WHERE date >= '2005-01-04' AND date < DATE '{{end}}' + INTERVAL 1 DAY
  GROUP BY date,instrument
),
t AS (
  SELECT date,instrument,count(*) AS status_rows,min(suspended) AS suspended,min(exdr) AS exdr
  FROM cn_stock_status
  WHERE date >= '2005-01-04' AND date < DATE '{{end}}' + INTERVAL 1 DAY
  GROUP BY date,instrument
),
b AS (
  SELECT instrument,count(*) AS basic_rows,min(security_type) AS security_type,min(list_date) AS listed_on
  FROM cn_stock_basic_info GROUP BY instrument
),
panel AS (
  SELECT u.date,u.instrument,c.market_day,u.universe_rows,p.price_rows,p.close,p.factor,p.pre_close,p.change_ratio,
         s.share_rows,s.shares,t.status_rows,t.suspended,t.exdr,b.basic_rows,b.security_type,b.listed_on,
         lag(c.market_day) OVER w AS previous_day,
         CASE WHEN u.universe_rows=1 AND p.price_rows=1 AND p.close>0 AND p.factor>0
              THEN p.close END AS actual_close,
         CASE WHEN c.market_day=1 AND u.universe_rows=1 AND p.price_rows=1 AND t.status_rows=1
              AND t.suspended=1 AND t.exdr=0 AND p.change_ratio=0 AND p.close IS NULL
              AND p.pre_close>0 AND p.factor>0 AND b.listed_on<'2005-01-04' THEN p.pre_close END AS starting_close,
         CASE WHEN c.market_day>1 AND lag(c.market_day) OVER w=c.market_day-1
              AND lag(p.factor) OVER w=p.factor AND p.factor>0 AND p.pre_close>0
              AND u.universe_rows=1 AND p.price_rows=1 AND t.status_rows=1
              AND t.suspended=1 AND t.exdr=0 AND p.change_ratio=0 AND p.close IS NULL
              THEN p.pre_close END AS reported_close
  FROM u JOIN calendar c USING(date)
  LEFT JOIN p USING(date,instrument) LEFT JOIN s USING(date,instrument)
  LEFT JOIN t USING(date,instrument) LEFT JOIN b USING(instrument)
  WINDOW w AS (PARTITION BY u.instrument ORDER BY u.date)
),
segmented AS (
  SELECT *,sum(CASE WHEN previous_day=market_day-1 THEN 0 ELSE 1 END)
           OVER (PARTITION BY instrument ORDER BY date ROWS UNBOUNDED PRECEDING) AS segment
  FROM panel
),
observed AS (
  SELECT *,
    last_value(coalesce(actual_close,starting_close,reported_close) IGNORE NULLS) OVER w AS observed_close,
    last_value(CASE WHEN coalesce(actual_close,starting_close,reported_close) IS NOT NULL THEN factor END IGNORE NULLS) OVER w AS observed_factor,
    last_value(CASE WHEN coalesce(actual_close,starting_close,reported_close) IS NOT NULL THEN market_day END IGNORE NULLS) OVER w AS observed_day
  FROM segmented
  WINDOW w AS (PARTITION BY instrument,segment ORDER BY date ROWS UNBOUNDED PRECEDING)
),
blocked AS (
  SELECT *,max(CASE WHEN actual_close IS NULL AND
     (universe_rows<>1 OR coalesce(price_rows,0)<>1 OR coalesce(status_rows,0)<>1
      OR coalesce(suspended,0)<>1 OR factor IS NULL OR factor<=0
      OR observed_factor IS NULL OR factor<>observed_factor) THEN market_day END)
      OVER (PARTITION BY instrument,segment ORDER BY date ROWS UNBOUNDED PRECEDING) AS blocked_day
  FROM observed
),
marked AS (
  SELECT *,CASE WHEN actual_close IS NOT NULL THEN actual_close
         WHEN starting_close IS NOT NULL THEN starting_close
         WHEN reported_close IS NOT NULL THEN reported_close
         WHEN observed_close IS NOT NULL AND suspended=1 AND status_rows=1 AND price_rows=1
              AND factor=observed_factor AND universe_rows=1
              AND (blocked_day IS NULL OR observed_day>blocked_day)
         THEN observed_close END AS marked_close
  FROM blocked
),
lagged AS (
  SELECT *,lag(date) OVER w AS prior_date,lag(actual_close) OVER w AS prior_close,lag(marked_close) OVER w AS prior_mark,
    lag(factor) OVER w AS prior_factor,lag(shares) OVER w AS prior_shares,
    lag(share_rows) OVER w AS prior_share_rows,
    lag(suspended) OVER w AS prior_suspended,lag(status_rows) OVER w AS prior_status_rows,
    lag(price_rows) OVER w AS prior_price_rows,
    lag(universe_rows) OVER w AS prior_universe_rows,
    lag(basic_rows) OVER w AS prior_basic_rows
  FROM marked WINDOW w AS (PARTITION BY instrument,segment ORDER BY date)
),
recovered AS (
  SELECT *,coalesce(prior_mark,CASE WHEN prior_close IS NULL AND prior_suspended=1 AND prior_status_rows=1
       AND prior_price_rows=1 AND listed_on<prior_date AND factor=prior_factor AND exdr=0 AND pre_close>0 THEN pre_close END) AS previous_mark
  FROM lagged
),
terms AS (
  SELECT *,
    CASE WHEN previous_day=market_day-1 AND security_type LIKE '%A股' AND basic_rows=1
          AND prior_basic_rows=1 AND prior_universe_rows=1 AND universe_rows=1
          AND prior_share_rows=1 AND prior_shares>0 AND factor>0 AND prior_factor>0
          AND actual_close>0 AND prior_close>0
      THEN prior_close*prior_shares END AS strict_cap,
    CASE WHEN previous_day=market_day-1 AND security_type LIKE '%A股' AND basic_rows=1
          AND prior_basic_rows=1 AND prior_universe_rows=1 AND universe_rows=1
          AND prior_share_rows=1 AND prior_shares>0 AND factor>0 AND prior_factor>0
          AND marked_close>0 AND previous_mark>0
      THEN previous_mark*prior_shares END AS marked_cap,
    actual_close*factor/(prior_close*prior_factor)-1 AS strict_return,
    marked_close*factor/(previous_mark*prior_factor)-1 AS marked_return
  FROM recovered
)
SELECT date,
  count(*) AS universe_members,
  sum(CASE WHEN security_type LIKE '%A股' THEN 1 ELSE 0 END) AS a_share_members,
  sum(CASE WHEN basic_rows IS NULL OR basic_rows<>1 THEN 1 ELSE 0 END) AS unclassified_members,
  sum(CASE WHEN security_type LIKE '%A股' AND previous_day=market_day-1 THEN 1 ELSE 0 END) AS eligible_members,
  sum(CASE WHEN security_type LIKE '%A股' AND previous_day=market_day-1 AND strict_cap IS NULL THEN 1 ELSE 0 END) AS strict_missing,
  sum(CASE WHEN security_type LIKE '%A股' AND previous_day=market_day-1 AND marked_cap IS NULL THEN 1 ELSE 0 END) AS marked_missing,
  sum(strict_cap) AS strict_cap_sum,
  sum(strict_cap*strict_return) AS strict_return_sum,
  sum(marked_cap) AS marked_cap_sum,
  sum(marked_cap*marked_return) AS marked_return_sum,
  sum(CASE WHEN security_type LIKE '%A股' AND previous_day=market_day-1 AND actual_close IS NULL AND marked_close IS NOT NULL THEN 1 ELSE 0 END) AS suspended_marks,
  sum(CASE WHEN security_type LIKE '%A股' AND previous_day=market_day-1 AND prior_close IS NULL AND prior_mark IS NOT NULL THEN 1 ELSE 0 END) AS prior_suspended_marks,
  sum(CASE WHEN security_type LIKE '%A股' AND previous_day=market_day-1 AND (prior_share_rows IS NULL OR prior_share_rows<>1 OR prior_shares IS NULL OR prior_shares<=0) THEN 1 ELSE 0 END) AS missing_shares,
  sum(CASE WHEN security_type LIKE '%A股' AND (universe_rows<>1 OR coalesce(price_rows,0)>1 OR coalesce(share_rows,0)>1 OR coalesce(status_rows,0)>1) THEN 1 ELSE 0 END) AS duplicate_keys,
  sum(CASE WHEN security_type LIKE '%A股' AND starting_close IS NOT NULL THEN 1 ELSE 0 END) AS initial_reference_marks,
  sum(CASE WHEN security_type LIKE '%A股' AND reported_close IS NOT NULL THEN 1 ELSE 0 END) AS reported_reference_marks,
  sum(CASE WHEN security_type LIKE '%A股' AND previous_day=market_day-1 AND prior_mark IS NULL AND previous_mark IS NOT NULL THEN 1 ELSE 0 END) AS prior_reference_restores
FROM terms
WHERE strftime(date,'%Y-%m-%d') >= '{{start}}'
GROUP BY date ORDER BY date LIMIT 10001
