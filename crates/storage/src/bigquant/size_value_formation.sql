,
context_terms AS (SELECT * FROM terms QUALIFY row_number() OVER ()>0),
formation_dates AS (
 SELECT CAST(year(date) AS INTEGER) AS formation_year,max(date) AS formation_date
 FROM calendar WHERE month(date)=6 AND year(date) BETWEEN {{start_year}} AND {{end_year}}
 GROUP BY year(date)
),
formation_calendar AS (
 SELECT f.*, (SELECT max(c.date) FROM calendar c WHERE year(c.date)=f.formation_year-1 AND month(c.date)=12) AS december_date
 FROM formation_dates f
),
financial_raw AS (
 SELECT f.formation_year,r.date,r.instrument,r.report_date,r.total_equity_to_parent_shareholders,
 r.other_equity_instruments,r.capital_reserves,r.surplus_reserve,r.undistributed_profit,
 count(*) OVER (PARTITION BY f.formation_year,r.instrument,r.date,r.report_date) AS raw_keys,
 row_number() OVER (PARTITION BY f.formation_year,r.instrument ORDER BY r.date DESC) AS sequence
 FROM cn_stock_financial_balance_general_pit r JOIN formation_calendar f
 ON r.report_date=make_date(f.formation_year-1,12,31) AND r.date<f.formation_date
 WHERE r.date>='{{start_year}}-01-01' AND r.date<='{{end_year}}-06-30'
),
financial_xps AS (
 SELECT f.formation_year,x.date,x.instrument,x.report_date,x.bps_lf,x.capital_reserves_ps_lf,
 x.surplus_reserve_ps_lf,x.undistributed_profit_ps_lf,
 count(*) OVER (PARTITION BY f.formation_year,x.instrument,x.date,x.report_date) AS xps_keys,
 row_number() OVER (PARTITION BY f.formation_year,x.instrument ORDER BY x.date DESC) AS sequence
 FROM cn_stock_financial_xps x JOIN formation_calendar f
 ON x.report_date=make_date(f.formation_year-1,12,31) AND x.date<f.formation_date
 WHERE x.date>='{{start_year}}-01-01' AND x.date<='{{end_year}}-06-30'
)
SELECT f.formation_year,f.formation_date,f.december_date,j.instrument,
 j.security_type,j.basic_rows,j.universe_rows AS june_universe_rows,d.universe_rows AS december_universe_rows,
 j.marked_close AS june_close,j.shares AS june_shares,j.share_rows AS june_share_rows,
 d.marked_close AS december_close,d.shares AS december_shares,d.share_rows AS december_share_rows,
 r.date AS raw_date,r.report_date,r.raw_keys,r.total_equity_to_parent_shareholders AS parent_equity,
 r.other_equity_instruments AS other_equity,r.capital_reserves,r.surplus_reserve,r.undistributed_profit,
 x.date AS xps_date,x.xps_keys,x.bps_lf,x.capital_reserves_ps_lf,x.surplus_reserve_ps_lf,x.undistributed_profit_ps_lf
FROM formation_calendar f JOIN context_terms j ON j.date=f.formation_date
 LEFT JOIN context_terms d ON d.date=f.december_date AND d.instrument=j.instrument
 LEFT JOIN financial_raw r ON r.formation_year=f.formation_year AND r.instrument=j.instrument AND r.sequence=1
 LEFT JOIN financial_xps x ON x.formation_year=f.formation_year AND x.instrument=j.instrument AND x.sequence=1
ORDER BY f.formation_year,j.instrument LIMIT 50001
