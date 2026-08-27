use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, LazyLock};

use anyhow::{Context, Result, bail};
use tokio::sync::{Mutex, OwnedMutexGuard, Semaphore};
use tracing::{debug, warn};

/// The models one warmup should touch, keyed by account *and* by the quota
/// pools that produced the selection (see [`warmup_cache_key`]).
///
/// The whole selected set is cached, not just the main-pool model: the main
/// request and the additional-pool requests are answered by a single `/models`
/// response, so caching only the first one made every warmup fetch that
/// response twice — and with no additional pools, threw the second away.
static MODEL_CACHE: LazyLock<Mutex<HashMap<WarmupModelCacheKey, WarmupModelSelection>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
// Serialize duplicate fetches for the same account without blocking unrelated accounts.
static MODEL_FETCH_LOCKS: LazyLock<Mutex<HashMap<WarmupModelCacheKey, Arc<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) type FirstNetworkPermit = crate::usage::FirstNetworkPermit;

/// Build the ordinary, non-cancellable first-request wait used by CLI and
/// daemon warmups. TUI callers supply the same future shape with their existing
/// safe-cancellation race around this first request.
pub(crate) fn first_network_permit(limiter: Arc<Semaphore>) -> FirstNetworkPermit {
    crate::usage::first_network_permit(limiter)
}

pub(crate) fn network_wait_was_cancelled(error: &anyhow::Error) -> bool {
    crate::usage::network_wait_was_cancelled(error)
}

#[derive(Debug, thiserror::Error)]
#[error("warmup cancelled before its first quota request")]
struct WarmupSideEffectCancelled;

pub(crate) fn warmup_wait_was_cancelled(error: &anyhow::Error) -> bool {
    network_wait_was_cancelled(error) || error.downcast_ref::<WarmupSideEffectCancelled>().is_some()
}

type WarmupCancellation = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;
type WarmupCommit = Box<dyn FnOnce() -> bool + Send + 'static>;

/// TUI warmups may stop while `/models` is still read-only or while restoring
/// the alias lease afterward. The commit callback is the single atomic boundary
/// after which model-cache publication and quota POSTs must drain normally.
/// CLI and daemon callers use the uncancellable form.
struct WarmupSideEffectBoundary {
    cancellation: Option<WarmupCancellation>,
    commit: Option<WarmupCommit>,
}

impl WarmupSideEffectBoundary {
    fn uncancellable() -> Self {
        Self {
            cancellation: None,
            commit: None,
        }
    }

    fn cancellable(
        cancellation: impl Future<Output = ()> + Send + 'static,
        commit: impl FnOnce() -> bool + Send + 'static,
    ) -> Self {
        Self {
            cancellation: Some(Box::pin(cancellation)),
            commit: Some(Box::new(commit)),
        }
    }

    async fn run_until_commit<T>(&mut self, work: impl Future<Output = Result<T>>) -> Result<T> {
        let result = if let Some(cancellation) = self.cancellation.as_mut() {
            tokio::select! {
                result = work => result,
                _ = cancellation.as_mut() => return Err(WarmupSideEffectCancelled.into()),
            }
        } else {
            work.await
        }?;
        self.commit()?;
        Ok(result)
    }

    fn commit(&mut self) -> Result<()> {
        self.cancellation = None;
        let Some(commit) = self.commit.take() else {
            return Ok(());
        };
        anyhow::ensure!(commit(), WarmupSideEffectCancelled);
        Ok(())
    }
}

pub(crate) struct WarmupExecutionControls {
    first_permit: FirstNetworkPermit,
    side_effect_boundary: WarmupSideEffectBoundary,
}

impl WarmupExecutionControls {
    fn uncancellable(first_permit: FirstNetworkPermit) -> Self {
        Self {
            first_permit,
            side_effect_boundary: WarmupSideEffectBoundary::uncancellable(),
        }
    }

    pub(crate) fn cancellable(
        first_permit: FirstNetworkPermit,
        cancellation: impl Future<Output = ()> + Send + 'static,
        commit: impl FnOnce() -> bool + Send + 'static,
    ) -> Self {
        Self {
            first_permit,
            side_effect_boundary: WarmupSideEffectBoundary::cancellable(cancellation, commit),
        }
    }
}

/// Own the first caller-defined admission wait, then reacquire from that same
/// semaphore for later requests. Direct warmup permits are requested
/// immediately before HTTP and returned before retry delay or local model-cache
/// work; the prepared usage executor applies the same boundary to its retries
/// and usage-cache publication.
type NetworkBudget = crate::usage::NetworkPermitBudget;

#[derive(Clone, Copy)]
struct WarmupRequestAuth<'a> {
    access_token: &'a str,
    account_id: Option<&'a str>,
    is_fedramp: bool,
}

struct WarmupPreflight {
    binding: crate::jwt::StrictAccountBinding,
    initial_auth: serde_json::Value,
    cached_usage: Option<crate::usage::UsageInfo>,
}

/// The exact credential state authorized to cross a lease-free model lookup.
/// A warmup may use this snapshot for the read-only `/models` request, but it
/// must reacquire the alias lease and prove the snapshot is still current before
/// sending the first side-effecting warmup POST.
struct WarmupCredentialSnapshot {
    auth: serde_json::Value,
    id_token: Option<String>,
    access_token: String,
    refresh_token: Option<String>,
    account_id: Option<String>,
    is_fedramp: bool,
}

impl WarmupCredentialSnapshot {
    fn from_auth(
        alias: &str,
        auth: serde_json::Value,
        expected_binding: &crate::jwt::StrictAccountBinding,
        identity_error: &str,
    ) -> Result<Self> {
        let info = crate::auth::account_info_from_auth_value(&auth);
        anyhow::ensure!(
            info.strict_binding().as_ref() == Some(expected_binding),
            "{alias}: {identity_error}"
        );
        let account_id = info.account_id;
        let is_fedramp = info.is_fedramp;
        let (access_token, refresh_token) = crate::auth::extract_tokens(&auth);
        let access_token = access_token
            .filter(|token| !token.is_empty())
            .ok_or_else(|| anyhow::anyhow!("{alias}: no access_token in profile"))?;
        let refresh_token = refresh_token.filter(|token| !token.is_empty());
        let id_token = crate::auth::extract_id_token(&auth);
        Ok(Self {
            auth,
            id_token,
            access_token,
            refresh_token,
            account_id,
            is_fedramp,
        })
    }

    fn request_auth(&self) -> WarmupRequestAuth<'_> {
        WarmupRequestAuth {
            access_token: &self.access_token,
            account_id: self.account_id.as_deref(),
            is_fedramp: self.is_fedramp,
        }
    }

    fn ensure_fresh_for_post(&self, alias: &str) -> Result<()> {
        anyhow::ensure!(
            crate::jwt::is_token_expiring(&self.access_token, 60)? != Some(true),
            "{alias}: access token expired or became too close to expiry during model discovery"
        );
        Ok(())
    }
}

fn model_cache_get(
    cache: &HashMap<WarmupModelCacheKey, WarmupModelSelection>,
    key: &WarmupModelCacheKey,
) -> Option<WarmupModelSelection> {
    cache.get(key).cloned()
}

fn model_cache_set(
    cache: &mut HashMap<WarmupModelCacheKey, WarmupModelSelection>,
    key: &WarmupModelCacheKey,
    models: WarmupModelSelection,
) {
    cache.insert(key.clone(), models);
}

fn model_cache_invalidate(
    cache: &mut HashMap<WarmupModelCacheKey, WarmupModelSelection>,
    key: &WarmupModelCacheKey,
) {
    cache.remove(key);
}

fn build_models_request(
    endpoints: &crate::auth::ServiceEndpoints,
    client: &reqwest::Client,
    access_token: &str,
    account_id: Option<&str>,
    is_fedramp: bool,
) -> Result<reqwest::RequestBuilder> {
    Ok(crate::usage::apply_account_routing_headers(
        client
            .get(endpoints.models()?)
            .query(&[("client_version", crate::auth::CODEX_COMPATIBILITY_VERSION)])
            .bearer_auth(access_token),
        account_id,
        is_fedramp,
    ))
}

/// One entry from the `/models` endpoint's `models[]` array.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ModelEntry {
    pub slug: String,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub visibility: Option<String>,
    pub priority: Option<i64>,
    pub supported_in_api: Option<bool>,
    pub context_window: Option<u64>,
    pub default_reasoning_effort: Option<String>,
    pub supported_reasoning_efforts: Vec<String>,
    pub input_modalities: Vec<String>,
    pub additional_speed_tiers: Vec<String>,
    pub service_tiers: Vec<String>,
    pub default_service_tier: Option<String>,
    pub max_context_window: Option<u64>,
    pub auto_compact_token_limit: Option<u64>,
    pub effective_context_window_percent: Option<i64>,
    pub supports_parallel_tool_calls: Option<bool>,
    pub supports_image_detail_original: Option<bool>,
    pub experimental_supported_tools: Vec<String>,
    pub supports_search_tool: Option<bool>,
    pub use_responses_lite: Option<bool>,
}

/// Parse the `/models` endpoint's JSON body into a `Vec<ModelEntry>`. Entries
/// missing a `slug` are skipped; other fields are treated as optional
/// (defensively ignoring unknown fields per the upstream contract).
fn parse_models_body(body: &serde_json::Value) -> Result<Vec<ModelEntry>> {
    let models = body["models"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("no models array in response"))?;

    Ok(models
        .iter()
        .filter_map(|m| {
            let slug = m["slug"].as_str()?.to_string();
            let string_list = |key: &str| {
                m.get(key)
                    .and_then(serde_json::Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|item| item.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default()
            };
            Some(ModelEntry {
                slug,
                display_name: m["display_name"].as_str().map(String::from),
                description: m["description"].as_str().map(String::from),
                visibility: m["visibility"].as_str().map(String::from),
                priority: m["priority"].as_i64(),
                supported_in_api: m["supported_in_api"].as_bool(),
                context_window: m["context_window"].as_u64(),
                default_reasoning_effort: m["default_reasoning_level"]
                    .as_str()
                    .or_else(|| m["default_reasoning_effort"].as_str())
                    .map(String::from),
                supported_reasoning_efforts: m
                    .get("supported_reasoning_levels")
                    .or_else(|| m.get("supported_reasoning_efforts"))
                    .and_then(serde_json::Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|item| {
                                item.as_str()
                                    .or_else(|| item.get("effort").and_then(|v| v.as_str()))
                                    .or_else(|| {
                                        item.get("reasoning_effort").and_then(|v| v.as_str())
                                    })
                                    .map(String::from)
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                input_modalities: string_list("input_modalities"),
                additional_speed_tiers: string_list("additional_speed_tiers"),
                service_tiers: m
                    .get("service_tiers")
                    .and_then(serde_json::Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|item| {
                                item.as_str()
                                    .or_else(|| item.get("id").and_then(|v| v.as_str()))
                                    .map(String::from)
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                default_service_tier: m["default_service_tier"].as_str().map(String::from),
                max_context_window: m["max_context_window"].as_u64(),
                auto_compact_token_limit: m["auto_compact_token_limit"].as_u64(),
                effective_context_window_percent: m["effective_context_window_percent"].as_i64(),
                supports_parallel_tool_calls: m["supports_parallel_tool_calls"].as_bool(),
                supports_image_detail_original: m["supports_image_detail_original"].as_bool(),
                experimental_supported_tools: string_list("experimental_supported_tools"),
                supports_search_tool: m["supports_search_tool"].as_bool(),
                use_responses_lite: m["use_responses_lite"].as_bool(),
            })
        })
        .collect())
}

/// Sort models for display: ascending priority (lowest number first), unknown
/// priority sorts last. Does not filter hidden models — callers decide how to
/// present `visibility == "hide"` entries (e.g. dim them rather than drop them).
pub(crate) fn sorted_models_for_display(models: &[ModelEntry]) -> Vec<&ModelEntry> {
    let mut sorted: Vec<&ModelEntry> = models.iter().collect();
    sorted.sort_by_key(|m| m.priority.unwrap_or(i64::MAX));
    sorted
}

/// Fetch and parse the full model list from the `/models` endpoint.
async fn fetch_models(
    endpoints: &crate::auth::ServiceEndpoints,
    client: &reqwest::Client,
    auth: WarmupRequestAuth<'_>,
    network: &mut NetworkBudget,
) -> Result<Vec<ModelEntry>> {
    for attempt in 1..=3 {
        let request = build_models_request(
            endpoints,
            client,
            auth.access_token,
            auth.account_id,
            auth.is_fedramp,
        )?;
        let permit = network.acquire().await?;
        let response = request.send().await;
        match response {
            Ok(resp) if resp.status().is_success() => {
                let body: serde_json::Value = resp.json().await?;
                drop(permit);
                return parse_models_body(&body);
            }
            Ok(resp) => {
                let status = resp.status();
                let retryable = status.is_server_error() || status.as_u16() == 429;
                drop(resp);
                drop(permit);
                if !retryable || attempt == 3 {
                    bail!("models endpoint returned {status}");
                }
                warn!("models fetch attempt {attempt}/3 returned {status}; retrying");
            }
            Err(error) => {
                drop(permit);
                if attempt == 3 {
                    return Err(crate::auth::format_reqwest_error(
                        "models fetch failed after 3 attempts",
                        &error,
                    ));
                }
                warn!("models fetch attempt {attempt}/3 failed: {error}; retrying");
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(250 * attempt)).await;
    }
    unreachable!("models fetch loop always returns")
}

/// Resolve every model this warmup should touch from one `/models` response.
async fn fetch_warmup_models(
    endpoints: &crate::auth::ServiceEndpoints,
    client: &reqwest::Client,
    auth: WarmupRequestAuth<'_>,
    additional_limits: &[crate::usage::AdditionalRateLimit],
    network: &mut NetworkBudget,
) -> Result<WarmupModelSelection> {
    let models = fetch_models(endpoints, client, auth, network).await?;
    require_official_model(select_warmup_models(&models, additional_limits))
}

fn require_official_model<T>(result: Result<T>) -> Result<T> {
    result.map_err(|error| anyhow::anyhow!("could not resolve an official warmup model: {error:#}"))
}

fn normalized_pool_name(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// The cache key for one account's resolved warmup model set.
///
/// A strict account binding prevents an alias that is rebound to a different
/// owner from inheriting its predecessor's models. The resolved set also bakes
/// in the additional pools that existed when it was built. A process that
/// outlives a pool change — the daemon with `auto_warmup`, which runs for days —
/// would otherwise keep warming the old set, and a pool the account just gained
/// would never get its quota window opened until someone restarted the daemon.
/// That failure is silent: nothing errors, so nothing invalidates the entry.
///
/// Only the pools `select_warmup_models` acts on take part, and they form a
/// normalized set, so upstream duplication or reordering does not needlessly
/// discard a good entry.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct WarmupModelCacheKey {
    binding: crate::jwt::StrictAccountBinding,
    model_pools: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SelectedPoolModel {
    pool_keys: Vec<String>,
    model: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WarmupModelSelection {
    main: String,
    additional: Vec<SelectedPoolModel>,
}

fn warmup_cache_key(
    binding: &crate::jwt::StrictAccountBinding,
    additional_limits: &[crate::usage::AdditionalRateLimit],
) -> WarmupModelCacheKey {
    let mut pools: Vec<String> = additional_limits
        .iter()
        .filter(|limit| is_model_quota_limit(limit))
        .map(|limit| normalized_pool_name(limit.limit_name.as_deref().unwrap_or_default()))
        .collect();
    pools.sort_unstable();
    pools.dedup();
    WarmupModelCacheKey {
        binding: binding.clone(),
        model_pools: pools,
    }
}

fn is_model_quota_limit(limit: &crate::usage::AdditionalRateLimit) -> bool {
    limit
        .metered_feature
        .as_deref()
        .is_some_and(|feature| feature.starts_with("codex_"))
        && limit.allowed != Some(false)
        && limit.limit_reached != Some(true)
}

fn matching_model_for_limit<'a>(
    visible_models: &[&'a ModelEntry],
    limit: &crate::usage::AdditionalRateLimit,
) -> Option<&'a ModelEntry> {
    let pool_name = normalized_pool_name(limit.limit_name.as_deref()?);
    if pool_name.is_empty() {
        return None;
    }

    visible_models.iter().copied().find(|model| {
        let slug = normalized_pool_name(&model.slug);
        let display = model
            .display_name
            .as_deref()
            .map(normalized_pool_name)
            .unwrap_or_default();
        pool_name == slug
            || pool_name == display
            || slug.contains(&pool_name)
            || display.contains(&pool_name)
    })
}

fn select_warmup_models(
    models: &[ModelEntry],
    additional_limits: &[crate::usage::AdditionalRateLimit],
) -> Result<WarmupModelSelection> {
    let visible: Vec<&ModelEntry> = models
        .iter()
        .filter(|m| m.visibility.as_deref() != Some("hide"))
        .collect();

    if visible.is_empty() {
        bail!("no visible models available");
    }

    let model_limits: Vec<&crate::usage::AdditionalRateLimit> = additional_limits
        .iter()
        .filter(|limit| is_model_quota_limit(limit))
        .collect();
    let matched_limits: Vec<(&crate::usage::AdditionalRateLimit, &ModelEntry)> = model_limits
        .iter()
        .copied()
        .filter_map(|limit| matching_model_for_limit(&visible, limit).map(|model| (limit, model)))
        .collect();
    if matched_limits.len() != model_limits.len() {
        let unmatched = model_limits
            .iter()
            .copied()
            .filter(|limit| matching_model_for_limit(&visible, limit).is_none())
            .map(|limit| limit.limit_name.as_deref().unwrap_or("unnamed"))
            .collect::<Vec<_>>()
            .join(", ");
        bail!("no model matched quota pool(s): {unmatched}");
    }
    let additional_slugs: HashSet<&str> = matched_limits
        .iter()
        .map(|(_, model)| model.slug.as_str())
        .collect();
    let main_candidates: Vec<&ModelEntry> = visible
        .iter()
        .copied()
        .filter(|model| {
            model.supported_in_api != Some(false) && !additional_slugs.contains(model.slug.as_str())
        })
        .collect();

    // Prefer mini (lightest), fall back to highest priority (lowest number).
    // Models mapped to additional pools must not replace the main-pool request.
    let main = main_candidates
        .iter()
        .find(|m| m.slug.contains("mini"))
        .or_else(|| {
            main_candidates
                .iter()
                .min_by_key(|m| m.priority.unwrap_or(i64::MAX))
        })
        .map(|m| m.slug.clone())
        .context("official models endpoint returned no main-pool model")?;

    let mut additional: Vec<SelectedPoolModel> = Vec::new();
    for (limit, model) in matched_limits {
        let pool_key = normalized_pool_name(limit.limit_name.as_deref().unwrap_or_default());
        if let Some(group) = additional
            .iter_mut()
            .find(|group| group.model == model.slug)
        {
            group.pool_keys.push(pool_key);
        } else {
            additional.push(SelectedPoolModel {
                pool_keys: vec![pool_key],
                model: model.slug.clone(),
            });
        }
    }
    for group in &mut additional {
        group.pool_keys.sort_unstable();
        group.pool_keys.dedup();
    }
    additional.sort_by(|left, right| {
        left.pool_keys
            .cmp(&right.pool_keys)
            .then_with(|| left.model.cmp(&right.model))
    });

    let selected = WarmupModelSelection { main, additional };
    debug!("warmup: models selected from API: {selected:?}");
    Ok(selected)
}

/// A freshly fetched model list is not published to the process cache until
/// the caller has revalidated the profile after its lease-free HTTP phase.
/// Keeping the per-key fetch guard here also prevents a second warmup from
/// observing or duplicating an unvalidated result.
struct WarmupModelResolution {
    models: WarmupModelSelection,
    pending_cache_key: Option<WarmupModelCacheKey>,
    _fetch_guard: Option<OwnedMutexGuard<()>>,
}

impl WarmupModelResolution {
    fn cached(models: WarmupModelSelection) -> Self {
        Self {
            models,
            pending_cache_key: None,
            _fetch_guard: None,
        }
    }

    async fn publish(self) -> WarmupModelSelection {
        let Self {
            models,
            pending_cache_key,
            _fetch_guard,
        } = self;
        if let Some(cache_key) = pending_cache_key {
            model_cache_set(&mut *MODEL_CACHE.lock().await, &cache_key, models.clone());
        }
        models
    }
}

async fn resolve_warmup_models(
    endpoints: &crate::auth::ServiceEndpoints,
    cache_key: &WarmupModelCacheKey,
    client: &reqwest::Client,
    auth: WarmupRequestAuth<'_>,
    additional_limits: &[crate::usage::AdditionalRateLimit],
    network: &mut NetworkBudget,
) -> Result<WarmupModelResolution> {
    if let Some(models) = model_cache_get(&*MODEL_CACHE.lock().await, cache_key) {
        return Ok(WarmupModelResolution::cached(models));
    }

    let fetch_lock = {
        let mut locks = MODEL_FETCH_LOCKS.lock().await;
        locks
            .entry(cache_key.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    };
    let fetch_guard = fetch_lock.lock_owned().await;
    if let Some(models) = model_cache_get(&*MODEL_CACHE.lock().await, cache_key) {
        return Ok(WarmupModelResolution::cached(models));
    }

    let models = fetch_warmup_models(endpoints, client, auth, additional_limits, network).await?;
    Ok(WarmupModelResolution {
        models,
        pending_cache_key: Some(cache_key.clone()),
        _fetch_guard: Some(fetch_guard),
    })
}

fn build_body(model: &str) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "instructions": "You are a helpful assistant.",
        "input": [{
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "ping"}]
        }],
        "tools": [],
        "tool_choice": "auto",
        "parallel_tool_calls": false,
        "stream": true,
        "store": false,
        "include": []
    })
}

fn make_request(
    endpoints: &crate::auth::ServiceEndpoints,
    client: &reqwest::Client,
    auth: WarmupRequestAuth<'_>,
    body: &serde_json::Value,
) -> Result<reqwest::RequestBuilder> {
    Ok(crate::usage::apply_account_routing_headers(
        client
            .post(endpoints.responses()?)
            .bearer_auth(auth.access_token)
            .header("Content-Type", "application/json"),
        auth.account_id,
        auth.is_fedramp,
    )
    .json(body))
}

struct WarmupResponse {
    status: reqwest::StatusCode,
    error_text: Option<String>,
}

/// Send and consume one warmup response while holding exactly one network
/// permit. Returning this buffered status ensures every caller releases the
/// permit before cache invalidation, token persistence, or another request.
async fn send_warmup_request(
    endpoints: &crate::auth::ServiceEndpoints,
    client: &reqwest::Client,
    auth: WarmupRequestAuth<'_>,
    body: &serde_json::Value,
    error_context: &'static str,
    network: &mut NetworkBudget,
) -> Result<WarmupResponse> {
    let request = make_request(endpoints, client, auth, body)?;
    let _permit = network.acquire().await?;
    let mut response = request
        .send()
        .await
        .map_err(|error| crate::auth::format_reqwest_error(error_context, &error))?;
    let status = response.status();
    let error_text = if status.is_success() {
        // Quota activation is server-side. One chunk proves streaming began;
        // the remainder is intentionally discarded.
        let _ = response.chunk().await;
        None
    } else {
        Some(response.text().await.unwrap_or_default())
    };
    Ok(WarmupResponse { status, error_text })
}

/// Warm one request per distinct additional model. The cached selection keeps
/// its pool identities so models shared by multiple pools stay grouped.
///
/// Takes the models already resolved for this warmup rather than fetching the
/// list again: both halves come from the same `/models` answer, and
/// `select_warmup_models` already excludes the main-pool model from this slice.
enum AdditionalWarmupOutcome {
    Complete,
    Unsupported {
        completed_models: HashSet<String>,
        model: String,
        status: reqwest::StatusCode,
        snippet: String,
    },
}

async fn warmup_additional_models(
    endpoints: &crate::auth::ServiceEndpoints,
    client: &reqwest::Client,
    auth: WarmupRequestAuth<'_>,
    additional_models: &[SelectedPoolModel],
    network: &mut NetworkBudget,
) -> Result<AdditionalWarmupOutcome> {
    let mut completed_models = HashSet::new();
    for selected in additional_models {
        let body = build_body(&selected.model);
        debug!(
            "warmup additional pool POST → {} (model={})",
            endpoints.responses()?,
            selected.model
        );
        let response = send_warmup_request(
            endpoints,
            client,
            auth,
            &body,
            "additional warmup failed",
            network,
        )
        .await?;
        if !response.status.is_success() {
            let status = response.status;
            let text = response.error_text.unwrap_or_default();
            let snippet: String = text.chars().take(160).collect();
            if status == reqwest::StatusCode::BAD_REQUEST && text.contains("not supported") {
                return Ok(AdditionalWarmupOutcome::Unsupported {
                    completed_models,
                    model: selected.model.clone(),
                    status,
                    snippet,
                });
            }
            bail!(
                "additional model {}: HTTP {status} — {snippet}",
                selected.model
            );
        }
        completed_models.insert(selected.model.clone());
    }
    Ok(AdditionalWarmupOutcome::Complete)
}

async fn reacquire_warmup_snapshot_after_model_discovery(
    alias: &str,
    profile_path: &Path,
    expected_binding: &crate::jwt::StrictAccountBinding,
    expected_snapshot: &WarmupCredentialSnapshot,
) -> Result<(crate::profile::ProfileLease, WarmupCredentialSnapshot)> {
    let lease = crate::profile::acquire_profile_lease_async(alias.to_string())
        .await
        .with_context(|| format!("{alias}: failed to reacquire profile after model discovery"))?;
    let auth = crate::auth::read_auth_async(profile_path)
        .await
        .map_err(|error| {
            anyhow::anyhow!("{alias}: cannot revalidate auth after model discovery: {error}")
        })?;
    let current = WarmupCredentialSnapshot::from_auth(
        alias,
        auth,
        expected_binding,
        "profile identity changed during model discovery",
    )?;
    anyhow::ensure!(
        current.auth == expected_snapshot.auth,
        "{alias}: profile credentials changed during model discovery"
    );
    current.ensure_fresh_for_post(alias)?;
    Ok((lease, current))
}

struct WarmupModelDiscoveryContext<'a> {
    alias: &'a str,
    profile_path: &'a Path,
    expected_binding: &'a crate::jwt::StrictAccountBinding,
    endpoints: &'a crate::auth::ServiceEndpoints,
    cache_key: &'a WarmupModelCacheKey,
    client: &'a reqwest::Client,
    additional_limits: &'a [crate::usage::AdditionalRateLimit],
}

/// Reuse an already validated cache hit under the current lease. When network
/// discovery is needed, release the lease and restore the side-effect boundary
/// only after the alias, credential snapshot, and token freshness have all been
/// revalidated. A freshly fetched model list remains unpublished until that
/// proof succeeds.
async fn discover_warmup_models_between_leases(
    context: &WarmupModelDiscoveryContext<'_>,
    lease: crate::profile::ProfileLease,
    snapshot: WarmupCredentialSnapshot,
    network: &mut NetworkBudget,
    side_effect_boundary: &mut WarmupSideEffectBoundary,
) -> Result<(
    crate::profile::ProfileLease,
    WarmupCredentialSnapshot,
    WarmupModelSelection,
)> {
    if let Some(models) = model_cache_get(&*MODEL_CACHE.lock().await, context.cache_key) {
        snapshot.ensure_fresh_for_post(context.alias)?;
        side_effect_boundary.commit()?;
        return Ok((lease, snapshot, models));
    }

    drop(lease);
    let (resolution, lease, current) = side_effect_boundary
        .run_until_commit(async {
            let resolution = resolve_warmup_models(
                context.endpoints,
                context.cache_key,
                context.client,
                snapshot.request_auth(),
                context.additional_limits,
                network,
            )
            .await?;
            let (lease, current) = reacquire_warmup_snapshot_after_model_discovery(
                context.alias,
                context.profile_path,
                context.expected_binding,
                &snapshot,
            )
            .await?;
            Ok((resolution, lease, current))
        })
        .await?;
    let models = resolution.publish().await;
    Ok((lease, current, models))
}

async fn finish_additional_warmup_with_one_model_refresh(
    context: &WarmupModelDiscoveryContext<'_>,
    lease: crate::profile::ProfileLease,
    snapshot: WarmupCredentialSnapshot,
    additional_models: &[SelectedPoolModel],
    network: &mut NetworkBudget,
    side_effect_boundary: &mut WarmupSideEffectBoundary,
) -> Result<crate::profile::ProfileLease> {
    let stale = match warmup_additional_models(
        context.endpoints,
        context.client,
        snapshot.request_auth(),
        additional_models,
        network,
    )
    .await?
    {
        AdditionalWarmupOutcome::Complete => return Ok(lease),
        AdditionalWarmupOutcome::Unsupported {
            completed_models,
            model,
            status,
            snippet,
        } => (completed_models, model, status, snippet),
    };

    let (completed_models, stale_model, stale_status, stale_snippet) = stale;
    debug!(
        "[{}] additional model {:?} returned {}; refreshing the official model set",
        context.alias, stale_model, stale_status
    );
    model_cache_invalidate(&mut *MODEL_CACHE.lock().await, context.cache_key);
    let (lease, snapshot, refreshed) = discover_warmup_models_between_leases(
        context,
        lease,
        snapshot,
        network,
        side_effect_boundary,
    )
    .await
    .with_context(|| {
        format!(
            "{}: failed to refresh models after additional model {stale_model:?} was rejected ({stale_status}: {stale_snippet})",
            context.alias
        )
    })?;
    let remaining: Vec<SelectedPoolModel> = refreshed
        .additional
        .into_iter()
        .filter(|selected| !completed_models.contains(&selected.model))
        .collect();

    match warmup_additional_models(
        context.endpoints,
        context.client,
        snapshot.request_auth(),
        &remaining,
        network,
    )
    .await?
    {
        AdditionalWarmupOutcome::Complete => Ok(lease),
        AdditionalWarmupOutcome::Unsupported {
            model,
            status,
            snippet,
            ..
        } => {
            model_cache_invalidate(&mut *MODEL_CACHE.lock().await, context.cache_key);
            bail!(
                "{}: additional model {model}: HTTP {status} after model refresh — {snippet}",
                context.alias
            )
        }
    }
}

/// Send a minimal completion request to trigger the quota window countdown for a profile.
///
/// The 5-hour and 7-day windows only start after the first real API call.
/// This sends the lightest valid request ("ping") and discards the response body,
/// which is enough for the server to stamp the window start time.
#[cfg(test)]
pub async fn warmup_account(alias: &str, profile_path: &Path) -> Result<()> {
    let lease = crate::profile::acquire_profile_lease_async(alias.to_string())
        .await
        .with_context(|| format!("{alias}: failed to lock profile for warmup"))?;
    let lease = warmup_account_leased(alias, profile_path, lease).await?;
    drop(lease);
    Ok(())
}

#[cfg(test)]
pub(crate) async fn warmup_account_leased(
    alias: &str,
    profile_path: &Path,
    lease: crate::profile::ProfileLease,
) -> Result<crate::profile::ProfileLease> {
    if lease.alias() != alias {
        anyhow::bail!(
            "warmup for '{alias}' received profile lease for '{}'",
            lease.alias()
        );
    }
    let client = crate::auth::build_http_client()?;
    warmup_account_leased_with_client(alias, profile_path, lease, &client).await
}

#[cfg(test)]
pub(crate) async fn warmup_account_leased_with_client(
    alias: &str,
    profile_path: &Path,
    lease: crate::profile::ProfileLease,
    client: &reqwest::Client,
) -> Result<crate::profile::ProfileLease> {
    if lease.alias() != alias {
        anyhow::bail!(
            "warmup for '{alias}' received profile lease for '{}'",
            lease.alias()
        );
    }
    let initial_auth = crate::auth::read_auth_async(profile_path)
        .await
        .map_err(|e| anyhow::anyhow!("{alias}: cannot read auth: {e}"))?;
    let binding = crate::auth::account_info_from_auth_value(&initial_auth)
        .strict_binding()
        .with_context(|| {
            format!("{alias}: warmup requires a verified account id and email identity")
        })?;
    let cached_usage = crate::cache::get_bound_async(alias, &binding).await?;
    let mut side_effect_boundary = WarmupSideEffectBoundary::uncancellable();
    warmup_account_leased_with_client_from_usage_preflight(
        alias,
        profile_path,
        lease,
        client,
        WarmupPreflight {
            binding,
            initial_auth,
            cached_usage,
        },
        &mut NetworkBudget::new(first_network_permit(Arc::new(Semaphore::new(1)))),
        &mut side_effect_boundary,
    )
    .await
}

/// Warm an account after the caller has already completed an identity-bound
/// usage-cache preflight.
///
/// `cached_usage` must be the exact result of that preflight for
/// `expected_binding`: `Some` carries the proven cache hit and `None` carries a
/// proven miss. This path deliberately does not consult the cache again. It is
/// intended for callers such as the TUI that inspect a batch before acquiring
/// each profile lease.
pub(crate) async fn warmup_account_leased_with_client_after_usage_preflight(
    alias: &str,
    profile_path: &Path,
    lease: crate::profile::ProfileLease,
    client: &reqwest::Client,
    expected_binding: &crate::jwt::StrictAccountBinding,
    cached_usage: Option<crate::usage::UsageInfo>,
    first_permit: FirstNetworkPermit,
) -> Result<crate::profile::ProfileLease> {
    warmup_account_leased_with_client_after_usage_preflight_with_controls(
        alias,
        profile_path,
        lease,
        client,
        expected_binding,
        cached_usage,
        WarmupExecutionControls::uncancellable(first_permit),
    )
    .await
}

pub(crate) async fn warmup_account_leased_with_client_after_usage_preflight_with_controls(
    alias: &str,
    profile_path: &Path,
    lease: crate::profile::ProfileLease,
    client: &reqwest::Client,
    expected_binding: &crate::jwt::StrictAccountBinding,
    cached_usage: Option<crate::usage::UsageInfo>,
    controls: WarmupExecutionControls,
) -> Result<crate::profile::ProfileLease> {
    let WarmupExecutionControls {
        first_permit,
        mut side_effect_boundary,
    } = controls;
    if lease.alias() != alias {
        anyhow::bail!(
            "warmup for '{alias}' received profile lease for '{}'",
            lease.alias()
        );
    }
    let initial_auth = crate::auth::read_auth_async(profile_path)
        .await
        .map_err(|e| anyhow::anyhow!("{alias}: cannot read auth: {e}"))?;
    anyhow::ensure!(
        crate::auth::account_info_from_auth_value(&initial_auth).strict_binding()
            == Some(expected_binding.clone()),
        "{alias}: profile identity changed after warmup cache preflight"
    );
    let mut network = NetworkBudget::new(first_permit);
    warmup_account_leased_with_client_from_usage_preflight(
        alias,
        profile_path,
        lease,
        client,
        WarmupPreflight {
            binding: expected_binding.clone(),
            initial_auth,
            cached_usage,
        },
        &mut network,
        &mut side_effect_boundary,
    )
    .await
}

async fn warmup_account_leased_with_client_from_usage_preflight(
    alias: &str,
    profile_path: &Path,
    lease: crate::profile::ProfileLease,
    client: &reqwest::Client,
    preflight: WarmupPreflight,
    network: &mut NetworkBudget,
    side_effect_boundary: &mut WarmupSideEffectBoundary,
) -> Result<crate::profile::ProfileLease> {
    let WarmupPreflight {
        binding,
        initial_auth,
        cached_usage,
    } = preflight;
    let endpoints = crate::auth::service_endpoints()?;
    let (usage, auth) = match cached_usage {
        Some(usage) => (Some(usage), initial_auth),
        None => {
            let usage = match crate::usage::prepare_core_usage_unattended_with_existing_lease(
                alias,
                profile_path,
                &lease,
                &binding,
            )
            .await
            {
                Ok(prepared) => {
                    let result =
                        crate::usage::execute_prepared_core_usage_with_existing_lease_and_client(
                            prepared, &lease, client, network,
                        )
                        .await;
                    if network.first_wait_was_cancelled() {
                        return Err(crate::usage::network_wait_cancelled_error());
                    }
                    result
                }
                Err(error) => Err(error),
            };
            let usage = match usage {
                Ok(usage) => Some(usage),
                Err(error) => {
                    warn!(
                        "[{alias}] could not discover additional quota pools: {}",
                        error.summary
                    );
                    None
                }
            };
            let refreshed_auth = crate::auth::read_auth_async(profile_path)
                .await
                .map_err(|e| anyhow::anyhow!("{alias}: cannot read auth: {e}"))?;
            (usage, refreshed_auth)
        }
    };
    let additional_limits = usage
        .map(|usage| usage.additional_limits)
        .unwrap_or_default();
    let mut snapshot = WarmupCredentialSnapshot::from_auth(
        alias,
        auth,
        &binding,
        "profile identity changed during warmup preparation",
    )?;

    // Set when the pre-warmup proactive refresh below is rejected by the auth
    // server outright (e.g. `refresh_token_reused`): that refresh_token is now
    // permanently dead, so a later 401/403 must not spend a second round trip
    // replaying it — it can only re-trigger reuse detection.
    let mut rejected_refresh: Option<anyhow::Error> = None;

    // Pre-refresh: if token is about to expire, refresh proactively
    if let Some(refresh_token) = snapshot.refresh_token.clone()
        && crate::jwt::is_token_expiring(&snapshot.access_token, 60)? == Some(true)
    {
        debug!("[{alias}] access_token expiring soon, refreshing before warmup");
        let activation_authorization =
            crate::profile::authorize_fresh_credentials_activation(&lease).with_context(|| {
                format!(
                    "{alias}: token refresh was not started because exact live-auth activation could not be authorized"
                )
            })?;
        let refresh_result = crate::usage::do_refresh_token_with_network(
            &endpoints,
            alias,
            client,
            snapshot.id_token.as_deref(),
            &refresh_token,
            network,
        )
        .await;
        match refresh_result {
            Ok(resolution) => {
                crate::usage::persist_refresh_resolution(
                    &lease,
                    activation_authorization,
                    &refresh_token,
                    resolution,
                )
                .map_err(|error| anyhow::anyhow!(error.detail))?;
                let refreshed_auth =
                    crate::auth::read_auth_async(profile_path)
                        .await
                        .map_err(|error| {
                            anyhow::anyhow!("{alias}: cannot read refreshed auth: {error}")
                        })?;
                snapshot = WarmupCredentialSnapshot::from_auth(
                    alias,
                    refreshed_auth,
                    &binding,
                    "profile identity changed while refreshed credentials were persisted",
                )?;
            }
            Err(e) => {
                if e.downcast_ref::<crate::usage::RefreshOutcomeUnknown>()
                    .is_some()
                {
                    return Err(e.context(format!(
                        "{alias}: proactive token refresh outcome is unknown; warmup stopped without replaying the single-use credential"
                    )));
                }
                if e.downcast_ref::<crate::usage::TerminalAuthError>()
                    .is_some()
                {
                    warn!("[{alias}] pre-warmup token refresh rejected permanently: {e:#}");
                    rejected_refresh = Some(e);
                } else {
                    warn!("[{alias}] pre-warmup token refresh failed: {e}");
                }
            }
        }
    }

    // One `/models` answer covers both the main-pool request below and every
    // additional-pool request after it.
    let cache_key = warmup_cache_key(&binding, &additional_limits);
    let model_discovery = WarmupModelDiscoveryContext {
        alias,
        profile_path,
        expected_binding: &binding,
        endpoints: &endpoints,
        cache_key: &cache_key,
        client,
        additional_limits: &additional_limits,
    };
    let (mut lease, mut snapshot, selected_models) = discover_warmup_models_between_leases(
        &model_discovery,
        lease,
        snapshot,
        network,
        side_effect_boundary,
    )
    .await
    .with_context(|| format!("{alias}: failed to select a supported warmup model"))?;
    let model = selected_models.main.as_str();
    let body = build_body(model);

    debug!(
        "[{alias}] warmup POST → {} (model={model})",
        endpoints.responses()?
    );

    let response = send_warmup_request(
        &endpoints,
        client,
        snapshot.request_auth(),
        &body,
        "warmup request failed",
        network,
    )
    .await?;

    let status = response.status;
    debug!("[{alias}] warmup status: {status}");

    match status.as_u16() {
        200 => {
            finish_additional_warmup_with_one_model_refresh(
                &model_discovery,
                lease,
                snapshot,
                &selected_models.additional,
                network,
                side_effect_boundary,
            )
            .await
        }
        400 => {
            let text = response.error_text.unwrap_or_default();
            if text.contains("not supported") {
                // Model deprecated — clear cache, fetch fresh model list, retry once
                debug!(
                    "[{alias}] model {model:?} not supported, refreshing model cache and retrying"
                );
                model_cache_invalidate(&mut *MODEL_CACHE.lock().await, &cache_key);
                let (reacquired_lease, current_snapshot, refreshed_models) =
                    discover_warmup_models_between_leases(
                        &model_discovery,
                        lease,
                        snapshot,
                        network,
                        side_effect_boundary,
                    )
                    .await
                    .with_context(|| {
                        format!("{alias}: failed to refresh the supported warmup model")
                    })?;
                lease = reacquired_lease;
                snapshot = current_snapshot;
                let new_model = refreshed_models.main.as_str();
                let retry_body = build_body(new_model);
                let retry_response = send_warmup_request(
                    &endpoints,
                    client,
                    snapshot.request_auth(),
                    &retry_body,
                    "warmup retry failed",
                    network,
                )
                .await?;
                let retry_status = retry_response.status;
                if retry_status.is_success() {
                    return finish_additional_warmup_with_one_model_refresh(
                        &model_discovery,
                        lease,
                        snapshot,
                        &refreshed_models.additional,
                        network,
                        side_effect_boundary,
                    )
                    .await;
                }
                let retry_text = retry_response.error_text.unwrap_or_default();
                if retry_status == reqwest::StatusCode::BAD_REQUEST
                    && retry_text.contains("not supported")
                {
                    model_cache_invalidate(&mut *MODEL_CACHE.lock().await, &cache_key);
                }
                let snippet: String = retry_text.chars().take(160).collect();
                bail!("{alias}: HTTP {retry_status} after model refresh — {snippet}")
            }
            let snippet: String = text.chars().take(160).collect();
            bail!("{alias}: HTTP 400 — {snippet}")
        }
        401 | 403 => {
            // The pre-warmup proactive refresh already got a terminal rejection
            // from the auth server for this same refresh_token — retrying here
            // would just replay a dead credential and burn another round trip.
            if let Some(e) = rejected_refresh {
                return Err(e.context(format!(
                    "{alias}: authentication failed (HTTP {status}) after proactive token refresh was already rejected"
                )));
            }
            // Retry once with refreshed token
            if let Some(refresh_token) = snapshot.refresh_token.clone() {
                debug!("[{alias}] got {status}, attempting token refresh and retry");
                let activation_authorization =
                    crate::profile::authorize_fresh_credentials_activation(&lease).with_context(
                        || {
                            format!(
                                "{alias}: token refresh was not started because exact live-auth activation could not be authorized"
                            )
                        },
                    )?;
                let refresh_result = crate::usage::do_refresh_token_with_network(
                    &endpoints,
                    alias,
                    client,
                    snapshot.id_token.as_deref(),
                    &refresh_token,
                    network,
                )
                .await;
                match refresh_result {
                    Ok(resolution) => {
                        crate::usage::persist_refresh_resolution(
                            &lease,
                            activation_authorization,
                            &refresh_token,
                            resolution,
                        )
                        .map_err(|error| anyhow::anyhow!(error.detail))?;
                        let refreshed_auth = crate::auth::read_auth_async(profile_path)
                            .await
                            .map_err(|error| {
                                anyhow::anyhow!(
                                    "{alias}: cannot read refreshed auth after warmup retry: {error}"
                                )
                            })?;
                        snapshot = WarmupCredentialSnapshot::from_auth(
                            alias,
                            refreshed_auth,
                            &binding,
                            "profile identity changed while warmup retry credentials were persisted",
                        )?;
                        let retry_response = send_warmup_request(
                            &endpoints,
                            client,
                            snapshot.request_auth(),
                            &body,
                            "warmup retry failed",
                            network,
                        )
                        .await?;
                        let retry_status = retry_response.status;
                        if retry_status.is_success() {
                            return finish_additional_warmup_with_one_model_refresh(
                                &model_discovery,
                                lease,
                                snapshot,
                                &selected_models.additional,
                                network,
                                side_effect_boundary,
                            )
                            .await;
                        }
                        let retry_text = retry_response.error_text.unwrap_or_default();
                        if retry_status == reqwest::StatusCode::BAD_REQUEST
                            && retry_text.contains("not supported")
                        {
                            model_cache_invalidate(&mut *MODEL_CACHE.lock().await, &cache_key);
                        }
                        let snippet: String = retry_text.chars().take(160).collect();
                        bail!("{alias}: HTTP {retry_status} after token refresh retry — {snippet}")
                    }
                    Err(e) => bail!("{alias}: authentication failed and token refresh failed: {e}"),
                }
            }
            bail!(
                "{alias}: authentication failed — token may be expired (run `codex-switch-global-pace list` to refresh)"
            )
        }
        429 => bail!("{alias}: rate limited"),
        code => {
            let text = response.error_text.unwrap_or_default();
            let snippet: String = text.chars().take(160).collect();
            bail!("{alias}: HTTP {code} — {snippet}")
        }
    }
}

pub(crate) struct PreparedModelRequest {
    endpoints: crate::auth::ServiceEndpoints,
    access_token: String,
    account_id: Option<String>,
    is_fedramp: bool,
    network: NetworkBudget,
}

/// Resolve and, when necessary, durably refresh the credential needed for a
/// model-list request. The profile lease is consumed here and released before
/// a prepared value can reach the read-only HTTP phase.
pub(crate) async fn prepare_models_for_profile_leased_with_client(
    alias: &str,
    profile_path: &Path,
    lease: crate::profile::ProfileLease,
    expected_binding: &crate::jwt::StrictAccountBinding,
    client: &reqwest::Client,
    first_permit: FirstNetworkPermit,
) -> Result<PreparedModelRequest> {
    if lease.alias() != alias {
        anyhow::bail!(
            "model discovery for '{alias}' received profile lease for '{}'",
            lease.alias()
        );
    }
    let val = crate::auth::read_auth_async(profile_path)
        .await
        .map_err(|e| anyhow::anyhow!("{alias}: cannot read auth: {e}"))?;
    let info = crate::auth::account_info_from_auth_value(&val);
    let actual_binding = info
        .strict_binding()
        .context("profile auth is missing a complete account id and email identity")?;
    anyhow::ensure!(
        actual_binding == *expected_binding,
        "{alias}: profile identity changed while model discovery was waiting for its credential lease"
    );

    let endpoints = crate::auth::service_endpoints()?;
    let (at, rt) = crate::auth::extract_tokens(&val);
    let id_token = crate::auth::extract_id_token(&val);
    let mut access_token = at
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("{alias}: no access_token in profile"))?;
    let refresh_token = rt.filter(|s| !s.is_empty());

    let account_id = info.account_id;
    let is_fedramp = info.is_fedramp;
    let mut network = NetworkBudget::new(first_permit);

    if let Some(ref rt) = refresh_token
        && crate::jwt::is_token_expiring(&access_token, 60)? == Some(true)
    {
        let activation_authorization =
            crate::profile::authorize_fresh_credentials_activation(&lease).with_context(|| {
                format!(
                    "{alias}: token refresh was not started because exact live-auth activation could not be authorized"
                )
            })?;
        let refresh_result = crate::usage::do_refresh_token_with_network(
            &endpoints,
            alias,
            client,
            id_token.as_deref(),
            rt,
            &mut network,
        )
        .await;
        match refresh_result {
            Ok(resolution) => {
                // No degrade here: the refresh *worked*, so the old token this
                // would fall back to has already been invalidated server-side.
                let refreshed = crate::usage::persist_refresh_resolution(
                    &lease,
                    activation_authorization,
                    rt,
                    resolution,
                )
                .map_err(|error| anyhow::anyhow!(error.detail))?;
                access_token = refreshed.access_token;
            }
            // Deliberate degrade: fall through and try /models with the
            // existing (possibly expiring) token rather than failing here.
            // Still worth a diagnosable trace — silently swallowing this
            // sent people chasing an unrelated /models error instead of the
            // real cause (a rejected/expired refresh_token).
            Err(e)
                if e.downcast_ref::<crate::usage::RefreshOutcomeUnknown>()
                    .is_some() =>
            {
                return Err(e.context(format!(
                    "{alias}: proactive token refresh outcome is unknown; model discovery stopped without replaying the single-use credential"
                )));
            }
            Err(e) => warn!(
                "[{alias}] proactive token refresh failed, continuing with existing token: {e:#}"
            ),
        }
    }

    // No credential mutation remains after this point. Releasing the profile
    // boundary before the read-only request keeps a slow model endpoint from
    // delaying an unrelated switch, rename, or delete of this alias.
    drop(lease);

    Ok(PreparedModelRequest {
        endpoints,
        access_token,
        account_id,
        is_fedramp,
        network,
    })
}

/// Execute only the safe-to-cancel, read-only portion of model discovery.
pub(crate) async fn fetch_prepared_models_with_client(
    prepared: PreparedModelRequest,
    client: &reqwest::Client,
) -> Result<Vec<ModelEntry>> {
    let PreparedModelRequest {
        endpoints,
        access_token,
        account_id,
        is_fedramp,
        mut network,
    } = prepared;
    fetch_models(
        &endpoints,
        client,
        WarmupRequestAuth {
            access_token: &access_token,
            account_id: account_id.as_deref(),
            is_fedramp,
        },
        &mut network,
    )
    .await
}

#[cfg(test)]
async fn fetch_models_for_profile_leased_with_client(
    alias: &str,
    profile_path: &Path,
    lease: crate::profile::ProfileLease,
    expected_binding: &crate::jwt::StrictAccountBinding,
    client: &reqwest::Client,
) -> Result<Vec<ModelEntry>> {
    let prepared = prepare_models_for_profile_leased_with_client(
        alias,
        profile_path,
        lease,
        expected_binding,
        client,
        first_network_permit(Arc::new(Semaphore::new(1))),
    )
    .await?;
    fetch_prepared_models_with_client(prepared, client).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_first_network_permit() -> FirstNetworkPermit {
        // A single slot makes accidental permit retention deterministic in
        // tests while exercising the same production acquisition path.
        first_network_permit(Arc::new(Semaphore::new(1)))
    }

    fn cache_binding(account_id: &str, email: &str) -> crate::jwt::StrictAccountBinding {
        crate::jwt::StrictAccountBinding {
            account_id: account_id.to_string(),
            email: email.to_string(),
        }
    }

    fn test_selection(main: &str, additional: &[(&str, &str)]) -> WarmupModelSelection {
        WarmupModelSelection {
            main: main.to_string(),
            additional: additional
                .iter()
                .map(|(pool, model)| SelectedPoolModel {
                    pool_keys: vec![normalized_pool_name(pool)],
                    model: (*model).to_string(),
                })
                .collect(),
        }
    }

    #[test]
    fn test_model_cache_keys_are_isolated_per_account() {
        let account_a = warmup_cache_key(&cache_binding("account-a", "a@example.com"), &[]);
        let account_b = warmup_cache_key(&cache_binding("account-b", "b@example.com"), &[]);
        let mut cache = HashMap::new();
        model_cache_set(&mut cache, &account_a, test_selection("model-a", &[]));

        assert_eq!(
            model_cache_get(&cache, &account_a),
            Some(test_selection("model-a", &[]))
        );
        assert_eq!(model_cache_get(&cache, &account_b), None);
    }

    #[test]
    fn cached_models_are_not_reused_after_same_alias_account_rebind() {
        let previous_owner = warmup_cache_key(&cache_binding("account-a", "old@example.com"), &[]);
        let rebound_owner = warmup_cache_key(&cache_binding("account-b", "new@example.com"), &[]);
        let mut cache = HashMap::new();
        model_cache_set(
            &mut cache,
            &previous_owner,
            test_selection("old-owner-model", &[]),
        );

        assert_eq!(model_cache_get(&cache, &rebound_owner), None);
        assert_eq!(
            model_cache_get(&cache, &previous_owner),
            Some(test_selection("old-owner-model", &[])),
            "rebinding an alias must isolate the new owner without mutating the old owner's entry"
        );
    }

    #[test]
    fn test_model_cache_invalidation_only_affects_target_key() {
        let account_a = warmup_cache_key(&cache_binding("account-a", "a@example.com"), &[]);
        let account_b = warmup_cache_key(&cache_binding("account-b", "b@example.com"), &[]);
        let mut cache = HashMap::new();
        model_cache_set(&mut cache, &account_a, test_selection("model-a", &[]));
        model_cache_set(&mut cache, &account_b, test_selection("model-b", &[]));

        model_cache_invalidate(&mut cache, &account_a);

        assert_eq!(model_cache_get(&cache, &account_a), None);
        assert_eq!(
            model_cache_get(&cache, &account_b),
            Some(test_selection("model-b", &[]))
        );
    }

    /// The cache holds the whole resolved set, so an additional-pool model
    /// survives alongside the main one and the second `/models` fetch that used
    /// to retrieve it is unnecessary.
    #[test]
    fn test_model_cache_round_trips_the_whole_selected_set() {
        let key = warmup_cache_key(&cache_binding("account-a", "a@example.com"), &[]);
        let mut cache = HashMap::new();
        let selected = test_selection("gpt-5-mini", &[("gpt-5-spark", "gpt-5-spark")]);
        model_cache_set(&mut cache, &key, selected.clone());

        let cached = model_cache_get(&cache, &key).expect("entry must round-trip");
        assert_eq!(cached, selected);
    }

    fn model_pool(limit_name: &str) -> crate::usage::AdditionalRateLimit {
        crate::usage::AdditionalRateLimit {
            limit_name: Some(limit_name.to_string()),
            metered_feature: Some("codex_mini".to_string()),
            allowed: Some(true),
            limit_reached: Some(false),
            primary: None,
            secondary: None,
        }
    }

    #[test]
    fn cache_key_separates_accounts_that_share_a_pool_set() {
        assert_ne!(
            warmup_cache_key(
                &cache_binding("account-a", "a@example.com"),
                &[model_pool("gpt-5-mini")]
            ),
            warmup_cache_key(
                &cache_binding("account-b", "b@example.com"),
                &[model_pool("gpt-5-mini")]
            )
        );
    }

    /// A changed pool set must produce a different key — that miss is the only
    /// thing that re-resolves the model list for a long-running daemon.
    #[test]
    fn cache_key_changes_when_a_pool_is_added() {
        let binding = cache_binding("account-a", "a@example.com");
        let before = warmup_cache_key(&binding, &[]);
        let after = warmup_cache_key(&binding, &[model_pool("gpt-5-mini")]);
        assert_ne!(before, after);
    }

    /// The mirror image: upstream reordering the same pools must not throw away
    /// a perfectly good entry and buy a `/models` round trip per warmup.
    #[test]
    fn cache_key_ignores_pool_order() {
        let binding = cache_binding("account-a", "a@example.com");
        let one = warmup_cache_key(
            &binding,
            &[model_pool("gpt-5-mini"), model_pool("gpt-5-spark")],
        );
        let other = warmup_cache_key(
            &binding,
            &[model_pool("gpt-5-spark"), model_pool("gpt-5-mini")],
        );
        assert_eq!(one, other);
    }

    #[test]
    fn cache_key_treats_normalized_pools_as_a_set() {
        let binding = cache_binding("account-a", "a@example.com");
        assert_eq!(
            warmup_cache_key(&binding, &[model_pool("GPT-5 Mini")]),
            warmup_cache_key(
                &binding,
                &[model_pool("gpt_5-mini"), model_pool("GPT-5 Mini")]
            )
        );
    }

    /// Pools that `select_warmup_models` never acts on must not perturb the key
    /// either, or an unrelated non-model quota would invalidate a good entry.
    #[test]
    fn cache_key_ignores_pools_that_are_not_warmed() {
        let binding = cache_binding("account-a", "a@example.com");
        let non_model = crate::usage::AdditionalRateLimit {
            metered_feature: Some("code_review".to_string()),
            ..model_pool("Code review")
        };
        let exhausted = crate::usage::AdditionalRateLimit {
            limit_reached: Some(true),
            ..model_pool("gpt-5-spark")
        };
        assert_eq!(
            warmup_cache_key(&binding, &[model_pool("gpt-5-mini")]),
            warmup_cache_key(&binding, &[model_pool("gpt-5-mini"), non_model, exhausted])
        );
    }

    #[test]
    fn test_parse_models_body_full_entry() {
        let body = serde_json::json!({
            "models": [{
                "slug": "gpt-5.3-codex",
                "display_name": "GPT-5.3 Codex",
                "description": "Best for coding",
                "visibility": "List",
                "priority": 1,
                "supported_in_api": true,
                "context_window": 128000,
                "default_reasoning_level": "medium",
                "supported_reasoning_levels": [
                    {"effort": "low"},
                    {"reasoning_effort": "high"}
                ],
                "input_modalities": ["text", "image"],
                "additional_speed_tiers": ["fast"],
                "service_tiers": [{"id": "fast"}],
                "default_service_tier": "fast",
                "max_context_window": 256000,
                "auto_compact_token_limit": 110000,
                "effective_context_window_percent": 95,
                "supports_parallel_tool_calls": true,
                "supports_image_detail_original": true,
                "experimental_supported_tools": ["computer"],
                "supports_search_tool": true,
                "use_responses_lite": false
            }]
        });

        let models = parse_models_body(&body).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(
            models[0],
            ModelEntry {
                slug: "gpt-5.3-codex".to_string(),
                display_name: Some("GPT-5.3 Codex".to_string()),
                description: Some("Best for coding".to_string()),
                visibility: Some("List".to_string()),
                priority: Some(1),
                supported_in_api: Some(true),
                context_window: Some(128000),
                default_reasoning_effort: Some("medium".to_string()),
                supported_reasoning_efforts: vec!["low".to_string(), "high".to_string()],
                input_modalities: vec!["text".to_string(), "image".to_string()],
                additional_speed_tiers: vec!["fast".to_string()],
                service_tiers: vec!["fast".to_string()],
                default_service_tier: Some("fast".to_string()),
                max_context_window: Some(256000),
                auto_compact_token_limit: Some(110000),
                effective_context_window_percent: Some(95),
                supports_parallel_tool_calls: Some(true),
                supports_image_detail_original: Some(true),
                experimental_supported_tools: vec!["computer".to_string()],
                supports_search_tool: Some(true),
                use_responses_lite: Some(false),
            }
        );
    }

    #[test]
    fn test_parse_models_body_missing_optional_fields() {
        let body = serde_json::json!({
            "models": [{"slug": "gpt-5-mini"}]
        });

        let models = parse_models_body(&body).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].slug, "gpt-5-mini");
        assert_eq!(models[0].display_name, None);
        assert_eq!(models[0].visibility, None);
        assert_eq!(models[0].priority, None);
        assert_eq!(models[0].supported_in_api, None);
        assert_eq!(models[0].context_window, None);
    }

    #[test]
    fn test_parse_models_body_empty_list() {
        let body = serde_json::json!({"models": []});
        let models = parse_models_body(&body).unwrap();
        assert!(models.is_empty());
    }

    #[test]
    fn test_parse_models_body_missing_array_errors() {
        let body = serde_json::json!({});
        assert!(parse_models_body(&body).is_err());
    }

    #[test]
    fn test_sorted_models_for_display_orders_by_priority_ascending() {
        let models = vec![
            ModelEntry {
                slug: "b".to_string(),
                display_name: None,
                visibility: None,
                priority: Some(3),
                ..Default::default()
            },
            ModelEntry {
                slug: "a".to_string(),
                display_name: None,
                visibility: None,
                priority: Some(1),
                ..Default::default()
            },
            ModelEntry {
                slug: "c-no-priority".to_string(),
                display_name: None,
                visibility: None,
                priority: None,
                ..Default::default()
            },
        ];

        let sorted = sorted_models_for_display(&models);
        let slugs: Vec<&str> = sorted.iter().map(|m| m.slug.as_str()).collect();
        assert_eq!(slugs, vec!["a", "b", "c-no-priority"]);
    }

    #[test]
    fn test_sorted_models_for_display_empty_list() {
        assert!(sorted_models_for_display(&[]).is_empty());
    }

    #[test]
    fn test_warmup_models_include_main_pool_and_spark_pool() {
        let models = vec![
            ModelEntry {
                slug: "gpt-5.4-mini".to_string(),
                display_name: None,
                visibility: Some("List".to_string()),
                priority: Some(10),
                supported_in_api: Some(true),
                ..Default::default()
            },
            ModelEntry {
                slug: "gpt-5.3-codex-spark".to_string(),
                display_name: None,
                visibility: Some("List".to_string()),
                priority: Some(26),
                supported_in_api: Some(false),
                ..Default::default()
            },
        ];
        let limits = vec![crate::usage::AdditionalRateLimit {
            limit_name: Some("GPT-5.3-Codex-Spark".to_string()),
            metered_feature: Some("codex_bengalfox".to_string()),
            ..Default::default()
        }];

        assert_eq!(
            select_warmup_models(&models, &limits).unwrap(),
            test_selection(
                "gpt-5.4-mini",
                &[("GPT-5.3-Codex-Spark", "gpt-5.3-codex-spark")]
            )
        );
    }

    #[test]
    fn test_warmup_models_exclude_disallowed_additional_pool() {
        let models = vec![
            ModelEntry {
                slug: "gpt-5.4-mini".to_string(),
                visibility: Some("List".to_string()),
                supported_in_api: Some(true),
                ..Default::default()
            },
            ModelEntry {
                slug: "gpt-5.3-codex-spark".to_string(),
                visibility: Some("List".to_string()),
                supported_in_api: Some(true),
                ..Default::default()
            },
        ];
        let limits = vec![crate::usage::AdditionalRateLimit {
            limit_name: Some("GPT-5.3-Codex-Spark".to_string()),
            metered_feature: Some("codex_bengalfox".to_string()),
            allowed: Some(false),
            limit_reached: Some(false),
            ..Default::default()
        }];

        assert_eq!(
            select_warmup_models(&models, &limits).unwrap(),
            test_selection("gpt-5.4-mini", &[])
        );
    }

    #[test]
    fn test_warmup_models_exclude_exhausted_additional_pool() {
        let models = vec![
            ModelEntry {
                slug: "gpt-5.4-mini".to_string(),
                visibility: Some("List".to_string()),
                supported_in_api: Some(true),
                ..Default::default()
            },
            ModelEntry {
                slug: "gpt-5.3-codex-spark".to_string(),
                visibility: Some("List".to_string()),
                supported_in_api: Some(true),
                ..Default::default()
            },
        ];
        let limits = vec![crate::usage::AdditionalRateLimit {
            limit_name: Some("GPT-5.3-Codex-Spark".to_string()),
            metered_feature: Some("codex_bengalfox".to_string()),
            allowed: Some(true),
            limit_reached: Some(true),
            ..Default::default()
        }];

        assert_eq!(
            select_warmup_models(&models, &limits).unwrap(),
            test_selection("gpt-5.4-mini", &[])
        );
    }

    #[test]
    fn test_warmup_models_do_not_use_spark_as_the_main_pool_fallback() {
        let models = vec![
            ModelEntry {
                slug: "gpt-5.6-codex".to_string(),
                visibility: Some("List".to_string()),
                priority: Some(10),
                supported_in_api: Some(true),
                ..Default::default()
            },
            ModelEntry {
                slug: "gpt-5.3-codex-spark".to_string(),
                visibility: Some("List".to_string()),
                priority: Some(1),
                supported_in_api: Some(true),
                ..Default::default()
            },
        ];
        let limits = vec![crate::usage::AdditionalRateLimit {
            limit_name: Some("GPT-5.3-Codex-Spark".to_string()),
            metered_feature: Some("codex_bengalfox".to_string()),
            ..Default::default()
        }];

        assert_eq!(
            select_warmup_models(&models, &limits).unwrap(),
            test_selection(
                "gpt-5.6-codex",
                &[("GPT-5.3-Codex-Spark", "gpt-5.3-codex-spark")]
            )
        );
    }

    #[test]
    fn test_warmup_models_require_a_distinct_main_pool_model() {
        let models = vec![ModelEntry {
            slug: "gpt-5.3-codex-spark".to_string(),
            visibility: Some("List".to_string()),
            priority: Some(1),
            supported_in_api: Some(true),
            ..Default::default()
        }];
        let limits = vec![crate::usage::AdditionalRateLimit {
            limit_name: Some("GPT-5.3-Codex-Spark".to_string()),
            metered_feature: Some("codex_bengalfox".to_string()),
            ..Default::default()
        }];

        let error = select_warmup_models(&models, &limits).unwrap_err();

        assert!(error.to_string().contains("no main-pool model"));
    }

    #[test]
    fn test_warmup_models_cover_all_matching_model_quota_pools() {
        let models = vec![
            ModelEntry {
                slug: "gpt-5.4-mini".to_string(),
                display_name: Some("GPT-5.4 Mini".to_string()),
                visibility: Some("List".to_string()),
                priority: Some(10),
                supported_in_api: Some(true),
                ..Default::default()
            },
            ModelEntry {
                slug: "gpt-5.3-codex-spark".to_string(),
                display_name: Some("GPT-5.3-Codex-Spark".to_string()),
                visibility: Some("List".to_string()),
                priority: Some(2),
                supported_in_api: Some(true),
                ..Default::default()
            },
            ModelEntry {
                slug: "gpt-6-codex-burst".to_string(),
                display_name: Some("GPT-6 Codex Burst".to_string()),
                visibility: Some("List".to_string()),
                priority: Some(1),
                supported_in_api: Some(true),
                ..Default::default()
            },
        ];
        let limits = vec![
            crate::usage::AdditionalRateLimit {
                limit_name: Some("GPT-5.3-Codex-Spark".to_string()),
                metered_feature: Some("codex_bengalfox".to_string()),
                ..Default::default()
            },
            crate::usage::AdditionalRateLimit {
                limit_name: Some("GPT-6-Codex-Burst".to_string()),
                metered_feature: Some("codex_futureburst".to_string()),
                ..Default::default()
            },
        ];

        assert_eq!(
            select_warmup_models(&models, &limits).unwrap(),
            test_selection(
                "gpt-5.4-mini",
                &[
                    ("GPT-5.3-Codex-Spark", "gpt-5.3-codex-spark"),
                    ("GPT-6-Codex-Burst", "gpt-6-codex-burst"),
                ]
            )
        );
    }

    #[test]
    fn test_warmup_models_group_pools_that_share_one_model_request() {
        let models = vec![
            ModelEntry {
                slug: "gpt-main-mini".to_string(),
                visibility: Some("List".to_string()),
                supported_in_api: Some(true),
                ..Default::default()
            },
            ModelEntry {
                slug: "spark-fast".to_string(),
                visibility: Some("List".to_string()),
                supported_in_api: Some(true),
                ..Default::default()
            },
        ];
        let limits = vec![
            crate::usage::AdditionalRateLimit {
                limit_name: Some("Spark".to_string()),
                metered_feature: Some("codex_spark".to_string()),
                ..Default::default()
            },
            crate::usage::AdditionalRateLimit {
                limit_name: Some("Spark Fast".to_string()),
                metered_feature: Some("codex_spark_fast".to_string()),
                ..Default::default()
            },
        ];

        assert_eq!(
            select_warmup_models(&models, &limits).unwrap(),
            WarmupModelSelection {
                main: "gpt-main-mini".to_string(),
                additional: vec![SelectedPoolModel {
                    pool_keys: vec!["spark".to_string(), "sparkfast".to_string()],
                    model: "spark-fast".to_string(),
                }],
            }
        );
    }

    #[test]
    fn test_unmatched_model_quota_pool_is_reported() {
        let models = vec![ModelEntry {
            slug: "gpt-5.4-mini".to_string(),
            display_name: Some("GPT-5.4 Mini".to_string()),
            visibility: Some("List".to_string()),
            supported_in_api: Some(true),
            ..Default::default()
        }];
        let limits = vec![crate::usage::AdditionalRateLimit {
            limit_name: Some("GPT-6-Codex-Burst".to_string()),
            metered_feature: Some("codex_futureburst".to_string()),
            ..Default::default()
        }];

        let error = select_warmup_models(&models, &limits).unwrap_err();
        assert!(error.to_string().contains("GPT-6-Codex-Burst"));
    }

    #[test]
    fn test_model_fetch_failure_is_not_replaced_with_a_hardcoded_model() {
        let error = require_official_model::<WarmupModelSelection>(Err(anyhow::anyhow!(
            "models endpoint unavailable"
        )))
        .unwrap_err();

        assert!(error.to_string().contains("models endpoint unavailable"));
        assert!(!error.to_string().contains("gpt-5.3-codex"));
    }

    #[test]
    fn test_models_request_uses_one_protocol_version_and_routing_headers() {
        let endpoints = crate::auth::ServiceEndpoints::production_for_test();
        let request = build_models_request(
            &endpoints,
            &reqwest::Client::new(),
            "access-token",
            Some("workspace-123"),
            true,
        )
        .unwrap()
        .build()
        .unwrap();

        assert!(
            request
                .url()
                .query_pairs()
                .any(|(key, value)| { key == "client_version" && value == "0.149.0" })
        );

        assert_eq!(
            request
                .headers()
                .get("ChatGPT-Account-ID")
                .and_then(|value| value.to_str().ok()),
            Some("workspace-123")
        );
        assert_eq!(
            request
                .headers()
                .get("X-OpenAI-Fedramp")
                .and_then(|value| value.to_str().ok()),
            Some("true")
        );
    }

    #[test]
    fn test_responses_request_includes_workspace_and_fedramp_headers() {
        let endpoints = crate::auth::ServiceEndpoints::production_for_test();
        let request = make_request(
            &endpoints,
            &reqwest::Client::new(),
            WarmupRequestAuth {
                access_token: "access-token",
                account_id: Some("workspace-123"),
                is_fedramp: true,
            },
            &build_body("gpt-test"),
        )
        .unwrap()
        .build()
        .unwrap();

        assert_eq!(
            request
                .headers()
                .get("ChatGPT-Account-ID")
                .and_then(|value| value.to_str().ok()),
            Some("workspace-123")
        );
        assert_eq!(
            request
                .headers()
                .get("X-OpenAI-Fedramp")
                .and_then(|value| value.to_str().ok()),
            Some("true")
        );
    }

    // ── Terminal refresh failure must not be replayed ──────────────────
    //
    // The debug-only endpoint context reads `CS_TOKEN_URL`, `CS_MODELS_URL`
    // and `CS_RESPONSES_URL`, which are process-global; login's tests retarget
    // `CS_TOKEN_URL` as well. Every test in this group therefore takes the
    // crate-wide `auth::URL_ENV_LOCK` rather than a module-private lock.
    mod refresh_short_circuit {
        use super::*;
        use axum::http::StatusCode;
        use axum::response::IntoResponse;
        use axum::routing::{get, post};
        use axum::{Json, Router};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        use crate::auth::URL_ENV_LOCK as ENV_LOCK;

        fn run_profile_url_test<F, Fut>(test: F)
        where
            F: FnOnce() -> Fut,
            Fut: std::future::Future<Output = ()>,
        {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let _url_env_lock = runtime.block_on(ENV_LOCK.lock());
            let _profile_env_lock = crate::profile::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            runtime.block_on(test());
        }

        struct EnvVarGuard {
            key: &'static str,
            previous: Option<String>,
        }

        impl EnvVarGuard {
            fn set(key: &'static str, value: &str) -> Self {
                let previous = std::env::var(key).ok();
                unsafe {
                    std::env::set_var(key, value);
                }
                Self { key, previous }
            }
        }

        impl Drop for EnvVarGuard {
            fn drop(&mut self) {
                unsafe {
                    match &self.previous {
                        Some(value) => std::env::set_var(self.key, value),
                        None => std::env::remove_var(self.key),
                    }
                }
            }
        }

        fn use_test_home(path: &std::path::Path) -> EnvVarGuard {
            let guard = EnvVarGuard::set("CODEX_SWITCH_HOME", &path.display().to_string());
            crate::config::init_defaults_for_tests();
            guard
        }

        fn make_jwt(claims: &serde_json::Value) -> String {
            use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
            let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).unwrap());
            format!("header.{payload}.signature")
        }

        fn test_id_token() -> String {
            make_jwt(&serde_json::json!({
                "email": "warmup-test@example.com",
                "https://api.openai.com/auth": {
                    "chatgpt_account_id": "acct-warmup-test"
                }
            }))
        }

        /// An access_token JWT that `is_token_expiring` already treats as expired,
        /// so `warmup_account`'s pre-warmup proactive refresh always fires.
        fn expired_access_token() -> String {
            make_jwt(&serde_json::json!({ "exp": crate::auth::now_unix_secs().unwrap() - 10 }))
        }

        fn write_test_auth(path: &std::path::Path, access_token: &str, refresh_token: &str) {
            write_test_auth_for_identity(
                path,
                access_token,
                refresh_token,
                "acct-warmup-test",
                "warmup-test@example.com",
            );
        }

        fn write_test_auth_for_identity(
            path: &std::path::Path,
            access_token: &str,
            refresh_token: &str,
            account_id: &str,
            email: &str,
        ) {
            let val = serde_json::json!({
                "tokens": {
                    "id_token": make_jwt(&serde_json::json!({
                        "email": email,
                        "https://api.openai.com/auth": {
                            "chatgpt_account_id": account_id
                        }
                    })),
                    "access_token": access_token,
                    "refresh_token": refresh_token,
                }
            });
            crate::auth::write_auth(path, &val)
                .unwrap()
                .assert_durably_published();
        }

        fn strict_binding_for_profile(
            profile_path: &std::path::Path,
        ) -> crate::jwt::StrictAccountBinding {
            crate::auth::account_info_from_auth_value(
                &crate::auth::read_auth(profile_path).unwrap(),
            )
            .strict_binding()
            .expect("the staged test profile has a strict account identity")
        }

        fn cache_usage_for_profile(
            alias: &str,
            profile_path: &std::path::Path,
            usage: &crate::usage::UsageInfo,
        ) {
            let auth = crate::auth::read_auth(profile_path).unwrap();
            let binding = crate::auth::account_info_from_auth_value(&auth)
                .strict_binding()
                .expect("the staged test profile has a strict account identity");
            crate::cache::put_bound_versioned(alias, &binding, usage).unwrap();
        }

        #[test]
        fn post_revalidation_rejects_a_token_that_is_no_longer_fresh() {
            let auth = serde_json::json!({
                "tokens": {
                    "id_token": test_id_token(),
                    "access_token": expired_access_token(),
                    "refresh_token": "refresh-token"
                }
            });
            let binding = crate::auth::account_info_from_auth_value(&auth)
                .strict_binding()
                .unwrap();
            let snapshot = WarmupCredentialSnapshot::from_auth(
                "freshness-test",
                auth,
                &binding,
                "test identity changed",
            )
            .unwrap();

            let error = snapshot
                .ensure_fresh_for_post("freshness-test")
                .expect_err("an expiring token must not authorize a warmup POST");
            assert!(
                error.to_string().contains("too close to expiry"),
                "freshness failure should identify the post-discovery boundary: {error:#}"
            );
        }

        #[allow(clippy::await_holding_lock)]
        #[tokio::test]
        async fn post_revalidation_rejects_same_identity_credential_rotation() {
            let _profile_env_lock = crate::profile::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let home = crate::fs_ops::create_direct_tempdir().unwrap();
            let _codex_switch_home = use_test_home(home.path());
            let alias = "warmup-snapshot-rotation";
            let profile_path = home.path().join("profiles").join(alias).join("auth.json");
            let first_access = make_jwt(&serde_json::json!({
                "exp": crate::auth::now_unix_secs().unwrap() + 3_600
            }));
            write_test_auth(&profile_path, &first_access, "refresh-token-before");
            let auth = crate::auth::read_auth(&profile_path).unwrap();
            let binding = crate::auth::account_info_from_auth_value(&auth)
                .strict_binding()
                .unwrap();
            let snapshot =
                WarmupCredentialSnapshot::from_auth(alias, auth, &binding, "test identity changed")
                    .unwrap();

            let rotated_access = make_jwt(&serde_json::json!({
                "exp": crate::auth::now_unix_secs().unwrap() + 7_200
            }));
            write_test_auth(&profile_path, &rotated_access, "refresh-token-after");
            let error = reacquire_warmup_snapshot_after_model_discovery(
                alias,
                &profile_path,
                &binding,
                &snapshot,
            )
            .await
            .map(|(lease, _)| drop(lease))
            .expect_err("credential rotation must invalidate the prepared warmup snapshot");

            assert!(
                error
                    .to_string()
                    .contains("credentials changed during model discovery"),
                "same-identity rotation should fail the exact snapshot check: {error:#}"
            );
            assert_eq!(
                crate::auth::extract_tokens(&crate::auth::read_auth(&profile_path).unwrap()).0,
                Some(rotated_access),
                "snapshot validation must not overwrite the concurrently rotated credential"
            );
        }

        /// Starts a mock server answering all three warmup-relevant endpoints and
        /// points `CS_TOKEN_URL` / `CS_MODELS_URL` / `CS_RESPONSES_URL` at it.
        /// Returns the request counters and the env guards (drop order keeps the
        /// guards alive for the caller's whole test).
        async fn start_mock_server(
            token_status: StatusCode,
            token_body: serde_json::Value,
            responses_status: StatusCode,
        ) -> (Arc<AtomicUsize>, Vec<EnvVarGuard>) {
            let token_calls = Arc::new(AtomicUsize::new(0));
            let counter = token_calls.clone();

            let app = Router::new()
                .route(
                    "/oauth/token",
                    post(move || {
                        let counter = counter.clone();
                        let body = token_body.clone();
                        async move {
                            counter.fetch_add(1, Ordering::SeqCst);
                            (token_status, Json(body))
                        }
                    }),
                )
                .route(
                    "/codex/models",
                    get(|| async {
                        (
                            StatusCode::OK,
                            Json(serde_json::json!({
                                "models": [{"slug": "gpt-5-mini", "supported_in_api": true}]
                            })),
                        )
                    }),
                )
                .route(
                    "/codex/responses",
                    post(move || async move { (responses_status, "") }),
                );

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });

            let guards = vec![
                EnvVarGuard::set("CS_TOKEN_URL", &format!("http://{addr}/oauth/token")),
                EnvVarGuard::set("CS_MODELS_URL", &format!("http://{addr}/codex/models")),
                EnvVarGuard::set(
                    "CS_RESPONSES_URL",
                    &format!("http://{addr}/codex/responses"),
                ),
            ];
            (token_calls, guards)
        }

        #[allow(clippy::await_holding_lock)]
        #[tokio::test]
        async fn model_resolution_for_different_accounts_fetches_concurrently() {
            let _lock = ENV_LOCK.lock().await;
            let (arrival_tx, mut arrival_rx) = tokio::sync::mpsc::unbounded_channel();
            let release_first = Arc::new(tokio::sync::Semaphore::new(0));

            let app = Router::new().route(
                "/codex/models",
                get({
                    let release_first = release_first.clone();
                    move |headers: axum::http::HeaderMap| {
                        let arrival_tx = arrival_tx.clone();
                        let release_first = release_first.clone();
                        async move {
                            let account_id = headers
                                .get("chatgpt-account-id")
                                .and_then(|value| value.to_str().ok())
                                .unwrap_or_default()
                                .to_string();
                            arrival_tx.send(account_id.clone()).unwrap();
                            if account_id == "workspace-one" {
                                let _permit = release_first.acquire().await.unwrap();
                            }
                            (
                                StatusCode::OK,
                                Json(serde_json::json!({
                                    "models": [{
                                        "slug": "gpt-5-mini",
                                        "supported_in_api": true
                                    }]
                                })),
                            )
                        }
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let _models_url =
                EnvVarGuard::set("CS_MODELS_URL", &format!("http://{addr}/codex/models"));

            let endpoints = crate::auth::service_endpoints().unwrap();
            let client = reqwest::Client::new();
            let first_client = client.clone();
            let first_endpoints = endpoints.clone();
            let first_cache_key =
                warmup_cache_key(&cache_binding("workspace-one", "one@example.com"), &[]);
            let first = tokio::spawn(async move {
                let mut network = NetworkBudget::new(test_first_network_permit());
                resolve_warmup_models(
                    &first_endpoints,
                    &first_cache_key,
                    &first_client,
                    WarmupRequestAuth {
                        access_token: "token-one",
                        account_id: Some("workspace-one"),
                        is_fedramp: false,
                    },
                    &[],
                    &mut network,
                )
                .await
            });
            assert_eq!(arrival_rx.recv().await.as_deref(), Some("workspace-one"));

            let second_cache_key =
                warmup_cache_key(&cache_binding("workspace-two", "two@example.com"), &[]);
            let second = tokio::spawn(async move {
                let mut network = NetworkBudget::new(test_first_network_permit());
                resolve_warmup_models(
                    &endpoints,
                    &second_cache_key,
                    &client,
                    WarmupRequestAuth {
                        access_token: "token-two",
                        account_id: Some("workspace-two"),
                        is_fedramp: false,
                    },
                    &[],
                    &mut network,
                )
                .await
            });
            let second_arrived_in_parallel =
                tokio::time::timeout(std::time::Duration::from_millis(300), arrival_rx.recv())
                    .await
                    .is_ok();

            release_first.add_permits(1);
            first.await.unwrap().unwrap();
            second.await.unwrap().unwrap();

            assert!(
                second_arrived_in_parallel,
                "a slow /models fetch for one account must not block another account"
            );
        }

        // `CODEX_SWITCH_HOME` is also mutated by `profile::tests` under its own
        // `TEST_ENV_LOCK` — take that lock too for the whole test body, or the two
        // test modules race on the same process-global env var when the harness
        // runs them in parallel. Holding it across `.await` is safe here: these
        // are `#[tokio::test]` current-thread runtimes, so no other task on this
        // thread ever needs the lock back before the test finishes.
        #[allow(clippy::await_holding_lock)]
        #[tokio::test]
        async fn terminal_pre_refresh_failure_is_not_replayed_on_the_401_retry() {
            let _lock = ENV_LOCK.lock().await;
            let _profile_env_lock = crate::profile::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let home = crate::fs_ops::create_direct_tempdir().unwrap();
            let _codex_switch_home = use_test_home(home.path());

            let alias = "terminal-refresh-test";
            let profile_path = home.path().join("profiles").join(alias).join("auth.json");
            write_test_auth(&profile_path, &expired_access_token(), "refresh-token-1");
            // Pre-populate the usage cache so `warmup_account` never calls the
            // (unrelated) usage-fetch path, which has its own independent
            // proactive-refresh call — that would inflate the auth-endpoint
            // call count for a reason this test isn't about.
            cache_usage_for_profile(alias, &profile_path, &crate::usage::UsageInfo::default());

            // Every refresh attempt is rejected as reused; the auth server never
            // issues new tokens. The warmup POST is unreachable with a live token,
            // so it must also come back unauthorized.
            let (token_calls, _guards) = start_mock_server(
                StatusCode::UNAUTHORIZED,
                serde_json::json!({
                    "error": {
                        "code": "refresh_token_reused",
                        "message": "This refresh token has already been used.",
                    }
                }),
                StatusCode::UNAUTHORIZED,
            )
            .await;

            let result = warmup_account(alias, &profile_path).await;

            assert!(
                result.is_err(),
                "a permanently rejected refresh_token must not be reported as a successful warmup"
            );
            assert_eq!(
                token_calls.load(Ordering::SeqCst),
                1,
                "a terminal refresh rejection must not be replayed a second time from the \
                 401 handler — it can only ever fail again and costs a full round trip"
            );
        }

        #[allow(clippy::await_holding_lock)]
        #[tokio::test]
        async fn unknown_pre_refresh_outcome_stops_without_replaying_the_credential() {
            let _lock = ENV_LOCK.lock().await;
            let _profile_env_lock = crate::profile::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let home = crate::fs_ops::create_direct_tempdir().unwrap();
            let _codex_switch_home = use_test_home(home.path());
            let alias = "unknown-refresh-test";
            let profile_path = home.path().join("profiles").join(alias).join("auth.json");
            write_test_auth(
                &profile_path,
                &expired_access_token(),
                "refresh-token-unknown",
            );
            cache_usage_for_profile(alias, &profile_path, &crate::usage::UsageInfo::default());
            let (token_calls, _guards) = start_mock_server(
                StatusCode::OK,
                serde_json::json!({
                    "id_token": test_id_token(),
                    "access_token": "new-access",
                    "refresh_token": ""
                }),
                StatusCode::UNAUTHORIZED,
            )
            .await;

            let error = warmup_account(alias, &profile_path)
                .await
                .expect_err("an unknown refresh outcome must stop warmup");

            assert!(format!("{error:#}").contains("outcome is unknown"));
            assert_eq!(
                token_calls.load(Ordering::SeqCst),
                1,
                "the presented single-use credential must never be replayed"
            );
        }

        // ── `fetch_models_for_profile` must not swallow a refresh failure ──

        #[derive(Clone, Default)]
        struct LogBuf(Arc<std::sync::Mutex<Vec<u8>>>);

        impl std::io::Write for LogBuf {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuf {
            type Writer = LogBuf;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        impl LogBuf {
            fn contents(&self) -> String {
                String::from_utf8_lossy(&self.0.lock().unwrap()).to_string()
            }
        }

        #[allow(clippy::await_holding_lock)]
        #[tokio::test]
        async fn fetch_models_for_profile_logs_the_reason_when_proactive_refresh_fails() {
            let _lock = ENV_LOCK.lock().await;
            let _profile_env_lock = crate::profile::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let home = crate::fs_ops::create_direct_tempdir().unwrap();
            let _codex_switch_home = use_test_home(home.path());

            let alias = "fetch-models-refresh-log-test";
            let profile_path = home.path().join("profiles").join(alias).join("auth.json");
            write_test_auth(&profile_path, &expired_access_token(), "refresh-token-2");

            // `fetch_models_for_profile` never sends a warmup ping, so the
            // responses-endpoint status is irrelevant here.
            let (_token_calls, _guards) = start_mock_server(
                StatusCode::UNAUTHORIZED,
                serde_json::json!({
                    "error": {
                        "code": "refresh_token_reused",
                        "message": "This refresh token has already been used.",
                    }
                }),
                StatusCode::UNAUTHORIZED,
            )
            .await;

            let log_buf = LogBuf::default();
            let subscriber = tracing_subscriber::fmt()
                .with_writer(log_buf.clone())
                .with_max_level(tracing::Level::WARN)
                .with_ansi(false)
                .without_time()
                .finish();

            let result = {
                let _tracing_guard = tracing::subscriber::set_default(subscriber);
                let client = crate::auth::build_http_client().unwrap();
                let lease = crate::profile::acquire_profile_lease_async(alias)
                    .await
                    .expect("model test acquires its profile lease");
                let expected_binding = strict_binding_for_profile(&profile_path);
                fetch_models_for_profile_leased_with_client(
                    alias,
                    &profile_path,
                    lease,
                    &expected_binding,
                    &client,
                )
                .await
            };

            // Existing degrade-gracefully behavior must be preserved: a failed
            // proactive refresh still falls through to /models with the old token.
            assert!(
                result.is_ok(),
                "a refresh failure must not abort the /models fetch: {result:?}"
            );

            let logs = log_buf.contents();
            assert!(
                logs.contains("refresh_token_reused"),
                "the real rejection reason must be traceable in the logs, not silently \
                 dropped — captured log output was: {logs:?}"
            );
        }

        #[allow(clippy::await_holding_lock)]
        #[tokio::test]
        async fn fetch_models_rejects_rebound_alias_before_any_request() {
            let _lock = ENV_LOCK.lock().await;
            let _profile_env_lock = crate::profile::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let home = crate::fs_ops::create_direct_tempdir().unwrap();
            let _codex_switch_home = use_test_home(home.path());

            let network_calls = Arc::new(AtomicUsize::new(0));
            let token_calls = Arc::clone(&network_calls);
            let model_calls = Arc::clone(&network_calls);
            let app = Router::new()
                .route(
                    "/oauth/token",
                    post(move || {
                        let calls = Arc::clone(&token_calls);
                        async move {
                            calls.fetch_add(1, Ordering::SeqCst);
                            (
                                StatusCode::OK,
                                Json(serde_json::json!({
                                    "access_token": "rotated-access",
                                    "refresh_token": "rotated-refresh"
                                })),
                            )
                        }
                    }),
                )
                .route(
                    "/codex/models",
                    get(move || {
                        let calls = Arc::clone(&model_calls);
                        async move {
                            calls.fetch_add(1, Ordering::SeqCst);
                            Json(serde_json::json!({
                                "models": [{"slug": "gpt-5-mini"}]
                            }))
                        }
                    }),
                );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let _token_url =
                EnvVarGuard::set("CS_TOKEN_URL", &format!("http://{addr}/oauth/token"));
            let _models_url =
                EnvVarGuard::set("CS_MODELS_URL", &format!("http://{addr}/codex/models"));

            let alias = "fetch-models-rebound";
            let profile_path = home.path().join("profiles").join(alias).join("auth.json");
            write_test_auth(
                &profile_path,
                &expired_access_token(),
                "refresh-token-rebound",
            );
            let mut expected_binding = strict_binding_for_profile(&profile_path);
            expected_binding.account_id = "previous-owner".to_string();
            let client = reqwest::Client::new();
            let lease = crate::profile::acquire_profile_lease_async(alias)
                .await
                .expect("model test acquires its profile lease");

            let error = fetch_models_for_profile_leased_with_client(
                alias,
                &profile_path,
                lease,
                &expected_binding,
                &client,
            )
            .await
            .expect_err("a model request must reject an alias rebound to another account");
            server.abort();

            assert!(
                format!("{error:#}").contains("profile identity changed"),
                "alias rebinding should fail with an identity-specific error: {error:#}"
            );
            assert_eq!(
                network_calls.load(Ordering::SeqCst),
                0,
                "identity validation must run before token refresh or model discovery HTTP"
            );
        }

        #[allow(clippy::await_holding_lock)]
        #[tokio::test]
        async fn model_endpoint_wait_does_not_retain_the_profile_lease() {
            let _lock = ENV_LOCK.lock().await;
            let _profile_env_lock = crate::profile::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let home = crate::fs_ops::create_direct_tempdir().unwrap();
            let _codex_switch_home = use_test_home(home.path());

            let (arrival_tx, mut arrival_rx) = tokio::sync::mpsc::unbounded_channel();
            let response_gate = Arc::new(tokio::sync::Semaphore::new(0));
            let handler_gate = Arc::clone(&response_gate);
            let app = Router::new().route(
                "/codex/models",
                get(move || {
                    let arrival_tx = arrival_tx.clone();
                    let gate = Arc::clone(&handler_gate);
                    async move {
                        arrival_tx.send(()).unwrap();
                        let _permit = gate.acquire_owned().await.unwrap();
                        Json(serde_json::json!({
                            "models": [{"slug": "gpt-5-mini", "supported_in_api": true}]
                        }))
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let _models_url =
                EnvVarGuard::set("CS_MODELS_URL", &format!("http://{addr}/codex/models"));

            let alias = "model-read-releases-profile-lease";
            let profile_path = stage_writable_profile(home.path(), alias, &live_access_token());
            let expected_binding = strict_binding_for_profile(&profile_path);
            let client = reqwest::Client::new();
            let lease = crate::profile::acquire_profile_lease_async(alias)
                .await
                .expect("model test acquires its initial profile lease");
            let task_path = profile_path.clone();
            let task_binding = expected_binding.clone();
            let task = tokio::spawn(async move {
                fetch_models_for_profile_leased_with_client(
                    alias,
                    &task_path,
                    lease,
                    &task_binding,
                    &client,
                )
                .await
            });

            tokio::time::timeout(std::time::Duration::from_secs(1), arrival_rx.recv())
                .await
                .expect("the model request must reach the deliberately blocked endpoint")
                .expect("the model endpoint must report request arrival");
            let replacement_lease = tokio::time::timeout(
                std::time::Duration::from_millis(300),
                crate::profile::acquire_profile_lease_async(alias),
            )
            .await
            .expect("the read-only model response must not retain the profile lease")
            .expect("the released profile lease must be acquirable");
            drop(replacement_lease);

            response_gate.add_permits(1);
            let models = tokio::time::timeout(std::time::Duration::from_secs(1), task)
                .await
                .expect("model discovery must finish after the endpoint is released")
                .expect("model discovery task must join")
                .expect("model discovery must succeed");
            server.abort();

            assert_eq!(models.len(), 1);
            assert_eq!(models[0].slug, "gpt-5-mini");
        }

        #[allow(clippy::await_holding_lock)]
        #[tokio::test]
        async fn warmup_model_wait_releases_then_reacquires_the_profile_lease() {
            let _lock = ENV_LOCK.lock().await;
            let _profile_env_lock = crate::profile::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let home = crate::fs_ops::create_direct_tempdir().unwrap();
            let _codex_switch_home = use_test_home(home.path());

            let (arrival_tx, mut arrival_rx) = tokio::sync::mpsc::unbounded_channel();
            let response_gate = Arc::new(tokio::sync::Semaphore::new(0));
            let handler_gate = Arc::clone(&response_gate);
            let responses_calls = Arc::new(AtomicUsize::new(0));
            let responses_counter = Arc::clone(&responses_calls);
            let app = Router::new()
                .route(
                    "/codex/models",
                    get(move || {
                        let arrival_tx = arrival_tx.clone();
                        let gate = Arc::clone(&handler_gate);
                        async move {
                            arrival_tx.send(()).unwrap();
                            let _permit = gate.acquire_owned().await.unwrap();
                            Json(serde_json::json!({
                                "models": [{
                                    "slug": "gpt-5-mini",
                                    "supported_in_api": true
                                }]
                            }))
                        }
                    }),
                )
                .route(
                    "/codex/responses",
                    post(move || {
                        let counter = Arc::clone(&responses_counter);
                        async move {
                            counter.fetch_add(1, Ordering::SeqCst);
                            (StatusCode::OK, "")
                        }
                    }),
                );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let _models_url =
                EnvVarGuard::set("CS_MODELS_URL", &format!("http://{addr}/codex/models"));
            let _responses_url = EnvVarGuard::set(
                "CS_RESPONSES_URL",
                &format!("http://{addr}/codex/responses"),
            );

            let alias = "warmup-model-read-releases-profile-lease";
            let profile_path = stage_writable_profile(home.path(), alias, &live_access_token());
            let expected_binding = strict_binding_for_profile(&profile_path);
            let cache_key = warmup_cache_key(&expected_binding, &[]);
            model_cache_invalidate(&mut *MODEL_CACHE.lock().await, &cache_key);
            let lease = crate::profile::acquire_profile_lease_async(alias)
                .await
                .expect("warmup test acquires its initial profile lease");
            let task_path = profile_path.clone();
            let task_binding = expected_binding.clone();
            let task = tokio::spawn(async move {
                warmup_account_leased_with_client_after_usage_preflight(
                    alias,
                    &task_path,
                    lease,
                    &reqwest::Client::new(),
                    &task_binding,
                    Some(crate::usage::UsageInfo::default()),
                    test_first_network_permit(),
                )
                .await
            });

            tokio::time::timeout(std::time::Duration::from_secs(1), arrival_rx.recv())
                .await
                .expect("the warmup model request must reach the blocked endpoint")
                .expect("the model endpoint must report request arrival");
            let concurrent_lease = tokio::time::timeout(
                std::time::Duration::from_millis(300),
                crate::profile::acquire_profile_lease_async(alias),
            )
            .await
            .expect("a blocked read-only /models request must not retain the alias lease")
            .expect("the released warmup lease must be acquirable");
            drop(concurrent_lease);

            response_gate.add_permits(1);
            let returned_lease = tokio::time::timeout(std::time::Duration::from_secs(1), task)
                .await
                .expect("warmup must finish after model discovery is released")
                .expect("warmup task must join")
                .expect("unchanged credentials must pass post-discovery revalidation");
            drop(returned_lease);
            server.abort();

            assert_eq!(
                responses_calls.load(Ordering::SeqCst),
                1,
                "the unchanged normal flow must still send exactly one warmup POST"
            );
            assert_eq!(
                model_cache_get(&*MODEL_CACHE.lock().await, &cache_key),
                Some(test_selection("gpt-5-mini", &[])),
                "a revalidated model result should be published normally"
            );
        }

        #[test]
        fn cached_models_commit_cancellation_boundary_before_warmup_post() {
            run_profile_url_test(|| async {
                let home = crate::fs_ops::create_direct_tempdir().unwrap();
                let _codex_switch_home = use_test_home(home.path());

                let responses_calls = Arc::new(AtomicUsize::new(0));
                let responses_counter = Arc::clone(&responses_calls);
                let app = Router::new().route(
                    "/codex/responses",
                    post(move || {
                        let counter = Arc::clone(&responses_counter);
                        async move {
                            counter.fetch_add(1, Ordering::SeqCst);
                            (StatusCode::OK, "")
                        }
                    }),
                );
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                let server = tokio::spawn(async move {
                    axum::serve(listener, app).await.unwrap();
                });
                let _models_url =
                    EnvVarGuard::set("CS_MODELS_URL", &format!("http://{addr}/codex/models"));
                let _responses_url = EnvVarGuard::set(
                    "CS_RESPONSES_URL",
                    &format!("http://{addr}/codex/responses"),
                );

                let alias = "warmup-cache-hit-commit-boundary";
                let profile_path = stage_writable_profile(home.path(), alias, &live_access_token());
                let binding = strict_binding_for_profile(&profile_path);
                let cache_key = warmup_cache_key(&binding, &[]);
                model_cache_set(
                    &mut *MODEL_CACHE.lock().await,
                    &cache_key,
                    test_selection("gpt-5-mini", &[]),
                );
                let commit_calls = Arc::new(AtomicUsize::new(0));
                let task_commit_calls = Arc::clone(&commit_calls);
                let controls = WarmupExecutionControls::cancellable(
                    test_first_network_permit(),
                    std::future::pending(),
                    move || {
                        task_commit_calls.fetch_add(1, Ordering::SeqCst);
                        false
                    },
                );
                let lease = crate::profile::acquire_profile_lease_async(alias)
                    .await
                    .unwrap();
                let error = warmup_account_leased_with_client_after_usage_preflight_with_controls(
                    alias,
                    &profile_path,
                    lease,
                    &reqwest::Client::new(),
                    &binding,
                    Some(crate::usage::UsageInfo::default()),
                    controls,
                )
                .await
                .map(drop)
                .expect_err("a cancelled cache-hit boundary must stop before quota mutation");
                server.abort();

                assert!(warmup_wait_was_cancelled(&error), "{error:#}");
                assert_eq!(commit_calls.load(Ordering::SeqCst), 1);
                assert_eq!(
                    responses_calls.load(Ordering::SeqCst),
                    0,
                    "a cache hit must atomically commit before its first warmup POST"
                );
                model_cache_invalidate(&mut *MODEL_CACHE.lock().await, &cache_key);
            });
        }

        #[test]
        fn cancelling_blocked_model_discovery_prevents_cache_publish_and_warmup_post() {
            run_profile_url_test(|| async {
                let home = crate::fs_ops::create_direct_tempdir().unwrap();
                let _codex_switch_home = use_test_home(home.path());

                let (arrival_tx, mut arrival_rx) = tokio::sync::mpsc::unbounded_channel();
                let response_gate = Arc::new(tokio::sync::Semaphore::new(0));
                let handler_gate = Arc::clone(&response_gate);
                let responses_calls = Arc::new(AtomicUsize::new(0));
                let responses_counter = Arc::clone(&responses_calls);
                let app = Router::new()
                    .route(
                        "/codex/models",
                        get(move || {
                            let arrival_tx = arrival_tx.clone();
                            let gate = Arc::clone(&handler_gate);
                            async move {
                                arrival_tx.send(()).unwrap();
                                let _permit = gate.acquire_owned().await.unwrap();
                                Json(serde_json::json!({
                                    "models": [{"slug": "gpt-5-mini", "supported_in_api": true}]
                                }))
                            }
                        }),
                    )
                    .route(
                        "/codex/responses",
                        post(move || {
                            let counter = Arc::clone(&responses_counter);
                            async move {
                                counter.fetch_add(1, Ordering::SeqCst);
                                (StatusCode::OK, "")
                            }
                        }),
                    );
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                let server = tokio::spawn(async move {
                    axum::serve(listener, app).await.unwrap();
                });
                let _models_url =
                    EnvVarGuard::set("CS_MODELS_URL", &format!("http://{addr}/codex/models"));
                let _responses_url = EnvVarGuard::set(
                    "CS_RESPONSES_URL",
                    &format!("http://{addr}/codex/responses"),
                );

                let alias = "warmup-cancel-blocked-model-discovery";
                let profile_path = stage_writable_profile(home.path(), alias, &live_access_token());
                let binding = strict_binding_for_profile(&profile_path);
                let cache_key = warmup_cache_key(&binding, &[]);
                model_cache_invalidate(&mut *MODEL_CACHE.lock().await, &cache_key);
                let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
                let committed = Arc::new(AtomicBool::new(false));
                let task_committed = Arc::clone(&committed);
                let controls = WarmupExecutionControls::cancellable(
                    test_first_network_permit(),
                    async move {
                        let _ = cancel_rx.await;
                    },
                    move || {
                        task_committed.store(true, Ordering::SeqCst);
                        true
                    },
                );
                let lease = crate::profile::acquire_profile_lease_async(alias)
                    .await
                    .unwrap();
                let task_path = profile_path.clone();
                let task_binding = binding.clone();
                let task = tokio::spawn(async move {
                    warmup_account_leased_with_client_after_usage_preflight_with_controls(
                        alias,
                        &task_path,
                        lease,
                        &reqwest::Client::new(),
                        &task_binding,
                        Some(crate::usage::UsageInfo::default()),
                        controls,
                    )
                    .await
                });

                tokio::time::timeout(std::time::Duration::from_secs(1), arrival_rx.recv())
                    .await
                    .expect("the cancellable model request must reach the blocked endpoint")
                    .expect("the model endpoint must report request arrival");
                cancel_tx.send(()).unwrap();
                let error = tokio::time::timeout(std::time::Duration::from_secs(1), task)
                    .await
                    .expect("cancellation must stop a blocked read-only model request")
                    .expect("warmup task must join")
                    .map(drop)
                    .expect_err("cancelled model discovery must not reach a quota POST");
                server.abort();

                assert!(warmup_wait_was_cancelled(&error), "{error:#}");
                assert!(!committed.load(Ordering::SeqCst));
                assert_eq!(responses_calls.load(Ordering::SeqCst), 0);
                assert_eq!(
                    model_cache_get(&*MODEL_CACHE.lock().await, &cache_key),
                    None
                );
            });
        }

        #[test]
        fn cancelling_post_discovery_lease_reacquisition_prevents_publish_and_post() {
            run_profile_url_test(|| async {
                let home = crate::fs_ops::create_direct_tempdir().unwrap();
                let _codex_switch_home = use_test_home(home.path());

                let (arrival_tx, mut arrival_rx) = tokio::sync::mpsc::unbounded_channel();
                let response_gate = Arc::new(tokio::sync::Semaphore::new(0));
                let handler_gate = Arc::clone(&response_gate);
                let responses_calls = Arc::new(AtomicUsize::new(0));
                let responses_counter = Arc::clone(&responses_calls);
                let app = Router::new()
                    .route(
                        "/codex/models",
                        get(move || {
                            let arrival_tx = arrival_tx.clone();
                            let gate = Arc::clone(&handler_gate);
                            async move {
                                arrival_tx.send(()).unwrap();
                                let permit = gate.acquire_owned().await.unwrap();
                                permit.forget();
                                Json(serde_json::json!({
                                    "models": [{"slug": "gpt-5-mini", "supported_in_api": true}]
                                }))
                            }
                        }),
                    )
                    .route(
                        "/codex/responses",
                        post(move || {
                            let counter = Arc::clone(&responses_counter);
                            async move {
                                counter.fetch_add(1, Ordering::SeqCst);
                                (StatusCode::OK, "")
                            }
                        }),
                    );
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                let server = tokio::spawn(async move {
                    axum::serve(listener, app).await.unwrap();
                });
                let _models_url =
                    EnvVarGuard::set("CS_MODELS_URL", &format!("http://{addr}/codex/models"));
                let _responses_url = EnvVarGuard::set(
                    "CS_RESPONSES_URL",
                    &format!("http://{addr}/codex/responses"),
                );

                let alias = "warmup-cancel-model-lease-reacquire";
                let profile_path = stage_writable_profile(home.path(), alias, &live_access_token());
                let binding = strict_binding_for_profile(&profile_path);
                let cache_key = warmup_cache_key(&binding, &[]);
                model_cache_invalidate(&mut *MODEL_CACHE.lock().await, &cache_key);
                let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
                let committed = Arc::new(AtomicBool::new(false));
                let task_committed = Arc::clone(&committed);
                let controls = WarmupExecutionControls::cancellable(
                    test_first_network_permit(),
                    async move {
                        let _ = cancel_rx.await;
                    },
                    move || {
                        task_committed.store(true, Ordering::SeqCst);
                        true
                    },
                );
                let lease = crate::profile::acquire_profile_lease_async(alias)
                    .await
                    .unwrap();
                let task_path = profile_path.clone();
                let task_binding = binding.clone();
                let task = tokio::spawn(async move {
                    warmup_account_leased_with_client_after_usage_preflight_with_controls(
                        alias,
                        &task_path,
                        lease,
                        &reqwest::Client::new(),
                        &task_binding,
                        Some(crate::usage::UsageInfo::default()),
                        controls,
                    )
                    .await
                });

                tokio::time::timeout(std::time::Duration::from_secs(1), arrival_rx.recv())
                    .await
                    .expect("the model request must reach the gated endpoint")
                    .expect("the model endpoint must report request arrival");
                let held_lease = crate::profile::acquire_profile_lease_async(alias)
                    .await
                    .expect("model discovery must release the alias lease");
                let (attempt_tx, attempt_rx) = std::sync::mpsc::channel();
                crate::profile::notify_on_test_lock_attempt(
                    &format!("profile '{alias}'"),
                    attempt_tx,
                );
                response_gate.add_permits(1);
                tokio::time::timeout(std::time::Duration::from_secs(1), async {
                    loop {
                        if attempt_rx.try_recv().is_ok() {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("warmup must reach the blocked post-discovery lease reacquisition");
                cancel_tx.send(()).unwrap();
                let error = tokio::time::timeout(std::time::Duration::from_secs(1), task)
                    .await
                    .expect("cancellation must stop post-discovery lease reacquisition")
                    .expect("warmup task must join")
                    .map(drop)
                    .expect_err("cancelled reacquisition must not publish models or send a POST");

                assert!(warmup_wait_was_cancelled(&error), "{error:#}");
                assert!(!committed.load(Ordering::SeqCst));
                assert_eq!(responses_calls.load(Ordering::SeqCst), 0);
                assert_eq!(
                    model_cache_get(&*MODEL_CACHE.lock().await, &cache_key),
                    None
                );
                drop(held_lease);
                drop(
                    crate::profile::acquire_profile_lease_async(alias)
                        .await
                        .expect(
                            "the cancelled reacquisition must eventually release its worker lease",
                        ),
                );
                server.abort();
            });
        }

        #[allow(clippy::await_holding_lock)]
        #[tokio::test]
        async fn alias_rebind_or_delete_during_model_discovery_prevents_warmup_mutation() {
            let _lock = ENV_LOCK.lock().await;
            let _profile_env_lock = crate::profile::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let home = crate::fs_ops::create_direct_tempdir().unwrap();
            let _codex_switch_home = use_test_home(home.path());

            let (arrival_tx, mut arrival_rx) = tokio::sync::mpsc::unbounded_channel();
            let response_gate = Arc::new(tokio::sync::Semaphore::new(0));
            let handler_gate = Arc::clone(&response_gate);
            let responses_calls = Arc::new(AtomicUsize::new(0));
            let responses_counter = Arc::clone(&responses_calls);
            let app = Router::new()
                .route(
                    "/codex/models",
                    get(move || {
                        let arrival_tx = arrival_tx.clone();
                        let gate = Arc::clone(&handler_gate);
                        async move {
                            arrival_tx.send(()).unwrap();
                            let permit = gate.acquire_owned().await.unwrap();
                            permit.forget();
                            Json(serde_json::json!({
                                "models": [{
                                    "slug": "gpt-5-mini",
                                    "supported_in_api": true
                                }]
                            }))
                        }
                    }),
                )
                .route(
                    "/codex/responses",
                    post(move || {
                        let counter = Arc::clone(&responses_counter);
                        async move {
                            counter.fetch_add(1, Ordering::SeqCst);
                            (StatusCode::OK, "")
                        }
                    }),
                );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let _models_url =
                EnvVarGuard::set("CS_MODELS_URL", &format!("http://{addr}/codex/models"));
            let _responses_url = EnvVarGuard::set(
                "CS_RESPONSES_URL",
                &format!("http://{addr}/codex/responses"),
            );

            for (alias, delete_profile) in [
                ("warmup-rebound-during-model-read", false),
                ("warmup-deleted-during-model-read", true),
            ] {
                let profile_path = stage_writable_profile(home.path(), alias, &live_access_token());
                let expected_binding = strict_binding_for_profile(&profile_path);
                let cache_key = warmup_cache_key(&expected_binding, &[]);
                model_cache_invalidate(&mut *MODEL_CACHE.lock().await, &cache_key);
                let lease = crate::profile::acquire_profile_lease_async(alias)
                    .await
                    .expect("warmup test acquires its initial profile lease");
                let task_path = profile_path.clone();
                let task_binding = expected_binding.clone();
                let task = tokio::spawn(async move {
                    warmup_account_leased_with_client_after_usage_preflight(
                        alias,
                        &task_path,
                        lease,
                        &reqwest::Client::new(),
                        &task_binding,
                        Some(crate::usage::UsageInfo::default()),
                        test_first_network_permit(),
                    )
                    .await
                });

                tokio::time::timeout(std::time::Duration::from_secs(1), arrival_rx.recv())
                    .await
                    .expect("the warmup model request must reach the blocked endpoint")
                    .expect("the model endpoint must report request arrival");
                let mutation_lease = crate::profile::acquire_profile_lease_async(alias)
                    .await
                    .expect("the concurrent alias mutation must acquire the released lease");
                if delete_profile {
                    std::fs::remove_file(&profile_path).unwrap();
                } else {
                    write_test_auth_for_identity(
                        &profile_path,
                        &live_access_token(),
                        "replacement-refresh-token",
                        "acct-rebound-owner",
                        "rebound-owner@example.com",
                    );
                }
                drop(mutation_lease);
                response_gate.add_permits(1);

                let error = tokio::time::timeout(std::time::Duration::from_secs(1), task)
                    .await
                    .expect("warmup must stop after the profile mutation is visible")
                    .expect("warmup task must join")
                    .map(drop)
                    .expect_err("a rebound or deleted alias must fail before warmup POST");
                if delete_profile {
                    assert!(
                        format!("{error:#}").contains("cannot revalidate auth"),
                        "deletion should fail at auth revalidation: {error:#}"
                    );
                    assert!(
                        !profile_path.exists(),
                        "warmup must not recreate the deleted profile"
                    );
                } else {
                    assert!(
                        format!("{error:#}").contains("identity changed during model discovery"),
                        "rebind should fail at strict identity revalidation: {error:#}"
                    );
                    assert_eq!(
                        strict_binding_for_profile(&profile_path).account_id,
                        "acct-rebound-owner",
                        "warmup must not overwrite the replacement owner's credentials"
                    );
                }
                assert_eq!(
                    model_cache_get(&*MODEL_CACHE.lock().await, &cache_key),
                    None,
                    "an unvalidated discovery result must not mutate the model cache"
                );
            }
            server.abort();

            assert_eq!(
                responses_calls.load(Ordering::SeqCst),
                0,
                "neither profile mutation may be followed by a warmup POST"
            );
        }

        #[allow(clippy::await_holding_lock)]
        #[tokio::test]
        async fn model_retry_backoff_releases_network_capacity() {
            let _lock = ENV_LOCK.lock().await;
            let calls = Arc::new(AtomicUsize::new(0));
            let server_calls = Arc::clone(&calls);
            let first_response = Arc::new(tokio::sync::Semaphore::new(0));
            let server_first_response = Arc::clone(&first_response);
            let app = Router::new().route(
                "/codex/models",
                get(move || {
                    let calls = Arc::clone(&server_calls);
                    let first_response = Arc::clone(&server_first_response);
                    async move {
                        let attempt = calls.fetch_add(1, Ordering::SeqCst);
                        if attempt == 0 {
                            first_response.add_permits(1);
                            (
                                StatusCode::INTERNAL_SERVER_ERROR,
                                Json(serde_json::json!({"error": "retry"})),
                            )
                        } else {
                            (
                                StatusCode::OK,
                                Json(serde_json::json!({
                                    "models": [{
                                        "slug": "gpt-5-mini",
                                        "supported_in_api": true
                                    }]
                                })),
                            )
                        }
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let _models_url =
                EnvVarGuard::set("CS_MODELS_URL", &format!("http://{addr}/codex/models"));
            let endpoints = crate::auth::service_endpoints().unwrap();
            let limiter = Arc::new(Semaphore::new(1));
            let task_limiter = Arc::clone(&limiter);
            let task = tokio::spawn(async move {
                let mut network = NetworkBudget::new(first_network_permit(task_limiter));
                fetch_models(
                    &endpoints,
                    &reqwest::Client::new(),
                    WarmupRequestAuth {
                        access_token: "access-token",
                        account_id: None,
                        is_fedramp: false,
                    },
                    &mut network,
                )
                .await
            });

            let observed = first_response.acquire_owned().await.unwrap();
            observed.forget();
            let spare = tokio::time::timeout(
                std::time::Duration::from_millis(500),
                limiter.clone().acquire_owned(),
            )
            .await
            .expect("the retry delay must not retain network capacity")
            .expect("the model limiter must remain open");
            tokio::time::sleep(std::time::Duration::from_millis(350)).await;
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "the retry must reserve fresh capacity before its second request"
            );
            drop(spare);

            let models = tokio::time::timeout(std::time::Duration::from_secs(2), task)
                .await
                .expect("the retried model request must finish")
                .expect("the model task must join")
                .expect("the second model attempt must succeed");
            server.abort();
            assert_eq!(models.len(), 1);
            assert_eq!(calls.load(Ordering::SeqCst), 2);
        }

        // ── rotated tokens that never reach disk must abort the account ──
        //
        // OpenAI's refresh_token is single-use: the moment the auth server hands
        // back a rotated one, the previous token is dead. If the write back to
        // the profile then fails, the only credential the server still accepts
        // lives in this process's memory. Finishing the request with it and
        // exiting zero leaves a bricked profile and no signal, so every one of
        // these paths has to report instead.

        /// Substring every persist-failure report must carry, so the message
        /// can never be mistaken for the auth server rejecting the refresh.
        const PERSIST_FAILURE_MARKER: &str =
            "token refresh succeeded but the rotated credentials could not be saved";

        /// An access_token JWT far from expiry, so the pre-warmup proactive
        /// refresh stays out of the way and the 401 retry path can be exercised
        /// on its own.
        fn live_access_token() -> String {
            make_jwt(&serde_json::json!({ "exp": crate::auth::now_unix_secs().unwrap() + 3600 }))
        }

        /// Stage a profile that reads fine but can never be written back: the
        /// alias-derived `profiles/<alias>/auth.json` is occupied by a
        /// *directory*, which fails the write on unix and Windows alike (no
        /// permission-bit semantics involved). The tokens the run starts from
        /// live in a separate readable file, so the refresh itself succeeds and
        /// only the persist step fails — exactly the production window.
        fn stage_unwritable_profile(
            home: &std::path::Path,
            alias: &str,
            access_token: &str,
        ) -> std::path::PathBuf {
            let readable = home.join("staged").join(alias).join("auth.json");
            let account_id = format!("acct-{alias}");
            let email = format!("{alias}@example.com");
            write_test_auth_for_identity(
                &readable,
                access_token,
                "refresh-token-live",
                &account_id,
                &email,
            );
            std::fs::create_dir_all(home.join("profiles").join(alias).join("auth.json")).unwrap();
            readable
        }

        /// Stage a normal profile whose rotated tokens can be written back.
        fn stage_writable_profile(
            home: &std::path::Path,
            alias: &str,
            access_token: &str,
        ) -> std::path::PathBuf {
            let path = home.join("profiles").join(alias).join("auth.json");
            let account_id = format!("acct-{alias}");
            let email = format!("{alias}@example.com");
            write_test_auth_for_identity(
                &path,
                access_token,
                "refresh-token-live",
                &account_id,
                &email,
            );
            path
        }

        #[allow(clippy::await_holding_lock)]
        #[tokio::test]
        async fn cache_miss_warmup_does_not_wait_for_reset_credit_metadata() {
            let _lock = ENV_LOCK.lock().await;
            let _profile_env_lock = crate::profile::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let home = crate::fs_ops::create_direct_tempdir().unwrap();
            let _codex_switch_home = use_test_home(home.path());

            let reset_credit_calls = Arc::new(AtomicUsize::new(0));
            let reset_credit_server_calls = Arc::clone(&reset_credit_calls);
            let reset_credit_gate = Arc::new(tokio::sync::Semaphore::new(0));
            let handler_gate = Arc::clone(&reset_credit_gate);
            let usage_response = Arc::new(tokio::sync::Semaphore::new(0));
            let handler_usage_response = Arc::clone(&usage_response);
            let app = Router::new()
                .route(
                    "/usage",
                    get(move || {
                        let usage_response = Arc::clone(&handler_usage_response);
                        async move {
                            usage_response.add_permits(1);
                            Json(serde_json::json!({
                                "plan_type": "pro",
                                "rate_limit": null,
                                "credits": null,
                                "spend_control": null,
                                "additional_rate_limits": null,
                                "rate_limit_reached_type": null
                            }))
                        }
                    }),
                )
                .route(
                    "/credits",
                    get(move || {
                        let calls = Arc::clone(&reset_credit_server_calls);
                        let gate = Arc::clone(&handler_gate);
                        async move {
                            calls.fetch_add(1, Ordering::SeqCst);
                            let permit = gate.acquire_owned().await.unwrap();
                            permit.forget();
                            Json(serde_json::json!({
                                "available_count": 0,
                                "credits": []
                            }))
                        }
                    }),
                )
                .route(
                    "/codex/models",
                    get(|| async {
                        Json(serde_json::json!({
                            "models": [{
                                "slug": "gpt-5-mini",
                                "supported_in_api": true
                            }]
                        }))
                    }),
                )
                .route("/codex/responses", post(|| async { (StatusCode::OK, "") }));

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let _usage_url = EnvVarGuard::set("CS_USAGE_URL", &format!("http://{addr}/usage"));
            let _credits_url =
                EnvVarGuard::set("CS_RESET_CREDITS_URL", &format!("http://{addr}/credits"));
            let _models_url =
                EnvVarGuard::set("CS_MODELS_URL", &format!("http://{addr}/codex/models"));
            let _responses_url = EnvVarGuard::set(
                "CS_RESPONSES_URL",
                &format!("http://{addr}/codex/responses"),
            );

            let alias = "warmup-quota-only-cache-miss";
            let profile_path = stage_writable_profile(home.path(), alias, &live_access_token());
            let binding = crate::auth::account_info_from_auth_value(
                &crate::auth::read_auth(&profile_path).unwrap(),
            )
            .strict_binding()
            .unwrap();
            let lease = crate::profile::acquire_profile_lease_async(alias)
                .await
                .unwrap();
            let client = crate::auth::build_http_client().unwrap();
            let cache_key = warmup_cache_key(&binding, &[]);
            model_cache_invalidate(&mut *MODEL_CACHE.lock().await, &cache_key);
            let fetch_lock = {
                let mut locks = MODEL_FETCH_LOCKS.lock().await;
                locks
                    .entry(cache_key)
                    .or_insert_with(|| Arc::new(Mutex::new(())))
                    .clone()
            };
            let fetch_guard = fetch_lock.lock_owned().await;
            let limiter = Arc::new(Semaphore::new(1));
            let task_limiter = Arc::clone(&limiter);
            let task = tokio::spawn(async move {
                let lease = warmup_account_leased_with_client_after_usage_preflight(
                    alias,
                    &profile_path,
                    lease,
                    &client,
                    &binding,
                    None,
                    first_network_permit(task_limiter),
                )
                .await?;
                drop(lease);
                Ok::<(), anyhow::Error>(())
            });

            let observed = usage_response.acquire_owned().await.unwrap();
            observed.forget();
            let spare =
                tokio::time::timeout(std::time::Duration::from_secs(1), limiter.acquire_owned())
                    .await
                    .expect("warmup cache preparation must not retain the usage request permit")
                    .expect("the warmup limiter must remain open");
            drop(spare);
            drop(fetch_guard);

            let result = tokio::time::timeout(std::time::Duration::from_secs(2), task).await;
            server.abort();

            let warmup_result = result
                .expect("warmup waited for the deliberately blocked reset-credit endpoint")
                .expect("warmup task must join");
            assert!(
                warmup_result.is_ok(),
                "quota-only discovery should leave the warmup healthy: {warmup_result:?}"
            );
            assert_eq!(
                reset_credit_calls.load(Ordering::SeqCst),
                0,
                "warmup only needs quota pools and must not contact the reset-credit endpoint"
            );
        }

        #[allow(clippy::await_holding_lock)]
        #[tokio::test]
        async fn supplied_usage_preflight_skips_cache_and_usage_probe() {
            let _lock = ENV_LOCK.lock().await;
            let _profile_env_lock = crate::profile::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let home = crate::fs_ops::create_direct_tempdir().unwrap();
            let _codex_switch_home = use_test_home(home.path());

            let usage_calls = Arc::new(AtomicUsize::new(0));
            let usage_counter = Arc::clone(&usage_calls);
            let responses_calls = Arc::new(AtomicUsize::new(0));
            let responses_counter = Arc::clone(&responses_calls);
            let app = Router::new()
                .route(
                    "/usage",
                    get(move || {
                        let counter = Arc::clone(&usage_counter);
                        async move {
                            counter.fetch_add(1, Ordering::SeqCst);
                            Json(serde_json::json!({
                                "plan_type": "pro",
                                "rate_limit": null,
                                "credits": null,
                                "spend_control": null,
                                "additional_rate_limits": null,
                                "rate_limit_reached_type": null
                            }))
                        }
                    }),
                )
                .route(
                    "/codex/models",
                    get(|| async {
                        Json(serde_json::json!({
                            "models": [
                                {"slug": "gpt-5-mini", "supported_in_api": true},
                                {"slug": "gpt-5-spark", "supported_in_api": true}
                            ]
                        }))
                    }),
                )
                .route(
                    "/codex/responses",
                    post(move || {
                        let counter = Arc::clone(&responses_counter);
                        async move {
                            counter.fetch_add(1, Ordering::SeqCst);
                            (StatusCode::OK, "")
                        }
                    }),
                );

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let _usage_url = EnvVarGuard::set("CS_USAGE_URL", &format!("http://{addr}/usage"));
            let _models_url =
                EnvVarGuard::set("CS_MODELS_URL", &format!("http://{addr}/codex/models"));
            let _responses_url = EnvVarGuard::set(
                "CS_RESPONSES_URL",
                &format!("http://{addr}/codex/responses"),
            );

            let alias = "warmup-supplied-cache-hit";
            let profile_path = stage_writable_profile(home.path(), alias, &live_access_token());
            let binding = crate::auth::account_info_from_auth_value(
                &crate::auth::read_auth(&profile_path).unwrap(),
            )
            .strict_binding()
            .unwrap();
            let lease = crate::profile::acquire_profile_lease_async(alias)
                .await
                .unwrap();
            let client = crate::auth::build_http_client().unwrap();
            let cached_usage = crate::usage::UsageInfo {
                additional_limits: vec![crate::usage::AdditionalRateLimit {
                    limit_name: Some("gpt-5-spark".to_string()),
                    metered_feature: Some("codex_spark".to_string()),
                    allowed: Some(true),
                    limit_reached: Some(false),
                    primary: None,
                    secondary: None,
                }],
                ..Default::default()
            };

            let result = warmup_account_leased_with_client_after_usage_preflight(
                alias,
                &profile_path,
                lease,
                &client,
                &binding,
                Some(cached_usage),
                test_first_network_permit(),
            )
            .await
            .map(drop);
            server.abort();

            assert!(
                result.is_ok(),
                "supplied cache hit should warm normally: {result:?}"
            );
            assert_eq!(
                usage_calls.load(Ordering::SeqCst),
                0,
                "a proven cache hit must not be replaced by another cache lookup and quota probe"
            );
            assert_eq!(
                responses_calls.load(Ordering::SeqCst),
                2,
                "the supplied additional pool must be warmed together with the main pool"
            );
        }

        #[allow(clippy::await_holding_lock)]
        #[tokio::test]
        async fn supplied_usage_preflight_is_rejected_after_alias_rebinding() {
            let _lock = ENV_LOCK.lock().await;
            let _profile_env_lock = crate::profile::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let home = crate::fs_ops::create_direct_tempdir().unwrap();
            let _codex_switch_home = use_test_home(home.path());

            let alias = "warmup-preflight-rebound";
            let profile_path = stage_writable_profile(home.path(), alias, &live_access_token());
            let mut stale_binding = crate::auth::account_info_from_auth_value(
                &crate::auth::read_auth(&profile_path).unwrap(),
            )
            .strict_binding()
            .unwrap();
            stale_binding.account_id = "previous-owner".to_string();
            let lease = crate::profile::acquire_profile_lease_async(alias)
                .await
                .unwrap();

            let error = warmup_account_leased_with_client_after_usage_preflight(
                alias,
                &profile_path,
                lease,
                &reqwest::Client::new(),
                &stale_binding,
                Some(crate::usage::UsageInfo::default()),
                test_first_network_permit(),
            )
            .await
            .map(drop)
            .expect_err("usage preflight from a previous alias owner must be rejected");

            assert!(
                format!("{error:#}").contains("identity changed after warmup cache preflight"),
                "rebound alias should fail with an identity-specific error: {error:#}"
            );
        }

        /// Mock server whose `/oauth/token` always rotates successfully, so the
        /// only thing that can go wrong is the write back. `/codex/responses`
        /// walks `responses_statuses` one entry per request and repeats the last
        /// entry once exhausted — a request counter, never a timer, decides what
        /// comes back.
        async fn start_rotating_mock_server(
            responses_statuses: Vec<StatusCode>,
            break_profile_after_authorization: Option<std::path::PathBuf>,
        ) -> (Arc<AtomicUsize>, Vec<EnvVarGuard>) {
            let token_calls = Arc::new(AtomicUsize::new(0));
            let counter = token_calls.clone();
            let responses_calls = Arc::new(AtomicUsize::new(0));

            let app = Router::new()
                .route(
                    "/oauth/token",
                    post(move || {
                        let counter = counter.clone();
                        let break_profile_after_authorization =
                            break_profile_after_authorization.clone();
                        async move {
                            let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
                            if let Some(path) = break_profile_after_authorization {
                                std::fs::remove_file(&path).unwrap();
                                std::fs::create_dir(&path).unwrap();
                            }
                            (
                                StatusCode::OK,
                                Json(serde_json::json!({
                                    "access_token": live_access_token(),
                                    "refresh_token": format!("rotated-refresh-{n}"),
                                })),
                            )
                        }
                    }),
                )
                .route(
                    "/codex/models",
                    get(|| async {
                        (
                            StatusCode::OK,
                            Json(serde_json::json!({
                                "models": [{"slug": "gpt-5-mini", "supported_in_api": true}]
                            })),
                        )
                    }),
                )
                .route(
                    "/codex/responses",
                    post(move || {
                        let calls = responses_calls.clone();
                        let statuses = responses_statuses.clone();
                        async move {
                            let index = calls.fetch_add(1, Ordering::SeqCst);
                            let status = statuses
                                .get(index)
                                .copied()
                                .or_else(|| statuses.last().copied())
                                .unwrap_or(StatusCode::OK);
                            (status, "")
                        }
                    }),
                );

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });

            let guards = vec![
                EnvVarGuard::set("CS_TOKEN_URL", &format!("http://{addr}/oauth/token")),
                EnvVarGuard::set("CS_MODELS_URL", &format!("http://{addr}/codex/models")),
                EnvVarGuard::set(
                    "CS_RESPONSES_URL",
                    &format!("http://{addr}/codex/responses"),
                ),
            ];
            (token_calls, guards)
        }

        fn assert_reports_persist_failure(detail: &str) {
            assert!(
                detail.contains(PERSIST_FAILURE_MARKER),
                "a rotated credential that never reached disk must be reported as a local \
                 write failure, got: {detail}"
            );
            assert!(
                !detail.contains("token refresh failed"),
                "the report must stay distinguishable from the auth server rejecting the \
                 refresh — that one needs a re-login, this one needs the write fixed. \
                 Got: {detail}"
            );
        }

        fn assert_reports_refresh_not_started(detail: &str) {
            assert!(
                detail.contains("token refresh was not started"),
                "a local precondition failure must be reported before a single-use token is sent, got: {detail}"
            );
        }

        #[allow(clippy::await_holding_lock)]
        #[tokio::test]
        async fn warmup_aborts_when_pre_warmup_rotated_tokens_cannot_be_saved() {
            let _lock = ENV_LOCK.lock().await;
            let _profile_env_lock = crate::profile::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let home = crate::fs_ops::create_direct_tempdir().unwrap();
            let _codex_switch_home = use_test_home(home.path());

            let alias = "persist-fail-pre-warmup";
            let profile_path = stage_writable_profile(home.path(), alias, &expired_access_token());
            // Keep the (independent) usage-fetch refresh path out of this test.
            cache_usage_for_profile(alias, &profile_path, &crate::usage::UsageInfo::default());

            let (_token_calls, _guards) =
                start_rotating_mock_server(vec![StatusCode::OK], Some(profile_path.clone())).await;

            let result = warmup_account(alias, &profile_path).await;

            let error = result.expect_err(
                "the pre-warmup refresh rotated the credential and the write back failed, so \
                 the warmup must not report success with a token that only exists in memory",
            );
            assert_reports_persist_failure(&format!("{error:#}"));
        }

        #[allow(clippy::await_holding_lock)]
        #[tokio::test]
        async fn warmup_aborts_when_the_401_retry_rotated_tokens_cannot_be_saved() {
            let _lock = ENV_LOCK.lock().await;
            let _profile_env_lock = crate::profile::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let home = crate::fs_ops::create_direct_tempdir().unwrap();
            let _codex_switch_home = use_test_home(home.path());

            let alias = "persist-fail-401-retry";
            // Not expiring, so only the 401 handler triggers a refresh.
            let profile_path = stage_writable_profile(home.path(), alias, &live_access_token());
            cache_usage_for_profile(alias, &profile_path, &crate::usage::UsageInfo::default());

            // First warmup POST is unauthorized (drives the refresh), the retry
            // would have succeeded — which is precisely how the failure used to
            // exit zero.
            let (_token_calls, _guards) = start_rotating_mock_server(
                vec![StatusCode::UNAUTHORIZED, StatusCode::OK],
                Some(profile_path.clone()),
            )
            .await;

            let result = warmup_account(alias, &profile_path).await;

            let error = result.expect_err(
                "the 401 retry refreshed and rotated the credential; a failed write back must \
                 abort rather than let the retry succeed on an unsaved token",
            );
            assert_reports_persist_failure(&format!("{error:#}"));
        }

        #[allow(clippy::await_holding_lock)]
        #[tokio::test]
        async fn fetch_models_for_profile_aborts_when_rotated_tokens_cannot_be_saved() {
            let _lock = ENV_LOCK.lock().await;
            let _profile_env_lock = crate::profile::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let home = crate::fs_ops::create_direct_tempdir().unwrap();
            let _codex_switch_home = use_test_home(home.path());

            let alias = "persist-fail-fetch-models";
            let profile_path = stage_writable_profile(home.path(), alias, &expired_access_token());

            let (_token_calls, _guards) =
                start_rotating_mock_server(vec![StatusCode::OK], Some(profile_path.clone())).await;

            let client = crate::auth::build_http_client().unwrap();
            let lease = crate::profile::acquire_profile_lease_async(alias)
                .await
                .expect("model test acquires its profile lease");
            let expected_binding = strict_binding_for_profile(&profile_path);
            let result = fetch_models_for_profile_leased_with_client(
                alias,
                &profile_path,
                lease,
                &expected_binding,
                &client,
            )
            .await;

            let error = result.map(|models| format!("{models:?}")).expect_err(
                "degrading to the old token is only correct when the refresh was refused; a \
                 refresh that succeeded and then failed to save must abort instead",
            );
            assert_reports_persist_failure(&format!("{error:#}"));
        }

        #[allow(clippy::await_holding_lock)]
        #[tokio::test]
        async fn one_unauthorizable_profile_does_not_abort_the_rest_of_the_warmup_batch() {
            let _lock = ENV_LOCK.lock().await;
            let _profile_env_lock = crate::profile::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let home = crate::fs_ops::create_direct_tempdir().unwrap();
            let _codex_switch_home = use_test_home(home.path());
            let _codex_home = EnvVarGuard::set(
                "CODEX_HOME",
                &home.path().join("codex").display().to_string(),
            );

            let broken = "batch-persist-broken";
            let healthy = "batch-persist-healthy";
            let broken_path =
                stage_unwritable_profile(home.path(), broken, &expired_access_token());
            let healthy_path =
                stage_writable_profile(home.path(), healthy, &expired_access_token());
            let healthy_binding = strict_binding_for_profile(&healthy_path);
            cache_usage_for_profile(broken, &broken_path, &crate::usage::UsageInfo::default());
            cache_usage_for_profile(healthy, &healthy_path, &crate::usage::UsageInfo::default());

            let (_token_calls, _guards) =
                start_rotating_mock_server(vec![StatusCode::OK], None).await;

            // Mirrors the batch driver in `commands::misc`: one task per alias,
            // outcomes collected independently.
            let mut tasks = tokio::task::JoinSet::new();
            for (alias, path) in [(broken, broken_path), (healthy, healthy_path.clone())] {
                let alias = alias.to_string();
                tasks.spawn(async move {
                    let result = warmup_account(&alias, &path).await;
                    (alias, result)
                });
            }
            let mut outcomes: HashMap<String, Result<()>> = HashMap::new();
            while let Some(joined) = tasks.join_next().await {
                let (alias, result) = joined.unwrap();
                outcomes.insert(alias, result);
            }

            let broken_error = outcomes
                .remove(broken)
                .expect("the broken profile must produce an outcome")
                .expect_err("the profile whose refresh could not be authorized must report");
            assert_reports_refresh_not_started(&format!("{broken_error:#}"));

            let healthy_result = outcomes
                .remove(healthy)
                .expect("the healthy profile must produce an outcome");
            assert!(
                healthy_result.is_ok(),
                "one account's persist failure must not take down the others in the batch: \
                 {healthy_result:?}"
            );
            assert_eq!(
                strict_binding_for_profile(&healthy_path),
                healthy_binding,
                "refreshing one batch member must preserve that profile's account binding"
            );
        }

        // ── /models is resolved once per warmup ──────────────────
        //
        // The model list decides both the main-pool request and the additional
        // -pool ones, so it is one question with one answer. Asking twice costs
        // an upstream round trip per warmup, and the daemon runs warmup on a
        // timer across every profile when `auto_warmup` is on.

        /// Mock server that counts `/codex/models` requests. `/codex/responses`
        /// always succeeds, so nothing but the fetch count is under test.
        async fn start_models_counting_mock_server() -> (Arc<AtomicUsize>, Vec<EnvVarGuard>) {
            let models_calls = Arc::new(AtomicUsize::new(0));
            let counter = models_calls.clone();

            let app = Router::new()
                .route(
                    "/codex/models",
                    get(move || {
                        let counter = counter.clone();
                        async move {
                            counter.fetch_add(1, Ordering::SeqCst);
                            (
                                StatusCode::OK,
                                Json(serde_json::json!({
                                    "models": [
                                        {"slug": "gpt-5-mini", "supported_in_api": true},
                                        {"slug": "gpt-5-spark", "supported_in_api": true}
                                    ]
                                })),
                            )
                        }
                    }),
                )
                .route("/codex/responses", post(|| async { (StatusCode::OK, "") }));

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });

            let guards = vec![
                EnvVarGuard::set("CS_MODELS_URL", &format!("http://{addr}/codex/models")),
                EnvVarGuard::set(
                    "CS_RESPONSES_URL",
                    &format!("http://{addr}/codex/responses"),
                ),
            ];
            (models_calls, guards)
        }

        /// The common case: an account with no additional quota pools. The
        /// second fetch's answer was filtered down to nothing and discarded, so
        /// the request bought precisely nothing.
        #[allow(clippy::await_holding_lock)]
        #[tokio::test]
        async fn a_warmup_without_additional_pools_fetches_the_model_list_once() {
            let _lock = ENV_LOCK.lock().await;
            let _profile_env_lock = crate::profile::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let home = crate::fs_ops::create_direct_tempdir().unwrap();
            let _codex_switch_home = use_test_home(home.path());

            // Unique alias: MODEL_CACHE is process-global and outlives one test.
            let alias = "models-fetch-count-no-pools";
            let profile_path = stage_writable_profile(home.path(), alias, &live_access_token());
            // Cached usage with no `additional_limits`, so the usage-fetch path
            // stays out of this and there is no additional pool to warm.
            cache_usage_for_profile(alias, &profile_path, &crate::usage::UsageInfo::default());

            let (models_calls, _guards) = start_models_counting_mock_server().await;

            warmup_account(alias, &profile_path)
                .await
                .expect("a warmup against a healthy mock server must succeed");

            assert_eq!(
                models_calls.load(Ordering::SeqCst),
                1,
                "the model list answers one question and must be fetched once; a second \
                 /models request with no additional pool to warm is a round trip whose \
                 answer is thrown away"
            );
        }

        /// The same guarantee where the second fetch actually had a consumer:
        /// an additional pool still gets warmed, from the list already in hand.
        #[allow(clippy::await_holding_lock)]
        #[tokio::test]
        async fn a_warmup_with_an_additional_pool_still_fetches_the_model_list_once() {
            let _lock = ENV_LOCK.lock().await;
            let _profile_env_lock = crate::profile::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let home = crate::fs_ops::create_direct_tempdir().unwrap();
            let _codex_switch_home = use_test_home(home.path());

            let alias = "models-fetch-count-with-pool";
            let profile_path = stage_writable_profile(home.path(), alias, &live_access_token());
            cache_usage_for_profile(
                alias,
                &profile_path,
                &crate::usage::UsageInfo {
                    additional_limits: vec![crate::usage::AdditionalRateLimit {
                        limit_name: Some("gpt-5-spark".to_string()),
                        metered_feature: Some("codex_spark".to_string()),
                        allowed: Some(true),
                        limit_reached: Some(false),
                        primary: None,
                        secondary: None,
                    }],
                    ..Default::default()
                },
            );

            let (models_calls, _guards) = start_models_counting_mock_server().await;

            warmup_account(alias, &profile_path)
                .await
                .expect("a warmup against a healthy mock server must succeed");

            assert_eq!(
                models_calls.load(Ordering::SeqCst),
                1,
                "warming an additional pool must reuse the list already resolved for the \
                 main pool rather than asking again"
            );
        }

        /// Same mock as above, but it serves two models and also counts the
        /// warmup requests, so a test can tell *which* pools were warmed rather
        /// than only how often the model list was fetched. Two models are the
        /// minimum that distinguishes them: a pool claiming the only model would
        /// leave the main pool no candidate (see `select_warmup_models`), and
        /// both requests would collapse into one.
        async fn start_counting_mock_server()
        -> (Arc<AtomicUsize>, Arc<AtomicUsize>, Vec<EnvVarGuard>) {
            let models_calls = Arc::new(AtomicUsize::new(0));
            let responses_calls = Arc::new(AtomicUsize::new(0));
            let models_counter = models_calls.clone();
            let responses_counter = responses_calls.clone();

            let app = Router::new()
                .route(
                    "/codex/models",
                    get(move || {
                        let counter = models_counter.clone();
                        async move {
                            counter.fetch_add(1, Ordering::SeqCst);
                            (
                                StatusCode::OK,
                                Json(serde_json::json!({
                                    "models": [
                                        {"slug": "gpt-5-mini", "supported_in_api": true},
                                        {"slug": "gpt-5-spark", "supported_in_api": true}
                                    ]
                                })),
                            )
                        }
                    }),
                )
                .route(
                    "/codex/responses",
                    post(move || {
                        let counter = responses_counter.clone();
                        async move {
                            counter.fetch_add(1, Ordering::SeqCst);
                            (StatusCode::OK, "")
                        }
                    }),
                );

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });

            let guards = vec![
                EnvVarGuard::set("CS_MODELS_URL", &format!("http://{addr}/codex/models")),
                EnvVarGuard::set(
                    "CS_RESPONSES_URL",
                    &format!("http://{addr}/codex/responses"),
                ),
            ];
            (models_calls, responses_calls, guards)
        }

        #[allow(clippy::await_holding_lock)]
        #[tokio::test]
        async fn stale_additional_model_refreshes_once_without_reposting_successful_targets() {
            let _lock = ENV_LOCK.lock().await;
            let _profile_env_lock = crate::profile::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let home = crate::fs_ops::create_direct_tempdir().unwrap();
            let _codex_switch_home = use_test_home(home.path());

            let models_calls = Arc::new(AtomicUsize::new(0));
            let model_counter = Arc::clone(&models_calls);
            let response_calls = Arc::new(std::sync::Mutex::new(HashMap::<String, usize>::new()));
            let response_counter = Arc::clone(&response_calls);
            let app = Router::new()
                .route(
                    "/codex/models",
                    get(move || {
                        let counter = Arc::clone(&model_counter);
                        async move {
                            counter.fetch_add(1, Ordering::SeqCst);
                            Json(serde_json::json!({
                                "models": [
                                    {
                                        "slug": "main-new-mini",
                                        "supported_in_api": true
                                    },
                                    {
                                        "slug": "addon-good",
                                        "display_name": "Pool B",
                                        "supported_in_api": true
                                    },
                                    {
                                        "slug": "addon-new",
                                        "display_name": "Pool A",
                                        "supported_in_api": true
                                    }
                                ]
                            }))
                        }
                    }),
                )
                .route(
                    "/codex/responses",
                    post(move |Json(body): Json<serde_json::Value>| {
                        let calls = Arc::clone(&response_counter);
                        async move {
                            let model = body["model"].as_str().unwrap_or_default().to_string();
                            *calls
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .entry(model.clone())
                                .or_default() += 1;
                            if model == "addon-old" {
                                (StatusCode::BAD_REQUEST, "model not supported").into_response()
                            } else {
                                (StatusCode::OK, "").into_response()
                            }
                        }
                    }),
                );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let _models_url =
                EnvVarGuard::set("CS_MODELS_URL", &format!("http://{addr}/codex/models"));
            let _responses_url = EnvVarGuard::set(
                "CS_RESPONSES_URL",
                &format!("http://{addr}/codex/responses"),
            );

            let alias = "stale-additional-model-refresh";
            let profile_path = stage_writable_profile(home.path(), alias, &live_access_token());
            let additional_limits = vec![
                crate::usage::AdditionalRateLimit {
                    limit_name: Some("Pool A".to_string()),
                    metered_feature: Some("codex_pool_a".to_string()),
                    allowed: Some(true),
                    limit_reached: Some(false),
                    ..Default::default()
                },
                crate::usage::AdditionalRateLimit {
                    limit_name: Some("Pool B".to_string()),
                    metered_feature: Some("codex_pool_b".to_string()),
                    allowed: Some(true),
                    limit_reached: Some(false),
                    ..Default::default()
                },
            ];
            cache_usage_for_profile(
                alias,
                &profile_path,
                &crate::usage::UsageInfo {
                    additional_limits: additional_limits.clone(),
                    ..Default::default()
                },
            );
            let binding = strict_binding_for_profile(&profile_path);
            let cache_key = warmup_cache_key(&binding, &additional_limits);
            model_cache_set(
                &mut *MODEL_CACHE.lock().await,
                &cache_key,
                test_selection(
                    "main-old",
                    &[("Pool A", "addon-good"), ("Pool B", "addon-old")],
                ),
            );

            warmup_account(alias, &profile_path)
                .await
                .expect("an unsupported additional model should recover from official metadata");
            server.abort();

            assert_eq!(models_calls.load(Ordering::SeqCst), 1);
            let calls = response_calls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            assert_eq!(calls.get("main-old"), Some(&1));
            assert_eq!(calls.get("main-new-mini"), None);
            assert_eq!(calls.get("addon-good"), Some(&1));
            assert_eq!(calls.get("addon-old"), Some(&1));
            assert_eq!(calls.get("addon-new"), Some(&1));
            drop(calls);
            assert_eq!(
                model_cache_get(&*MODEL_CACHE.lock().await, &cache_key),
                Some(test_selection(
                    "main-new-mini",
                    &[("Pool A", "addon-new"), ("Pool B", "addon-good")],
                ))
            );
            model_cache_invalidate(&mut *MODEL_CACHE.lock().await, &cache_key);
        }

        /// The resolved set bakes in the additional pools that existed when it
        /// was cached, so keying the cache on the alias alone freezes it for the
        /// life of the process. The CLI exits between warmups and never notices;
        /// the daemon with `auto_warmup` runs for days, so an account that gains
        /// a model quota pool would keep warming the old set — the new pool's
        /// quota window silently never opens until someone restarts the daemon.
        #[allow(clippy::await_holding_lock)]
        #[tokio::test]
        async fn a_pool_added_after_the_first_warmup_is_still_warmed() {
            let _lock = ENV_LOCK.lock().await;
            let _profile_env_lock = crate::profile::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let home = crate::fs_ops::create_direct_tempdir().unwrap();
            let _codex_switch_home = use_test_home(home.path());

            let alias = "models-cache-pool-set-changed";
            let profile_path = stage_writable_profile(home.path(), alias, &live_access_token());
            let (models_calls, responses_calls, _guards) = start_counting_mock_server().await;

            // First warmup: the account has no additional quota pool.
            cache_usage_for_profile(alias, &profile_path, &crate::usage::UsageInfo::default());
            warmup_account(alias, &profile_path)
                .await
                .expect("the first warmup against a healthy mock server must succeed");
            assert_eq!(
                responses_calls.load(Ordering::SeqCst),
                1,
                "with no additional pool only the main-pool request is expected"
            );

            // The account gains a model quota pool while the process keeps running.
            cache_usage_for_profile(
                alias,
                &profile_path,
                &crate::usage::UsageInfo {
                    additional_limits: vec![crate::usage::AdditionalRateLimit {
                        limit_name: Some("gpt-5-spark".to_string()),
                        metered_feature: Some("codex_spark".to_string()),
                        allowed: Some(true),
                        limit_reached: Some(false),
                        primary: None,
                        secondary: None,
                    }],
                    ..Default::default()
                },
            );
            warmup_account(alias, &profile_path)
                .await
                .expect("the second warmup against a healthy mock server must succeed");

            assert_eq!(
                models_calls.load(Ordering::SeqCst),
                2,
                "a changed pool set must miss the cache; reusing the entry resolved for the \
                 old set is what leaves the new pool cold"
            );
            assert_eq!(
                responses_calls.load(Ordering::SeqCst),
                3,
                "the second warmup must open a quota window for the main pool AND the pool \
                 the account just gained"
            );
        }
    }
}
