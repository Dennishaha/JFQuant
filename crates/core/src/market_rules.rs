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
fn money(value: f64) -> f64 {
    (value * 100.0 + 1e-9).round() / 100.0
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

/// Side of a filled order; stamp duty applies only to sells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderSide {
    /// Purchase.
    Buy,
    /// Sale.
    Sell,
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

/// Additive, non-overlapping cost components in CNY (rounded per order to fen).
#[derive(Debug, Default, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FeeBreakdown {
    /// Broker-retained commission, excluding embedded public fees.
    pub broker_commission: f64,
    /// Exchange fee, reported even when embedded in commission.
    pub exchange_handling: f64,
    /// Regulatory fee, reported even when embedded in commission.
    pub regulatory: f64,
    /// Transfer fee, reported even when embedded in commission.
    pub transfer: f64,
    /// Sell-only stamp duty.
    pub stamp_duty: f64,
}
impl FeeBreakdown {
    /// Total cash charge; embedded fees must not be added again.
    pub fn total(&self) -> f64 {
        money(
            self.broker_commission
                + self.exchange_handling
                + self.regulatory
                + self.transfer
                + self.stamp_duty,
        )
    }
    /// Accumulate order components, keeping currency precision.
    pub fn add(&mut self, other: Self) {
        self.broker_commission = money(self.broker_commission + other.broker_commission);
        self.exchange_handling = money(self.exchange_handling + other.exchange_handling);
        self.regulatory = money(self.regulatory + other.regulatory);
        self.transfer = money(self.transfer + other.transfer);
        self.stamp_duty = money(self.stamp_duty + other.stamp_duty);
    }
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
    /// Quote an actually filled order, independently of the backtest date.
    /// Zero turnover is a no-fill and incurs no minimum commission.
    pub fn fees(
        &self,
        instrument: &str,
        side: OrderSide,
        turnover_cny: f64,
    ) -> Result<FeeBreakdown> {
        let (exchange, _) = classify(instrument)?;
        let schedule = self
            .exchanges
            .iter()
            .find(|f| f.exchange == exchange)
            .ok_or_else(|| invalid("exchange fee schedule missing"))?;
        if !nonnegative(self.commission.rate)
            || self.commission.rate > 0.003
            || !nonnegative(self.commission.minimum_cny)
            || [
                schedule.handling_rate,
                schedule.regulatory_rate,
                schedule.transfer_rate,
                schedule.sell_stamp_rate,
            ]
            .iter()
            .any(|r| !nonnegative(*r) || *r > 0.01)
        {
            return Err(invalid("invalid commission or public fee rate"));
        }
        if !nonnegative(turnover_cny) || turnover_cny > 1e13 {
            return Err(invalid("invalid order turnover"));
        }
        if turnover_cny == 0.0 {
            return Ok(FeeBreakdown::default());
        }
        let exchange_handling = money(turnover_cny * schedule.handling_rate);
        let regulatory = money(turnover_cny * schedule.regulatory_rate);
        let transfer = money(turnover_cny * schedule.transfer_rate);
        let included_rate = match self.commission.mode {
            CommissionMode::Net => 0.0,
            CommissionMode::Gross => schedule.handling_rate + schedule.regulatory_rate,
            CommissionMode::AllInclusive => {
                schedule.handling_rate + schedule.regulatory_rate + schedule.transfer_rate
            }
        };
        if self.commission.rate + 1e-15 < included_rate {
            return Err(invalid(
                "gross commission rate is below its included public fees",
            ));
        }
        let commission =
            money((turnover_cny * self.commission.rate).max(self.commission.minimum_cny));
        let included = match self.commission.mode {
            CommissionMode::Net => 0.0,
            CommissionMode::Gross => exchange_handling + regulatory,
            CommissionMode::AllInclusive => exchange_handling + regulatory + transfer,
        };
        Ok(FeeBreakdown {
            broker_commission: money((commission - included).max(0.0)),
            exchange_handling,
            regulatory,
            transfer,
            stamp_duty: if side == OrderSide::Sell {
                money(turnover_cny * schedule.sell_stamp_rate)
            } else {
                0.0
            },
        })
    }
    /// Largest fractional notional affordable including nonlinear minimum charges.
    /// Quantity/lot rounding belongs to the execution adapter using raw shares.
    pub fn affordable_buy_notional(&self, instrument: &str, cash_cny: f64) -> Result<f64> {
        if !nonnegative(cash_cny) || cash_cny > 1e13 {
            return Err(invalid("invalid available cash"));
        }
        self.fees(instrument, OrderSide::Buy, cash_cny)?;
        let (mut low, mut high) = (0.0, cash_cny);
        for _ in 0..64 {
            let middle = low + (high - low) / 2.0;
            if middle + self.fees(instrument, OrderSide::Buy, middle)?.total() <= cash_cny {
                low = middle;
            } else {
                high = middle;
            }
        }
        Ok(low)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_fees_are_asymmetric_and_do_not_double_charge_embedded_fees() {
        let rules = MarketRulesPreset::a_share_latest();
        rules.validate().unwrap();
        for symbol in [
            "SH600000", "SZ000001", "SH688001", "SZ301001", "SZ302132", "BJ920001",
        ] {
            let buy = rules.fees(symbol, OrderSide::Buy, 100_000.0).unwrap();
            let sell = rules.fees(symbol, OrderSide::Sell, 100_000.0).unwrap();
            assert_eq!(buy.total(), 26.0);
            assert_eq!(sell.total(), 76.0);
            assert_eq!(buy.stamp_duty, 0.0);
            assert_eq!(sell.stamp_duty, 50.0);
        }
        let sh = rules.fees("SH600000", OrderSide::Buy, 100_000.0).unwrap();
        assert_eq!(sh.exchange_handling, 3.41);
        assert_eq!(sh.regulatory, 2.0);
        assert_eq!(sh.broker_commission, 19.59);
        let bj = rules.fees("BJ920001", OrderSide::Buy, 100_000.0).unwrap();
        assert_eq!(bj.exchange_handling, 12.5);
        assert_eq!(bj.broker_commission, 10.5);
    }
    #[test]
    fn net_and_all_inclusive_contracts_have_distinct_totals() {
        let mut rules = MarketRulesPreset::a_share_latest();
        rules.commission.mode = CommissionMode::Net;
        assert_eq!(
            rules
                .fees("SH600000", OrderSide::Buy, 100_000.0)
                .unwrap()
                .total(),
            31.41
        );
        rules.commission.mode = CommissionMode::AllInclusive;
        assert_eq!(
            rules
                .fees("SH600000", OrderSide::Buy, 100_000.0)
                .unwrap()
                .total(),
            25.0
        );
        rules.commission.rate = 0.00001;
        assert!(rules.fees("SH600000", OrderSide::Buy, 100_000.0).is_err());
    }
    #[test]
    fn minimum_commission_rounding_zero_fill_and_cash_budget_are_per_order() {
        let rules = MarketRulesPreset::a_share_latest();
        assert_eq!(
            rules
                .fees("SH600000", OrderSide::Buy, 1000.0)
                .unwrap()
                .total(),
            5.01
        );
        assert_eq!(
            rules
                .fees("SH600000", OrderSide::Sell, 1000.0)
                .unwrap()
                .total(),
            5.51
        );
        assert_eq!(
            rules.fees("SH600000", OrderSide::Buy, 0.0).unwrap().total(),
            0.0
        );
        let value = rules.affordable_buy_notional("SH600000", 100026.0).unwrap();
        assert!((value - 100000.0).abs() < 1e-7);
        assert!(
            value
                + rules
                    .fees("SH600000", OrderSide::Buy, value)
                    .unwrap()
                    .total()
                <= 100026.0
        );
        assert_eq!(
            rules.affordable_buy_notional("SH600000", 4.99).unwrap(),
            0.0
        );
        assert!(rules.fees("SH600000", OrderSide::Buy, f64::NAN).is_err());
    }
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
