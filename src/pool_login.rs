//! `rcd pool-login`: signs in the download pool accounts from the terminal, the same way `/login` does in the bot.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use grammers_client::{InvocationError, SignInError, client::PasswordToken, session::Session, tl};
use grammers_session::{SessionData, storages::MemorySession};

use crate::{
    config::Config,
    db,
    sessions::{self, Account, Kind},
    telegram,
};

/// Reads one line; with `hidden` the typed text is not shown (for the two-step password).
async fn ask(prompt: &str, hidden: bool) -> Result<String> {
    let prompt = prompt.to_string();
    tokio::task::spawn_blocking(move || {
        use std::io::Write;
        print!("{prompt}");
        std::io::stdout().flush().ok();
        let stty = |arg: &str| {
            let _ = std::process::Command::new("stty")
                .arg(arg)
                .stdin(std::process::Stdio::inherit())
                .status();
        };
        if hidden {
            stty("-echo");
        }
        let mut line = String::new();
        let read = std::io::stdin().read_line(&mut line);
        if hidden {
            stty("echo");
            println!();
        }
        read?;
        Ok(line.trim().to_string())
    })
    .await?
}

/// The phone's digits, which also name the pool session.
pub fn label_for(phone: &str) -> String {
    phone.chars().filter(char::is_ascii_digit).collect()
}

/// "App", "Sms", "Call"...: where Telegram says the code went.
fn delivery(kind: &tl::enums::auth::SentCodeType) -> String {
    let text = format!("{kind:?}");
    text.split('(').next().unwrap_or("").to_string()
}

pub async fn run(config: Config, phones: Vec<String>) -> Result<()> {
    let pg = db::connect(&config.database_url).await?;
    db::migrate(&pg).await?;
    let phones: Vec<String> = if phones.is_empty() {
        config.pool_phones.clone()
    } else {
        // `--phone +1 202 555 0123` arrives as several words: they form one number.
        vec![phones.join("")]
    };
    if phones.is_empty() {
        bail!("POOL_PHONES is empty and no --phone was given");
    }
    for phone in phones {
        let label = label_for(&phone);
        if label.len() < 7 {
            eprintln!("{phone}: not a phone number, skipped");
            continue;
        }
        // Sessions imported from the Python bot may carry the old `20_` file name.
        let mut known = false;
        for existing in [label.clone(), format!("20_{label}")] {
            if sessions::get(&pg, &config.session_key, Kind::Pool, &existing)
                .await?
                .is_some()
            {
                println!("+{label}: already stored as pool account {existing}, skipped");
                known = true;
            }
        }
        if known {
            continue;
        }
        println!("Signing in +{label}");
        if let Err(error) = login_one(&config, &pg, &format!("+{label}"), &label).await {
            eprintln!("+{label}: failed: {error:#}");
        }
    }
    Ok(())
}

async fn login_one(config: &Config, pg: &sqlx::PgPool, phone: &str, label: &str) -> Result<()> {
    let api_id = config.api_id as u32;
    let memory = Arc::new(MemorySession::from(SessionData::default()));
    let client = telegram::connect_with(memory.clone(), api_id).client;
    let request = tl::functions::auth::SendCode {
        phone_number: phone.to_string(),
        api_id: config.api_id,
        api_hash: config.api_hash.clone(),
        settings: tl::types::CodeSettings {
            allow_flashcall: false,
            current_number: false,
            allow_app_hash: false,
            allow_missed_call: false,
            allow_firebase: false,
            logout_tokens: None,
            token: None,
            app_sandbox: None,
            unknown_number: false,
        }
        .into(),
    };
    let mut sent = client.invoke(&request).await;
    if let Err(InvocationError::Rpc(rpc)) = &sent
        && rpc.code == 303
        && let Some(dc) = rpc.value
    {
        memory.set_home_dc_id(dc as i32).await?;
        sent = client.invoke(&request).await;
    }
    let code = match sent.context("Telegram refused to send a login code")? {
        tl::enums::auth::SentCode::Code(code) => code,
        other => {
            let answer = format!("{other:?}");
            bail!(
                "Telegram did not send a code ({})",
                answer.split('(').next().unwrap_or("")
            );
        }
    };
    let next = code
        .next_type
        .as_ref()
        .map(|t| format!(", next would be {t:?}"))
        .unwrap_or_default();
    println!(
        "Code sent via {}{next}. In the Telegram app it arrives in the chat named Telegram",
        delivery(&code.r#type)
    );
    let entered = ask("Code: ", false).await?;
    let signed = client
        .invoke(&tl::functions::auth::SignIn {
            phone_number: phone.to_string(),
            phone_code_hash: code.phone_code_hash.clone(),
            phone_code: Some(entered),
            email_verification: None,
        })
        .await;
    let user_id = match signed {
        Ok(tl::enums::auth::Authorization::Authorization(auth)) => match auth.user {
            tl::enums::User::User(user) => user.id,
            tl::enums::User::Empty(_) => bail!("Telegram returned no account"),
        },
        Ok(_) => bail!("this number has no Telegram account"),
        Err(error) if error.is("SESSION_PASSWORD_NEEDED") => {
            let info = client
                .invoke(&tl::functions::account::GetPassword {})
                .await
                .context("could not start the two-step check")?;
            let info: tl::types::account::Password = info.into();
            if let Some(hint) = info.hint.as_deref().filter(|h| !h.is_empty()) {
                println!("Password hint: {hint}");
            }
            let password = ask("Two-step password (hidden): ", true).await?;
            match client
                .check_password(PasswordToken::new(info), password.as_bytes())
                .await
            {
                Ok(user) => user.id().bare_id().unwrap_or(0),
                Err(SignInError::InvalidPassword(_)) => bail!("wrong two-step password"),
                Err(error) => bail!("two-step check failed: {error}"),
            }
        }
        Err(error) if error.is("PHONE_CODE_*") => bail!("wrong or expired code"),
        Err(error) => return Err(error).context("sign-in failed"),
    };
    let exported = telegram::export(&memory, api_id, user_id as u64, false)
        .context("the new session could not be read back")?;
    let account = Account {
        kind: Kind::Pool,
        label,
        owner_id: None,
        phone: Some(phone),
    };
    sessions::put(pg, &config.session_key, &account, &exported).await?;
    println!("{phone}: stored as pool account {label} (user {user_id})");
    client.disconnect();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::label_for;

    #[test]
    fn labels_are_the_digits() {
        assert_eq!(label_for("+20 100 123 4567"), "201001234567");
        assert_eq!(label_for("+1 202 555 0123"), "12025550123");
    }
}
