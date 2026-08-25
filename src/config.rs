use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::auth::app_home;

static CONFIG: OnceLock<AppConfig> = OnceLock::new();
static STARTUP_WARNINGS: OnceLock<Vec<String>> = OnceLock::new();
static CLI_PROXY: OnceLock<String> = OnceLock::new();

pub(crate) const POLL_BACKOFF_MAX_MULTIPLIER: u64 = 16;

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct AppConfig {
    pub proxy: ProxyConfig,
    pub cache: CacheConfig,
    pub network: NetworkConfig,
    #[serde(default)]
    pub tui: TuiConfig,
    #[serde(rename = "use")]
    pub use_cfg: UseConfig,
    #[serde(default)]
    pub daemon: DaemonConfig,
    /// Accepted only so installations upgraded from the removed launch command
    /// receive the targeted startup warning below. No active setting is derived
    /// from this value.
    #[serde(rename = "launch")]
    removed_launch: Option<toml::Value>,
}

impl AppConfig {
    fn validate(&self) -> Result<()> {
        if let Some(proxy_url) = self.proxy.url.as_deref() {
            if proxy_url.trim().is_empty() {
                anyhow::bail!("config.proxy.url cannot be empty");
            }
            crate::auth::parse_http_proxy_url(proxy_url)
                .context("config.proxy.url is not a valid proxy URL")?;
        }
        if self.network.max_concurrent == 0 {
            anyhow::bail!("config.network.max_concurrent must be at least 1");
        }
        if self.network.max_concurrent > tokio::sync::Semaphore::MAX_PERMITS {
            anyhow::bail!(
                "config.network.max_concurrent={} exceeds the runtime limit of {}",
                self.network.max_concurrent,
                tokio::sync::Semaphore::MAX_PERMITS
            );
        }
        validate_interval(
            "tui.auto_refresh_interval_secs",
            self.tui.auto_refresh_interval_secs,
            30,
            1,
        )?;
        validate_percentage("use.safety_margin_7d", self.use_cfg.safety_margin_7d)?;
        // Failed polls back off to sixteen poll intervals, so validate the
        // actual timer horizon instead of only the configured base interval.
        let poll_backoff_horizon = validate_interval(
            "daemon.poll_interval_secs",
            self.daemon.poll_interval_secs,
            1,
            POLL_BACKOFF_MAX_MULTIPLIER,
        )?;
        let poll_backoff_horizon = i64::try_from(poll_backoff_horizon)
            .context("config.daemon.poll_interval_secs is too large for persisted daemon state")?;
        crate::auth::now_unix_secs()
            .checked_add(poll_backoff_horizon)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "config.daemon.poll_interval_secs is too large for persisted daemon state"
                )
            })?;
        validate_percentage("daemon.switch_threshold", self.daemon.switch_threshold)?;
        validate_interval(
            "daemon.cache_refresh_interval_secs",
            self.daemon.cache_refresh_interval_secs,
            1,
            1,
        )?;
        validate_interval(
            "daemon.token_check_interval_secs",
            self.daemon.token_check_interval_secs,
            1,
            1,
        )?;
        let log_level = self.daemon.log_level.trim();
        if log_level.is_empty() {
            anyhow::bail!("config.daemon.log_level cannot be empty");
        }
        tracing_subscriber::EnvFilter::try_new(crate::logging::application_filter(log_level))
            .context("config.daemon.log_level is not a valid tracing filter level")?;
        Ok(())
    }
}

fn validate_interval(name: &str, seconds: u64, minimum: u64, multiplier: u64) -> Result<u64> {
    if seconds < minimum {
        anyhow::bail!("config.{name} must be at least {minimum} second(s)");
    }
    let horizon = seconds
        .checked_mul(multiplier)
        .filter(|value| {
            Instant::now()
                .checked_add(Duration::from_secs(*value))
                .is_some()
        })
        .ok_or_else(|| anyhow::anyhow!("config.{name} is too large for the runtime timer"))?;
    debug_assert!(horizon >= seconds);
    Ok(horizon)
}

fn validate_percentage(name: &str, value: f64) -> Result<()> {
    if !value.is_finite() || !(0.0..=100.0).contains(&value) {
        anyhow::bail!("config.{name} must be a finite percentage from 0 through 100");
    }
    Ok(())
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProxyConfig {
    pub url: Option<String>,
    pub no_proxy: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CacheConfig {
    /// Cache TTL in seconds (default: 300)
    pub ttl: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self { ttl: 300 }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NetworkConfig {
    /// Max concurrent usage requests (default: 20)
    pub max_concurrent: usize,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self { max_concurrent: 20 }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TuiConfig {
    /// TUI auto-refresh interval in seconds (default: 120, minimum: 30)
    pub auto_refresh_interval_secs: u64,
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            auto_refresh_interval_secs: 120,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UseConfig {
    /// 7d safety margin: when 7d remaining% falls below this, a scoring penalty kicks in (default: 20)
    pub safety_margin_7d: f64,
    /// Prioritize Team plan accounts (default: true)
    pub team_priority: bool,
    /// Compatibility-only fields for the targeted removal warnings. They are
    /// never consulted by account selection.
    mode: Option<toml::Value>,
    min_remaining: Option<toml::Value>,
}

impl Default for UseConfig {
    fn default() -> Self {
        Self {
            safety_margin_7d: 20.0,
            team_priority: true,
            mode: None,
            min_remaining: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DaemonConfig {
    /// Usage poll interval in seconds (default: 60)
    pub poll_interval_secs: u64,
    /// Primary usage % that starts candidate search, or secondary when no primary exists (default: 80.0)
    pub switch_threshold: f64,
    /// Background cache refresh interval in seconds (default: 300)
    pub cache_refresh_interval_secs: u64,
    /// Warm up accounts whose quota window is not active during cache refresh (default: false)
    pub auto_warmup: bool,
    /// Token expiry check interval in seconds (default: 300)
    pub token_check_interval_secs: u64,
    /// Send desktop notification on switch (default: false)
    pub notify: bool,
    /// Log level for daemon (default: "error")
    pub log_level: String,
    /// Hold a pending switch while a Codex session is running (default: true)
    pub defer_switch_while_codex_running: bool,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            poll_interval_secs: 60,
            switch_threshold: 80.0,
            cache_refresh_interval_secs: 300,
            auto_warmup: false,
            token_check_interval_secs: 300,
            notify: false,
            log_level: "error".to_string(),
            defer_switch_while_codex_running: true,
        }
    }
}

pub fn config_path() -> anyhow::Result<PathBuf> {
    Ok(app_home()?.join("config.toml"))
}

fn deprecated_key_warnings(config: &AppConfig) -> Vec<String> {
    let mut warnings = Vec::new();
    if config.use_cfg.mode.is_some() {
        warnings.push(
            "config: [use] 'mode' is deprecated and ignored in v0.0.13+, \
             the adaptive algorithm replaces all selection modes"
                .into(),
        );
    }
    if config.use_cfg.min_remaining.is_some() {
        warnings.push(
            "config: [use] 'min_remaining' is deprecated and ignored in v0.0.13+, \
             the adaptive algorithm replaces all selection modes"
                .into(),
        );
    }
    if config.removed_launch.is_some() {
        warnings.push(
            "config: [launch] is ignored because the launch command was removed in \
             v20260824.2.0; delete this table"
                .into(),
        );
    }
    warnings
}

#[cfg(test)]
fn load_from_str_with_warnings(raw: &str) -> Result<(AppConfig, Vec<String>)> {
    let config = toml::from_str::<AppConfig>(raw)?;
    config.validate()?;
    let warnings = deprecated_key_warnings(&config);
    Ok((config, warnings))
}

#[cfg(test)]
fn load_from_str(raw: &str) -> Result<AppConfig> {
    load_from_str_with_warnings(raw).map(|(config, _)| config)
}

fn load_from_file() -> Result<(AppConfig, Vec<String>)> {
    let path = config_path().context("failed to determine config path")?;
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            match std::fs::symlink_metadata(&path) {
                Err(meta_err) if meta_err.kind() == std::io::ErrorKind::NotFound => {
                    return Ok((AppConfig::default(), vec![]));
                }
                Ok(_) => {
                    return Err(err)
                        .with_context(|| format!("failed to read config file {}", path.display()));
                }
                Err(meta_err) => {
                    return Err(meta_err).with_context(|| {
                        format!("failed to inspect config path {}", path.display())
                    });
                }
            }
        }
        Err(err) => {
            return Err(err)
                .with_context(|| format!("failed to read config file {}", path.display()));
        }
    };
    let config = toml::from_str::<AppConfig>(&content).map_err(|error| {
        anyhow::anyhow!(
            "failed to parse config file {}: {}",
            path.display(),
            error.message()
        )
    })?;
    config.validate().map_err(|error| {
        anyhow::anyhow!("failed to validate config file {}: {error}", path.display())
    })?;
    let warnings = deprecated_key_warnings(&config);
    Ok((config, warnings))
}

pub fn init() -> Result<()> {
    let (config, warnings) = load_from_file()?;
    CONFIG
        .set(config)
        .map_err(|_| anyhow::anyhow!("configuration was initialized before config::init"))?;
    STARTUP_WARNINGS
        .set(warnings)
        .map_err(|_| anyhow::anyhow!("configuration warnings were already initialized"))
}

pub fn startup_warnings() -> &'static [String] {
    STARTUP_WARNINGS.get().map(Vec::as_slice).unwrap_or(&[])
}

pub fn try_get() -> Result<&'static AppConfig> {
    CONFIG.get().ok_or_else(|| {
        anyhow::anyhow!(
            "configuration is not initialized; call codex_switch::config::init() before using APIs that read configuration"
        )
    })
}

pub(crate) fn get() -> &'static AppConfig {
    try_get().expect("binary configuration initialization invariant violated")
}

#[cfg(test)]
pub(crate) fn init_defaults_for_tests() {
    CONFIG.get_or_init(AppConfig::default);
    STARTUP_WARNINGS.get_or_init(Vec::new);
}

pub fn set_cli_proxy(proxy: String) -> Result<()> {
    crate::auth::parse_http_proxy_url(&proxy).context("--proxy is not a valid proxy URL")?;
    CLI_PROXY
        .set(proxy)
        .map_err(|_| anyhow::anyhow!("CLI proxy was initialized more than once"))
}

pub fn resolve_proxy() -> Result<Option<String>> {
    Ok(select_proxy(
        CLI_PROXY.get().map(String::as_str),
        try_get()?.proxy.url.as_deref(),
    ))
}

fn select_proxy(cli_proxy: Option<&str>, configured_proxy: Option<&str>) -> Option<String> {
    cli_proxy
        .or_else(|| configured_proxy.filter(|proxy| !proxy.is_empty()))
        .map(str::to_owned)
}

pub fn resolve_no_proxy() -> Result<Option<String>> {
    if let Some(np) = &try_get()?.proxy.no_proxy
        && !np.is_empty()
    {
        return Ok(Some(np.clone()));
    }
    Ok(None)
}

pub fn daemon_log_level() -> Result<String> {
    Ok(try_get()?.daemon.log_level.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::{load_from_str, load_from_str_with_warnings, select_proxy};

    #[test]
    fn invalid_intervals_and_concurrency_fail_instead_of_being_replaced() {
        for raw in [
            "[network]\nmax_concurrent = 0\n",
            "[tui]\nauto_refresh_interval_secs = 29\n",
            "[daemon]\npoll_interval_secs = 0\n",
            "[daemon]\ntoken_check_interval_secs = 0\n",
            "[daemon]\ncache_refresh_interval_secs = 0\n",
        ] {
            let error = load_from_str(raw).expect_err("invalid values must fail at startup");
            assert!(error.to_string().contains("config."), "{error:#}");
        }

        let too_many = format!(
            "[network]\nmax_concurrent = {}\n",
            tokio::sync::Semaphore::MAX_PERMITS + 1
        );
        let error = load_from_str(&too_many).expect_err("runtime semaphore limit is exact");
        assert!(error.to_string().contains("runtime limit"), "{error:#}");

        let huge_poll = format!("[daemon]\npoll_interval_secs = {}\n", i64::MAX);
        let error = load_from_str(&huge_poll).expect_err("timer horizon must not overflow");
        assert!(error.to_string().contains("too large"), "{error:#}");
    }

    #[test]
    fn invalid_percentage_values_fail_closed() {
        for raw in [
            "[use]\nsafety_margin_7d = -1\n",
            "[use]\nsafety_margin_7d = 101\n",
            "[use]\nsafety_margin_7d = nan\n",
            "[daemon]\nswitch_threshold = -1\n",
            "[daemon]\nswitch_threshold = 101\n",
            "[daemon]\nswitch_threshold = inf\n",
        ] {
            let error = load_from_str(raw).expect_err("invalid percentages must fail at startup");
            assert!(error.to_string().contains("finite percentage"), "{error:#}");
        }
    }

    #[test]
    fn unsupported_daemon_log_level_fails_closed() {
        let error = load_from_str("[daemon]\nlog_level = \"verbose\"\n").unwrap_err();
        assert!(error.to_string().contains("log_level"), "{error:#}");

        let error = load_from_str("[daemon]\nlog_level = \"  \"\n").unwrap_err();
        assert!(error.to_string().contains("cannot be empty"), "{error:#}");
    }

    #[test]
    fn unknown_active_config_keys_fail_instead_of_selecting_defaults() {
        for raw in [
            "mystery = true\n",
            "[network]\nmax_concurent = 4\n",
            "[use]\nunknown_mode = \"adaptive\"\n",
            "[daemon]\npoll_intervals_secs = 60\n",
        ] {
            let error = load_from_str(raw).expect_err("unknown configuration must fail closed");
            assert!(
                error.to_string().contains("unknown field"),
                "unexpected error for {raw:?}: {error:#}"
            );
        }
    }

    #[test]
    fn proxy_url_uses_runtime_parser_at_startup_without_exposing_credentials() {
        let sentinel = "SENTINEL_PROXY_PASSWORD";
        let raw = format!("[proxy]\nurl = \"http://user:{sentinel}@[\"\n");
        let error = load_from_str(&raw).expect_err("invalid proxy URL must fail at startup");
        let rendered = format!("{error:#}");
        assert!(rendered.contains("config.proxy.url"), "{rendered}");
        assert!(rendered.contains("credentials"), "{rendered}");
        assert!(!rendered.contains(sentinel), "{rendered}");

        load_from_str("[proxy]\nurl = \"http://127.0.0.1:8080\"\n")
            .expect("the runtime proxy parser must accept an ordinary HTTP proxy");
    }

    #[test]
    fn proxy_selection_uses_config_unless_the_cli_supplies_an_override() {
        assert_eq!(
            select_proxy(None, Some("http://configured.example:8080")),
            Some("http://configured.example:8080".to_string())
        );
        assert_eq!(
            select_proxy(
                Some("http://cli.example:9090"),
                Some("http://configured.example:8080")
            ),
            Some("http://cli.example:9090".to_string())
        );
        assert_eq!(select_proxy(None, Some("")), None);
    }

    #[test]
    fn removed_launch_config_is_ignored_with_a_warning() {
        let (_, warnings) =
            load_from_str_with_warnings("[launch]\nrestore_delay_secs = 3\n").unwrap();

        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("[launch]") && warning.contains("removed")),
            "removed configuration must not look active: {warnings:?}"
        );
    }

    #[test]
    fn removed_use_keys_remain_compatible_but_only_as_explicit_warnings() {
        let (config, warnings) = load_from_str_with_warnings(
            "[use]\nmode = \"adaptive\"\nmin_remaining = 10\nsafety_margin_7d = 25\n",
        )
        .unwrap();

        assert_eq!(config.use_cfg.safety_margin_7d, 25.0);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings.iter().any(|warning| warning.contains("'mode'")));
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("'min_remaining'"))
        );
    }
}
