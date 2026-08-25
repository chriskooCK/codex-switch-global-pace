#[test]
fn public_http_client_reports_missing_configuration_without_panicking() {
    let error = codex_switch::auth::build_http_client()
        .expect_err("library callers must initialize configuration explicitly");

    assert!(
        error
            .to_string()
            .contains("configuration is not initialized"),
        "{error:#}"
    );
}

#[tokio::test]
async fn public_usage_reports_missing_configuration_before_cache_or_auth_io() {
    let error = codex_switch::usage::fetch_usage_retried(
        "uninitialized",
        std::path::Path::new("this-path-must-not-be-read.json"),
    )
    .await
    .expect_err("public usage must require explicit configuration initialization");

    assert_eq!(error.summary, "configuration unavailable");
    assert!(
        error.detail.contains("configuration is not initialized"),
        "{}",
        error.detail
    );
    assert!(
        !error.detail.contains("this-path-must-not-be-read"),
        "configuration validation must happen before auth-path I/O: {}",
        error.detail
    );
}
