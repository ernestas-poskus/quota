use quota_core::antigravity::local::{
    choose_credentials, parse_keyring_secret, LocalAntigravityCredentials, LocalCredentialError,
};

// 2026-10-29, between the expired and the fresh expiry below.
const NOW_MS: i64 = 1_793_300_000_000;
const EXPIRED: &str = "2026-10-19T18:57:30Z";
const FRESH: &str = "2027-01-01T00:00:00Z";

type Lookup = Result<LocalAntigravityCredentials, LocalCredentialError>;

fn creds(access_token: &str, expiry: &str) -> Lookup {
    parse_keyring_secret(&format!(
        r#"{{"token":{{"access_token":"{}","refresh_token":"1//refresh","expiry":"{}"}}}}"#,
        access_token, expiry
    ))
}

fn not_found() -> Lookup {
    Err(LocalCredentialError::NotFound)
}

fn never_called() -> Lookup {
    panic!("this source should not be read")
}

fn token(result: Lookup) -> String {
    result.expect("credentials").access_token
}

#[test]
fn a_fresh_cli_file_wins_without_touching_the_keyring() {
    let picked = choose_credentials(creds("file", FRESH), never_called, never_called, NOW_MS);
    assert_eq!(token(picked), "file");
}

#[test]
fn an_expired_cli_file_loses_to_the_keyring() {
    let picked = choose_credentials(
        creds("file", EXPIRED),
        || creds("keyring", FRESH),
        never_called,
        NOW_MS,
    );
    assert_eq!(token(picked), "keyring");
}

#[test]
fn an_expired_cli_file_is_still_used_when_the_keyring_is_empty() {
    let picked = choose_credentials(creds("file", EXPIRED), not_found, never_called, NOW_MS);
    assert_eq!(token(picked), "file");
}

#[test]
fn the_keyring_is_used_when_there_is_no_cli_file() {
    let picked = choose_credentials(not_found(), || creds("keyring", FRESH), never_called, NOW_MS);
    assert_eq!(token(picked), "keyring");
}

#[test]
fn the_gemini_file_is_the_last_resort() {
    let picked = choose_credentials(not_found(), not_found, || creds("gemini", FRESH), NOW_MS);
    assert_eq!(token(picked), "gemini");
}

#[test]
fn a_broken_cli_file_is_reported_instead_of_not_found() {
    let picked = choose_credentials(
        Err(LocalCredentialError::Malformed("bad file".to_string())),
        not_found,
        not_found,
        NOW_MS,
    );
    assert!(matches!(picked, Err(LocalCredentialError::Malformed(_))));
}

#[test]
fn nothing_anywhere_reports_the_keyring_error() {
    let picked = choose_credentials(not_found(), not_found, not_found, NOW_MS);
    assert!(matches!(picked, Err(LocalCredentialError::NotFound)));
}
