//! Manual check against Telegram: `DIAG_LINK=<post link> cargo test --release --test diagnose_download -- --ignored --nocapture`.
//! Needs a filled environment and a stored pool account; the bot must not be running with the same accounts.

#[tokio::test]
#[ignore]
async fn download_one_post() {
    let config = rcd::config::Config::from_env().expect("environment");
    let pg = rcd::db::connect(&config.database_url).await.expect("db");
    let accounts =
        rcd::telegram::accounts::Accounts::new(pg, config.session_key, config.api_id as u32, None);
    println!("pool loaded: {:?}", accounts.load_pool().await);
    let account = accounts.for_download(0).await.expect("account");
    let link = std::env::var("DIAG_LINK").expect("DIAG_LINK");
    let parsed = rcd::links::parse_message_link(&link).expect("link");
    let peer = rcd::tasks::access::resolve_chat(&account, &parsed.chat)
        .await
        .expect("chat");
    println!("peer: {peer:?}");
    let client = account.client().await;
    let message = rcd::tasks::access::get_message(&client, peer, parsed.message_id)
        .await
        .expect("get message")
        .expect("message exists");
    let raw = rcd::tasks::download::raw_message(&message).expect("raw");
    let item = rcd::media::info::downloadable(raw).expect("downloadable");
    println!(
        "item: kind={:?} size={} dc={} name={:?}",
        item.kind, item.size, item.dc_id, item.name
    );
    let path = std::env::temp_dir().join("rcd-diag.bin");
    let result = rcd::media::download::fetch(&client, &item, &path, |done, total| {
        if done % (8 * 1024 * 1024) < 1024 * 1024 {
            println!("progress {done}/{total}");
        }
    })
    .await;
    println!("result: {result:?}");
    println!("file: {:?}", std::fs::metadata(&path).map(|m| m.len()));
    let _ = std::fs::remove_file(&path);
}
