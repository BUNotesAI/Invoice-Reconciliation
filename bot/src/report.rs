//! The report card. Every fact sentence is a code template; agent text appears only as a labelled note.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::reconcile::{Bucket, MissingRow};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemView {
    pub item_id: String,
    pub file: String,
    pub seller: Option<String>,
    pub short_name: Option<String>,
    pub amount_cents: Option<i64>,
    pub issue_date: Option<String>,
    pub service_date: Option<String>,
    pub category: Option<String>,
    pub nights: Option<i64>,
    pub check_in: Option<String>,
    pub disposition: String,
    pub reasons: Vec<String>,
    pub notes: Vec<String>,
    pub explanation: Option<String>,
}

pub struct Input {
    pub period: String,
    pub applicant: String,
    pub agent_available: bool,
    pub rules_steps: Vec<String>,
    pub summary: BTreeMap<String, Bucket>,
    pub items: Vec<ItemView>,
    pub missing: Vec<MissingRow>,
    pub not_included: u32,
    pub ignored_merchants: Vec<String>,
    pub unsupported: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub period: String,
    pub applicant: String,
    pub rules_mode: bool,
    pub rules_steps: Vec<String>,
    pub automatic: Bucket,
    pub needs_decision: Bucket,
    pub rejected: Bucket,
    pub items: Vec<ItemView>,
    pub missing_candidates: Vec<MissingRow>,
    pub not_included_count: u32,
    pub ignored_merchants: Vec<String>,
    pub unsupported_files: Vec<String>,
    pub text: String,
}

/// Rule-template wording for each review reason; used in the card and as the rules-mode note.
pub fn reason_text(code: &str) -> &'static str {
    match code {
        "EVIDENCE_MISSING" => "没有找到对应的支付记录，请补一张订单截图",
        "OVER_LIMIT" => "金额超过政策标准，请补充超标原因，由财务审核",
        "FACT_UNCONFIRMED" => "这张票是图片，读数需要你对照原图确认",
        "VISION_REQUIRED" => "这张票是图片且没能读出，请发原始 PDF 或手动核对",
        "REPLACEMENT_REQUIRES_DECISION" => "这是重开票，原票已作废且未付款，请确认按替换入账",
        "HISTORY_ALREADY_PAID" => "原票已付款，只能更换凭证，不能再报销一次",
        "HISTORY_UNKNOWN" => "原票状态不明，需要人工确认",
        "WRONG_BUYER" => "发票抬头不是公司，请换开",
        "DUPLICATE_INVOICE" => "这张票已经报销过",
        "FULL_REFUND" => "对应支付已全额退款",
        "PARTIAL_REFUND" => "对应支付有部分退款，基础版转人工",
        "PAYMENT_STATUS_UNKNOWN" => "支付状态不明，需要人工确认",
        "LINK_CONFLICT" => "同一笔支付或行程被两张票争用，请人工指定",
        "MULTIPLE_CANDIDATES" => "有多笔可能对应的支付，请人工指定",
        "DATE_CONFLICT" => "几份证据给出的日期不一致，请确认",
        "SERVICE_DATE_MISSING" => "定不出消费日期，请补充证据",
        "OUT_OF_WINDOW" => "消费日期不在本期可报范围内",
        "STAY_PERIOD_MISSING" => "住宿票缺入住离店日期，请补充",
        "CATEGORY_UNKNOWN" | "CATEGORY_CONFLICT" => "报销类别需要你确认",
        _ => "需要人工确认",
    }
}

/// Text for HTML contexts (Matrix formatted_body, desk pages): escapes the five significant characters.
pub fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

pub fn yuan(cents: i64) -> String {
    let whole = (cents / 100).to_string();
    let mut grouped = String::new();
    for (index, digit) in whole.chars().enumerate() {
        if index > 0 && (whole.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    format!("¥{grouped}.{:02}", cents % 100)
}

fn label(item: &ItemView) -> String {
    let name = item
        .short_name
        .clone()
        .or_else(|| item.seller.clone())
        .unwrap_or_else(|| item.file.clone());
    match item.amount_cents {
        Some(cents) => format!("{} {}", name, yuan(cents)),
        None => format!("{name}（金额待读取）"),
    }
}

pub fn build(input: Input) -> Report {
    let bucket = |key: &str| input.summary.get(key).copied().unwrap_or_default();
    let automatic = bucket("automatic");
    let needs = bucket("needs_decision");
    let rejected = bucket("rejected");
    let accepted = bucket("accepted");
    let rules_mode = !input.agent_available || !input.rules_steps.is_empty();
    let mut lines = vec![format!("对账报告 · {} · {}", input.period, input.applicant)];
    if rules_mode {
        lines.push("（规则模式：部分说明由规则模板生成，判断交由你确认）".into());
    }
    lines.push(format!(
        "自动匹配 {} 张，合计 {}",
        automatic.count + accepted.count,
        yuan(automatic.cents + accepted.cents)
    ));
    lines.push(format!(
        "待你判断 {} 张，合计 {}",
        needs.count,
        yuan(needs.cents)
    ));
    lines.push(format!(
        "拒收 {} 张，合计 {}",
        rejected.count,
        yuan(rejected.cents)
    ));
    let pending: Vec<&ItemView> = input
        .items
        .iter()
        .filter(|item| item.disposition == "needs_decision")
        .collect();
    if !pending.is_empty() {
        lines.push("待判断：".into());
        for item in &pending {
            let reasons: Vec<&str> = item.reasons.iter().map(|code| reason_text(code)).collect();
            lines.push(format!("· {}：{}", label(item), reasons.join("；")));
            if let Some(note) = &item.explanation {
                lines.push(format!("  Agent 说明：{note}"));
            }
        }
    }
    let refused: Vec<&ItemView> = input
        .items
        .iter()
        .filter(|item| item.disposition == "rejected")
        .collect();
    if !refused.is_empty() {
        lines.push("拒收：".into());
        for item in refused {
            let reasons: Vec<&str> = item.reasons.iter().map(|code| reason_text(code)).collect();
            lines.push(format!("· {}：{}", label(item), reasons.join("；")));
        }
    }
    let unread = input
        .items
        .iter()
        .filter(|item| item.amount_cents.is_none())
        .count();
    if unread > 0 && !input.missing.is_empty() {
        // Without a reading the unread invoice's payment cannot be reserved, so it may show up below.
        lines.push(format!(
            "有 {unread} 张图片票未读出，下面的可能漏票里可能含有它们的支付"
        ));
    }
    if !input.missing.is_empty() {
        let parts: Vec<String> = input
            .missing
            .iter()
            .map(|row| {
                format!(
                    "{} {}（{}）",
                    row.merchant,
                    yuan(row.amount_cents),
                    row.payment_date
                )
            })
            .collect();
        lines.push(format!(
            "可能漏票 {} 笔：{}",
            input.missing.len(),
            parts.join("、")
        ));
    }
    lines.push(format!(
        "暂未纳入的消费 {} 笔，可展开查看",
        input.not_included
    ));
    if !input.ignored_merchants.is_empty() {
        lines.push(format!(
            "已忽略商户：{}",
            input.ignored_merchants.join("、")
        ));
    }
    if !input.unsupported.is_empty() {
        lines.push(format!("不支持的文件：{}", input.unsupported.join("、")));
    }
    Report {
        period: input.period,
        applicant: input.applicant,
        rules_mode,
        rules_steps: input.rules_steps,
        automatic: Bucket {
            count: automatic.count + accepted.count,
            cents: automatic.cents + accepted.cents,
        },
        needs_decision: needs,
        rejected,
        items: input.items,
        missing_candidates: input.missing,
        not_included_count: input.not_included,
        ignored_merchants: input.ignored_merchants,
        unsupported_files: input.unsupported,
        text: lines.join("\n") + "\n",
    }
}

#[cfg(test)]
mod tests {
    use super::yuan;

    #[test]
    fn money_is_grouped_and_exact() {
        assert_eq!(yuan(147540), "¥1,475.40");
        assert_eq!(yuan(5), "¥0.05");
        assert_eq!(yuan(100000000), "¥1,000,000.00");
    }
}
