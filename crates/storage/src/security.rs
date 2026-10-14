//! Rehash identity evidence before trusting a dated source binding.
use analytics_core::security::SecurityRegistry;
use sha2::{Digest, Sha256};
use std::{fs, io, path::Path};
fn invalid(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}
/// Validate captured originals, extracted text, quotes and issuer/source relationships.
pub fn verify(r: &SecurityRegistry, root: &Path) -> io::Result<()> {
    r.validate().map_err(invalid)?;
    let root = root.canonicalize()?;
    for e in r.evidence.values() {
        if e.quote.trim().is_empty()
            || e.parser_version.trim().is_empty()
            || !matches!(
                e.kind.as_str(),
                "supplier_identity"
                    | "implementation"
                    | "listing"
                    | "source_membership"
                    | "synthetic_fixture"
            )
        {
            return Err(invalid("invalid identity evidence metadata"));
        }
        let allowed = [
            "https://finance.sina.com.cn/",
            "https://money.finance.sina.com.cn/",
            "https://vip.stock.finance.sina.com.cn/",
            "https://static.cninfo.com.cn/",
            "https://www.cninfo.com.cn/",
            "https://www.sse.com.cn/",
            "https://www.szse.cn/",
            "https://github.com/chenditc/investment_data/",
        ];
        if !r.synthetic_fixture && !allowed.iter().any(|p| e.source_url.starts_with(p)) {
            return Err(invalid("untrusted identity evidence origin"));
        }
        for (path, hash) in [(&e.original, &e.original_sha256), (&e.text, &e.text_sha256)] {
            if path.is_absolute()
                || path
                    .components()
                    .any(|c| !matches!(c, std::path::Component::Normal(_)))
            {
                return Err(invalid("identity evidence path must be relative"));
            }
            let p = root.join(path).canonicalize()?;
            if !p.starts_with(&root) || !p.is_file() {
                return Err(invalid("identity evidence escapes package"));
            }
            if hash != &format!("sha256:{:x}", Sha256::digest(fs::read(p)?)) {
                return Err(invalid("identity evidence hash mismatch"));
            }
        }
        if !fs::read_to_string(root.join(&e.text))?.contains(&e.quote) {
            return Err(invalid("identity evidence quote mismatch"));
        }
    }
    if !r.synthetic_fixture {
        for link in &r.successions {
            let quoted = link
                .evidence
                .iter()
                .map(|key| r.evidence[key].quote.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            let before = &r.securities[&link.predecessor];
            let after = &r.securities[&link.successor];
            if !quoted.contains(&before.name)
                || !quoted.contains(&after.name)
                || !quoted.contains(&link.ratio)
                || !link.evidence.iter().any(|key| {
                    matches!(r.evidence[key].kind.as_str(), "implementation" | "listing")
                })
            {
                return Err(invalid(
                    "succession names/ratio/implementation evidence mismatch",
                ));
            }
        }
        for s in r.securities.values() {
            if !s
                .evidence
                .iter()
                .any(|k| r.evidence[k].quote.contains(&s.issuer))
            {
                return Err(invalid("issuer identity evidence missing"));
            }
            let lifecycle = s
                .evidence
                .iter()
                .map(|k| r.evidence[k].quote.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            for date in [s.lifetime.start, s.lifetime.end] {
                if !quotes_date(&lifecycle, date) {
                    return Err(invalid("security lifecycle date lacks quoted evidence"));
                }
            }
            for l in &s.locators {
                let quoted = l
                    .evidence
                    .iter()
                    .map(|k| r.evidence[k].quote.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                let locator = if l.provider == "exchange"
                    || (l.provider == "qlib"
                        && analytics_core::pool::validate_exchange_symbol(&l.symbol).is_ok())
                {
                    &l.symbol[2..]
                } else {
                    &l.symbol
                };
                if !quoted.contains(locator)
                    || (!quoted.contains(&s.issuer) && !quoted.contains(&s.name))
                {
                    return Err(invalid(format!(
                        "{}:{} alias/issuer evidence missing",
                        l.provider, l.symbol
                    )));
                }
                for date in [l.coverage.start, l.coverage.end] {
                    if !quotes_date(&quoted, date) {
                        return Err(invalid("locator date lacks quoted evidence"));
                    }
                }
                if l.provider == "exchange"
                    && !l.evidence.iter().any(|k| {
                        matches!(r.evidence[k].kind.as_str(), "implementation" | "listing")
                    })
                {
                    return Err(invalid(
                        "exchange binding needs implementation/listing proof",
                    ));
                }
            }
        }
    }
    Ok(())
}
/// Read and verify the current portable identity package.
pub fn read(path: &Path) -> io::Result<SecurityRegistry> {
    let r: SecurityRegistry = serde_json::from_slice(&fs::read(path)?).map_err(invalid)?;
    verify(
        &r,
        path.parent()
            .ok_or_else(|| invalid("identity package root missing"))?,
    )?;
    Ok(r)
}

fn quotes_date(text: &str, date: analytics_core::Date) -> bool {
    use chrono::Datelike;
    [
        date.to_string(),
        format!("{}年{}月{}日", date.year(), date.month(), date.day()),
        format!("{}/{}/{}", date.year(), date.month(), date.day()),
        format!("{}年{:02}月{:02}日", date.year(), date.month(), date.day()),
    ]
    .iter()
    .any(|d| text.contains(d))
}

#[cfg(test)]
mod tests {
    use super::*;
    use analytics_core::{
        DateRange,
        security::{IdentityEvidence, SecurityLocator, SecurityRecord},
    };
    #[test]
    fn separate_supplier_and_listing_proofs_are_checked_and_tampering_fails() {
        let d = tempfile::tempdir().unwrap();
        let dates = DateRange {
            start: "2005-04-08".parse().unwrap(),
            end: "2006-01-03".parse().unwrap(),
        };
        let supplier = "SHT00018 上港集箱 2005-04-08 2006-01-03";
        let implementation =
            "上海港集装箱股份有限公司 上港集箱 证券代码600018 2005-04-08 2006-01-03";
        let mut evidence = std::collections::BTreeMap::new();
        for (key, body, kind) in [
            ("supplier", supplier, "supplier_identity"),
            ("implementation", implementation, "implementation"),
        ] {
            let file = format!("{key}.txt");
            fs::write(d.path().join(&file), body).unwrap();
            let hash = format!("sha256:{:x}", Sha256::digest(body));
            evidence.insert(
                key.into(),
                IdentityEvidence {
                    original: file.clone().into(),
                    original_sha256: hash.clone(),
                    text: file.into(),
                    text_sha256: hash,
                    source_url: "https://finance.sina.com.cn/realstock/company/shT00018/nc.shtml"
                        .into(),
                    quote: body.into(),
                    kind: kind.into(),
                    parser_version: "fixture-v1".into(),
                },
            );
        }
        let id = "SEC-00000000000000000000000000000001";
        let record = SecurityRecord {
            security_id: id.into(),
            issuer: "上海港集装箱股份有限公司".into(),
            name: "上港集箱".into(),
            kind: "a_share".into(),
            lifetime: dates.clone(),
            locators: vec![
                SecurityLocator {
                    provider: "exchange".into(),
                    symbol: "SH600018".into(),
                    coverage: dates.clone(),
                    evidence: vec!["implementation".into()],
                },
                SecurityLocator {
                    provider: "qlib".into(),
                    symbol: "SHT00018".into(),
                    coverage: dates,
                    evidence: vec!["supplier".into()],
                },
            ],
            evidence: vec!["implementation".into()],
        };
        let r = SecurityRegistry {
            successions: Vec::new(),
            schema_version: 1,
            reason: "two independent facts joined by issuer identity".into(),
            synthetic_fixture: false,
            securities: [(id.into(), record)].into(),
            evidence,
        };
        verify(&r, d.path()).unwrap();
        let mut conflict = r.clone();
        conflict.securities.get_mut(id).unwrap().locators[1].symbol = "SHT99999".into();
        assert!(verify(&conflict, d.path()).is_err());
        fs::write(d.path().join("supplier.txt"), "altered body").unwrap();
        assert!(
            verify(&r, d.path())
                .unwrap_err()
                .to_string()
                .contains("hash mismatch")
        );
    }
}

/// Capture fixed allowed HTML identity sources as auditable candidates; never creates identities.
pub fn capture_sources(urls: &[String], output: &Path) -> io::Result<serde_json::Value> {
    capture_with(urls, output, |url| {
        let result = crate::transport::curl(std::process::Command::new("/usr/bin/curl").args([
            "--fail",
            "--silent",
            "--show-error",
            "--connect-timeout",
            "15",
            "--max-time",
            "45",
            "--retry",
            "2",
            "--max-filesize",
            "20971520",
            "--write-out",
            "\nJFQuantHTTP:%{http_code}",
            url,
        ]))?;
        if !result.status.success() {
            return Err(invalid(String::from_utf8_lossy(&result.stderr)));
        }
        let marker = b"\nJFQuantHTTP:";
        let pos = result
            .stdout
            .windows(marker.len())
            .rposition(|w| w == marker)
            .ok_or_else(|| invalid("identity HTTP status missing"))?;
        if &result.stdout[pos + marker.len()..] != b"200" {
            return Err(invalid("identity source must return 200 without redirects"));
        }
        Ok(result.stdout[..pos].to_vec())
    })
}
fn capture_with<F>(urls: &[String], output: &Path, mut fetch: F) -> io::Result<serde_json::Value>
where
    F: FnMut(&str) -> io::Result<Vec<u8>>,
{
    if urls.is_empty() || urls.len() > 32 || output.exists() {
        return Err(invalid("invalid capture request or existing destination"));
    }
    let allowed = [
        "https://finance.sina.com.cn/realstock/company/",
        "https://money.finance.sina.com.cn/corp/",
        "https://finance.sina.com.cn/stock/",
        "https://vip.stock.finance.sina.com.cn/corp/",
    ];
    for url in urls {
        if !allowed.iter().any(|p| url.starts_with(p)) || url.chars().any(char::is_control) {
            return Err(invalid("unsupported identity source address"));
        }
    }
    fs::create_dir_all(output)?;
    let mut sources = vec![];
    let mut failures = vec![];
    for url in urls {
        analytics_core::task::TaskContext::current().check()?;
        let result = (|| -> io::Result<serde_json::Value> {
            let bytes = fetch(url)?;
            if bytes.is_empty() || bytes.len() > 20 * 1024 * 1024 || bytes.starts_with(b"%PDF") {
                return Err(invalid(
                    "identity HTML body empty/oversized/PDF; use announcement PDF acquisition",
                ));
            }
            let html = match std::str::from_utf8(&bytes) {
                Ok(s) => s.to_owned(),
                Err(_) => {
                    let (s, _, errors) = encoding_rs::GB18030.decode(&bytes);
                    if errors {
                        return Err(invalid("identity source encoding invalid"));
                    }
                    s.into_owned()
                }
            };
            let mut text = String::new();
            let mut tag = false;
            for c in html.chars() {
                if c == '<' {
                    tag = true;
                    text.push('\n');
                } else if c == '>' {
                    tag = false;
                } else if !tag {
                    text.push(c);
                }
            }
            let text = text
                .replace("&nbsp;", " ")
                .replace("&amp;", "&")
                .replace("&quot;", "\"");
            let key = format!("identity-{}", &format!("{:x}", Sha256::digest(url))[..16]);
            let original = format!("{key}.html");
            let extracted = format!("{key}.txt");
            fs::write(output.join(&original), &bytes)?;
            fs::write(output.join(&extracted), &text)?;
            Ok(
                serde_json::json!({"source_url":url,"original":original,"original_sha256":format!("sha256:{:x}",Sha256::digest(bytes)),"text":extracted,"text_sha256":format!("sha256:{:x}",Sha256::digest(text)),"http_status":200,"observed_at":chrono::Utc::now().to_rfc3339(),"parser_version":"JFQuant identity-html-v1","candidate_only":true}),
            )
        })();
        match result {
            Ok(s) => sources.push(s),
            Err(e) => {
                analytics_core::task::TaskContext::current().check()?;
                failures.push(serde_json::json!({"source_url":url,"message":e.to_string()}));
            }
        }
    }
    let result = serde_json::json!({"schema_version":1,"sources":sources,"failures":failures,"candidate_only":true});
    fs::write(
        output.join("capture.json"),
        serde_json::to_vec_pretty(&result).map_err(invalid)?,
    )?;
    Ok(result)
}
#[cfg(test)]
mod capture_tests {
    use super::*;
    #[test]
    fn identity_capture_keeps_other_sources_after_failure_and_rejects_redirect_targets() {
        let d = tempfile::tempdir().unwrap();
        let urls = vec![
            "https://finance.sina.com.cn/stock/a.shtml".into(),
            "https://finance.sina.com.cn/stock/b.shtml".into(),
        ];
        let mut count = 0;
        let r = capture_with(&urls, &d.path().join("capture"), |_| {
            count += 1;
            if count == 1 {
                Err(invalid("timeout"))
            } else {
                Ok("<html>上港集箱 T00018</html>".as_bytes().to_vec())
            }
        })
        .unwrap();
        assert_eq!(count, 2);
        assert_eq!(r["sources"].as_array().unwrap().len(), 1);
        assert_eq!(r["failures"].as_array().unwrap().len(), 1);
        assert!(
            capture_with(
                &["https://finance.sina.com.cn.evil/stock/a".into()],
                &d.path().join("untrusted"),
                |_| unreachable!()
            )
            .is_err()
        );
    }
}
