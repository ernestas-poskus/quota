use quota_core::antigravity::local_usage::{
    agy_quota_summary, gemini_quota_from_models, needs_agy_fallback, normalize_quota_response,
    normalize_quota_summaries, project_id, user_agent,
};
use serde_json::json;

fn quota_payload() -> serde_json::Value {
    json!({
        "groups": [
            {
                "buckets": [
                    { "bucketId": "gemini-5h", "remainingFraction": 0.88, "resetTime": "2027-01-01T00:00:00Z" },
                    { "bucketId": "gemini-weekly", "remainingFraction": 0.74 }
                ]
            },
            {
                "buckets": [
                    { "bucketId": "3p-5h", "remainingFraction": 0.5 },
                    { "bucketId": "3p-weekly", "remainingFraction": 0.31 }
                ]
            }
        ]
    })
}

#[test]
fn keeps_remaining_fraction_as_remaining_without_inverting() {
    // Antigravity is the exception among these providers. It reports what is
    // left, not what was used, so nothing here inverts.
    let usage = normalize_quota_response(&quota_payload(), None);

    assert_eq!(usage.provider, "antigravity");
    assert_eq!(usage.windows[0].remaining_percent, Some(88));
    assert_eq!(usage.windows[1].remaining_percent, Some(74));
}

#[test]
fn the_compact_token_shows_only_the_gemini_pair() {
    // Four numbers stop being a compact token, and Gemini is the reason people
    // run Antigravity.
    let usage = normalize_quota_response(&quota_payload(), None);

    assert_eq!(
        usage.compact_token().as_deref(),
        Some("Gemini/Argon 5h 88% · Gemini/Argon Wk 74%")
    );
}

#[test]
fn the_third_party_windows_are_still_carried_in_full() {
    let usage = normalize_quota_response(&quota_payload(), None);

    assert_eq!(usage.windows.len(), 4);
    assert_eq!(usage.windows[2].label, "Claude/GPT 5h");
    assert_eq!(usage.windows[2].remaining_percent, Some(50));
    assert_eq!(usage.windows[3].label, "Claude/GPT Wk");
    assert_eq!(usage.windows[3].remaining_percent, Some(31));
}

#[test]
fn group_names_route_renamed_buckets_to_the_right_pool() {
    // The vendor has renamed bucket ids before; the group display name is the
    // durable signal. A bucket id that says `argon` still belongs to the Gemini
    // pool when the group says "Gemini Models".
    let raw = json!({
        "groups": [
            {
                "displayName": "Gemini Models",
                "buckets": [
                    { "bucketId": "argon-weekly", "window": "weekly", "remainingFraction": 0.6 }
                ]
            },
            {
                "displayName": "Claude and GPT models",
                "buckets": [
                    { "bucketId": "third-party-5h", "window": "5h", "remainingFraction": 0.4 }
                ]
            }
        ]
    });

    let usage = normalize_quota_response(&raw, None);

    assert_eq!(usage.windows[1].label, "Gemini/Argon Wk");
    assert_eq!(usage.windows[1].remaining_percent, Some(60));
    assert_eq!(usage.windows[2].label, "Claude/GPT 5h");
    assert_eq!(usage.windows[2].remaining_percent, Some(40));
}

/// The AI Code summary. It carries the Gemini pool and the separate Gemini 4
/// Argon allowance, and wraps the same group/bucket shape the Cloud Code
/// summary returns directly.
fn aicode_payload() -> serde_json::Value {
    json!({
        "summary": {
            "response": {
                "groups": [
                    {
                        "displayName": "Gemini Models",
                        "buckets": [
                            { "bucketId": "gemini-5h", "window": "5h", "remainingFraction": 0.94, "resetTime": "2027-01-01T00:00:00Z" },
                            { "bucketId": "gemini-weekly", "window": "weekly", "remainingFraction": 0.53 }
                        ]
                    },
                    {
                        "displayName": "Gemini 4 Argon",
                        "buckets": [
                            { "bucketId": "argon-weekly", "window": "weekly", "remainingFraction": 1 }
                        ]
                    }
                ]
            }
        }
    })
}

fn models_payload() -> serde_json::Value {
    json!({
        "models": {
            "gemini-3-flash": {
                "apiProvider": "API_PROVIDER_GOOGLE_GEMINI",
                "quotaInfo": { "remainingFraction": 0.94, "resetTime": "2027-01-01T00:00:00Z" }
            },
            "gemini-3-flash-fast": {
                "apiProvider": "API_PROVIDER_GOOGLE_GEMINI",
                "quotaInfo": { "remainingFraction": 0.9, "resetTime": "2027-01-01T00:00:00Z" }
            },
            "claude-sonnet-5-5-high": {
                "apiProvider": "API_PROVIDER_ANTHROPIC_VERTEX",
                "quotaInfo": { "remainingFraction": 0.1, "resetTime": "2027-01-01T00:00:00Z" }
            },
            "chat_20706": {
                "apiProvider": "API_PROVIDER_INTERNAL",
                "quotaInfo": { "remainingFraction": 1 }
            }
        }
    })
}

#[test]
fn the_models_response_recovers_the_gemini_five_hour_window() {
    // The models response carries the pool's 5-hour reading even when the AI
    // Code summary is out of reach, so the Gemini slot is not left blank. The
    // strictest reading wins, and other pools plus internal models are skipped.
    let derived = gemini_quota_from_models(&models_payload()).expect("derive Gemini summary");

    let usage = normalize_quota_summaries(&[&derived], None);

    assert_eq!(usage.windows[0].label, "Gemini/Argon 5h");
    assert_eq!(usage.windows[0].remaining_percent, Some(90));
    assert_eq!(usage.windows[0].reset_at, Some(1_798_761_600));
    // Only the 5-hour window can be recovered this way; the rest stay unknown.
    assert_eq!(usage.windows[1].remaining_percent, None);
    assert_eq!(usage.windows[3].remaining_percent, None);
}

#[test]
fn the_models_fallback_never_overrides_a_summary() {
    let aicode = aicode_payload();
    let derived = gemini_quota_from_models(&models_payload()).expect("derive Gemini summary");

    let usage = normalize_quota_summaries(&[&aicode, &derived], None);

    // AI Code reports 94%; the models fallback would be 90%.
    assert_eq!(usage.windows[0].remaining_percent, Some(94));
}

#[test]
fn a_models_response_without_quota_yields_no_summary() {
    let empty = json!({ "models": {} });
    let no_reset = json!({
        "models": {
            "chat_1": { "quotaInfo": { "remainingFraction": 1 } }
        }
    });

    assert!(gemini_quota_from_models(&empty).is_none());
    assert!(gemini_quota_from_models(&no_reset).is_none());
    assert!(gemini_quota_from_models(&json!({})).is_none());
}

#[test]
fn the_ai_code_summary_fills_the_gemini_pool_and_appends_argon() {
    let aicode = aicode_payload();

    let usage = normalize_quota_summaries(&[&aicode], None);

    assert_eq!(usage.windows.len(), 5);
    assert_eq!(usage.windows[0].label, "Gemini/Argon 5h");
    assert_eq!(usage.windows[0].remaining_percent, Some(94));
    assert_eq!(usage.windows[0].reset_at, Some(1_798_761_600));
    assert_eq!(usage.windows[1].remaining_percent, Some(53));
    // The separate Gemini 4 Argon allowance follows the four standard windows.
    assert_eq!(usage.windows[4].label, "Gemini 4 Argon Wk");
    assert_eq!(usage.windows[4].remaining_percent, Some(100));
}

#[test]
fn both_summaries_merge_into_one_record() {
    // Gemini/Argon comes from the AI Code summary, Claude/GPT from Cloud Code.
    let aicode = aicode_payload();
    let cloud = quota_payload();

    let usage = normalize_quota_summaries(&[&aicode, &cloud], None);

    assert_eq!(usage.windows.len(), 5);
    assert_eq!(usage.windows[0].remaining_percent, Some(94));
    assert_eq!(usage.windows[2].label, "Claude/GPT 5h");
    assert_eq!(usage.windows[2].remaining_percent, Some(50));
    assert_eq!(usage.windows[3].remaining_percent, Some(31));
}

#[test]
fn the_first_summary_wins_when_both_report_a_window() {
    // The AI Code summary is passed first, so its reading, not Cloud Code's, is
    // the one the desktop app shows.
    let aicode = json!({
        "groups": [{
            "displayName": "Claude and GPT models",
            "buckets": [
                { "bucketId": "3p-weekly", "window": "weekly", "remainingFraction": 0.25 }
            ]
        }]
    });
    let cloud = quota_payload();

    let usage = normalize_quota_summaries(&[&aicode, &cloud], None);

    assert_eq!(usage.windows[3].remaining_percent, Some(25));
}

#[test]
fn the_ai_code_summary_is_accepted_with_or_without_its_wrapper() {
    // The AI Code endpoint wraps the summary twice today; ride out a wrapper
    // rename without losing the numbers.
    let body = json!({
        "groups": [{
            "displayName": "Gemini Models",
            "buckets": [
                { "bucketId": "gemini-weekly", "window": "weekly", "remainingFraction": 0.42 }
            ]
        }]
    });
    let wrapped_once = json!({ "summary": body.clone() });
    let bare = body;

    for raw in [wrapped_once, bare] {
        let usage = normalize_quota_summaries(&[&raw], None);
        assert_eq!(usage.windows[1].remaining_percent, Some(42));
    }
}

#[test]
fn a_disabled_bucket_reports_zero_rather_than_a_stale_fraction() {
    // Antigravity disables the 5-hour bucket once the weekly limit is
    // exhausted; the raw fraction can still read 1.
    let raw = json!({
        "groups": [{
            "displayName": "Claude and GPT models",
            "buckets": [
                { "bucketId": "3p-5h", "window": "5h", "remainingFraction": 1, "disabled": true },
                { "bucketId": "3p-weekly", "window": "weekly", "remainingFraction": 0 }
            ]
        }]
    });

    let usage = normalize_quota_response(&raw, None);

    assert_eq!(usage.windows[2].remaining_percent, Some(0));
    assert_eq!(usage.windows[3].remaining_percent, Some(0));
}

#[test]
fn buckets_are_found_regardless_of_which_group_holds_them() {
    // The grouping carries no meaning Quota needs, so a payload that puts every
    // bucket in one group must read the same.
    let raw = json!({
        "groups": [{
            "buckets": [
                { "bucketId": "3p-weekly", "remainingFraction": 0.31 },
                { "bucketId": "gemini-weekly", "remainingFraction": 0.74 },
                { "bucketId": "gemini-5h", "remainingFraction": 0.88 }
            ]
        }]
    });

    let usage = normalize_quota_response(&raw, None);

    assert_eq!(usage.windows[0].remaining_percent, Some(88));
    assert_eq!(usage.windows[1].remaining_percent, Some(74));
    assert_eq!(usage.windows[3].remaining_percent, Some(31));
}

#[test]
fn a_fraction_sent_as_a_string_still_parses() {
    let raw = json!({
        "groups": [{ "buckets": [{ "bucketId": "gemini-5h", "remainingFraction": "0.42" }] }]
    });

    let usage = normalize_quota_response(&raw, None);

    assert_eq!(usage.windows[0].remaining_percent, Some(42));
}

#[test]
fn an_empty_payload_produces_four_windows_with_no_numbers() {
    let usage = normalize_quota_response(&json!({}), None);

    assert_eq!(usage.windows.len(), 4);
    assert!(usage.windows.iter().all(|w| w.remaining_percent.is_none()));
    assert_eq!(usage.compact_token(), None);
}

#[test]
fn an_unknown_bucket_id_is_ignored() {
    let raw = json!({
        "groups": [{
            "buckets": [
                { "bucketId": "gemini-5h", "remainingFraction": 0.9 },
                { "bucketId": "something-new", "remainingFraction": 0.1 }
            ]
        }]
    });

    let usage = normalize_quota_response(&raw, None);

    assert_eq!(usage.windows.len(), 4);
    assert_eq!(usage.windows[0].remaining_percent, Some(90));
}

#[test]
fn reset_times_parse_from_rfc3339_and_from_epochs() {
    let raw = json!({
        "groups": [{
            "buckets": [
                { "bucketId": "gemini-5h", "remainingFraction": 1, "resetTime": "2027-01-01T00:00:00Z" },
                { "bucketId": "gemini-weekly", "remainingFraction": 1, "resetTime": 1_798_761_600_000i64 },
                { "bucketId": "3p-5h", "remainingFraction": 1, "resetTime": "1798761600" }
            ]
        }]
    });

    let usage = normalize_quota_response(&raw, None);

    assert_eq!(usage.windows[0].reset_at, Some(1_798_761_600));
    assert_eq!(usage.windows[1].reset_at, Some(1_798_761_600));
    assert_eq!(usage.windows[2].reset_at, Some(1_798_761_600));
}

#[test]
fn a_zero_reset_time_is_treated_as_unknown() {
    let raw = json!({
        "groups": [{ "buckets": [{ "bucketId": "gemini-5h", "remainingFraction": 1, "resetTime": 0 }] }]
    });

    let usage = normalize_quota_response(&raw, None);

    assert_eq!(usage.windows[0].reset_at, None);
}

#[test]
fn the_project_id_reads_from_every_shape_google_sends() {
    assert_eq!(
        project_id(&json!({ "cloudaicompanionProject": "proj-bare" })).as_deref(),
        Some("proj-bare")
    );
    assert_eq!(
        project_id(&json!({ "cloudaicompanionProject": { "id": "proj-id" } })).as_deref(),
        Some("proj-id")
    );
    assert_eq!(
        project_id(&json!({ "cloudaicompanionProject": { "projectId": "proj-field" } })).as_deref(),
        Some("proj-field")
    );
    assert_eq!(project_id(&json!({})), None);
    assert_eq!(project_id(&json!({ "cloudaicompanionProject": "  " })), None);
}

#[test]
fn the_quota_summary_call_uses_the_cli_user_agent() {
    // Without it, Google leaves the Gemini group out of the quota summary (issue #17).
    assert!(user_agent("v1internal:retrieveUserQuotaSummary").starts_with("antigravity/cli/"));
}

#[test]
fn the_other_code_assist_calls_keep_the_ide_user_agent() {
    assert!(user_agent("v1internal:fetchAvailableModels").starts_with("antigravity/1."));
    let load = user_agent("v1internal:loadCodeAssist");
    assert!(load.starts_with("antigravity/1.") && load.contains("google-api-nodejs-client/"));
}

/// The Antigravity CLI's `/quota --output-format json` payload, trimmed to the
/// fields Quota reads. The buckets use snake_case and `id`/`name` spellings.
fn agy_output() -> serde_json::Value {
    json!({
        "status": "SUCCESS",
        "response": "Gemini Models\tWeekly Limit Remaining\t53%\t2027-01-01T00:00:00Z\n",
        "command": {
            "name": "usage",
            "data": {
                "description": "Within each group, models share a weekly limit and a 5-hour limit.",
                "groups": [
                    {
                        "name": "Gemini Models",
                        "buckets": [
                            { "id": "gemini-weekly", "name": "Weekly Limit Remaining", "window": "weekly", "remaining_fraction": 0.5253065824508667, "reset_time": "2027-01-01T00:00:00Z" },
                            { "id": "gemini-5h", "name": "Five Hour Limit Remaining", "window": "5h", "remaining_fraction": 0.9374297857284546, "reset_time": "2027-01-01T00:00:00Z" }
                        ]
                    },
                    {
                        "name": "Claude and GPT models",
                        "buckets": [
                            { "id": "3p-weekly", "name": "Weekly Limit Remaining", "window": "weekly", "remaining_fraction": 0, "reset_time": "2027-01-01T00:00:00Z" },
                            { "id": "3p-5h", "name": "Five Hour Limit Remaining", "window": "5h", "disabled": true, "remaining_fraction": 1 }
                        ]
                    },
                    {
                        "name": "Gemini 4 Argon",
                        "buckets": [
                            { "id": "gemini-argon-wtu-weekly", "name": "Gemini 4 Argon Weekly Limit", "window": "weekly", "remaining_fraction": 1, "reset_time": "2027-01-01T00:00:00Z" }
                        ]
                    }
                ]
            }
        }
    })
}

#[test]
fn the_agy_quota_view_is_normalized_like_an_api_summary() {
    let summary = agy_quota_summary(&agy_output()).expect("convert the CLI view");

    let usage = normalize_quota_summaries(&[&summary], None);

    assert_eq!(usage.windows.len(), 5);
    assert_eq!(usage.windows[0].remaining_percent, Some(94));
    assert_eq!(usage.windows[1].label, "Gemini/Argon Wk");
    assert_eq!(usage.windows[1].remaining_percent, Some(53));
    assert_eq!(usage.windows[1].reset_at, Some(1_798_761_600));
    assert_eq!(usage.windows[2].remaining_percent, Some(0));
    assert_eq!(usage.windows[3].remaining_percent, Some(0));
    assert_eq!(usage.windows[4].label, "Gemini 4 Argon Wk");
    assert_eq!(usage.windows[4].remaining_percent, Some(100));
}

#[test]
fn the_agy_fallback_fills_the_weekly_window_the_apis_leave_blank() {
    // The models response covers the 5-hour window; the CLI view supplies the
    // Gemini weekly window and the Argon allowance the APIs do not report.
    let derived = gemini_quota_from_models(&models_payload()).expect("derive Gemini summary");
    let agy = agy_quota_summary(&agy_output()).expect("convert the CLI view");

    let usage = normalize_quota_summaries(&[&derived, &agy], None);

    assert_eq!(usage.windows[0].remaining_percent, Some(90));
    assert_eq!(usage.windows[1].remaining_percent, Some(53));
    assert_eq!(usage.windows[4].label, "Gemini 4 Argon Wk");
    assert_eq!(usage.windows[4].remaining_percent, Some(100));
}

#[test]
fn an_agy_view_without_a_command_payload_is_ignored() {
    assert!(agy_quota_summary(&json!({})).is_none());
    assert!(agy_quota_summary(&json!({ "status": "ERROR", "response": "" })).is_none());
    assert!(agy_quota_summary(&json!({ "command": { "data": { "groups": [] } } })).is_none());
}

#[test]
fn the_cli_is_asked_when_the_argon_allowance_is_missing() {
    // Cloud Code reports the Gemini weekly window, but the separate Gemini 4
    // Argon allowance is only in the CLI's own view.
    let cloud = quota_payload();

    assert!(needs_agy_fallback(&[&cloud]));
}

#[test]
fn the_cli_is_asked_when_the_gemini_weekly_window_is_missing() {
    // The models response only recovers the 5-hour reading, so the weekly
    // window still needs a source.
    let derived = gemini_quota_from_models(&models_payload()).expect("derive Gemini summary");

    assert!(needs_agy_fallback(&[&derived]));
}

#[test]
fn the_cli_is_not_asked_when_the_apis_cover_the_windows_it_reports() {
    let cloud = quota_payload();
    let agy = agy_quota_summary(&agy_output()).expect("convert the CLI view");

    assert!(!needs_agy_fallback(&[&cloud, &agy]));
}
