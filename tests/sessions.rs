mod common;

use rcd::{
    pyrogram_session::PyrogramSession,
    sessions::{self, Account, ImportReport, Kind, Stored},
};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};

const KEY: [u8; 32] = [9; 32];

fn session(user_id: u64, is_bot: bool, fill: u8) -> PyrogramSession {
    PyrogramSession {
        dc_id: 4,
        api_id: 12345,
        test_mode: false,
        auth_key: [fill; 256],
        user_id,
        is_bot,
    }
}

async fn write_pyrogram_file(path: &std::path::Path, s: &PyrogramSession) {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true);
    let mut conn = SqliteConnection::connect_with(&options).await.unwrap();
    sqlx::query("CREATE TABLE sessions (dc_id INTEGER, api_id INTEGER, test_mode INTEGER, auth_key BLOB, date INTEGER, user_id INTEGER, is_bot INTEGER)")
        .execute(&mut conn).await.unwrap();
    sqlx::query("INSERT INTO sessions VALUES (?, ?, 0, ?, 0, ?, ?)")
        .bind(s.dc_id as i64)
        .bind(s.api_id as i64)
        .bind(s.auth_key.to_vec())
        .bind(s.user_id as i64)
        .bind(s.is_bot as i64)
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::query("CREATE TABLE peers (id INTEGER, access_hash INTEGER, type TEXT, phone_number TEXT, last_update_on INTEGER)")
        .execute(&mut conn).await.unwrap();
    sqlx::query("INSERT INTO peers (id, access_hash, type) VALUES (555, 77, 'user'), (-1001234567890, 88, 'channel'), (-42, 0, 'group')")
        .execute(&mut conn).await.unwrap();
}

#[tokio::test]
async fn put_get_and_wrong_key() {
    let Some(temp) = common::TempSchema::create().await else {
        return;
    };
    rcd::db::migrate(&temp.pool).await.unwrap();
    let account = Account {
        kind: Kind::Pool,
        label: "p1",
        owner_id: None,
        phone: None,
    };
    assert_eq!(
        sessions::put(&temp.pool, &KEY, &account, &session(5, false, 1))
            .await
            .unwrap(),
        Stored::Added
    );
    assert_eq!(
        sessions::put(&temp.pool, &KEY, &account, &session(5, false, 1))
            .await
            .unwrap(),
        Stored::Unchanged
    );
    assert_eq!(
        sessions::put(&temp.pool, &KEY, &account, &session(5, false, 2))
            .await
            .unwrap(),
        Stored::Updated
    );
    let got = sessions::get(&temp.pool, &KEY, Kind::Pool, "p1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.auth_key, [2; 256]);
    assert!(
        sessions::get(&temp.pool, &[1; 32], Kind::Pool, "p1")
            .await
            .is_err()
    );
    let raw: Vec<u8> = sqlx::query_scalar("SELECT ciphertext FROM account_sessions")
        .fetch_one(&temp.pool)
        .await
        .unwrap();
    assert!(
        !raw.windows(8).any(|w| w == [2u8; 8]),
        "auth key must not appear in clear"
    );
    assert!(
        sessions::remove(&temp.pool, Kind::Pool, "p1")
            .await
            .unwrap()
    );
    temp.finish().await;
}

#[tokio::test]
async fn import_is_idempotent() {
    let Some(temp) = common::TempSchema::create().await else {
        return;
    };
    rcd::db::migrate(&temp.pool).await.unwrap();
    sqlx::query("INSERT INTO users (user_id) VALUES (10), (11), (12)")
        .execute(&temp.pool)
        .await
        .unwrap();
    for (id, s) in [
        (10, session(100, false, 3).encode()),
        (11, session(101, false, 4).encode()),
        (12, "garbage".to_string()),
    ] {
        sqlx::query("INSERT INTO user_sessions (user_id, phone_number, session_string) VALUES ($1, '+20100', $2)")
            .bind(id).bind(s).execute(&temp.pool).await.unwrap();
    }
    let first = sessions::import_linked(&temp.pool, &KEY).await.unwrap();
    assert_eq!(
        first,
        ImportReport {
            added: 2,
            updated: 0,
            unchanged: 0,
            skipped: 1
        }
    );
    let second = sessions::import_linked(&temp.pool, &KEY).await.unwrap();
    assert_eq!(
        second,
        ImportReport {
            added: 0,
            updated: 0,
            unchanged: 2,
            skipped: 1
        }
    );
    let linked = sessions::get(&temp.pool, &KEY, Kind::Linked, "10")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(linked.user_id, 100);
    let owner: i64 = sqlx::query_scalar("SELECT owner_id FROM account_sessions WHERE label = '11'")
        .fetch_one(&temp.pool)
        .await
        .unwrap();
    assert_eq!(owner, 11);

    let dir = std::env::temp_dir().join(format!("rcd_pool_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    write_pyrogram_file(&dir.join("rcd.session"), &session(7, true, 5)).await;
    write_pyrogram_file(&dir.join("2003.session"), &session(8, false, 6)).await;
    write_pyrogram_file(&dir.join("log_cleaner.session"), &session(7, true, 5)).await;
    write_pyrogram_file(&dir.join("other_bot.session"), &session(9, true, 5)).await;
    let report = sessions::import_pool_files(&temp.pool, &KEY, &dir, "rcd", &["log_cleaner"])
        .await
        .unwrap();
    assert_eq!(
        report,
        ImportReport {
            added: 2,
            updated: 0,
            unchanged: 0,
            skipped: 2
        }
    );
    assert!(
        sessions::get(&temp.pool, &KEY, Kind::Bot, "rcd")
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        sessions::get(&temp.pool, &KEY, Kind::Pool, "2003")
            .await
            .unwrap()
            .is_some()
    );
    let again = sessions::import_pool_files(&temp.pool, &KEY, &dir, "rcd", &["log_cleaner"])
        .await
        .unwrap();
    assert_eq!(again.unchanged, 2);
    let cached: i64 =
        sqlx::query_scalar("SELECT count(*) FROM peer_cache WHERE session_key = 'bot:rcd'")
            .fetch_one(&temp.pool)
            .await
            .unwrap();
    assert_eq!(
        cached, 2,
        "users and channels are cached, basic groups are not"
    );
    std::fs::remove_dir_all(&dir).ok();
    temp.finish().await;
}
