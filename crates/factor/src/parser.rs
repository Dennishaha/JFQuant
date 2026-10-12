use super::FactorExpr;
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Number(f64),
    Ident(String),
    Op(String),
    LParen,
    RParen,
    Comma,
    Question,
    Colon,
    End,
}

#[derive(Debug)]
pub struct ParseError(pub String);

impl std::fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ParseError {}

pub fn parse_formula(source: &str) -> Result<FactorExpr, ParseError> {
    let normalized = normalize_source(source);
    let mut parser = Parser {
        tokens: tokenize(&normalized)?,
        index: 0,
    };
    let result = parser.parse_conditional()?;
    if !matches!(parser.peek(), Token::End) {
        return Err(ParseError(format!("unexpected token {:?}", parser.peek())));
    }
    Ok(result)
}

pub fn input_columns(source: &str) -> Vec<String> {
    let functions: BTreeSet<&str> = [
        "ABS",
        "CORR",
        "COUNT",
        "COV",
        "DECAYLINEAR",
        "DELAY",
        "DELTA",
        "FILTER",
        "HIGHDAY",
        "LOG",
        "LOWDAY",
        "MA",
        "MAX",
        "MEAN",
        "MIN",
        "PROD",
        "RANK",
        "REGBETA",
        "REGRESI",
        "SIGN",
        "SMA",
        "STD",
        "SUM",
        "SUMIF",
        "TSMAX",
        "TSMIN",
        "TSRANK",
        "WMA",
        "SEQUENCE",
        "SUMAC",
    ]
    .into_iter()
    .collect();
    let mut columns = BTreeSet::new();
    for token in tokenize(&normalize_source(source)).unwrap_or_default() {
        if let Token::Ident(name) = token
            && !functions.contains(name.as_str())
            && name != "TRUE"
            && name != "FALSE"
        {
            columns.insert(name.to_ascii_lowercase());
        }
    }
    columns.into_iter().collect()
}

pub fn estimate_lookback(source: &str) -> u32 {
    tokenize(&normalize_source(source))
        .unwrap_or_default()
        .into_iter()
        .filter_map(|token| match token {
            Token::Number(value) if value.fract() == 0.0 && (1.0..=1000.0).contains(&value) => {
                Some(value as u32)
            }
            _ => None,
        })
        .max()
        .unwrap_or(0)
}

fn normalize_source(source: &str) -> String {
    let normalized = source
        .replace(['–', '—'], "-")
        .replace('≥', ">=")
        .replace('≤', "<=")
        .replace('，', ",")
        .replace("./", "/")
        .replace(".*", "*")
        .replace("SMEAN", "SMA")
        .replace("COVIANCE", "COV")
        .replace("DELAT", "DELTA")
        .replace("SM A", "SMA")
        .replace("CLOS E", "CLOSE")
        .replace("OP EN", "OPEN")
        .replace("D ELAY", "DELAY")
        .replace("B ANCH", "BANCH")
        .replace("STD(CLOSE:20)", "STD(CLOSE,20)")
        .replace("?STD(CLOSE,20),0", "?STD(CLOSE,20):0")
        .replace("SIGN(DELTA(CLOSE, 7)) :", "SIGN(DELTA(CLOSE, 7))) :")
        .replace(
            "SUM((CLOSE/DELAY(CLOSE,1),20)^2,20)",
            "SUM((CLOSE/DELAY(CLOSE,1))^2,20)",
        )
        .replace("VOLUME)))", "VOLUME))");
    let balance = normalized.chars().fold(0isize, |balance, character| {
        balance
            + match character {
                '(' => 1,
                ')' => -1,
                _ => 0,
            }
    });
    if balance > 0 {
        format!("{normalized}{}", ")".repeat(balance as usize))
    } else {
        normalized
    }
}

fn tokenize(source: &str) -> Result<Vec<Token>, ParseError> {
    let chars: Vec<char> = source.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c.is_ascii_digit()
            || (c == '.' && chars.get(i + 1).is_some_and(|next| next.is_ascii_digit()))
        {
            let start = i;
            i += 1;
            while i < chars.len()
                && (chars[i].is_ascii_digit() || matches!(chars[i], '.' | 'e' | 'E' | '+' | '-'))
            {
                if (chars[i] == '+' || chars[i] == '-')
                    && !matches!(chars.get(i.wrapping_sub(1)), Some('e' | 'E'))
                {
                    break;
                }
                i += 1;
            }
            let text: String = chars[start..i].iter().collect();
            tokens.push(Token::Number(
                text.parse()
                    .map_err(|_| ParseError(format!("invalid number {text}")))?,
            ));
            continue;
        }
        if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            i += 1;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let name = chars[start..i]
                .iter()
                .collect::<String>()
                .to_ascii_uppercase();
            if name == "OR" {
                tokens.push(Token::Op("||".into()));
            } else if name == "AND" {
                tokens.push(Token::Op("&&".into()));
            } else {
                tokens.push(Token::Ident(name));
            }
            continue;
        }
        match c {
            '(' => tokens.push(Token::LParen),
            ')' => tokens.push(Token::RParen),
            ',' => tokens.push(Token::Comma),
            '?' => tokens.push(Token::Question),
            ':' => tokens.push(Token::Colon),
            '+' | '-' | '*' | '/' | '^' => tokens.push(Token::Op(c.to_string())),
            '<' | '>' | '=' | '!' | '&' | '|' => {
                let mut op = c.to_string();
                if let Some(next) = chars.get(i + 1)
                    && matches!(
                        (*next, c),
                        ('=', '<' | '>' | '=' | '!') | ('&', '&') | ('|', '|')
                    )
                {
                    op.push(*next);
                    i += 1;
                }
                if op == "&" {
                    op = "&&".into();
                }
                if op == "|" {
                    op = "||".into();
                }
                tokens.push(Token::Op(op));
            }
            '\\' => tokens.push(Token::Op("/".into())),
            ';' => {}
            _ => return Err(ParseError(format!("unsupported character {c:?}"))),
        }
        i += 1;
    }
    tokens.push(Token::End);
    Ok(tokens)
}

struct Parser {
    tokens: Vec<Token>,
    index: usize,
}

impl Parser {
    fn peek(&self) -> &Token {
        &self.tokens[self.index]
    }
    fn next(&mut self) -> Token {
        let token = self.tokens[self.index].clone();
        self.index += 1;
        token
    }
    fn eat(&mut self, token: &Token) -> bool {
        if self.peek() == token {
            self.index += 1;
            true
        } else {
            false
        }
    }
    fn expect(&mut self, token: &Token) -> Result<(), ParseError> {
        self.eat(token)
            .then_some(())
            .ok_or_else(|| ParseError(format!("expected {token:?}, got {:?}", self.peek())))
    }

    fn parse_conditional(&mut self) -> Result<FactorExpr, ParseError> {
        let condition = self.parse_or()?;
        if self.eat(&Token::Question) {
            let yes = self.parse_conditional()?;
            self.expect(&Token::Colon)?;
            let no = self.parse_conditional()?;
            Ok(FactorExpr::If(
                Box::new(condition),
                Box::new(yes),
                Box::new(no),
            ))
        } else {
            Ok(condition)
        }
    }
    fn parse_or(&mut self) -> Result<FactorExpr, ParseError> {
        self.parse_chain(Self::parse_and, &["||"], FactorExpr::Or)
    }
    fn parse_and(&mut self) -> Result<FactorExpr, ParseError> {
        self.parse_chain(Self::parse_compare, &["&&"], FactorExpr::And)
    }
    fn parse_chain(
        &mut self,
        child: fn(&mut Self) -> Result<FactorExpr, ParseError>,
        ops: &[&str],
        make: fn(Box<FactorExpr>, Box<FactorExpr>) -> FactorExpr,
    ) -> Result<FactorExpr, ParseError> {
        let mut left = child(self)?;
        loop {
            let matched = matches!(self.peek(), Token::Op(op) if ops.iter().any(|candidate| *candidate == op));
            if !matched {
                break;
            }
            self.next();
            let right = child(self)?;
            left = make(Box::new(left), Box::new(right));
        }
        Ok(left)
    }
    fn parse_compare(&mut self) -> Result<FactorExpr, ParseError> {
        let mut left = self.parse_add()?;
        if let Token::Op(op) = self.peek().clone() {
            let constructor = match op.as_str() {
                "==" | "=" => Some(0),
                "!=" => Some(1),
                ">" => Some(2),
                ">=" => Some(3),
                "<" => Some(4),
                "<=" => Some(5),
                _ => None,
            };
            if let Some(kind) = constructor {
                self.next();
                let right = self.parse_add()?;
                left = match kind {
                    0 => FactorExpr::Eq(Box::new(left), Box::new(right)),
                    1 => FactorExpr::Neg(Box::new(FactorExpr::Eq(Box::new(left), Box::new(right)))),
                    2 => FactorExpr::Gt(Box::new(left), Box::new(right)),
                    3 => FactorExpr::Or(
                        Box::new(FactorExpr::Gt(
                            Box::new(left.clone()),
                            Box::new(right.clone()),
                        )),
                        Box::new(FactorExpr::Eq(Box::new(left), Box::new(right))),
                    ),
                    4 => FactorExpr::Lt(Box::new(left), Box::new(right)),
                    5 => FactorExpr::Or(
                        Box::new(FactorExpr::Lt(
                            Box::new(left.clone()),
                            Box::new(right.clone()),
                        )),
                        Box::new(FactorExpr::Eq(Box::new(left), Box::new(right))),
                    ),
                    _ => unreachable!(),
                };
            }
        }
        Ok(left)
    }
    fn parse_add(&mut self) -> Result<FactorExpr, ParseError> {
        let mut left = self.parse_mul()?;
        loop {
            let op = match self.peek() {
                Token::Op(op) if op == "+" || op == "-" => Some(op.clone()),
                _ => None,
            };
            let Some(op) = op else {
                if matches!(self.peek(), Token::LParen | Token::Ident(_)) {
                    let right = self.parse_pow()?;
                    left = FactorExpr::Mul(Box::new(left), Box::new(right));
                    continue;
                }
                break;
            };
            self.next();
            let right = self.parse_mul()?;
            left = if op == "+" {
                FactorExpr::Add(Box::new(left), Box::new(right))
            } else {
                FactorExpr::Sub(Box::new(left), Box::new(right))
            };
        }
        Ok(left)
    }
    fn parse_mul(&mut self) -> Result<FactorExpr, ParseError> {
        let mut left = self.parse_pow()?;
        loop {
            let op = match self.peek() {
                Token::Op(op) if op == "*" || op == "/" => Some(op.clone()),
                _ => None,
            };
            let Some(op) = op else { break };
            self.next();
            let right = self.parse_pow()?;
            left = if op == "*" {
                FactorExpr::Mul(Box::new(left), Box::new(right))
            } else {
                FactorExpr::Div(Box::new(left), Box::new(right))
            };
        }
        Ok(left)
    }
    fn parse_pow(&mut self) -> Result<FactorExpr, ParseError> {
        let mut left = self.parse_unary()?;
        if self.eat(&Token::Op("^".into())) {
            let right = self.parse_pow()?;
            if let FactorExpr::Const(power) = right {
                left = FactorExpr::Pow(Box::new(left), power);
            } else {
                left = FactorExpr::PowExpr(Box::new(left), Box::new(right));
            }
        }
        Ok(left)
    }
    fn parse_unary(&mut self) -> Result<FactorExpr, ParseError> {
        if self.eat(&Token::Op("-".into())) {
            Ok(FactorExpr::Neg(Box::new(self.parse_unary()?)))
        } else if self.eat(&Token::Op("+".into())) {
            self.parse_unary()
        } else {
            self.parse_primary()
        }
    }
    fn parse_primary(&mut self) -> Result<FactorExpr, ParseError> {
        match self.next() {
            Token::Number(number) => Ok(FactorExpr::Const(number)),
            Token::Ident(name) => {
                if !self.eat(&Token::LParen) {
                    return Ok(match name.as_str() {
                        "TRUE" => FactorExpr::Const(1.0),
                        "FALSE" => FactorExpr::Const(0.0),
                        "SEQUENCE" => FactorExpr::Sequence(1),
                        _ => FactorExpr::Column(name.to_ascii_lowercase()),
                    });
                }
                let mut args = Vec::new();
                if !self.eat(&Token::RParen) {
                    loop {
                        args.push(self.parse_conditional()?);
                        if self.eat(&Token::RParen) {
                            break;
                        }
                        self.expect(&Token::Comma)?;
                    }
                }
                self.function(&name, args)
            }
            Token::LParen => {
                let value = self.parse_conditional()?;
                self.expect(&Token::RParen)?;
                Ok(value)
            }
            token => Err(ParseError(format!("unexpected token {token:?}"))),
        }
    }
    fn function(&self, name: &str, args: Vec<FactorExpr>) -> Result<FactorExpr, ParseError> {
        let unary = |f: fn(Box<FactorExpr>) -> FactorExpr| {
            if args.len() == 1 {
                Ok(f(Box::new(args[0].clone())))
            } else {
                Err(ParseError(format!("{name} expects one argument")))
            }
        };
        let rolling = |f: fn(Box<FactorExpr>, usize) -> FactorExpr| {
            if args.len() == 2 {
                Ok(f(Box::new(args[0].clone()), constant_usize(&args[1])?))
            } else {
                Err(ParseError(format!("{name} expects two arguments")))
            }
        };
        match name {
            "ABS" => unary(FactorExpr::Abs),
            "LOG" => unary(FactorExpr::Log),
            "SIGN" => unary(FactorExpr::Sign),
            "RANK" => {
                if args.len() == 1 || args.len() == 2 {
                    Ok(FactorExpr::Rank(Box::new(args[0].clone())))
                } else {
                    Err(ParseError("RANK expects one or two arguments".into()))
                }
            }
            "DELAY" => {
                if args.len() == 1 {
                    Ok(FactorExpr::Delay(Box::new(args[0].clone()), 1))
                } else {
                    rolling(FactorExpr::Delay)
                }
            }
            "DELTA" => {
                if args.len() == 1 {
                    Ok(FactorExpr::Delta(Box::new(args[0].clone()), 1))
                } else {
                    rolling(FactorExpr::Delta)
                }
            }
            "SUM" => {
                if args.len() == 1 {
                    Ok(FactorExpr::CumulativeSum(Box::new(args[0].clone())))
                } else {
                    rolling(FactorExpr::TsSum)
                }
            }
            "MEAN" | "MA" => {
                if args.len() == 1 {
                    Ok(args[0].clone())
                } else {
                    rolling(FactorExpr::TsMean)
                }
            }
            "STD" => {
                if args.len() == 1 {
                    Ok(args[0].clone())
                } else {
                    rolling(FactorExpr::TsStd)
                }
            }
            "TSMAX" => rolling(FactorExpr::TsMax),
            "TSMIN" => rolling(FactorExpr::TsMin),
            "TSRANK" => rolling(FactorExpr::TsRank),
            "DECAYLINEAR" => rolling(FactorExpr::DecayLinear),
            "COUNT" => rolling(FactorExpr::Count),
            "PROD" => rolling(FactorExpr::Product),
            "WMA" => rolling(FactorExpr::WeightedMean),
            "CORR" => binary_rolling(&args, name, FactorExpr::Corr),
            "COV" => binary_rolling(&args, name, FactorExpr::Cov),
            "REGBETA" => {
                if args.len() == 2 {
                    let window = match &args[1] {
                        FactorExpr::Sequence(w) => *w,
                        _ => 1,
                    };
                    Ok(FactorExpr::RegBeta(
                        Box::new(args[0].clone()),
                        Box::new(args[1].clone()),
                        window,
                    ))
                } else {
                    binary_rolling(&args, name, FactorExpr::RegBeta)
                }
            }
            "REGRESI" => {
                if args.len() >= 3 {
                    let window = constant_usize(args.last().expect("checked len"))?;
                    Ok(FactorExpr::RegResi(
                        Box::new(args[0].clone()),
                        Box::new(args[1].clone()),
                        window,
                    ))
                } else {
                    Err(ParseError(
                        "REGRESI expects at least three arguments".into(),
                    ))
                }
            }
            "SMA" => {
                if args.len() == 3 {
                    Ok(FactorExpr::Ema(
                        Box::new(args[0].clone()),
                        constant_usize(&args[1])?,
                        constant_f64(&args[2])?,
                    ))
                } else if args.len() == 2 {
                    Ok(FactorExpr::TsMean(
                        Box::new(args[0].clone()),
                        constant_usize(&args[1])?,
                    ))
                } else {
                    Err(ParseError("SMA expects three arguments".into()))
                }
            }
            "MAX" => {
                if args.len() == 1 {
                    Ok(args[0].clone())
                } else {
                    minmax(&args, name, true)
                }
            }
            "MIN" => {
                if args.len() == 1 {
                    Ok(args[0].clone())
                } else {
                    minmax(&args, name, false)
                }
            }
            "HIGHDAY" => rolling(FactorExpr::HighDay),
            "LOWDAY" => rolling(FactorExpr::LowDay),
            "SUMIF" => {
                if args.len() == 3 {
                    Ok(FactorExpr::SumIf(
                        Box::new(args[0].clone()),
                        Box::new(args[2].clone()),
                        constant_usize(&args[1])?,
                    ))
                } else {
                    Err(ParseError("SUMIF expects three arguments".into()))
                }
            }
            "FILTER" => {
                if args.len() == 2 {
                    Ok(FactorExpr::Filter(
                        Box::new(args[0].clone()),
                        Box::new(args[1].clone()),
                    ))
                } else {
                    Err(ParseError("FILTER expects two arguments".into()))
                }
            }
            "SUMAC" => {
                if args.len() == 1 {
                    Ok(FactorExpr::CumulativeSum(Box::new(args[0].clone())))
                } else {
                    Err(ParseError("SUMAC expects one argument".into()))
                }
            }
            "SEQUENCE" => Ok(FactorExpr::Sequence(
                args.first().map(constant_usize).transpose()?.unwrap_or(1),
            )),
            _ => Err(ParseError(format!("unsupported function {name}"))),
        }
    }
}

fn constant_usize(expr: &FactorExpr) -> Result<usize, ParseError> {
    match expr {
        FactorExpr::Const(value) if *value >= 0.0 => Ok(*value as usize),
        _ => Err(ParseError("window must be constant".into())),
    }
}
fn constant_f64(expr: &FactorExpr) -> Result<f64, ParseError> {
    match expr {
        FactorExpr::Const(value) => Ok(*value),
        _ => Err(ParseError("parameter must be constant".into())),
    }
}
fn binary_rolling(
    args: &[FactorExpr],
    name: &str,
    make: fn(Box<FactorExpr>, Box<FactorExpr>, usize) -> FactorExpr,
) -> Result<FactorExpr, ParseError> {
    if args.len() == 3 {
        Ok(make(
            Box::new(args[0].clone()),
            Box::new(args[1].clone()),
            constant_usize(&args[2])?,
        ))
    } else {
        Err(ParseError(format!("{name} expects three arguments")))
    }
}
fn minmax(args: &[FactorExpr], name: &str, is_max: bool) -> Result<FactorExpr, ParseError> {
    if args.len() != 2 {
        return Err(ParseError(format!("{name} expects two arguments")));
    }
    if let Ok(window) = constant_usize(&args[1]) {
        Ok(if is_max {
            FactorExpr::TsMax(Box::new(args[0].clone()), window)
        } else {
            FactorExpr::TsMin(Box::new(args[0].clone()), window)
        })
    } else {
        Ok(if is_max {
            FactorExpr::Max(Box::new(args[0].clone()), Box::new(args[1].clone()))
        } else {
            FactorExpr::Min(Box::new(args[0].clone()), Box::new(args[1].clone()))
        })
    }
}
