//! Latest-only mainland cash-equity market presets and per-order CNY fees.
//! Rates are decimal fractions of turnover, never percentages or basis points.
//! This module owns no backend handles and never performs network requests.
use crate::{CoreError, Date};
use serde::{Deserialize, Serialize};

type Result<T> = std::result::Result<T, CoreError>;
fn invalid(message: &str) -> CoreError {
    CoreError::InvalidRequest(message.into())
}
fn nonnegative(value: f64) -> bool {
    value.is_finite() && value >= 0.0
}
/// Supported domestic A-share cash-equity venues, excluding Connect and funds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Exchange {
    /// Shanghai Stock Exchange.
    Shanghai,
    /// Shenzhen Stock Exchange.
    Shenzhen,
    /// Beijing Stock Exchange (explicit BJ identifiers only).
    Beijing,
}

/// The board selects current daily-limit and order-size defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Board {
    /// Shanghai or Shenzhen main board.
    Main,
    /// Shanghai STAR board.
    Star,
    /// Shenzhen ChiNext board.
    Chinext,
    /// Beijing cash equities.
    Beijing,
}

/// Classify explicit canonical A-share symbols; never guess another asset's fees.
pub fn classify(instrument: &str) -> Result<(Exchange, Board)> {
    let bytes = instrument.as_bytes();
    if bytes.len() != 8 || !bytes[2..].iter().all(u8::is_ascii_digit) {
        return Err(invalid(
            "expected canonical A-share symbol such as SH600000",
        ));
    }
    let prefix = &instrument[2..5];
    match (&instrument[..2], prefix) {
        ("SH", "600" | "601" | "603" | "605") => Ok((Exchange::Shanghai, Board::Main)),
        ("SH", "688" | "689") => Ok((Exchange::Shanghai, Board::Star)),
        ("SZ", "000" | "001" | "002" | "003") => Ok((Exchange::Shenzhen, Board::Main)),
        ("SZ", "300" | "301" | "302") => Ok((Exchange::Shenzhen, Board::Chinext)),
        ("BJ", _) if matches!(bytes[2], b'4' | b'8') || prefix == "920" => {
            Ok((Exchange::Beijing, Board::Beijing))
        }
        _ => Err(invalid(
            "unsupported asset: preset only covers domestic A-share cash equities",
        )),
    }
}

/// Explicit broker-contract inclusions prevent double charging statutory fees.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommissionMode {
    /// Net broker commission; exchange, regulatory and transfer fees added.
    Net,
    /// Commission includes exchange and regulatory fees; transfer fee added.
    Gross,
    /// Commission also includes transfer fee; only sell stamp duty is added.
    AllInclusive,
}

/// User's negotiated broker commission; the example default is not a uniform tariff.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BrokerCommission {
    /// Decimal turnover rate; default 0.00025 (万2.5).
    pub rate: f64,
    /// Minimum charged once per filled order in CNY; default 5.
    pub minimum_cny: f64,
    /// Which third-party charges are already inside commission.
    pub mode: CommissionMode,
}
impl Default for BrokerCommission {
    fn default() -> Self {
        Self {
            rate: 0.00025,
            minimum_cny: 5.0,
            mode: CommissionMode::Gross,
        }
    }
}

/// Public fee data for one exchange, separate from negotiated commission.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExchangeFees {
    /// Venue.
    pub exchange: Exchange,
    /// Exchange handling fee, both sides.
    pub handling_rate: f64,
    /// Securities regulatory fee, both sides.
    pub regulatory_rate: f64,
    /// CSDC transfer fee, both sides.
    pub transfer_rate: f64,
    /// Securities stamp duty, sells only.
    pub sell_stamp_rate: f64,
}

/// Current regular-session board rules; listing/relisting exemptions need metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoardRules {
    /// Board.
    pub board: Board,
    /// Regular daily price-limit fraction.
    pub daily_limit: f64,
    /// Regular minimum buy quantity in raw shares.
    pub minimum_buy_shares: u32,
    /// Buy-quantity increment in raw shares.
    pub buy_increment_shares: u32,
    /// Minimum price tick in CNY.
    pub tick_cny: f64,
    /// Minimum trading-calendar days from purchase until sale.
    pub sell_delay_trading_days: u32,
}

/// Latest-only snapshot, serializable to user-editable JSON and run manifests.
/// The same snapshot applies to every backtest date; no historical rate lookup.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MarketRulesPreset {
    /// Preset JSON format version (currently 1).
    pub schema_version: u32,
    /// Stable preset identifier; custom rates must retain their run snapshot.
    pub preset_id: String,
    /// Last date on which the built-in sources were checked.
    pub verified_on: Date,
    /// Provenance URLs, retained in every result.
    pub sources: Vec<String>,
    /// Latest public cash-equity fees for each supported venue.
    pub exchanges: Vec<ExchangeFees>,
    /// Current regular-session trading defaults by board.
    pub boards: Vec<BoardRules>,
    /// Customizable broker contract.
    #[serde(default)]
    pub commission: BrokerCommission,
}

impl Default for MarketRulesPreset {
    fn default() -> Self {
        Self::a_share_latest()
    }
}

impl MarketRulesPreset {
    /// Built-in 2026-10-13 snapshot; regular cash-equity trades, no historical schedules.
    pub fn a_share_latest() -> Self {
        let exchanges = [Exchange::Shanghai, Exchange::Shenzhen, Exchange::Beijing]
            .into_iter()
            .map(|exchange| ExchangeFees {
                exchange,
                handling_rate: if exchange == Exchange::Beijing {
                    0.000125
                } else {
                    0.0000341
                },
                regulatory_rate: 0.00002,
                transfer_rate: 0.00001,
                sell_stamp_rate: 0.0005,
            })
            .collect();
        let boards = [
            (Board::Main, 0.1, 100, 100),
            (Board::Star, 0.2, 200, 1),
            (Board::Chinext, 0.2, 100, 100),
            (Board::Beijing, 0.3, 100, 1),
        ]
        .into_iter()
        .map(
            |(board, daily_limit, minimum_buy_shares, buy_increment_shares)| BoardRules {
                board,
                daily_limit,
                minimum_buy_shares,
                buy_increment_shares,
                tick_cny: 0.01,
                sell_delay_trading_days: 1,
            },
        )
        .collect();
        Self {
            schema_version: 1, preset_id: "cn-a-share-latest".into(), verified_on: Date::from_ymd_opt(2026, 10, 13).unwrap(),
            sources: vec![
                "https://investor.szse.cn/marketServices/deal/payFees/index.html".into(),
                "https://one.sse.com.cn/onething/gptz/".into(),
                "https://www.csrc.gov.cn/csrc/c100028/c7426794/content.shtml".into(),
                "https://www.chinaclear.cn/zdjs/fbzyls/service_tlist.shtml".into(),
                "https://www.xcsc.com/main/a/20260730/1023018024.shtml".into(),
                "https://shanghai.chinatax.gov.cn/zcfw/zcfgk/yhs/202308/t468451.html".into(),
                "https://www.sse.com.cn/lawandrules/sselawsrules2025/stocks/exchange/c/c_20260424_10816482.shtml".into(),
                "https://investor.szse.cn/lawrules/rule/trade/t20260424_620190.html".into(),
                "https://www.bse.cn/important_news/200010675.html".into(),
            ], exchanges, boards, commission: BrokerCommission::default(),
        }
    }
    /// Decode overrides; missing commission uses defaults, invalid data never falls back.
    pub fn from_json(json: &str) -> Result<Self> {
        let value: Self =
            serde_json::from_str(json).map_err(|e| CoreError::Serialization(e.to_string()))?;
        value.validate()?;
        Ok(value)
    }
    /// Reject incomplete, ambiguous, nonfinite and negative fee/rule settings.
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != 1
            || self.preset_id.trim().is_empty()
            || self.sources.is_empty()
            || !nonnegative(self.commission.rate)
            || self.commission.rate > 0.003
            || !nonnegative(self.commission.minimum_cny)
            || self.commission.minimum_cny > 1e9
        {
            return Err(invalid(
                "invalid preset version, provenance or broker commission",
            ));
        }
        for exchange in [Exchange::Shanghai, Exchange::Shenzhen, Exchange::Beijing] {
            let entries: Vec<_> = self
                .exchanges
                .iter()
                .filter(|f| f.exchange == exchange)
                .collect();
            if entries.len() != 1
                || [
                    entries[0].handling_rate,
                    entries[0].regulatory_rate,
                    entries[0].transfer_rate,
                    entries[0].sell_stamp_rate,
                ]
                .iter()
                .any(|r| !nonnegative(*r) || *r > 0.01)
            {
                return Err(invalid(
                    "each exchange needs exactly one valid public fee schedule",
                ));
            }
        }
        for board in [Board::Main, Board::Star, Board::Chinext, Board::Beijing] {
            let entries: Vec<_> = self.boards.iter().filter(|f| f.board == board).collect();
            if entries.len() != 1 {
                return Err(invalid("each board needs exactly one rule"));
            }
            let rule = entries[0];
            if !rule.daily_limit.is_finite()
                || rule.daily_limit <= 0.0
                || rule.daily_limit >= 1.0
                || !rule.tick_cny.is_finite()
                || rule.tick_cny <= 0.0
                || rule.minimum_buy_shares == 0
                || rule.buy_increment_shares == 0
                || rule.sell_delay_trading_days != 1
            {
                return Err(invalid(
                    "invalid board rule (A-share cash equity requires T+1)",
                ));
            }
        }
        Ok(())
    }
    /// Look up the current regular-session board rule for a canonical equity symbol.
    pub fn board_for(&self, instrument: &str) -> Result<&BoardRules> {
        let (_, board) = classify(instrument)?;
        self.boards
            .iter()
            .find(|r| r.board == board)
            .ok_or_else(|| invalid("board rule missing"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn malformed_custom_settings_never_fall_back_to_defaults() {
        for json in [
            r#"{"commission":{"rate":-0.1}}"#,
            r#"{"commission":{"minimum_cny":-5}}"#,
            r#"{"commission":{"rate":0.01}}"#,
            r#"{"commission":{"mode":"unknown"}}"#,
            r#"{"commision":{"rate":0.0002}}"#,
        ] {
            assert!(MarketRulesPreset::from_json(json).is_err(), "{json}");
        }
        let mut rules = MarketRulesPreset::a_share_latest();
        rules.exchanges.push(rules.exchanges[0].clone());
        assert!(rules.validate().is_err());
        rules = MarketRulesPreset::a_share_latest();
        rules.commission.rate = f64::NAN;
        assert!(rules.validate().is_err());
    }

    #[test]
    fn json_defaults_roundtrip_validation_and_asset_boundaries() {
        let rules = MarketRulesPreset::a_share_latest();
        let mut json = serde_json::to_value(&rules).unwrap();
        json.as_object_mut().unwrap().remove("commission");
        assert_eq!(
            MarketRulesPreset::from_json(&json.to_string())
                .unwrap()
                .commission
                .minimum_cny,
            5.0
        );
        let partial = MarketRulesPreset::from_json(r#"{"commission":{"rate":0.0002}}"#).unwrap();
        assert_eq!(partial.commission.rate, 0.0002);
        assert_eq!(partial.commission.minimum_cny, 5.0);
        assert_eq!(partial.exchanges.len(), 3);
        json["schema_version"] = 99.into();
        assert!(MarketRulesPreset::from_json(&json.to_string()).is_err());
        for symbol in [
            "SH000905",
            "SH510500",
            "SZ200001",
            "HK00700",
            "SH60000é",
            "sh600000",
            "SH6000000",
        ] {
            assert!(classify(symbol).is_err(), "{symbol}");
        }
        assert_eq!(rules.board_for("SZ300001").unwrap().daily_limit, 0.2);
        assert_eq!(rules.board_for("SZ302132").unwrap().daily_limit, 0.2);
        assert_eq!(rules.board_for("SH688001").unwrap().minimum_buy_shares, 200);
    }
}
