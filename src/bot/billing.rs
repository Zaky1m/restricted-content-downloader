//! Premium by Telegram Stars: the invoice, the pre-checkout check and the payment itself.

use std::sync::Arc;

use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use grammers_client::{tl, update::CallbackQuery};

use super::{Bot, Who};
use crate::plans::{PLANS, Plan, extend_premium, plan_from_payment};

fn plan_named(name: &str) -> Option<Plan> {
    PLANS.iter().find(|p| p.name == name).copied()
}

/// `premium_<plan>_<user id>_<unix time>`, exactly as the Python bot wrote it.
fn parse_payload(payload: &str) -> Option<(Plan, i64)> {
    let mut parts = payload.splitn(4, '_');
    if parts.next()? != "premium" {
        return None;
    }
    let plan = plan_named(parts.next()?)?;
    let user = parts.next()?.parse::<i64>().ok()?;
    Some((plan, user))
}

/// The buy buttons: sends a Stars invoice for the chosen plan.
pub async fn buy(bot: &Arc<Bot>, query: &CallbackQuery, who: &Who, data: &str) -> Result<()> {
    let lang = bot.lang(who.id).await;
    let Some(plan) = plan_named(data.trim_start_matches("buy_premium_")) else {
        query
            .answer()
            .alert(bot.t(&lang, "payments.invalid_plan_popup", &[]))
            .send()
            .await
            .ok();
        return Ok(());
    };
    let Some(peer) = bot.user_ref(who.id).await else {
        return Ok(());
    };
    let payload = format!(
        "premium_{}_{}_{}",
        plan.name,
        who.id,
        Utc::now().timestamp()
    );
    let invoice = tl::types::Invoice {
        test: false,
        name_requested: false,
        phone_requested: false,
        email_requested: false,
        shipping_address_requested: false,
        flexible: false,
        phone_to_provider: false,
        email_to_provider: false,
        recurring: false,
        currency: "XTR".into(),
        prices: vec![
            tl::types::LabeledPrice {
                label: bot.t(&lang, "payments.total_label", &[]),
                amount: plan.stars,
            }
            .into(),
        ],
        max_tip_amount: None,
        suggested_tip_amounts: None,
        terms_url: None,
        subscription_period: None,
    };
    let media = tl::types::InputMediaInvoice {
        title: bot.t(&lang, &format!("payments.{}.title", plan.name), &[]),
        description: bot.t(&lang, &format!("payments.{}.description", plan.name), &[]),
        photo: None,
        invoice: invoice.into(),
        payload: payload.into_bytes(),
        provider: None,
        provider_data: tl::types::DataJson { data: "{}".into() }.into(),
        start_param: None,
        extended_media: None,
    };
    let sent = bot
        .client
        .send_message(
            peer,
            grammers_client::message::InputMessage::new().media(media),
        )
        .await;
    match sent {
        Ok(_) => {
            query.answer().send().await.ok();
        }
        Err(error) => {
            tracing::error!(user = who.id, %error, "invoice failed");
            query
                .answer()
                .alert(bot.t(&lang, "payments.invoice_failed_popup", &[]))
                .send()
                .await
                .ok();
        }
    }
    Ok(())
}

/// Telegram asks whether to accept the payment; anything that does not match what was invoiced is refused.
pub async fn precheckout(
    bot: &Arc<Bot>,
    update: &tl::types::UpdateBotPrecheckoutQuery,
) -> Result<()> {
    let payload = String::from_utf8_lossy(&update.payload).into_owned();
    let verdict = match parse_payload(&payload) {
        Some((plan, user))
            if user == update.user_id
                && update.currency == "XTR"
                && update.total_amount == plan.stars =>
        {
            Ok(())
        }
        _ => Err("This payment does not match the invoice"),
    };
    let request = tl::functions::messages::SetBotPrecheckoutResults {
        success: verdict.is_ok(),
        query_id: update.query_id,
        error: verdict.err().map(str::to_string),
    };
    bot.client.invoke(&request).await?;
    Ok(())
}

/// The service message that carries a finished payment.
pub async fn paid(
    bot: &Arc<Bot>,
    user_id: i64,
    user_mention: &str,
    payment: &tl::types::MessageActionPaymentSentMe,
) -> Result<()> {
    let lang = bot.lang(user_id).await;
    let payload = String::from_utf8_lossy(&payment.payload).into_owned();
    let tl::enums::PaymentCharge::Charge(charge) = &payment.charge;
    let Some(plan) = plan_from_payment(&payload, payment.total_amount) else {
        bot.send_to(user_id, &bot.t(&lang, "payments.unrecognized", &[]), None)
            .await?;
        return Ok(());
    };
    let mut tx = bot.pg.begin().await?;
    let fresh = sqlx::query(
        "INSERT INTO payments (charge_id, user_id, plan, amount, currency, payload) VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (charge_id) DO NOTHING",
    )
    .bind(&charge.id)
    .bind(user_id)
    .bind(plan.name)
    .bind(payment.total_amount)
    .bind(&payment.currency)
    .bind(&payload)
    .execute(&mut *tx)
    .await?;
    if fresh.rows_affected() == 0 {
        tracing::warn!(user_id, "payment already recorded, ignoring the repeat");
        return Ok(());
    }
    let current: Option<(Option<DateTime<Utc>>,)> =
        sqlx::query_as("SELECT premium_expiry_date FROM users WHERE user_id = $1 FOR UPDATE")
            .bind(user_id)
            .fetch_optional(&mut *tx)
            .await?;
    let now = Utc::now();
    let expiry_ts = extend_premium(
        current.and_then(|c| c.0).map(|d| d.timestamp()),
        now.timestamp(),
        plan.days * 86_400,
    );
    let expiry =
        DateTime::<Utc>::from_timestamp(expiry_ts, 0).unwrap_or(now + Duration::days(plan.days));
    sqlx::query("UPDATE users SET is_premium = TRUE, premium_expiry_date = $2, premium_added_by_admin = FALSE WHERE user_id = $1")
        .bind(user_id)
        .bind(expiry)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    let plan_name = bot.t(&lang, &format!("plans.{}", plan.name), &[]);
    let date = expiry.format("%Y-%m-%d").to_string();
    bot.send_to(
        user_id,
        &bot.t(
            &lang,
            "payments.completed",
            &[("plan_name", &plan_name), ("expiry_date", &date)],
        ),
        None,
    )
    .await
    .ok();
    let amount = payment.total_amount.to_string();
    bot.notify_owners("admin.payments.new_subscription", |owner_lang| {
        vec![
            ("user_name".to_string(), user_mention.to_string()),
            ("user_id".to_string(), user_id.to_string()),
            (
                "plan_name".to_string(),
                bot.t(owner_lang, &format!("plans.{}", plan.name), &[]),
            ),
            ("amount".to_string(), amount.clone()),
        ]
    })
    .await;
    Ok(())
}
