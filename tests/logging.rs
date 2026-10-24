//! A repeated (throttled) message must never hide the different messages that follow it.

#[test]
fn every_warning_reaches_the_log_file() {
    let dir = std::env::temp_dir().join(format!("rcd-log-test-{}", std::process::id()));
    let _logs = rcd::logging::init(&dir).unwrap();
    for _ in 0..3 {
        tracing::warn!(attempt = 1, "same message again");
    }
    let error = std::io::Error::other("rpc error 400: CHANNEL_INVALID");
    tracing::warn!(channel = Some(-100123i64), %error, "channel could not be looked up");
    std::thread::spawn(|| tracing::warn!("from another thread"))
        .join()
        .unwrap();
    tracing::info!("connected!");
    let text = std::fs::read_to_string(dir.join("logs.txt")).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(text.matches("same message again").count(), 3);
    assert!(text.contains(
        "channel could not be looked up channel=-100123 error=rpc error 400: CHANNEL_INVALID"
    ));
    assert!(text.contains("from another thread"));
    assert!(!text.contains("connected!"), "noise must be dropped");
}
