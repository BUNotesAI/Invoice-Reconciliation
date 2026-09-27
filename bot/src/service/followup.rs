//! Missing-invoice follow-up (design §5.5 step 3, §8.3): one record per payment, opened when finance approves.
//!
//! A record moves discovered → business (after looking in the history first) → waiting (a way to get the invoice is
//! chosen) → claimed (the invoice arrives in chat and the core matches it); or ends as ignored, found, no_invoice or
//! abandoned. Reminders are keyed by (record, kind, slot) so none goes out twice, and they stop at the deadline.
use serde_json::{Value, json};

use super::{BUSINESS_OFFSET_SECONDS, Service, ServiceError, answer, civil, notice, utc};
use crate::{
    core_client::CoreCommand,
    reconcile::SourceFile,
    report::yuan,
    store::{Batch, State, Work},
};

const OPEN: [&str; 3] = ["discovered", "business", "waiting"];
const NOTE_LIMIT: usize = 200;

/// What a file sent in chat turned out to be for the follow-ups.
pub(super) enum Claim {
    /// It is the invoice of this follow-up: file it into the current batch and close the follow-up.
    Matched { spend: Value, text: String },
    /// It belongs to a follow-up but does not qualify; the chat was answered and nothing else changes.
    Refused,
    /// Not related to any open follow-up: an ordinary upload.
    Unrelated,
}

fn label(spend: &Value) -> String {
    format!(
        "{} {}（{}）",
        spend["merchant"].as_str().unwrap_or_default(),
        yuan(spend["amount_cents"].as_i64().unwrap_or_default()),
        spend["payment_date"].as_str().unwrap_or_default()
    )
}

fn billing(spend: &Value) -> String {
    format!(
        "名称：{}\n税号：{}",
        spend["billing"]["name"].as_str().unwrap_or_default(),
        spend["billing"]["tax_id"].as_str().unwrap_or_default()
    )
}

fn step(spend: &mut Value, action: &str, actor: &str, at: &str, detail: Value) {
    if let Some(history) = spend["history"].as_array_mut() {
        history.push(json!({"action": action, "actor": actor, "at": at, "detail": detail}));
    }
}

/// Local (Asia/Shanghai) calendar date and seconds into that day.
fn local(now: i64) -> (i64, i64) {
    let shifted = now + BUSINESS_OFFSET_SECONDS;
    (shifted.div_euclid(86_400), shifted.rem_euclid(86_400))
}

fn day_text(days: i64) -> String {
    let (year, month, day) = civil(days);
    format!("{year:04}-{month:02}-{day:02}")
}

fn day_of(text: &str) -> Option<i64> {
    let mut parts = text.split('-').map(|p| p.parse::<i64>().ok());
    let (year, month, day) = (parts.next()??, parts.next()??, parts.next()??);
    // Days since 1970-01-01 of a proleptic Gregorian date (inverse of `civil`).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    (civil(days) == (year, month, day)).then_some(days)
}

/// The reminder slots due at `now` for one follow-up: (kind, slot, text).
fn due(spend: &Value, now: i64) -> Vec<(String, String, String)> {
    let (today, seconds) = local(now);
    let Some(deadline) = spend["deadline"].as_str().and_then(day_of) else {
        return Vec::new();
    };
    let what = label(spend);
    if today > deadline {
        return vec![(
            "overdue".into(),
            day_text(deadline),
            format!(
                "「{what}」已过截止日（{}），还没收到发票。请在对账台选择：写无票说明，还是放弃。",
                day_text(deadline)
            ),
        )];
    }
    let created = local(spend["created"].as_i64().unwrap_or_default()).0;
    let mut slots = Vec::new();
    for rule in spend["remind"].as_array().into_iter().flatten() {
        let rule = rule.as_str().unwrap_or_default();
        if let Some(rest) = rule.strip_prefix("weekly_") {
            let (weekday, time) = rest.split_once('_').unwrap_or_default();
            let target = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"]
                .iter()
                .position(|d| *d == weekday);
            let at = time.split_once(':').and_then(|(h, m)| {
                Some(h.parse::<i64>().ok()? * 3600 + m.parse::<i64>().ok()? * 60)
            });
            let (Some(target), Some(at)) = (target, at) else {
                continue;
            };
            // 1970-01-01 was a Thursday (index 3 with Monday = 0).
            let back = ((today + 3).rem_euclid(7) - target as i64).rem_euclid(7);
            let mut day = today - back;
            if back == 0 && seconds < at {
                day -= 7;
            }
            if day > created {
                slots.push((
                    "weekly".to_string(),
                    day_text(day),
                    format!(
                        "漏票提醒：{what} 还没收到发票，截止 {}。开好后把 PDF 发到这里，我会自动认领。",
                        day_text(deadline)
                    ),
                ));
            }
        } else if let Some(days) = rule
            .strip_prefix("deadline_minus_")
            .and_then(|d| d.trim_end_matches('d').parse::<i64>().ok())
        {
            let day = deadline - days;
            if today > day || (today == day && seconds >= 9 * 3600) {
                slots.push((
                    format!("deadline_minus_{days}d"),
                    day_text(day),
                    format!(
                        "漏票提醒：{what} 距截止（{}）还有 {days} 天，还没收到发票。",
                        day_text(deadline)
                    ),
                ));
            }
        }
    }
    slots
}

impl Service {
    /// One follow-up per missing-invoice candidate of the approved reading, and the on-detect notice when the
    /// policy asks for one. Approval writes them in its own unit of work (`write_follow_ups`).
    pub(super) fn plan_follow_ups(
        &self,
        batch: &Batch,
        now: i64,
    ) -> Result<(Vec<Value>, Option<String>), ServiceError> {
        let Some(assessment) = self.assessment(&batch.id)? else {
            return Ok((Vec::new(), None));
        };
        let follow = &assessment.follow_up;
        let at = utc(now);
        let spends: Vec<Value> = assessment
            .report
            .missing_candidates
            .iter()
            .filter(|row| !row.id.is_empty())
            .map(|row| {
                json!({
                    "id": format!("{}-{}", batch.id, row.id), "batch_id": batch.id, "applicant": batch.applicant,
                    "room_id": batch.room_id, "payment_evidence_id": row.payment_evidence_id,
                    "merchant": row.merchant, "amount_cents": row.amount_cents, "payment_date": row.payment_date,
                    "deadline": if row.deadline.is_empty() { follow["deadline"].clone() } else { json!(row.deadline) },
                    "likelihood": row.likelihood, "status": "discovered", "method": null, "note": null,
                    "claimed_invoice": null, "next_batch": null, "found_invoices": [], "billing": follow["billing"],
                    "remind": follow["remind"], "created": now,
                    "history": [{"action": "detected", "actor": "system", "at": at, "detail": {}}],
                })
            })
            .collect();
        let on_detect = follow["remind"]
            .as_array()
            .is_some_and(|rules| rules.iter().any(|r| r == "on_detect"));
        let text = (on_detect && !spends.is_empty()).then(|| {
            format!(
                "还有 {} 笔可能漏票要跟进：{}。截止 {}。请在对账台逐笔确认是不是公务、怎么取票。",
                spends.len(),
                spends.iter().map(label).collect::<Vec<_>>().join("、"),
                follow["deadline"].as_str().unwrap_or_default()
            )
        });
        Ok((spends, text))
    }

    pub(super) fn write_follow_ups(
        work: &Work<'_>,
        spends: &[Value],
        on_detect: bool,
        now: i64,
    ) -> rusqlite::Result<()> {
        for spend in spends {
            work.put_spend(spend)?;
            if on_detect {
                work.record_reminder(
                    spend["id"].as_str().unwrap_or_default(),
                    "on_detect",
                    "detect",
                    now,
                )?;
            }
        }
        Ok(())
    }

    /// Follow-ups of a batch as the desk shows them.
    pub fn follow_ups(&self, batch: &str) -> Result<Vec<Value>, ServiceError> {
        Ok(self
            .with_store(|store| store.spends(batch))?
            .into_iter()
            .map(|spend| {
                json!({"id": spend["id"], "merchant": spend["merchant"], "amount_cents": spend["amount_cents"],
                       "payment_date": spend["payment_date"], "deadline": spend["deadline"], "status": spend["status"],
                       "method": spend["method"], "note": spend["note"], "claimed_invoice": spend["claimed_invoice"],
                       "next_batch": spend["next_batch"], "found_invoices": spend["found_invoices"],
                       "billing": spend["billing"]})
            })
            .collect())
    }

    /// One applicant step on one follow-up, from the desk (step 15).
    pub async fn follow_up(
        &self,
        batch_id: &str,
        user: &str,
        expected: i64,
        spend_id: &str,
        action: &str,
        payload: &Value,
    ) -> Result<(), ServiceError> {
        let _serial = self.serial.lock().await;
        let batch = self.batch(batch_id)?;
        self.applicant_only(&batch, user, expected)?;
        if batch.state != State::Approved {
            return Err(ServiceError::Invalid("batch is not approved".into()));
        }
        let mut spend = self
            .with_store(|store| store.spend(spend_id))?
            .filter(|spend| spend["batch_id"] == batch_id)
            .ok_or(ServiceError::NotFound)?;
        let status = spend["status"].as_str().unwrap_or_default().to_string();
        let open = OPEN.contains(&status.as_str());
        let now = self.now();
        let at = utc(now);
        let mut reply: Option<(String, String)> = None;
        let mut mute = None;
        let next = match (action, status.as_str()) {
            ("business", "discovered") => {
                // 「先找」: the invoice may already be in this batch or an earlier one.
                let history = self.history_all(batch_id).await?;
                let found = self
                    .core(batch_id)
                    .call(
                        CoreCommand::Claim,
                        &json!({"missing_spends": [spend.clone()], "history_snapshot": history, "missing_id": spend_id}),
                    )
                    .await?;
                if found["result"] == "found" {
                    spend["found_invoices"] = found["invoice_nos"].clone();
                    "found"
                } else {
                    "business"
                }
            }
            ("not_business", "discovered") => "ignored",
            ("mute", _) if open => {
                mute = spend["merchant"].as_str().map(String::from);
                "ignored"
            }
            ("method", "business" | "waiting") => {
                let method = payload["method"].as_str().unwrap_or_default();
                let guide = match method {
                    "platform" => "在下单平台的订单里自助申请开票（已开成个人抬头的，申请换开）",
                    "merchant" => "联系商家开票",
                    _ => return Err(ServiceError::Invalid("unknown method".into())),
                };
                spend["method"] = json!(method);
                reply = Some((
                    format!("{spend_id}-method-{method}"),
                    format!(
                        "{}：{guide}，抬头填：\n{}\n开好后把 PDF 发到这里，我会自动认领，归入下一批次。",
                        label(&spend),
                        billing(&spend)
                    ),
                ));
                "waiting"
            }
            ("no_invoice", _) if open => {
                let note = payload["note"].as_str().unwrap_or_default().trim();
                if note.is_empty() || note.chars().count() > NOTE_LIMIT {
                    return Err(ServiceError::Invalid(
                        "note must be 1-200 characters".into(),
                    ));
                }
                spend["note"] = json!(note);
                "no_invoice"
            }
            ("abandon", _) if open => "abandoned",
            _ => {
                return Err(ServiceError::Invalid(format!(
                    "follow-up is {status}; {action} is not possible"
                )));
            }
        };
        spend["status"] = json!(next);
        step(&mut spend, action, user, &at, payload.clone());
        self.with_store(|store| -> rusqlite::Result<()> {
            let work = store.begin()?;
            work.put_spend(&spend)?;
            if let Some(merchant) = &mute {
                work.mute_merchant(user, merchant, now)?;
            }
            if let Some((key, text)) = &reply {
                work.enqueue(batch_id, &batch.room_id, key, &notice(text))?;
            }
            work.commit()
        })?;
        Ok(())
    }

    /// Takes a merchant off this applicant's never-remind list (the desk's 「撤销忽略」).
    pub async fn unmute(
        &self,
        batch_id: &str,
        user: &str,
        expected: i64,
        merchant: &str,
    ) -> Result<(), ServiceError> {
        let _serial = self.serial.lock().await;
        let batch = self.batch(batch_id)?;
        self.applicant_only(&batch, user, expected)?;
        let removed = self.with_store(|store| -> rusqlite::Result<bool> {
            let work = store.begin()?;
            let removed = work.unmute_merchant(user, merchant)?;
            work.commit()?;
            Ok(removed)
        })?;
        if removed {
            Ok(())
        } else {
            Err(ServiceError::NotFound)
        }
    }

    /// Sends every follow-up reminder that is due and not yet sent. Safe to call as often as wanted.
    pub fn remind_follow_ups(&self) -> Result<usize, ServiceError> {
        let now = self.now();
        let open = self.with_store(|store| store.open_spends(None))?;
        let mut sent = 0;
        for spend in open {
            let id = spend["id"].as_str().unwrap_or_default().to_string();
            let batch = spend["batch_id"].as_str().unwrap_or_default().to_string();
            let room = spend["room_id"].as_str().unwrap_or_default().to_string();
            for (kind, slot, text) in due(&spend, now) {
                let fresh = self.with_store(|store| -> rusqlite::Result<bool> {
                    let work = store.begin()?;
                    if !work.record_reminder(&id, &kind, &slot, now)? {
                        return Ok(false);
                    }
                    work.enqueue(
                        &batch,
                        &room,
                        &format!("rm-{id}-{kind}-{slot}"),
                        &notice(&text),
                    )?;
                    work.commit()?;
                    Ok(true)
                })?;
                sent += usize::from(fresh);
            }
        }
        Ok(sent)
    }

    /// A file sent in chat while the sender has open follow-ups: is it one of their invoices?
    pub(super) async fn claim_upload(
        &self,
        batch: &Batch,
        sender: &str,
        room: &str,
        event_id: &str,
        source: &SourceFile,
        display: &str,
    ) -> Result<Claim, ServiceError> {
        let open = self.with_store(|store| store.open_spends(Some(sender)))?;
        if open.is_empty() || source.detected_type != "invoice_pdf" {
            return Ok(Claim::Unrelated);
        }
        let core = self.core(&batch.id);
        let Ok(extracted) = core
            .call(CoreCommand::Extract, &json!({"source_file_id": source.id}))
            .await
        else {
            return Ok(Claim::Unrelated);
        };
        let history = self.history_all(&batch.id).await?;
        let result = core
            .call(
                CoreCommand::Claim,
                &json!({"missing_spends": open, "history_snapshot": history, "invoice": extracted["invoice"]}),
            )
            .await?;
        let target = result["missing_id"]
            .as_str()
            .and_then(|id| open.iter().find(|spend| spend["id"] == id).cloned());
        let now = self.now();
        let at = utc(now);
        match (result["result"].as_str(), target) {
            (Some("match"), Some(mut spend)) => {
                let invoice_no = result["next_batch_item"]["invoice_no"].clone();
                spend["status"] = json!("claimed");
                spend["claimed_invoice"] = invoice_no.clone();
                spend["next_batch"] = json!(batch.id);
                step(
                    &mut spend,
                    "claimed",
                    sender,
                    &at,
                    json!({"invoice_no": invoice_no, "file": display}),
                );
                let text = format!(
                    "收到「{display}」：这是漏票 {} 的发票，已认领，归入本批次（{}）。",
                    label(&spend),
                    batch.period
                );
                Ok(Claim::Matched { spend, text })
            }
            (Some("rejected"), Some(mut spend)) => {
                let reason = result["reason"].as_str().unwrap_or_default();
                let why = match reason {
                    "WRONG_BUYER" => format!(
                        "抬头不是公司，不能报销。请按开票信息申请换开：\n{}",
                        billing(&spend)
                    ),
                    "AMOUNT_MISMATCH" => "金额和这笔支付对不上".to_string(),
                    "DATE_CONFLICT" => "日期和这笔支付对不上".to_string(),
                    "DUPLICATE_INVOICE" => "这张票已经报销过".to_string(),
                    _ => "不符合认领条件".to_string(),
                };
                let text = format!(
                    "「{display}」看起来是漏票 {} 的发票，但{why}。没有认领，也没有收进批次。",
                    label(&spend)
                );
                step(
                    &mut spend,
                    "claim_refused",
                    sender,
                    &at,
                    json!({"reason": reason, "file": display}),
                );
                self.with_store(|store| -> rusqlite::Result<()> {
                    let work = store.begin()?;
                    work.put_spend(&spend)?;
                    answer(&work, &batch.id, room, event_id, &[notice(&text)])?;
                    work.commit()
                })?;
                Ok(Claim::Refused)
            }
            _ => Ok(Claim::Unrelated),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spend(created: i64) -> Value {
        json!({"merchant": "悦途酒店", "amount_cents": 48000, "payment_date": "2026-10-24", "deadline": "2026-11-30",
               "remind": ["on_detect", "weekly_mon_09:00", "deadline_minus_3d"], "created": created})
    }

    // 2026-11-02 is a Monday; 01:00Z is 09:00 in Shanghai.
    const MONDAY_0900: i64 = 1_793_581_200;

    #[test]
    fn dates_round_trip() {
        for text in ["2026-11-30", "2026-12-01", "2027-02-28", "2028-02-29"] {
            assert_eq!(day_text(day_of(text).unwrap()), text);
        }
        assert_eq!(day_text(local(MONDAY_0900).0), "2026-11-02");
    }

    #[test]
    fn weekly_slot_is_the_latest_monday_after_detection() {
        let created = MONDAY_0900 - 3 * 86_400;
        assert!(
            due(&spend(created), MONDAY_0900 - 60).is_empty(),
            "08:59 is too early"
        );
        let slots = due(&spend(created), MONDAY_0900);
        assert_eq!(slots.len(), 1);
        assert_eq!(
            (slots[0].0.as_str(), slots[0].1.as_str()),
            ("weekly", "2026-11-02")
        );
        // Wednesday of the same week still names Monday's slot, so it is not sent again.
        assert_eq!(
            due(&spend(created), MONDAY_0900 + 2 * 86_400)[0].1,
            "2026-11-02"
        );
        // Detected on that Monday itself: nothing until the next one.
        assert!(due(&spend(MONDAY_0900 - 3600), MONDAY_0900 + 86_400).is_empty());
    }

    #[test]
    fn deadline_reminders_and_overdue() {
        let created = MONDAY_0900;
        // 2026-11-27 09:00 Shanghai is three days before the deadline.
        let minus3 = MONDAY_0900 + 25 * 86_400;
        let kinds = |now| {
            due(&spend(created), now)
                .into_iter()
                .map(|(kind, slot, _)| format!("{kind}@{slot}"))
                .collect::<Vec<_>>()
        };
        assert_eq!(kinds(minus3 - 60), ["weekly@2026-11-23"]);
        assert_eq!(
            kinds(minus3),
            ["weekly@2026-11-23", "deadline_minus_3d@2026-11-27"]
        );
        // The day after the deadline only the overdue prompt remains.
        assert_eq!(kinds(MONDAY_0900 + 29 * 86_400), ["overdue@2026-11-30"]);
    }
}
