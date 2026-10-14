//! Explicit cash payment dates, including mixed cash/bonus distributions.
use super::*;
fn parse(html: &str, start: Date, end: Date) -> io::Result<Vec<(Date, Date, f64, Date)>> {
    let section = html
        .split("id=\"cp_pBottom\"")
        .nth(1)
        .and_then(|s| s.split("</table>").next())
        .ok_or_else(|| invalid("AAStocks dividend table missing"))?;
    if !section.contains("派送日") || !section.contains("股权登记日") {
        return Err(invalid("AAStocks dividend schema changed"));
    }
    let mut out = Vec::new();
    for row in section.split("<tr>").skip(1) {
        let c = cells(row);
        if c.len() != 7 {
            return Err(invalid("AAStocks row schema changed"));
        }
        if !c[2].starts_with('¥') {
            continue;
        }
        let cash = c[2]
            .trim_start_matches('¥')
            .replace(',', "")
            .parse::<f64>()
            .map_err(invalid)?
            / 10.0;
        if !cash.is_finite() || cash <= 0.0 {
            continue;
        }
        let d = |i: usize| c[i].replace('/', "-").parse::<Date>().ok();
        if let (Some(record), Some(ex), Some(pay)) = (d(4), d(5), d(6)) {
            // Old provider placeholder dates must not invalidate unrelated years.
            // Requested events still require an explicit valid matching payment.
            if ex < start || ex > end {
                continue;
            }
            if record >= ex || pay < ex {
                return Err(invalid("AAStocks payment chronology invalid"));
            }
            out.push((record, ex, cash, pay));
        }
    }
    Ok(out)
}
/// Supplement explicitly named cash-row payment dates from AAStocks. Matches
/// record date, ex date and per-share cash; never uses bonus-row arrival dates.
/// Existing provider evidence is validated and retained in the new directory.
pub fn fetch_pay_dates(
    source: &ActionBundle,
    source_root: &Path,
    output: &Path,
    start: Date,
    end: Date,
) -> io::Result<ActionBundle> {
    supplement_pay_dates(source, source_root, output, start, end, false)
}
/// Retry parsing JFQuant's hash-verified saved payment pages without a new download.
pub fn repair_pay_dates(
    source: &ActionBundle,
    source_root: &Path,
    output: &Path,
    start: Date,
    end: Date,
) -> io::Result<ActionBundle> {
    supplement_pay_dates(source, source_root, output, start, end, true)
}
fn supplement_pay_dates(
    source: &ActionBundle,
    source_root: &Path,
    output: &Path,
    start: Date,
    end: Date,
    saved_only: bool,
) -> io::Result<ActionBundle> {
    source.validate_identity()?;
    if start > end || source.schema_version != 3 {
        return Err(invalid("invalid payment range/bundle"));
    }
    let symbols: Vec<_> = source
        .actions
        .iter()
        .filter(|a| {
            a.ex_date >= start && a.ex_date <= end && a.cash_per_share > 0.0 && a.pay_date.is_none()
        })
        .map(|a| a.instrument.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    checkpoints::prepare(
        source,
        source_root,
        output,
        if saved_only {
            "repair-payments"
        } else {
            "payments"
        },
        serde_json::json!([start, end]),
    )?;
    let index = AtomicUsize::new(0);
    let results = Mutex::new(Vec::new());
    let task_context = analytics_core::task::TaskContext::current();
    std::thread::scope(|scope| {
        for _ in 0..4.min(symbols.len()) {
            scope.spawn(|| {
                let _scope = task_context.install();
                loop {
                    if task_context.check().is_err() {
                        break;
                    }
                    let i = index.fetch_add(1, Ordering::Relaxed);
                    let Some(id) = symbols.get(i) else { break };
                    let selected = source.source_symbol(id).map(str::to_owned);
                    let code = selected.as_deref().unwrap_or("");
                    let url = format!(
                        "https://quotes.aastocks.com/sc/cnhk/analysis/dividend.aspx?shsymbol={}",
                        code.get(2..).unwrap_or("")
                    );
                    let result = (|| {
                        selected.as_ref().map_err(invalid)?;
                        if saved_only {
                            let bytes = fs::read(output.join(format!("raw/{id}-aastocks.html")))?;
                            let html = String::from_utf8(bytes).map_err(invalid)?;
                            if !html.contains(&format!(
                                "({}.{})",
                                code.get(2..).unwrap_or(""),
                                code.get(..2).unwrap_or("")
                            )) {
                                return Err(invalid("AAStocks security identity mismatch"));
                            }
                            return parse(&html, start, end);
                        }
                        let bytes = checkpoints::page(
                            &url,
                            Command::new("/usr/bin/curl").args([
                                "--fail",
                                "--silent",
                                "--show-error",
                                "--location",
                                "--retry",
                                "2",
                                "--max-time",
                                "45",
                                "--connect-timeout",
                                "15",
                                &url,
                            ]),
                            &output.join(format!("raw/{id}-aastocks.html")),
                        )?;
                        let html = String::from_utf8(bytes).map_err(invalid)?;
                        if !html.contains(&format!(
                            "({}.{})",
                            code.get(2..).unwrap_or(""),
                            code.get(..2).unwrap_or("")
                        )) {
                            return Err(invalid("AAStocks security identity mismatch"));
                        }
                        parse(&html, start, end)
                    })();
                    if i.is_multiple_of(50) {
                        let _ = task_context.report(format!("现金到账 {i}/{}", symbols.len()));
                    }
                    results
                        .lock()
                        .expect("worker result mutex")
                        .push((id.clone(), url, result));
                }
            });
        }
    });
    task_context_check()?;
    let mut bundle = source.clone();
    if !saved_only {
        bundle.downloaded_at = chrono::Utc::now().to_rfc3339();
    }
    let mut failures = BTreeMap::new();
    let mut added = 0;
    for (id, url, result) in results.into_inner().map_err(|_| invalid("worker panic"))? {
        match result {
            Err(e) => {
                failures.insert(id, e.to_string());
            }
            Ok(rows) => {
                for a in bundle.actions.iter_mut().filter(|a| {
                    a.instrument == id
                        && a.ex_date >= start
                        && a.ex_date <= end
                        && a.cash_per_share > 0.0
                        && a.pay_date.is_none()
                }) {
                    let matches: Vec<_> = rows
                        .iter()
                        .filter(|(record, ex, cash, _)| {
                            *record == a.record_date
                                && *ex == a.ex_date
                                && (*cash - a.cash_per_share).abs() <= 0.000051
                        })
                        .collect();
                    let dates: BTreeSet<_> = matches.iter().map(|(_, _, _, pay)| *pay).collect();
                    if dates.len() == 1 {
                        a.pay_date = dates.first().copied();
                        a.source = format!("{}; {url}", a.source);
                        added += 1;
                    } else if dates.len() > 1 {
                        failures.insert(
                            a.id.clone(),
                            "AAStocks cash rows have conflicting payment dates".into(),
                        );
                    }
                }
            }
        }
    }
    for entry in fs::read_dir(output.join("raw"))? {
        let p = entry?.path();
        bundle.raw_sha256.insert(
            p.file_name().unwrap().to_string_lossy().into(),
            format!("sha256:{:x}", Sha256::digest(fs::read(p)?)),
        );
    }
    fs::write(
        output.join("actions.json"),
        serde_json::to_vec_pretty(&bundle).map_err(invalid)?,
    )?;
    let missing: Vec<_> = bundle
        .actions
        .iter()
        .filter(|a| a.ex_date >= start && a.ex_date <= end)
        .filter_map(|a| a.validate().err().map(|e| format!("{}: {e}", a.id)))
        .collect();
    fs::write(output.join("payment_coverage.json"),serde_json::to_vec_pretty(&serde_json::json!({"start":start,"end":end,"saved_evidence_only":saved_only,"processed_at":chrono::Utc::now().to_rfc3339(),"symbols_requested":symbols.len(),"cash_dates_added":added,"failures":failures,"incomplete_events":missing,"complete_for_execution":false,"event_categories":"unverified"})).map_err(invalid)?)?;
    Ok(bundle)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cash_row_payment_is_separate_from_bonus_arrival_and_must_match_dates() {
        let html = "<div id=\"cp_pBottom\"><table><tr><td>公告日期</td><td>年度</td><td>分红</td><td>送股</td><td>股权登记日</td><td>除权除息日</td><td>派送日</td></tr><tr><td>2017/07/08</td><td>2016</td><td>¥3.700</td><td>-/-</td><td>2017/07/13</td><td>2017/07/14</td><td>2017/07/18</td></tr><tr><td>2017/07/08</td><td>2016</td><td>-</td><td>4.5/-</td><td>2017/07/13</td><td>2017/07/14</td><td>2017/07/14</td></tr></table></div>";
        let start = "2013-01-01".parse().unwrap();
        let end = "2026-10-12".parse().unwrap();
        let rows = parse(html, start, end).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].3, "2017-07-18".parse().unwrap());
        assert!((rows[0].2 - 0.37).abs() < 1e-12);
        assert!(parse("blocked page", start, end).is_err());
    }
    #[test]
    fn invalid_old_placeholder_does_not_hide_verified_recent_cash_date() {
        let html = "<div id=\"cp_pBottom\"><table><tr><td>公告日期</td><td>年度</td><td>分红</td><td>送股</td><td>股权登记日</td><td>除权除息日</td><td>派送日</td></tr><tr><td>2000/04/21</td><td>1999</td><td>¥1.000</td><td>-/-</td><td>2000/04/26</td><td>2000/04/27</td><td>1900/01/01</td></tr><tr><td>2017/07/08</td><td>2016</td><td>¥3.700</td><td>-/-</td><td>2017/07/13</td><td>2017/07/14</td><td>2017/07/18</td></tr></table></div>";
        assert_eq!(
            parse(
                html,
                "2013-01-01".parse().unwrap(),
                "2026-10-12".parse().unwrap()
            )
            .unwrap()
            .len(),
            1
        );
        assert!(
            parse(
                html,
                "2000-01-01".parse().unwrap(),
                "2026-10-12".parse().unwrap()
            )
            .is_err()
        );
    }
    #[test]
    fn offline_payment_repair_preserves_acquisition_and_rejects_tampered_evidence() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("source");
        fs::create_dir_all(root.join("raw")).unwrap();
        let name = "SH600000-aastocks.html";
        let html = "(600000.SH)<div id=\"cp_pBottom\"><table><tr><td>公告日期</td><td>年度</td><td>分红</td><td>送股</td><td>股权登记日</td><td>除权除息日</td><td>派送日</td></tr><tr><td>2017/07/08</td><td>2016</td><td>¥3.700</td><td>-/-</td><td>2017/07/13</td><td>2017/07/14</td><td>2017/07/18</td></tr></table></div>";
        fs::write(root.join("raw").join(name), html).unwrap();
        let source = ActionBundle {
            schema_version: 3,
            source_bindings: Default::default(),
            coverage: Default::default(),
            downloader: "JFQuant analytics-storage corporate-actions".into(),
            downloaded_at: "2026-10-13T00:00:00Z".into(),
            queried_symbols: BTreeSet::from(["SH600000".into()]),
            actions: vec![CorporateAction {
                id: "SH600000:2017-07-14".into(),
                instrument: "SH600000".into(),
                announced: "2017-07-08".parse().unwrap(),
                record_date: "2017-07-13".parse().unwrap(),
                ex_date: "2017-07-14".parse().unwrap(),
                pay_date: None,
                listing_date: None,
                cash_per_share: 0.37,
                shares_per_share: 0.0,
                terms: Default::default(),
                source: "https://example.test/evidence".into(),
            }],
            failures: BTreeMap::new(),
            raw_sha256: BTreeMap::from([(
                name.into(),
                format!("sha256:{:x}", Sha256::digest(html.as_bytes())),
            )]),
        };
        let start = "2013-01-01".parse().unwrap();
        let end = "2026-10-12".parse().unwrap();
        assert!(
            source
                .require_coverage(&source.queried_symbols, start, end)
                .is_err()
        );
        let repaired =
            repair_pay_dates(&source, &root, &temp.path().join("repaired"), start, end).unwrap();
        assert_eq!(repaired.downloaded_at, source.downloaded_at);
        assert_eq!(repaired.raw_sha256, source.raw_sha256);
        assert_eq!(
            repaired.actions[0].pay_date,
            Some("2017-07-18".parse().unwrap())
        );
        repaired.actions[0].validate().unwrap();
        assert!(
            repaired
                .require_coverage(&source.queried_symbols, start, end)
                .is_err(),
            "payment repair must not certify other event categories"
        );
        fs::write(root.join("raw").join(name), "altered evidence").unwrap();
        assert!(
            repair_pay_dates(&source, &root, &temp.path().join("tampered"), start, end).is_err()
        );
    }
}
