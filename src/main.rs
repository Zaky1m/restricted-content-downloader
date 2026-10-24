use anyhow::Result;
use clap::{Parser, Subcommand};
use rcd::{config::Config, db, sessions};

#[derive(Parser)]
#[command(version, about = "Telegram media downloader bot")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Validate the environment, database and locales without starting the bot
    Check,
    /// Run the bot
    Serve,
    /// Sign in the download pool accounts from POOL_PHONES (or one number) and store their sessions
    PoolLogin {
        /// Log in just this number instead of POOL_PHONES (spaces are fine)
        #[arg(long, num_args = 1.., value_delimiter = None)]
        phone: Vec<String>,
    },
    /// Copy the Python bot's linked and pool sessions into the encrypted store (safe to repeat)
    Import {
        /// Folder with the Pyrogram `.session` files
        #[arg(long, default_value = "sessions")]
        sessions_dir: std::path::PathBuf,
        /// File name (without `.session`) of the bot's own session
        #[arg(long, default_value = "rcd")]
        bot_file: String,
        /// Files to leave out
        #[arg(
            long,
            value_delimiter = ',',
            default_value = "log_cleaner,recover_admin"
        )]
        skip: Vec<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let data_dir =
        std::path::PathBuf::from(std::env::var("DATA_DIR").unwrap_or_else(|_| "data".into()));
    let logs = rcd::logging::init(&data_dir)?;
    match Cli::parse().command {
        Command::Check => check().await,
        Command::PoolLogin { phone } => rcd::pool_login::run(Config::from_env()?, phone).await,
        Command::Serve => rcd::bot::serve(Config::from_env()?, logs).await,
        Command::Import {
            sessions_dir,
            bot_file,
            skip,
        } => import(&sessions_dir, &bot_file, &skip).await,
    }
}

async fn check() -> Result<()> {
    let config = Config::from_env()?;
    tracing::info!(owners = config.owner_ids.len(), "environment is complete");
    let pool = db::connect(&config.database_url).await?;
    let one: i32 = sqlx::query_scalar("SELECT 1").fetch_one(&pool).await?;
    anyhow::ensure!(one == 1, "database did not answer");
    tracing::info!("database reachable");
    let locales = rcd::i18n::Locales::load(&config.locales_dir)?;
    tracing::info!(languages = locales.codes().len(), "locales load");
    let pool_sessions: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM account_sessions WHERE kind = 'pool'")
            .fetch_one(&pool)
            .await
            .unwrap_or(0);
    anyhow::ensure!(
        pool_sessions > 0 || config.pool_phones.is_empty(),
        "POOL_PHONES is set but no pool session is stored; run `rcd pool-login` or `rcd import`"
    );
    tracing::info!(pool_sessions, "pool sessions stored");
    Ok(())
}

async fn import(dir: &std::path::Path, bot_file: &str, skip: &[String]) -> Result<()> {
    let config = Config::from_env()?;
    let pool = db::connect(&config.database_url).await?;
    db::migrate(&pool).await?;
    let linked = sessions::import_linked(&pool, &config.session_key).await?;
    tracing::info!(?linked, "linked sessions imported");
    let skip: Vec<&str> = skip.iter().map(String::as_str).collect();
    let files =
        sessions::import_pool_files(&pool, &config.session_key, dir, bot_file, &skip).await?;
    tracing::info!(?files, "session files imported");
    Ok(())
}
