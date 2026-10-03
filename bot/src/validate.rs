//! Agent output is data to check, never instructions. Parsing is strict; facts come only from code.
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::LazyLock,
};

use regex::Regex;
use serde::{Deserialize, de::DeserializeOwned};

/// Words that claim a process state. Only code may state these (design §6.4 rule 4).
pub const STATUS_WORDS: &[&str] = &[
    "通过",
    "已提交",
    "已付款",
    "已支付",
    "付款完成",
    "终审",
    "核验",
    "已审批",
    "审批完成",
    "批准",
    "已入账",
    "已报销",
    "合规",
    "已确认",
    "已发布",
    "已交财务",
];
pub const EXPLANATION_LIMIT: usize = 300;

pub type Violations = Vec<String>;

/// Accepts one optional markdown fence around the JSON, as observed from real models; nothing else.
pub fn unfence(text: &str) -> &str {
    let trimmed = text.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    let Some(body_start) = rest.find('\n') else {
        return trimmed;
    };
    let body = &rest[body_start + 1..];
    body.trim_end()
        .strip_suffix("```")
        .map(str::trim)
        .unwrap_or(trimmed)
}

pub fn parse_strict<T: DeserializeOwned>(text: &str) -> Result<T, Violations> {
    serde_json::from_str(unfence(text))
        .map_err(|error| vec![format!("output is not the required JSON object: {error}")])
}

fn text_field(violations: &mut Violations, name: &str, value: &str, limit: usize) {
    if value.chars().count() > limit || value.chars().any(|c| c.is_control() && c != '\n') {
        violations.push(format!(
            "field {name} is too long or has control characters"
        ));
    }
}

// --- A1: reading an invoice or an order screenshot ---------------------------------------------

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct InvoiceReading {
    pub invoice_no: String,
    pub issue_date: String,
    pub amount_cents: String,
    pub amount_upper: String,
    pub buyer_name: String,
    pub buyer_tax_id: String,
    pub seller_name: String,
    pub project: String,
    pub remark: String,
    #[serde(default)]
    pub order_ref: Option<String>,
}

/// Structure only; the core checks formats and cross-checks amounts, and the result stays a candidate.
pub fn invoice_reading(text: &str) -> Result<InvoiceReading, Violations> {
    let reading: InvoiceReading = parse_strict(text)?;
    let mut violations = Vec::new();
    for (name, value) in [
        ("invoice_no", &reading.invoice_no),
        ("issue_date", &reading.issue_date),
        ("amount_cents", &reading.amount_cents),
        ("amount_upper", &reading.amount_upper),
        ("buyer_name", &reading.buyer_name),
        ("buyer_tax_id", &reading.buyer_tax_id),
        ("seller_name", &reading.seller_name),
        ("project", &reading.project),
    ] {
        text_field(&mut violations, name, value, 512);
    }
    text_field(&mut violations, "remark", &reading.remark, 4096);
    if yuan_to_cents(&reading.amount_cents).is_none() {
        violations.push("amount_cents must be a yuan amount such as 386.00".into());
    }
    if violations.is_empty() {
        Ok(reading)
    } else {
        Err(violations)
    }
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ScreenshotReading {
    pub merchant: String,
    pub amount: String,
    pub service_date: String,
    pub order_ref: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScreenshotFields {
    pub merchant: String,
    pub amount_cents: i64,
    pub service_date: String,
    pub order_ref: String,
}

pub fn screenshot_reading(text: &str) -> Result<ScreenshotFields, Violations> {
    let reading: ScreenshotReading = parse_strict(text)?;
    let mut violations = Vec::new();
    text_field(&mut violations, "merchant", &reading.merchant, 255);
    text_field(&mut violations, "order_ref", &reading.order_ref, 128);
    let amount = yuan_to_cents(&reading.amount);
    if amount.is_none() {
        violations.push("amount must be a yuan amount such as 20.00".into());
    }
    if !DATE.is_match(&reading.service_date) {
        violations.push("service_date must be YYYY-MM-DD".into());
    }
    if reading.merchant.trim().is_empty() || reading.order_ref.trim().is_empty() {
        violations.push("merchant and order_ref are required".into());
    }
    match (violations.is_empty(), amount) {
        (true, Some(amount_cents)) => Ok(ScreenshotFields {
            merchant: reading.merchant.trim().into(),
            amount_cents,
            service_date: reading.service_date,
            order_ref: reading.order_ref.trim().into(),
        }),
        _ => Err(violations),
    }
}

// --- A3: category and short name suggestion ------------------------------------------------------

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Classification {
    pub category: String,
    pub short_name: String,
    pub rationale: String,
}

pub fn classification(
    text: &str,
    categories: &BTreeSet<String>,
) -> Result<Classification, Violations> {
    let value: Classification = parse_strict(text)?;
    let mut violations = Vec::new();
    if !categories.contains(&value.category) {
        violations.push(format!(
            "category must be one of: {}",
            categories.iter().cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    if !label_name(&value.short_name) {
        violations.push(
            "short_name must be 1-20 Chinese or Latin letters, digits, · or brackets, not starting or ending with a digit"
                .into(),
        );
    }
    // The rationale is free text shown as an agent note; it may not carry numbers or state claims.
    violations.extend(explanation_rules(&value.rationale, &[], &BTreeMap::new()));
    if violations.is_empty() {
        Ok(value)
    } else {
        Err(violations)
    }
}

/// Mirrors the core's label rule: names sit between `_` separators in attachment names.
pub fn label_name(value: &str) -> bool {
    static LABEL: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^[\p{Han}A-Za-z][\p{Han}A-Za-z0-9·（）()]{0,19}$").unwrap());
    LABEL.is_match(value) && !value.chars().last().is_some_and(|c| c.is_ascii_digit())
}

// --- A2: ranking several payment candidates ------------------------------------------------------

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Ranking {
    pub choice: String,
    pub reason: String,
}

/// The choice must be one of the offered candidates; occupancy and amount are re-checked by the core.
pub fn ranking(text: &str, candidates: &BTreeSet<String>) -> Result<Ranking, Violations> {
    let value: Ranking = parse_strict(text)?;
    let mut violations = explanation_rules(&value.reason, &[], &BTreeMap::new());
    if !candidates.contains(&value.choice) {
        violations.push("choice must be one of the candidate ids".into());
    }
    if violations.is_empty() {
        Ok(value)
    } else {
        Err(violations)
    }
}

// --- A4: report explanations ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FactValue {
    Amount(i64),
    Date(String),
    Count(i64),
    Text(String),
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Explanation {
    pub item_id: String,
    pub fact_refs: Vec<String>,
    pub explanation: String,
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Explanations {
    pub items: Vec<Explanation>,
}

/// `allowed` maps each requested item to the fact ids it may cite.
pub fn explanations(
    text: &str,
    allowed: &BTreeMap<String, BTreeSet<String>>,
    facts: &BTreeMap<String, FactValue>,
) -> Result<Explanations, Violations> {
    let value: Explanations = parse_strict(text)?;
    let mut violations = Vec::new();
    let mut seen = BTreeSet::new();
    for item in &value.items {
        let Some(permitted) = allowed.get(&item.item_id) else {
            violations.push(format!("item {} was not requested", item.item_id));
            continue;
        };
        if !seen.insert(item.item_id.clone()) {
            violations.push(format!("item {} appears twice", item.item_id));
        }
        for reference in &item.fact_refs {
            if !permitted.contains(reference) {
                violations.push(format!(
                    "item {}: fact {reference} does not exist for this item",
                    item.item_id
                ));
            }
        }
        let refs: Vec<&str> = item
            .fact_refs
            .iter()
            .map(String::as_str)
            .filter(|id| permitted.contains(*id))
            .collect();
        for problem in explanation_rules(&item.explanation, &refs, facts) {
            violations.push(format!("item {}: {problem}", item.item_id));
        }
    }
    if violations.is_empty() {
        Ok(value)
    } else {
        Err(violations)
    }
}

/// Design §6.4: numbers must equal the cited facts, in citation order, with no extras; no state claims.
pub fn explanation_rules(
    text: &str,
    refs: &[&str],
    facts: &BTreeMap<String, FactValue>,
) -> Violations {
    let mut violations = Vec::new();
    if text.trim().is_empty() || text.chars().count() > EXPLANATION_LIMIT {
        violations.push(format!(
            "explanation must be 1-{EXPLANATION_LIMIT} characters"
        ));
    }
    for word in STATUS_WORDS {
        if text.contains(word) {
            violations.push(format!(
                "explanation claims a process state ({word}); only code states that"
            ));
        }
    }
    let numeric: Vec<&FactValue> = refs
        .iter()
        .filter_map(|id| facts.get(*id))
        .filter(|value| !matches!(value, FactValue::Text(_)))
        .collect();
    let tokens = numeric_tokens(text);
    if tokens.len() != numeric.len() {
        violations.push(format!(
            "explanation has {} numbers but cites {} numeric facts; every number must be a cited fact, in order",
            tokens.len(),
            numeric.len()
        ));
    } else {
        for (index, (token, fact)) in tokens.iter().zip(&numeric).enumerate() {
            if !token.matches(fact) {
                violations.push(format!(
                    "number {} does not equal cited fact {}",
                    index + 1,
                    index + 1
                ));
            }
        }
    }
    violations
}

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    Date {
        year: Option<i32>,
        month: u32,
        day: u32,
    },
    Time,
    /// Decimal value scaled by 100, and whether it had a fraction.
    Number {
        hundredths: i128,
        fractional: bool,
    },
}

impl Token {
    fn matches(&self, fact: &FactValue) -> bool {
        match (self, fact) {
            (Token::Number { hundredths, .. }, FactValue::Amount(cents)) => {
                *hundredths == *cents as i128
            }
            (
                Token::Number {
                    hundredths,
                    fractional: false,
                },
                FactValue::Count(count),
            ) => *hundredths == *count as i128 * 100,
            (Token::Date { year, month, day }, FactValue::Date(iso)) => {
                let parts: Vec<&str> = iso.split('-').collect();
                parts.len() == 3
                    && year.is_none_or(|y| parts[0].parse() == Ok(y))
                    && parts[1].parse() == Ok(*month)
                    && parts[2].parse() == Ok(*day)
            }
            _ => false,
        }
    }
}

static DATE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[0-9]{4}-[0-9]{2}-[0-9]{2}$").unwrap());
static TOKENS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?P<iso>(?P<iy>[0-9]{4})[-/.年](?P<im>[0-9]{1,2})[-/.月](?P<id>[0-9]{1,2})日?)",
        r"|(?P<md>(?P<mm>[0-9]{1,2})(?:月|/)(?P<mdd>[0-9]{1,2})[日号]?)",
        r"|(?P<time>[0-9]{1,2}:[0-9]{2})",
        r"|(?P<num>(?:[0-9]{1,3}(?:,[0-9]{3})+|[0-9]+)(?:\.(?P<frac>[0-9]+))?)",
        r"|(?P<cn>[零〇一二两三四五六七八九十百千万亿]+)(?P<unit>张|晚|笔|天|次|个|元|块|单|趟)?",
    ))
    .unwrap()
});

/// Full-width digits and punctuation become ASCII, formal (大写) numerals become plain ones,
/// so every way of writing a number reaches the same token rules.
pub fn normalize_numerals(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '０'..='９' => char::from_u32(c as u32 - '０' as u32 + '0' as u32).unwrap_or(c),
            '．' => '.',
            '：' => ':',
            '／' => '/',
            '－' => '-',
            '壹' => '一',
            '贰' | '貳' => '二',
            '叁' | '參' => '三',
            '肆' => '四',
            '伍' => '五',
            '陆' | '陸' => '六',
            '柒' => '七',
            '捌' => '八',
            '玖' => '九',
            '拾' => '十',
            '佰' => '百',
            '仟' => '千',
            '萬' => '万',
            '億' => '亿',
            other => other,
        })
        .collect()
}

/// Every number a reader would see: dates, times, Arabic, full-width and Chinese numerals, with or without a unit.
/// It errs towards counting: a false number only sends the step to rules mode, a missed one lets a fabrication through.
pub fn numeric_tokens(text: &str) -> Vec<Token> {
    let text = normalize_numerals(text);
    let mut tokens = Vec::new();
    for capture in TOKENS.captures_iter(&text) {
        if capture.name("iso").is_some() {
            tokens.push(Token::Date {
                year: capture["iy"].parse().ok(),
                month: capture["im"].parse().unwrap_or(0),
                day: capture["id"].parse().unwrap_or(0),
            });
        } else if capture.name("md").is_some() {
            tokens.push(Token::Date {
                year: None,
                month: capture["mm"].parse().unwrap_or(0),
                day: capture["mdd"].parse().unwrap_or(0),
            });
        } else if capture.name("time").is_some() {
            tokens.push(Token::Time);
        } else if let Some(number) = capture.name("num") {
            let plain = number.as_str().replace(',', "");
            let (whole, fraction) = plain.split_once('.').unwrap_or((&plain, ""));
            let hundredths = if fraction.len() > 2 {
                // More than two decimals can never equal a cent amount; keep it unmatched.
                -1
            } else {
                let whole: i128 = whole.parse().unwrap_or(-1);
                let fraction: i128 = format!("{fraction:0<2}").parse().unwrap_or(0);
                if whole < 0 {
                    -1
                } else {
                    whole * 100 + fraction
                }
            };
            tokens.push(Token::Number {
                hundredths,
                fractional: !fraction.is_empty(),
            });
        } else if let Some(chinese) = capture.name("cn") {
            // A lone 一 without a unit is ordinary prose (一下, 进一步), not a quantity.
            if chinese.as_str() == "一" && capture.name("unit").is_none() {
                continue;
            }
            let value = chinese_number(chinese.as_str())
                .map(|value| value as i128 * 100)
                .unwrap_or(-1);
            tokens.push(Token::Number {
                hundredths: value,
                fractional: false,
            });
        }
    }
    tokens
}

fn chinese_number(text: &str) -> Option<i64> {
    let digit = |c: char| match c {
        '零' | '〇' => Some(0),
        '两' => Some(2),
        _ => "一二三四五六七八九"
            .find(c)
            .map(|index| (index / 3) as i64 + 1),
    };
    // Without any place character the run is read digit by digit: 一五六〇 is 1560.
    if !text.chars().any(|c| "十百千万亿".contains(c)) {
        return text.chars().try_fold(0i64, |value, c| {
            value.checked_mul(10)?.checked_add(digit(c)?)
        });
    }
    let (mut total, mut section, mut current) = (0i64, 0i64, 0i64);
    for c in text.chars() {
        match c {
            '十' | '百' | '千' => {
                let place = match c {
                    '十' => 10,
                    '百' => 100,
                    _ => 1000,
                };
                section += current.max(1) * place;
                current = 0;
            }
            '万' | '亿' => {
                let place = if c == '万' { 10_000 } else { 100_000_000 };
                let group = (section + current).max(1);
                total = if c == '亿' {
                    (total + group) * place
                } else {
                    total + group * place
                };
                section = 0;
                current = 0;
            }
            _ => current = digit(c)?,
        }
    }
    Some(total + section + current)
}

/// Strict yuan text to integer cents: "386", "386.0", "386.00", optional leading ¥ or ￥.
pub fn yuan_to_cents(text: &str) -> Option<i64> {
    static YUAN: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^[¥￥]?(0|[1-9][0-9]{0,11})(?:\.([0-9]{1,2}))?$").unwrap());
    let capture = YUAN.captures(text.trim())?;
    let whole: i64 = capture[1].parse().ok()?;
    let fraction: i64 = capture
        .get(2)
        .map_or(Ok(0), |m| format!("{:0<2}", m.as_str()).parse())
        .ok()?;
    whole.checked_mul(100)?.checked_add(fraction)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> BTreeMap<String, FactValue> {
        BTreeMap::from([
            ("F08.amount".into(), FactValue::Amount(156000)),
            ("F08.nights".into(), FactValue::Count(3)),
            ("F08.limit".into(), FactValue::Amount(150000)),
            ("F08.check_in".into(), FactValue::Date("2026-10-06".into())),
            (
                "F08.seller".into(),
                FactValue::Text("北京燕园会展酒店有限公司".into()),
            ),
        ])
    }

    fn check(text: &str, refs: &[&str]) -> Violations {
        explanation_rules(text, refs, &facts())
    }

    #[test]
    fn legitimate_explanation_passes() {
        let refs = [
            "F08.seller",
            "F08.check_in",
            "F08.nights",
            "F08.amount",
            "F08.limit",
        ];
        assert!(
            check(
                "10月6日入住，住了三晚，房费 1,560.00 元，高于 1500 元的标准，需要申请人补充说明。",
                &refs
            )
            .is_empty()
        );
        assert!(check("住宿发票高于每晚标准，需要补充超标原因。", &["F08.seller"]).is_empty());
    }

    #[test]
    fn fabricated_amount_is_rejected() {
        assert!(
            !check(
                "房费 1,660.00 元，高于 1500 元。",
                &["F08.amount", "F08.limit"]
            )
            .is_empty()
        );
    }

    #[test]
    fn fabricated_date_is_rejected() {
        assert!(!check("10月7日入住。", &["F08.check_in"]).is_empty());
        assert!(!check("2026-10-07 入住。", &["F08.check_in"]).is_empty());
    }

    #[test]
    fn fabricated_count_is_rejected() {
        assert!(!check("住了四晚。", &["F08.nights"]).is_empty());
        assert!(!check("住了 4 晚。", &["F08.nights"]).is_empty());
    }

    #[test]
    fn uncited_number_is_rejected() {
        assert!(!check("共 3 晚，房费 1560 元。", &["F08.nights"]).is_empty());
        assert!(!check("大约 12:30 入住。", &[]).is_empty());
    }

    #[test]
    fn swapped_amount_roles_are_rejected() {
        // Both values are real facts, but each number must be the fact cited at that position.
        assert!(!check("标准 1560 元，实际 1500 元。", &["F08.limit", "F08.amount"]).is_empty());
        assert!(check("标准 1500 元，实际 1560 元。", &["F08.limit", "F08.amount"]).is_empty());
    }

    #[test]
    fn state_claims_are_rejected() {
        for text in [
            "终审通过。",
            "财务已付款。",
            "这张票已核验。",
            "已提交财务。",
        ] {
            assert!(!check(text, &[]).is_empty(), "{text}");
        }
    }

    #[test]
    fn explanations_check_items_and_refs() {
        let allowed = BTreeMap::from([("item-F08".to_string(), facts().keys().cloned().collect())]);
        let good = r#"{"items":[{"item_id":"item-F08","fact_refs":["F08.amount"],"explanation":"房费 1560 元。"}]}"#;
        assert!(explanations(good, &allowed, &facts()).is_ok());
        let unknown_item =
            r#"{"items":[{"item_id":"item-F01","fact_refs":[],"explanation":"说明。"}]}"#;
        assert!(explanations(unknown_item, &allowed, &facts()).is_err());
        let unknown_fact = r#"{"items":[{"item_id":"item-F08","fact_refs":["F01.amount"],"explanation":"说明。"}]}"#;
        assert!(explanations(unknown_fact, &allowed, &facts()).is_err());
        let extra_field = r#"{"items":[],"status":"ok"}"#;
        assert!(explanations(extra_field, &allowed, &facts()).is_err());
        let duplicate = r#"{"items":[],"items":[]}"#;
        assert!(explanations(duplicate, &allowed, &facts()).is_err());
    }

    #[test]
    fn numbers_in_any_script_are_counted() {
        // Fabricated amounts in full-width, formal and Chinese numerals, with and without units.
        for text in [
            "房费 １６６０ 元。",
            "房费伍佰元。",
            "房费壹仟伍佰陆拾元，另加三千元。",
            "共一万",
            "合计一六六〇",
            "房费一千六百",
        ] {
            assert!(!check(text, &["F08.amount"]).is_empty(), "{text}");
        }
        // The same numbers written correctly still pass.
        for text in [
            "房费 １５６０ 元。",
            "房费壹仟伍佰陆拾元。",
            "房费一千五百六十元。",
            "房费一五六〇元。",
        ] {
            assert!(check(text, &["F08.amount"]).is_empty(), "{text}");
        }
        assert!(check("住了三晚，需要看一下。", &["F08.nights"]).is_empty());
        assert!(!check("两晚。", &["F08.nights"]).is_empty());
    }

    #[test]
    fn chinese_numerals_parse() {
        for (text, value) in [
            ("一千五百六十", 1560),
            ("三千", 3000),
            ("一万", 10000),
            ("十二", 12),
            ("两百", 200),
            ("一亿零五", 100000005),
            ("一五六〇", 1560),
            ("三", 3),
        ] {
            assert_eq!(chinese_number(text), Some(value), "{text}");
        }
    }

    #[test]
    fn fenced_json_is_accepted_once() {
        assert_eq!(unfence("```json\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(unfence("{\"a\":1}"), "{\"a\":1}");
    }

    #[test]
    fn classification_is_bounded_by_policy() {
        let categories = BTreeSet::from(["餐饮".to_string(), "打车".to_string()]);
        assert!(
            classification(
                r#"{"category":"餐饮","short_name":"潮海居","rationale":"餐饮服务类发票。"}"#,
                &categories
            )
            .is_ok()
        );
        assert!(
            classification(
                r#"{"category":"娱乐","short_name":"潮海居","rationale":"餐饮。"}"#,
                &categories
            )
            .is_err()
        );
        assert!(
            classification(
                r#"{"category":"餐饮","short_name":"../x","rationale":"餐饮。"}"#,
                &categories
            )
            .is_err()
        );
        assert!(
            classification(
                r#"{"category":"餐饮","short_name":"潮海居","rationale":"金额 386 元已核验。"}"#,
                &categories
            )
            .is_err()
        );
        for bad in ["瑞幸_130.8", "瑞幸8", "8瑞幸", "瑞 幸", ""] {
            let text =
                format!(r#"{{"category":"餐饮","short_name":"{bad}","rationale":"餐饮。"}}"#);
            assert!(classification(&text, &categories).is_err(), "{bad}");
        }
    }

    #[test]
    fn readings_are_structural_only() {
        let reading = r#"{"invoice_no":"x","issue_date":"2026-10-19","amount_cents":"386.00","amount_upper":"叁佰捌拾陆圆整",
            "buyer_name":"示例","buyer_tax_id":"","seller_name":"某店","project":"餐费","remark":""}"#;
        assert!(invoice_reading(reading).is_ok());
        assert!(invoice_reading(&reading.replace("386.00", "三百")).is_err());
        let shot = r#"{"merchant":"滴滴出行","amount":"20.00","service_date":"2026-10-17","order_ref":"DD-1"}"#;
        assert_eq!(screenshot_reading(shot).unwrap().amount_cents, 2000);
        assert!(screenshot_reading(&shot.replace("2026-10-17", "10月17日")).is_err());
    }

    #[test]
    fn yuan_parsing() {
        assert_eq!(yuan_to_cents("386.00"), Some(38600));
        assert_eq!(yuan_to_cents("￥20"), Some(2000));
        assert_eq!(yuan_to_cents("30.8"), Some(3080));
        for bad in ["-1", "01", "1.234", "1e3", "", "1,000"] {
            assert_eq!(yuan_to_cents(bad), None, "{bad}");
        }
    }
}
