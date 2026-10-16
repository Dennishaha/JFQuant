//! One operator catalogue, strict user compiler, and versioned execution AST.
#![allow(missing_docs)]
use crate::{FactorError, FactorExpr, Result, alpha158};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ParameterKind {
    Series,
    Window,
    Number,
    Probability,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperatorSpec {
    pub id: String,
    pub name: String,
    pub legacy_aliases: Vec<String>,
    pub category: String,
    pub parameters: Vec<ParameterKind>,
    pub parameter_names: Vec<String>,
    pub variadic_predictors: bool,
    pub output_type: String,
    pub version: String,
    pub window_rule: String,
    pub missing_rule: String,
    pub history_rule: String,
    pub example: String,
    pub description: String,
    kernel: Kernel,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
enum Kernel {
    Gtja(String),
    QlibCall(String),
    QlibBinary(String),
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub enum Expression {
    Input(String),
    Number(f64),
    Call {
        operator: String,
        args: Vec<Expression>,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CompiledExpression {
    pub schema_version: u32,
    pub expression: Expression,
    pub inputs: Vec<String>,
    pub lookback: u32,
    pub requires_recursive_state: bool,
    pub operator_versions: BTreeMap<String, String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormulaError {
    pub message: String,
    pub start: usize,
    pub end: usize,
}
impl std::fmt::Display for FormulaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}（位置{}—{}）", self.message, self.start, self.end)
    }
}
impl std::error::Error for FormulaError {}

pub struct OperatorRegistry;
impl OperatorRegistry {
    pub fn all() -> &'static [OperatorSpec] {
        static SPECS: std::sync::OnceLock<Vec<OperatorSpec>> = std::sync::OnceLock::new();
        SPECS.get_or_init(|| {
            use ParameterKind::*;
            let mut out = Vec::new();
            let gtja = [
                ("ADD", "Add", vec![Series, Series]),
                ("SUB", "Sub", vec![Series, Series]),
                ("MUL", "Mul", vec![Series, Series]),
                ("DIV", "Div", vec![Series, Series]),
                ("NEG", "Neg", vec![Series]),
                ("ABS", "Abs", vec![Series]),
                ("LOG", "Log", vec![Series]),
                ("SIGN", "Sign", vec![Series]),
                ("POW_CONST", "Pow", vec![Series, Number]),
                ("POW", "PowExpr", vec![Series, Series]),
                ("EQ", "Eq", vec![Series, Series]),
                ("GT", "Gt", vec![Series, Series]),
                ("LT", "Lt", vec![Series, Series]),
                ("IF", "If", vec![Series, Series, Series]),
                ("DELAY", "Delay", vec![Series, Window]),
                ("DELTA", "Delta", vec![Series, Window]),
                ("SUM", "TsSum", vec![Series, Window]),
                ("MEAN", "TsMean", vec![Series, Window]),
                ("STD", "TsStd", vec![Series, Window]),
                ("TSMIN", "TsMin", vec![Series, Window]),
                ("TSMAX", "TsMax", vec![Series, Window]),
                ("TSRANK", "TsRank", vec![Series, Window]),
                ("DECAYLINEAR", "DecayLinear", vec![Series, Window]),
                ("CORR", "Corr", vec![Series, Series, Window]),
                ("COV", "Cov", vec![Series, Series, Window]),
                ("REGBETA", "RegBeta", vec![Series, Series, Window]),
                ("REGRESI_SINGLE", "RegResi", vec![Series, Series, Window]),
                ("REGRESI", "RegResiMulti", vec![Series, Series, Window]),
                ("RANK", "Rank", vec![Series]),
                ("ZSCORE", "ZScore", vec![Series]),
                ("SCALE", "Scale", vec![Series]),
                ("MIN", "Min", vec![Series, Series]),
                ("MAX", "Max", vec![Series, Series]),
                ("COUNT", "Count", vec![Series, Window]),
                ("PROD", "Product", vec![Series, Window]),
                ("WMA", "WeightedMean", vec![Series, Window]),
                ("SMA", "Ema", vec![Series, Window, Number]),
                ("HIGHDAY", "HighDay", vec![Series, Window]),
                ("LOWDAY", "LowDay", vec![Series, Window]),
                ("SUMIF", "SumIf", vec![Series, Series, Window]),
                ("FILTER", "Filter", vec![Series, Series]),
                ("CUMSUM", "CumulativeSum", vec![Series]),
                ("AND", "And", vec![Series, Series]),
                ("OR", "Or", vec![Series, Series]),
                ("SEQUENCE", "Sequence", vec![Window]),
            ];
            for (name, variant, parameters) in gtja {
                out.push(spec(
                    format!("GTJA_{name}"),
                    parameters,
                    Kernel::Gtja(variant.into()),
                ));
            }
            for (name, parameters) in [
                ("Abs", vec![Series]),
                ("Log", vec![Series]),
                ("Greater", vec![Series, Series]),
                ("Less", vec![Series, Series]),
                ("Ref", vec![Series, Window]),
                ("Mean", vec![Series, Window]),
                ("Sum", vec![Series, Window]),
                ("Std", vec![Series, Window]),
                ("Max", vec![Series, Window]),
                ("Min", vec![Series, Window]),
                ("Quantile", vec![Series, Window, Probability]),
                ("Rank", vec![Series, Window]),
                ("Slope", vec![Series, Window]),
                ("Rsquare", vec![Series, Window]),
                ("Resi", vec![Series, Window]),
                ("IdxMax", vec![Series, Window]),
                ("IdxMin", vec![Series, Window]),
                ("Corr", vec![Series, Series, Window]),
            ] {
                out.push(spec(
                    format!("QLIB_{}", name.to_ascii_uppercase()),
                    parameters,
                    Kernel::QlibCall(name.into()),
                ));
            }
            for name in ["add", "sub", "mul", "div", "gt", "lt"] {
                out.push(spec(
                    format!("QLIB_{}", name.to_ascii_uppercase()),
                    vec![Series, Series],
                    Kernel::QlibBinary(name.into()),
                ));
            }
            out
        })
    }
    pub fn get(id: &str) -> Option<&'static OperatorSpec> {
        Self::all().iter().find(|s| s.id == id)
    }
    pub fn search(query: &str) -> Vec<OperatorSpec> {
        let q = query.to_ascii_lowercase();
        Self::all()
            .iter()
            .filter(|s| {
                format!(
                    "{} {} {} {}",
                    s.id, s.category, s.description, s.window_rule
                )
                .to_ascii_lowercase()
                .contains(&q)
            })
            .cloned()
            .collect()
    }
}
fn spec(id: String, parameters: Vec<ParameterKind>, kernel: Kernel) -> OperatorSpec {
    let qlib = id.starts_with("QLIB_");
    let rolling = parameters.contains(&ParameterKind::Window);
    let cross = matches!(id.as_str(), "GTJA_RANK" | "GTJA_ZSCORE" | "GTJA_SCALE");
    let recursive = matches!(id.as_str(), "GTJA_SMA" | "GTJA_CUMSUM");
    let example = format!(
        "{}({})",
        id,
        parameters
            .iter()
            .enumerate()
            .map(|(i, p)| match p {
                ParameterKind::Series =>
                    if i == 0 {
                        "CLOSE"
                    } else {
                        "VOLUME"
                    },
                ParameterKind::Window => "20",
                ParameterKind::Probability => "0.5",
                ParameterKind::Number => "2",
            })
            .collect::<Vec<_>>()
            .join(", ")
    );
    let description = match id.as_str() {
        "GTJA_RANK" => "当日成员横截面百分位排名；并列取最大名次，使用既有容差。",
        "GTJA_TSRANK" => "完整滚动窗口的当前值百分位排名；并列取最大名次。",
        "QLIB_RANK" => "滚动窗口当前值百分位排名；并列取平均名次。",
        "GTJA_SMA" => "递归SMA(X,N,M)，系数M/N，以首个有限值为种子；缺失时保留状态。",
        "GTJA_WMA" => "完整窗口，按0.9^lag归一化加权。",
        "GTJA_HIGHDAY" | "GTJA_LOWDAY" => "距窗口最新极值的观测距离，当前极值为0。",
        "QLIB_IDXMAX" | "QLIB_IDXMIN" => "窗口内首个极值的位置，从1开始；保留Qlib缺失位置语义。",
        "QLIB_CORR" => "有效配对相关系数；各输入标准差<=2e-5时为空。",
        "GTJA_REGRESI" => "带截距多元OLS残差；响应、一个或多个预测量、窗口。",
        "GTJA_SUMIF" => "参数为值、条件、窗口；完整窗口内条件非零值的和。",
        "QLIB_GREATER" | "QLIB_LESS" => "逐元素最大/最小值，不是布尔比较。",
        "QLIB_GT" | "QLIB_LT" => "逐元素布尔比较；任一输入缺失时按Qlib规则返回0。",
        "GTJA_STD" | "QLIB_STD" => "滚动样本标准差，分母n-1；窗口缺失规则依语义体系。",
        _ => "固定版本Rust算子；输入与窗口要求见参数及空值规则。",
    };
    let parameter_names = parameters
        .iter()
        .enumerate()
        .map(|(i, p)| match p {
            ParameterKind::Series => {
                if i == 0 {
                    "X".to_string()
                } else {
                    format!("X{}", i + 1)
                }
            }
            ParameterKind::Window => "N".to_string(),
            ParameterKind::Probability => "q".to_string(),
            ParameterKind::Number => {
                if id == "GTJA_SMA" {
                    "M".into()
                } else {
                    "value".into()
                }
            }
        })
        .collect();
    let lag = matches!(id.as_str(), "GTJA_DELAY" | "GTJA_DELTA" | "QLIB_REF");
    let window_rule = if lag {
        "按证券观测位置偏移N；不因缺失顺延"
    } else if id == "GTJA_SEQUENCE" {
        "连续观测位置，不按N循环"
    } else if recursive {
        "从首个有限观测开始递归，保留载入历史"
    } else if qlib && rolling {
        "按证券观测行；min_periods=1，Std/Corr至少2个有效值"
    } else if rolling {
        "按证券观测行；完整有限窗口"
    } else if cross {
        "按交易日及成员掩码"
    } else {
        "逐行"
    };
    let missing_rule = if matches!(id.as_str(), "QLIB_GT" | "QLIB_LT") {
        "任一输入缺失时返回0"
    } else if id == "GTJA_IF" {
        "条件缺失为null；只要求被选分支有效"
    } else if id == "GTJA_SEQUENCE" {
        "每证券观测位置从1递增"
    } else if recursive {
        "当前输入缺失输出null，并保留先前状态"
    } else if lag {
        "只检查指定滞后位置；DELTA同时要求当前值有效"
    } else if qlib && rolling {
        "忽略窗口内缺失；Rank/Resi要求当前值有效；IdxMax/IdxMin保留缺失位置语义"
    } else if rolling {
        "完整窗口任一缺失为null；双FILTER的REGBETA按配对条件观测选择"
    } else if cross {
        "只使用当日有效成员；非成员和缺失输出null"
    } else {
        "任一必要输入缺失或非法算术输出null"
    };
    OperatorSpec {
        parameter_names,
        legacy_aliases: if qlib {
            Vec::new()
        } else {
            let name = id.strip_prefix("GTJA_").unwrap();
            let mut aliases = vec![name.to_string()];
            if name == "MEAN" {
                aliases.push("MA".into());
            }
            aliases
        },
        name: id.clone(),
        variadic_predictors: id == "GTJA_REGRESI",
        id,
        category: if cross {
            "横截面"
        } else if recursive {
            "递归"
        } else if rolling {
            "时间序列"
        } else {
            "逐元素"
        }
        .into(),
        parameters,
        output_type: "nullable_float64".into(),
        version: if qlib {
            alpha158::FORMULA_VERSION
        } else {
            crate::FORMULA_VERSION
        }
        .into(),
        window_rule: window_rule.into(),
        missing_rule: missing_rule.into(),
        history_rule: if recursive {
            "依赖种子以来载入历史"
        } else if rolling {
            "嵌套历史依赖加窗口偏移"
        } else {
            "输入依赖的最大历史"
        }
        .into(),
        example,
        description: description.into(),
        kernel,
    }
}
fn invalid(message: impl Into<String>) -> FactorError {
    FactorError::InvalidRequest(message.into())
}
impl Expression {
    pub fn call(operator: &str, args: Vec<Self>) -> Self {
        Self::Call {
            operator: operator.into(),
            args,
        }
    }
    pub fn from_legacy(expr: &FactorExpr) -> Result<Self> {
        match expr {
            FactorExpr::Unified(expr) => Ok((**expr).clone()),
            FactorExpr::Alpha158(expr) => Self::from_qlib(expr),
            FactorExpr::Column(c) => Ok(Self::Input(c.clone())),
            FactorExpr::Const(n) => Ok(Self::Number(*n)),
            FactorExpr::Unsupported { formula } => Err(invalid(formula.clone())),
            _ => {
                let v = serde_json::to_value(expr).map_err(|e| invalid(e.to_string()))?;
                let (tag, data) = v.as_object().unwrap().iter().next().unwrap();
                let op = OperatorRegistry::all()
                    .iter()
                    .find(|s| matches!(&s.kernel,Kernel::Gtja(t) if t == tag))
                    .ok_or_else(|| invalid(format!("unregistered native operator {tag}")))?;
                let raw = if op.parameters.len() == 1 {
                    vec![data.clone()]
                } else {
                    data.as_array().unwrap().clone()
                };
                let mut args = Vec::new();
                for v in raw {
                    if let Some(n) = v.as_f64() {
                        args.push(Self::Number(n));
                    } else if let Some(predictors) = v.as_array() {
                        for p in predictors {
                            args.push(Self::from_legacy(
                                &serde_json::from_value(p.clone())
                                    .map_err(|e| invalid(e.to_string()))?,
                            )?);
                        }
                    } else {
                        args.push(Self::from_legacy(
                            &serde_json::from_value(v).map_err(|e| invalid(e.to_string()))?,
                        )?);
                    }
                }
                Ok(Self::call(&op.id, args))
            }
        }
    }
    fn from_qlib(e: &alpha158::Expr) -> Result<Self> {
        use alpha158::Expr;
        Ok(match e {
            Expr::Unified(e) => (**e).clone(),
            Expr::Input(c) => Self::Input(c.clone()),
            Expr::Constant(v) => Self::Number(*v),
            Expr::Binary { op, left, right } => Self::call(
                &format!("QLIB_{}", op.to_ascii_uppercase()),
                vec![Self::from_qlib(left)?, Self::from_qlib(right)?],
            ),
            Expr::Call { name, args } => Self::call(
                &format!("QLIB_{}", name.to_ascii_uppercase()),
                args.iter().map(Self::from_qlib).collect::<Result<_>>()?,
            ),
        })
    }
    pub fn lower(&self) -> Result<FactorExpr> {
        match self {
            Self::Input(c) => Ok(FactorExpr::Column(c.clone())),
            Self::Number(n) => Ok(FactorExpr::Const(*n)),
            Self::Call { operator, args } => {
                let op = OperatorRegistry::get(operator)
                    .ok_or_else(|| invalid(format!("unknown operator {operator}")))?;
                match &op.kernel {
                    Kernel::Gtja(tag) => {
                        let mut v = Vec::new();
                        for (i, arg) in args.iter().enumerate() {
                            let kind = if op.variadic_predictors {
                                if i == args.len() - 1 {
                                    ParameterKind::Window
                                } else {
                                    ParameterKind::Series
                                }
                            } else {
                                *op.parameters.get(i).ok_or_else(|| invalid("wrong arity"))?
                            };
                            v.push(if kind == ParameterKind::Series {
                                serde_json::to_value(arg.lower()?)
                                    .map_err(|e| invalid(e.to_string()))?
                            } else if let Self::Number(n) = arg {
                                if kind == ParameterKind::Window {
                                    serde_json::json!(*n as usize)
                                } else {
                                    serde_json::json!(n)
                                }
                            } else {
                                return Err(invalid("constant scalar argument required"));
                            });
                        }
                        if op.variadic_predictors {
                            v = vec![
                                v[0].clone(),
                                serde_json::Value::Array(v[1..v.len() - 1].to_vec()),
                                v.last().unwrap().clone(),
                            ];
                        }
                        let data = if op.parameters.len() == 1 {
                            v.into_iter()
                                .next()
                                .ok_or_else(|| invalid("missing argument"))?
                        } else {
                            serde_json::Value::Array(v)
                        };
                        serde_json::from_value(serde_json::json!({tag:data}))
                            .map_err(|e| invalid(e.to_string()))
                    }
                    Kernel::QlibCall(name) => Ok(FactorExpr::Alpha158(alpha158::Expr::Call {
                        name: name.clone(),
                        args: args.iter().map(Self::qlib_arg).collect::<Result<_>>()?,
                    })),
                    Kernel::QlibBinary(name) => Ok(FactorExpr::Alpha158(alpha158::Expr::Binary {
                        op: name.clone(),
                        left: Box::new(args[0].qlib_arg()?),
                        right: Box::new(args[1].qlib_arg()?),
                    })),
                }
            }
        }
    }
    fn qlib_arg(&self) -> Result<alpha158::Expr> {
        match self {
            Self::Number(v) => Ok(alpha158::Expr::Constant(*v)),
            Self::Input(c) => Ok(alpha158::Expr::Input(c.clone())),
            _ => match self.lower()? {
                FactorExpr::Alpha158(e) => Ok(e),
                _ => Ok(alpha158::Expr::Unified(Box::new(self.clone()))),
            },
        }
    }
    fn collect(
        &self,
        depth: usize,
        inputs: &mut BTreeSet<String>,
        versions: &mut BTreeMap<String, String>,
    ) -> Result<()> {
        if depth > 64 {
            return Err(invalid("formula nesting exceeds 64"));
        }
        match self {
            Self::Input(c) => {
                inputs.insert(c.clone());
            }
            Self::Number(n) => {
                if !n.is_finite() {
                    return Err(invalid("numeric constant must be finite"));
                }
            }
            Self::Call { operator, args } => {
                let op = OperatorRegistry::get(operator)
                    .ok_or_else(|| invalid(format!("unknown operator {operator}")))?;
                if (!op.variadic_predictors && args.len() != op.parameters.len())
                    || (op.variadic_predictors && args.len() < 3)
                {
                    return Err(invalid(format!(
                        "{} 参数数量错误，应为{}",
                        operator,
                        op.parameters.len()
                    )));
                }
                for (i, arg) in args.iter().enumerate() {
                    let kind = if op.variadic_predictors {
                        if i == args.len() - 1 {
                            ParameterKind::Window
                        } else {
                            ParameterKind::Series
                        }
                    } else {
                        op.parameters[i]
                    };
                    if kind != ParameterKind::Series {
                        let Self::Number(v) = arg else {
                            return Err(invalid(format!("{operator} 参数{}必须是常量", i + 1)));
                        };
                        if !v.is_finite()
                            || (kind == ParameterKind::Window
                                && (v.fract() != 0. || *v < 1. || *v > 10000.))
                            || (kind == ParameterKind::Probability && !(0. ..=1.).contains(v))
                        {
                            return Err(invalid(format!("{operator} 参数{}超出范围", i + 1)));
                        }
                    }
                    arg.collect(depth + 1, inputs, versions)?;
                }
                if operator == "GTJA_SMA"
                    && let (Self::Number(n), Self::Number(m)) = (&args[1], &args[2])
                    && (*m <= 0. || m > n)
                {
                    return Err(invalid("GTJA_SMA requires 0 < M <= N"));
                }
                versions.insert(operator.clone(), op.version.clone());
            }
        }
        Ok(())
    }
    fn nesting(&self) -> usize {
        match self {
            Self::Call { args, .. } => 1 + args.iter().map(Self::nesting).max().unwrap_or(0),
            _ => 1,
        }
    }
    pub fn compile(&self) -> Result<CompiledExpression> {
        let (mut inputs, mut versions) = (BTreeSet::new(), BTreeMap::new());
        self.collect(0, &mut inputs, &mut versions)?;
        let native = self.lower()?;
        let (lookback, requires_recursive_state) = native.history_requirement();
        Ok(CompiledExpression {
            schema_version: 1,
            expression: self.clone(),
            inputs: inputs.into_iter().collect(),
            lookback,
            requires_recursive_state,
            operator_versions: versions,
        })
    }
}
/// All legacy and user definitions pass through the same registry before native kernels.
pub(crate) fn evaluate(
    expr: &FactorExpr,
    panel: &crate::EvaluationPanel<'_>,
) -> Result<Vec<Option<f64>>> {
    let ast = Expression::from_legacy(expr)?;
    // Legacy definitions may exceed user expression bounds; catalogue ASTs are separately audited.
    crate::evaluate(&ast.lower()?, panel)
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Number(f64),
    Id(String),
    Op(String),
    LParen,
    RParen,
    Comma,
    End,
}
struct Parser {
    tokens: Vec<(Token, usize, usize)>,
    at: usize,
    depth: usize,
}
impl Parser {
    fn error(&self, message: impl Into<String>) -> FormulaError {
        let t = &self.tokens[self.at.min(self.tokens.len() - 1)];
        FormulaError {
            message: message.into(),
            start: t.1,
            end: t.2,
        }
    }
    fn token(&self) -> &Token {
        &self.tokens[self.at].0
    }
    fn take(&mut self) -> Token {
        let t = self.token().clone();
        if t != Token::End {
            self.at += 1;
        }
        t
    }
    fn expr(&mut self, min: u8) -> std::result::Result<Expression, FormulaError> {
        self.depth += 1;
        if self.depth > 24 {
            return Err(self.error("自定义公式嵌套不得超过24层"));
        }
        let expression_start = self.tokens[self.at].1;
        let mut left = match self.take() {
            Token::Number(n) => Expression::Number(n),
            Token::Op(op) if op == "-" || op == "+" => {
                let v = self.expr(7)?;
                if op == "-" {
                    Expression::call("GTJA_NEG", vec![v])
                } else {
                    v
                }
            }
            Token::LParen => {
                let v = self.expr(0)?;
                if self.take() != Token::RParen {
                    return Err(self.error("缺少右括号"));
                }
                v
            }
            Token::Id(id) => {
                if self.token() == &Token::LParen {
                    self.take();
                    let mut args = Vec::new();
                    if self.token() != &Token::RParen {
                        loop {
                            args.push(self.expr(0)?);
                            if self.token() != &Token::Comma {
                                break;
                            }
                            self.take();
                        }
                    }
                    if self.take() != Token::RParen {
                        return Err(self.error("缺少右括号"));
                    }
                    if OperatorRegistry::get(&id).is_none() {
                        return Err(FormulaError {
                            message: format!("未知算子{id}；请选择GTJA_或QLIB_算子"),
                            start: expression_start,
                            end: expression_start + id.len(),
                        });
                    }
                    let call = Expression::call(&id, args);
                    call.compile().map_err(|e| FormulaError {
                        message: e.to_string(),
                        start: expression_start,
                        end: self.tokens[self.at.saturating_sub(1)].2,
                    })?;
                    call
                } else {
                    if ![
                        "OPEN", "HIGH", "LOW", "CLOSE", "VOLUME", "AMOUNT", "VWAP", "RET",
                    ]
                    .contains(&id.as_str())
                    {
                        return Err(self.error(format!("不支持输入{id}；不能引用其他因子")));
                    }
                    Expression::Input(id.to_ascii_lowercase())
                }
            }
            _ => return Err(self.error("需要数值、行情字段或算子调用")),
        };
        while let Token::Op(op) = self.token() {
            let (precedence, name) = match op.as_str() {
                "||" => (1, "GTJA_OR"),
                "&&" => (2, "GTJA_AND"),
                "==" => (3, "GTJA_EQ"),
                ">" => (3, "GTJA_GT"),
                "<" => (3, "GTJA_LT"),
                "+" => (4, "GTJA_ADD"),
                "-" => (4, "GTJA_SUB"),
                "*" => (5, "GTJA_MUL"),
                "/" => (5, "GTJA_DIV"),
                "^" => (6, "GTJA_POW"),
                _ => break,
            };
            if precedence < min {
                break;
            }
            self.take();
            let right = self.expr(if name == "GTJA_POW" {
                precedence
            } else {
                precedence + 1
            })?;
            left = Expression::call(name, vec![left, right]);
        }
        self.depth -= 1;
        Ok(left)
    }
}
pub fn compile_formula(source: &str) -> std::result::Result<CompiledExpression, FormulaError> {
    let error = |message: String, start, end| FormulaError {
        message,
        start,
        end,
    };
    if source.len() > 16384 {
        return Err(error("公式长度不得超过16384字节".into(), 0, source.len()));
    }
    let bytes = source.as_bytes();
    let mut i = 0;
    let mut tokens = Vec::new();
    while i < bytes.len() {
        if bytes[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        let c = bytes[i];
        let t = if c.is_ascii_alphabetic() || c == b'_' {
            i += 1;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            Token::Id(source[start..i].to_ascii_uppercase())
        } else if c.is_ascii_digit() || c == b'.' {
            i += 1;
            while i < bytes.len()
                && (bytes[i].is_ascii_digit()
                    || matches!(bytes[i], b'.' | b'e' | b'E')
                    || (matches!(bytes[i], b'+' | b'-') && matches!(bytes[i - 1], b'e' | b'E')))
            {
                i += 1;
            }
            let n: f64 = source[start..i]
                .parse()
                .map_err(|_| error("非法数值".into(), start, i))?;
            if !n.is_finite() {
                return Err(error("数值必须有限".into(), start, i));
            }
            Token::Number(n)
        } else {
            i += 1;
            match c {
                b'(' => Token::LParen,
                b')' => Token::RParen,
                b',' => Token::Comma,
                b'+' | b'-' | b'*' | b'/' | b'^' | b'>' | b'<' => {
                    Token::Op((c as char).to_string())
                }
                b'=' | b'&' | b'|' if bytes.get(i) == Some(&c) => {
                    i += 1;
                    Token::Op(source[start..i].into())
                }
                _ => return Err(error("不支持的字符或操作符".into(), start, i)),
            }
        };
        tokens.push((t, start, i));
    }
    tokens.push((Token::End, i, i));
    let mut p = Parser {
        tokens,
        at: 0,
        depth: 0,
    };
    let expr = p.expr(0)?;
    if p.token() != &Token::End {
        return Err(p.error("公式存在多余内容"));
    }
    if expr.nesting() > 24 {
        return Err(error("自定义表达式不得超过24层".into(), 0, source.len()));
    }
    expr.compile()
        .map_err(|e| error(e.to_string(), 0, source.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        FactorExecutor, FactorRegistry, FactorRunRequest, InMemoryFactorExecutor, Panel,
        selection::{CustomFactorDefinition, FactorRef},
    };
    use std::sync::{Arc, atomic::AtomicBool};
    #[test]
    fn strict_user_syntax_rejects_repairs_future_windows_and_unknown_references() {
        for formula in [
            "MEAN(CLOSE,20)",
            "GTJA_DELAY(CLOSE,-1)",
            "GTJA_DELAY(CLOSE,1.5)",
            "GTJA_MEAN(CLOSE,20,2)",
            "QLIB_QUANTILE(CLOSE,20,2)",
            "GTJA_SMA(CLOSE,3,4)",
            "GTJA_MEAN(CLOSE,20",
            "custom_other + CLOSE",
            "CLOSE;",
            "CLOSE + 1e999",
        ] {
            let error = compile_formula(formula).expect_err(formula);
            assert!(error.start <= formula.len() && error.end <= formula.len());
        }
        assert!(compile_formula("CLOSE / GTJA_DELAY(CLOSE,20) - 1").is_ok());
        assert!(compile_formula(&format!("{}CLOSE{}", "(".repeat(70), ")".repeat(70))).is_err());
    }
    #[test]
    fn hand_calculated_cross_system_fixture_and_recursive_metadata() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../testdata/custom_operator_golden.json")).unwrap();
        let values: Vec<Option<f64>> = serde_json::from_value(fixture["close"].clone()).unwrap();
        let panel = Panel {
            instrument_id: vec!["a".into(); 5],
            trade_date: (1..=5)
                .map(|d| format!("2024-01-0{d}").parse().unwrap())
                .collect(),
            columns: std::collections::HashMap::from([("close".into(), values)]),
        };
        for case in fixture["cases"].as_array().unwrap() {
            let definition = CustomFactorDefinition::new(
                "custom_fixture".into(),
                "fixture".into(),
                String::new(),
                case["formula"].as_str().unwrap().into(),
                1,
            )
            .unwrap();
            assert_eq!(
                definition.compiled.requires_recursive_state,
                definition.formula.contains("SMA")
            );
            let refs = vec![FactorRef::Custom {
                definition: Box::new(definition),
            }];
            let ids = vec!["custom_fixture".to_string()];
            let registry = FactorRegistry::resolve(Default::default(), &refs, &ids).unwrap();
            let version = registry.selection_version(&ids).unwrap();
            let (tx, _) = std::sync::mpsc::channel();
            let artifact = InMemoryFactorExecutor {
                registry,
                panel: panel.clone(),
            }
            .compute(
                FactorRunRequest {
                    date_range: analytics_core::DateRange {
                        start: panel.trade_date[0],
                        end: panel.trade_date[4],
                    },
                    universe_id: "test".into(),
                    factor_ids: ids,
                    formula_version: version,
                },
                tx,
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap();
            let expected: Vec<Option<f64>> =
                serde_json::from_value(case["expected"].clone()).unwrap();
            for (a, b) in artifact.values["custom_fixture"].iter().zip(expected) {
                match (a, b) {
                    (Some(a), Some(b)) => assert!((a - b).abs() < 1e-12, "{a} {b}"),
                    (None, None) => {}
                    _ => panic!("null mismatch"),
                }
            }
        }
    }
    #[test]
    fn catalogues_lower_to_registered_operators_and_preserve_legacy_ast() {
        for set in [
            analytics_core::research_quality::FactorSet::Gtja191,
            analytics_core::research_quality::FactorSet::Alpha158,
        ] {
            for spec in FactorRegistry::for_set(set).all() {
                if matches!(spec.expression, FactorExpr::Unsupported { .. }) {
                    continue;
                }
                let ast = Expression::from_legacy(&spec.expression).unwrap();
                let lower = ast.lower().unwrap();
                assert_eq!(
                    serde_json::to_value(lower).unwrap(),
                    serde_json::to_value(&spec.expression).unwrap(),
                    "{}",
                    spec.id
                );
            }
        }
    }
}
