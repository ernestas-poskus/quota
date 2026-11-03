//! Fetch Antigravity usage with the locally stored Google token and normalize
//! the response into the shared usage model.
//!
//! Antigravity is the one provider Quota refreshes for. Google access tokens
//! last about an hour, so a strict never-refresh rule would leave the sidebar
//! blank almost always. Google does not rotate the refresh token on a standard
//! refresh, so the fresh access token is held in memory for the length of one
//! request and `oauth_creds.json` is never written. Antigravity's own copy of
//! the refresh token stays valid.

use crate::usage::{ProviderUsage, UsageWindow};
use serde_json::{json, Value};

use super::local::{read_any_credentials, LocalAntigravityCredentials, LocalCredentialError};

const GOOGLE_TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";

/// Google binds a refresh token to the OAuth client that issued it, and two
/// different tools write `~/.gemini/oauth_creds.json`: Antigravity itself, and
/// the Gemini CLI it grew out of. Refreshing with the wrong one fails with
/// `unauthorized_client`, so the client is chosen by the id token's `aud`.
///
/// The Code Assist host travels with the client rather than being a single
/// constant, because different clients talk to different hosts.
///
/// Only Antigravity's own client is listed. The Gemini CLI, which writes the
/// same `oauth_creds.json`, uses a different client and a different host, but
/// its credentials are for Gemini Code Assist rather than Antigravity and
/// Quota has no reason to query them. Credentials from an unlisted client are
/// reported by name rather than guessed at, since the client determines both
/// how to refresh the token and which host to send it to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OauthClient {
    pub client_id: &'static str,
    pub client_secret: &'static str,
    pub code_assist_base: &'static str,
}

const OAUTH_CLIENTS: [OauthClient; 1] = [OauthClient {
    client_id: "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com",
    client_secret: "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",
    code_assist_base: "https://daily-cloudcode-pa.googleapis.com",
}];

/// Which client these credentials belong to, if we recognize it.
pub fn oauth_client_for(client_id: Option<&str>) -> Option<OauthClient> {
    let client_id = client_id?;
    OAUTH_CLIENTS
        .iter()
        .find(|client| client.client_id == client_id)
        .copied()
}

const LOAD_ENDPOINT: &str = "v1internal:loadCodeAssist";
const FETCH_MODELS_ENDPOINT: &str = "v1internal:fetchAvailableModels";
const RETRIEVE_QUOTA_ENDPOINT: &str = "v1internal:retrieveUserQuotaSummary";

/// The Antigravity product quota summary. It accepts the same group/bucket
/// shape as the Cloud Code summary below and is asked as well, because the two
/// backends have not always returned the same groups.
const AICODE_QUOTA_ENDPOINT: &str =
    "https://aicode.googleapis.com/v1alpha/remote:getUserQuotaSummary";

/// The AI Code summary is optional enrichment, and the Cloud Code quota request
/// runs after it. `reqwest` sets no default timeout, so a stalled endpoint here
/// would hold up the whole refresh; bound it to the same grace as the CLI
/// fallback without touching the shared client.
const AICODE_QUOTA_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// The Antigravity CLI's own quota view, in print mode with structured output.
///
/// The CLI renders the Gemini weekly window and the separate `Gemini 4 Argon`
/// allowance from data that none of the quota endpoints Quota can reach with a
/// consumer sign-in expose, so those windows are borrowed from the CLI itself.
const AGY_QUOTA_ARGS: [&str; 4] = ["-p", "/quota", "--output-format", "json"];

/// A signed-out CLI can wait on an OAuth flow. The report is a status line, not
/// a login prompt, so the fallback is abandoned after this long.
const AGY_QUOTA_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

const IDE_VERSION: &str = "1.20.5";
const GOOGLE_API_NODEJS_CLIENT_VERSION: &str = "10.3.0";
/// Google only returns the Gemini quota group to the Antigravity CLI's user agent.
const CLI_VERSION: &str = "1.4.10";
const X_GOOG_API_CLIENT: &str = "gl-node/22.21.1";

/// The four windows every Antigravity account is expected to report, in the
/// order the sidebar shows them.
///
/// The vendor groups models into shared pools: Gemini and Claude/GPT, each with
/// a 5-hour and a weekly window. The slots stay present when a pool is missing
/// from the response so the caller can tell "this account has no Gemini quota"
/// from "this provider is broken", and so the compact token always keeps the
/// Gemini pair in front.
const WINDOWS: [WindowSpec; 4] = [
    WindowSpec {
        label: "Gemini/Argon 5h",
        pool: Pool::Gemini,
        weekly: false,
    },
    WindowSpec {
        label: "Gemini/Argon Wk",
        pool: Pool::Gemini,
        weekly: true,
    },
    WindowSpec {
        label: "Claude/GPT 5h",
        pool: Pool::ThirdParty,
        weekly: false,
    },
    WindowSpec {
        label: "Claude/GPT Wk",
        pool: Pool::ThirdParty,
        weekly: true,
    },
];

/// Antigravity's own quota view shows `Gemini 4 Argon` as a separate allowance.
/// Its windows are appended to the report only when a summary carries the group,
/// so accounts without it do not grow a permanently blank row.
const ARGON_WINDOWS: [WindowSpec; 2] = [
    WindowSpec {
        label: "Gemini 4 Argon 5h",
        pool: Pool::Argon,
        weekly: false,
    },
    WindowSpec {
        label: "Gemini 4 Argon Wk",
        pool: Pool::Argon,
        weekly: true,
    },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pool {
    Gemini,
    ThirdParty,
    Argon,
}

#[derive(Debug, Clone, Copy)]
struct WindowSpec {
    label: &'static str,
    pool: Pool,
    weekly: bool,
}

/// Convert a quota summary into windows, Gemini first so the compact token
/// shows the pair people actually run Antigravity for.
///
/// Antigravity is the exception among these providers: it reports
/// `remainingFraction`, which is already remaining rather than used. Nothing is
/// inverted here.
pub fn normalize_quota_response(raw: &Value, account_label: Option<String>) -> ProviderUsage {
    normalize_quota_summaries(&[raw], account_label)
}

/// Convert one or more quota summaries into a single usage record.
///
/// Summaries are merged with the earlier source winning, so a window that more
/// than one source describes keeps the first reading the caller supplies.
pub fn normalize_quota_summaries(
    summaries: &[&Value],
    account_label: Option<String>,
) -> ProviderUsage {
    let buckets = collect_buckets(summaries);

    let mut windows: Vec<UsageWindow> = WINDOWS
        .iter()
        .map(|spec| match locate(&buckets, spec) {
            Some(bucket) => window_from(bucket, spec.label),
            None => UsageWindow {
                label: spec.label.to_string(),
                remaining_percent: None,
                reset_at: None,
            },
        })
        .collect();

    for spec in ARGON_WINDOWS.iter() {
        if let Some(bucket) = locate(&buckets, spec) {
            windows.push(window_from(bucket, spec.label));
        }
    }

    ProviderUsage {
        provider: "antigravity".to_string(),
        account_label,
        windows,
        note: None,
    }
}

struct BucketRef<'a> {
    bucket: &'a Value,
    pool: Pool,
    weekly: bool,
}

fn locate<'a>(buckets: &'a [BucketRef<'a>], spec: &WindowSpec) -> Option<&'a Value> {
    buckets
        .iter()
        .find(|candidate| candidate.pool == spec.pool && candidate.weekly == spec.weekly)
        .map(|candidate| candidate.bucket)
}

/// Flatten the groups of every summary into classified buckets, keeping the
/// first bucket seen for each pool/window pair.
fn collect_buckets<'a>(summaries: &[&'a Value]) -> Vec<BucketRef<'a>> {
    let mut buckets: Vec<BucketRef<'a>> = Vec::new();
    for summary in summaries {
        for group in summary_groups(summary) {
            let group_name = group
                .get("displayName")
                .and_then(Value::as_str)
                .unwrap_or("");
            let Some(group_buckets) = group.get("buckets").and_then(Value::as_array) else {
                continue;
            };
            for bucket in group_buckets {
                let Some((pool, weekly)) = classify_bucket(bucket, group_name) else {
                    continue;
                };
                if buckets
                    .iter()
                    .any(|existing| existing.pool == pool && existing.weekly == weekly)
                {
                    continue;
                }
                buckets.push(BucketRef {
                    bucket,
                    pool,
                    weekly,
                });
            }
        }
    }
    buckets
}

/// The AI Code endpoint wraps the same summary the Cloud Code endpoint returns
/// directly, so accept either shape (and the unwrapped form) rather than pinning
/// the client to one wrapper version.
fn summary_groups(raw: &Value) -> &[Value] {
    let candidates = [
        raw.get("summary")
            .and_then(|summary| summary.get("response")),
        raw.get("summary"),
        raw.get("response"),
        Some(raw),
    ];
    for candidate in candidates.into_iter().flatten() {
        if let Some(groups) = candidate.get("groups").and_then(Value::as_array) {
            return groups;
        }
    }
    &[]
}

/// Recover the Gemini 5-hour window from `fetchAvailableModels`.
///
/// The Gemini pool does not appear in the quota summaries the CLI reads, but
/// the models response includes a `quotaInfo` block per model. Every Gemini
/// model shares the pool's 5-hour reading, so the strictest fraction across
/// those models is the number to report. Models without a fraction (Claude/GPT
/// while their weekly limit is exhausted) or without a reset time (internal tab
/// models) are skipped.
pub fn gemini_quota_from_models(models: &Value) -> Option<Value> {
    let models = models.get("models")?.as_object()?;

    let mut fraction: Option<f64> = None;
    let mut reset_time: Option<&Value> = None;
    for model in models.values() {
        // Only the Gemini pool shares a quota this way; an Anthropic/OpenAI
        // model in the same list reports its own pool's numbers.
        if let Some(provider) = model.get("apiProvider").and_then(Value::as_str) {
            if !provider.contains("GOOGLE_GEMINI") {
                continue;
            }
        }
        let Some(quota) = model.get("quotaInfo") else {
            continue;
        };
        let Some(remaining) = quota.get("remainingFraction").and_then(as_number) else {
            continue;
        };
        let Some(reset) = quota.get("resetTime").filter(|value| !value.is_null()) else {
            continue;
        };
        if fraction.is_none_or(|current| remaining < current) {
            fraction = Some(remaining);
            reset_time = Some(reset);
        }
    }

    Some(json!({
        "groups": [{
            "displayName": "Gemini Models",
            "buckets": [{
                "bucketId": "gemini-models-5h",
                "displayName": "Five Hour Limit Remaining",
                "window": "5h",
                "remainingFraction": fraction?,
                "resetTime": reset_time?,
            }]
        }]
    }))
}

/// Convert the Antigravity CLI's `/quota --output-format json` view into the
/// summary shape the normalizer already understands.
///
/// The CLI's command payload uses snake_case (`remaining_fraction`,
/// `reset_time`, bucket `id`/`name`) and group `name`, so the fields are
/// renamed here rather than teaching the normalizer every spelling.
pub fn agy_quota_summary(output: &Value) -> Option<Value> {
    let data = output.get("command")?.get("data")?;
    let groups = data.get("groups")?.as_array()?;
    if groups.is_empty() {
        return None;
    }

    let groups: Vec<Value> = groups
        .iter()
        .map(|group| {
            let buckets: Vec<Value> = group
                .get("buckets")
                .and_then(Value::as_array)
                .map(|buckets| {
                    buckets
                        .iter()
                        .map(|bucket| {
                            json!({
                                "bucketId": bucket.get("id").cloned().unwrap_or(Value::Null),
                                "displayName": bucket.get("name").cloned().unwrap_or(Value::Null),
                                "window": bucket.get("window").cloned().unwrap_or(Value::Null),
                                "remainingFraction": bucket
                                    .get("remaining_fraction")
                                    .cloned()
                                    .unwrap_or(Value::Null),
                                "resetTime": bucket.get("reset_time").cloned().unwrap_or(Value::Null),
                                "disabled": bucket
                                    .get("disabled")
                                    .cloned()
                                    .unwrap_or(Value::Bool(false)),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            json!({
                "displayName": group.get("name").cloned().unwrap_or(Value::Null),
                "buckets": buckets,
            })
        })
        .collect();

    Some(json!({ "groups": groups }))
}

/// Which pool and window a bucket describes.
///
/// The group display name wins over the bucket id: a bucket named `argon-*`
/// still belongs to the Gemini pool when the group says "Gemini Models", while
/// Antigravity's own quota view puts `Gemini 4 Argon` in a group of its own.
/// Bucket ids are the fallback for payloads that carry no group names.
fn classify_bucket(bucket: &Value, group_name: &str) -> Option<(Pool, bool)> {
    let id = bucket
        .get("bucketId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let weekly = match bucket.get("window").and_then(Value::as_str) {
        Some("weekly") => true,
        Some("5h") => false,
        _ if id.ends_with("weekly") => true,
        _ if id.ends_with("5h") => false,
        _ => return None,
    };

    let group = group_name.to_ascii_lowercase();
    let pool = if group.contains("argon") {
        Pool::Argon
    } else if group.contains("claude") || group.contains("gpt") {
        Pool::ThirdParty
    } else if group.contains("gemini") || id.starts_with("gemini") || id.starts_with("argon") {
        Pool::Gemini
    } else if id.starts_with("3p") || id.starts_with("claude") || id.starts_with("gpt") {
        Pool::ThirdParty
    } else {
        return None;
    };
    Some((pool, weekly))
}

fn window_from(bucket: &Value, label: &str) -> UsageWindow {
    // A disabled bucket is not usable quota: Antigravity disables the 5-hour
    // bucket once the paired weekly limit is exhausted, and the raw fraction
    // can still read 1. Reporting 0 is the honest answer.
    let disabled = bucket
        .get("disabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    UsageWindow {
        label: label.to_string(),
        remaining_percent: if disabled {
            Some(0)
        } else {
            bucket
                .get("remainingFraction")
                .and_then(as_number)
                .map(|fraction| (fraction * 100.0).round().clamp(0.0, 100.0) as i32)
        },
        reset_at: bucket.get("resetTime").and_then(parse_reset_at),
    }
}

/// Numbers sometimes arrive as strings in this API.
fn as_number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|item| item.trim().parse::<f64>().ok()))
        .filter(|item| item.is_finite())
}

fn parse_reset_at(value: &Value) -> Option<i64> {
    if let Some(number) = value.as_i64() {
        return normalize_epoch(number);
    }
    let raw = value.as_str()?.trim();
    if raw.is_empty() {
        return None;
    }
    if let Ok(number) = raw.parse::<i64>() {
        return normalize_epoch(number);
    }
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|date| date.timestamp())
}

fn normalize_epoch(number: i64) -> Option<i64> {
    if number <= 0 {
        return None;
    }
    Some(if number > 10_000_000_000 {
        number / 1000
    } else {
        number
    })
}

#[derive(Debug)]
pub enum LocalUsageError {
    Credentials(LocalCredentialError),
    ExpiredWithoutRefreshToken,
    UnknownOauthClient(Option<String>),
    RefreshRejected,
    NoProject,
    Unauthorized,
    Request(String),
}

impl std::fmt::Display for LocalUsageError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Credentials(inner) => write!(formatter, "{}", inner),
            Self::ExpiredWithoutRefreshToken => write!(
                formatter,
                "The stored Antigravity token has expired and there is no refresh token. Sign in to Antigravity again."
            ),
            Self::UnknownOauthClient(client_id) => match client_id {
                Some(id) => write!(
                    formatter,
                    "These credentials were issued to OAuth client {}, which is not Antigravity's. They are most likely Gemini Code Assist's, which Quota does not report on. Sign in through Antigravity itself.",
                    id
                ),
                None => write!(
                    formatter,
                    "These credentials do not say which OAuth client issued them, so Quota cannot tell whether they are Antigravity's. Sign in through Antigravity again."
                ),
            },
            Self::RefreshRejected => write!(
                formatter,
                "Google rejected the stored Antigravity refresh token. Sign in to Antigravity again."
            ),
            Self::NoProject => write!(
                formatter,
                "Antigravity did not report a project to query usage for."
            ),
            Self::Unauthorized => write!(
                formatter,
                "Antigravity rejected the stored token. Sign in to Antigravity again."
            ),
            Self::Request(detail) => {
                write!(formatter, "Antigravity usage request failed: {}", detail)
            }
        }
    }
}

/// Read local credentials and fetch current usage. Never writes to disk.
pub async fn fetch_local_usage() -> Result<ProviderUsage, LocalUsageError> {
    let credentials = read_any_credentials().map_err(LocalUsageError::Credentials)?;
    let client = reqwest::Client::new();

    // The CLI's current keyring payload names the account but carries no id
    // token, so it cannot name its OAuth client. A CLI-owned token can safely
    // use Antigravity's single registered client; the shared Gemini file still
    // has to prove which client issued it.
    let oauth = oauth_client_for(credentials.issued_to_client_id.as_deref())
        .or_else(|| {
            (credentials.cli_owned && credentials.issued_to_client_id.is_none())
                .then(|| OAUTH_CLIENTS[0])
        })
        .ok_or_else(|| {
            LocalUsageError::UnknownOauthClient(credentials.issued_to_client_id.clone())
        })?;

    let access_token = resolve_access_token(&client, &credentials, &oauth).await?;

    // loadCodeAssist names the project the quota call needs.
    let load = post_code_assist(
        &client,
        &oauth,
        &access_token,
        LOAD_ENDPOINT,
        &json!({
            "mode": "FULL_ELIGIBILITY_CHECK",
            "metadata": {
                "ideName": "antigravity",
                "ideType": "ANTIGRAVITY",
                "ideVersion": IDE_VERSION,
                "pluginVersion": env!("CARGO_PKG_NAME"),
                "platform": platform_name(),
                "updateChannel": "stable",
                "pluginType": "GEMINI"
            }
        }),
    )
    .await?;

    let project = project_id(&load).ok_or(LocalUsageError::NoProject)?;
    let project_payload = json!({ "project": project });

    // The quota endpoint answers with empty buckets unless the models call has
    // run first. Its body is kept as a fallback for the Gemini 5-hour window.
    let models = post_code_assist(
        &client,
        &oauth,
        &access_token,
        FETCH_MODELS_ENDPOINT,
        &project_payload,
    )
    .await?;

    // The Cloud Code summary is the source the desktop app and the extension
    // already trust, so it wins when it reports a window. Ask the AI Code
    // summary as well, since it has carried groups the Cloud Code one omitted,
    // and recover at least the 5-hour reading from the models response when the
    // summaries do not carry it.
    let aicode = fetch_aicode_quota(&client, &access_token).await.ok();
    let models_quota = gemini_quota_from_models(&models);

    let quota = post_code_assist(
        &client,
        &oauth,
        &access_token,
        RETRIEVE_QUOTA_ENDPOINT,
        &project_payload,
    )
    .await?;

    // Cloud Code first, then AI Code, then the models fallback: when more than
    // one source describes a window, the earliest reading wins.
    let mut summaries: Vec<&Value> = std::iter::once(&quota)
        .chain(aicode.iter())
        .chain(models_quota.iter())
        .collect();

    // The separate Gemini 4 Argon allowance is not in any of those responses
    // today. Borrow it from the Antigravity CLI, which renders it from its own
    // backend. That is the only source for those windows, so the CLI is asked
    // whenever the APIs leave the Gemini weekly or Argon weekly window blank;
    // the result is merged last and therefore only fills gaps.
    let agy_quota = if needs_agy_fallback(&summaries) {
        fetch_agy_quota().await
    } else {
        None
    };
    if let Some(value) = agy_quota.as_ref() {
        summaries.push(value);
    }

    Ok(normalize_quota_summaries(
        &summaries,
        credentials.email.clone().or_else(|| tier_name(&load)),
    ))
}

/// Use the stored token while it is good, otherwise trade the refresh token for
/// a fresh one that lives only as long as this call.
async fn resolve_access_token(
    client: &reqwest::Client,
    credentials: &LocalAntigravityCredentials,
    oauth: &OauthClient,
) -> Result<String, LocalUsageError> {
    if !credentials.needs_refresh_at_ms(chrono::Utc::now().timestamp_millis()) {
        return Ok(credentials.access_token.clone());
    }

    let refresh_token = credentials
        .refresh_token
        .as_deref()
        .ok_or(LocalUsageError::ExpiredWithoutRefreshToken)?;

    let response = client
        .post(GOOGLE_TOKEN_ENDPOINT)
        .form(&[
            ("client_id", oauth.client_id),
            ("client_secret", oauth.client_secret),
            ("refresh_token", refresh_token),
            ("grant_type", "refresh_token"),
        ])
        .send()
        .await
        .map_err(|err| LocalUsageError::Request(err.to_string()))?;

    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|err| LocalUsageError::Request(err.to_string()))?;

    if !status.is_success() {
        // The body of a failed token exchange can echo credential material, so
        // only the status and length are reported.
        if status.as_u16() == 400 || status.as_u16() == 401 {
            return Err(LocalUsageError::RefreshRejected);
        }
        return Err(LocalUsageError::Request(format!(
            "token refresh status={} body_length={}",
            status.as_u16(),
            body.len()
        )));
    }

    let parsed: Value = serde_json::from_str(&body)
        .map_err(|_| LocalUsageError::Request("token refresh returned unreadable JSON".to_string()))?;

    parsed
        .get("access_token")
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|token| !token.is_empty())
        .ok_or(LocalUsageError::RefreshRejected)
}

async fn post_code_assist(
    client: &reqwest::Client,
    oauth: &OauthClient,
    access_token: &str,
    endpoint: &str,
    payload: &Value,
) -> Result<Value, LocalUsageError> {
    let url = format!("{}/{}", oauth.code_assist_base, endpoint);
    let response = client
        .post(&url)
        .header(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", access_token),
        )
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::ACCEPT, "*/*")
        .header(reqwest::header::USER_AGENT, user_agent(endpoint))
        .header("x-goog-api-client", X_GOOG_API_CLIENT)
        .json(payload)
        .send()
        .await
        .map_err(|err| LocalUsageError::Request(err.to_string()))?;

    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|err| LocalUsageError::Request(err.to_string()))?;

    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Err(LocalUsageError::Unauthorized);
    }
    if !status.is_success() {
        return Err(LocalUsageError::Request(format!(
            "{} status={} body_length={}",
            endpoint,
            status.as_u16(),
            text.len()
        )));
    }

    if text.trim().is_empty() {
        return Ok(json!({}));
    }

    serde_json::from_str(&text)
        .map_err(|_| LocalUsageError::Request(format!("{} returned unreadable JSON", endpoint)))
}

/// Borrow the Antigravity CLI's quota view for the windows the APIs leave blank.
///
/// Returns `None` when the CLI is not on `PATH`, when `QUOTA_NO_AGY` is set, or
/// when it fails to answer within the timeout. Everything is best-effort: a
/// missing CLI only leaves the weekly windows blank, as before.
async fn fetch_agy_quota() -> Option<Value> {
    if std::env::var_os("QUOTA_NO_AGY").is_some() {
        return None;
    }

    let mut command = tokio::process::Command::new("agy");
    command
        .args(AGY_QUOTA_ARGS)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);

    let output = tokio::time::timeout(AGY_QUOTA_TIMEOUT, command.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let parsed: Value = serde_json::from_slice(&output.stdout).ok()?;
    agy_quota_summary(&parsed)
}

/// Does any of the summaries carry this pool/window pair?
fn has_window(summaries: &[&Value], pool: Pool, weekly: bool) -> bool {
    collect_buckets(summaries)
        .iter()
        .any(|bucket| bucket.pool == pool && bucket.weekly == weekly)
}

/// Whether the API summaries are missing a window only the Antigravity CLI
/// reports. `agy` is a subprocess, so it is only asked when the response would
/// otherwise leave a blank the CLI can fill.
pub fn needs_agy_fallback(summaries: &[&Value]) -> bool {
    !has_window(summaries, Pool::Gemini, true) || !has_window(summaries, Pool::Argon, true)
}

/// Read the AI Code quota summary with the same token the Cloud Code calls use.
///
/// The two backends have not always returned the same groups, so both are
/// asked. The endpoint requires the `auth/aicode` OAuth scope in some
/// deployments; a token without it fails with 403, which the caller treats as
/// "this summary has nothing to add". The request carries its own timeout, so
/// this optional source cannot delay the required calls.
async fn fetch_aicode_quota(
    client: &reqwest::Client,
    access_token: &str,
) -> Result<Value, LocalUsageError> {
    let response = client
        .get(AICODE_QUOTA_ENDPOINT)
        .timeout(AICODE_QUOTA_TIMEOUT)
        .header(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", access_token),
        )
        .header(reqwest::header::ACCEPT, "*/*")
        .header(
            reqwest::header::USER_AGENT,
            user_agent(RETRIEVE_QUOTA_ENDPOINT),
        )
        .header("x-goog-api-client", X_GOOG_API_CLIENT)
        .send()
        .await
        .map_err(|err| LocalUsageError::Request(err.to_string()))?;

    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|err| LocalUsageError::Request(err.to_string()))?;

    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Err(LocalUsageError::Unauthorized);
    }
    if !status.is_success() {
        return Err(LocalUsageError::Request(format!(
            "AI Code quota status={} body_length={}",
            status.as_u16(),
            text.len()
        )));
    }

    if text.trim().is_empty() {
        return Ok(json!({}));
    }

    serde_json::from_str(&text)
        .map_err(|_| LocalUsageError::Request("AI Code quota returned unreadable JSON".to_string()))
}

/// The project id arrives either as a bare string or as an object.
pub fn project_id(load: &Value) -> Option<String> {
    let field = load.get("cloudaicompanionProject")?;
    field
        .as_str()
        .or_else(|| field.get("id").and_then(Value::as_str))
        .or_else(|| field.get("projectId").and_then(Value::as_str))
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_string)
}

/// A display label when no signed-in address is known.
fn tier_name(load: &Value) -> Option<String> {
    ["paidTier", "currentTier"]
        .iter()
        .filter_map(|key| load.get(*key)?.get("name")?.as_str())
        .map(str::trim)
        .find(|item| !item.is_empty())
        .map(str::to_string)
}

pub fn user_agent(endpoint: &str) -> String {
    if endpoint.contains(RETRIEVE_QUOTA_ENDPOINT) {
        return format!("antigravity/cli/{} {}/{}", CLI_VERSION, os_name(), arch_name());
    }
    let base = format!("antigravity/{} {}/{}", IDE_VERSION, os_name(), arch_name());
    if endpoint.contains(LOAD_ENDPOINT) {
        format!(
            "{} google-api-nodejs-client/{}",
            base, GOOGLE_API_NODEJS_CLIENT_VERSION
        )
    } else {
        base
    }
}

fn os_name() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "linux" => "linux",
        _ => "windows",
    }
}

fn arch_name() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        _ => "amd64",
    }
}

fn platform_name() -> &'static str {
    match (os_name(), arch_name()) {
        ("darwin", "amd64") => "DARWIN_AMD64",
        ("darwin", "arm64") => "DARWIN_ARM64",
        ("linux", "amd64") => "LINUX_AMD64",
        ("linux", "arm64") => "LINUX_ARM64",
        ("windows", "amd64") => "WINDOWS_AMD64",
        _ => "PLATFORM_UNSPECIFIED",
    }
}
