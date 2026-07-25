//! GoDaddy Domains REST client. Ported from the Electron app's
//! `src/main/godaddy.ts` onto a blocking `reqwest` client.

use std::thread;
use std::time::Duration;

use reqwest::blocking::Client;
use serde_json::Value;

use super::config::AccountWithSecret;

const BASE_URL: &str = "https://api.godaddy.com";

// `id`/`account_id` mirror the original Electron app's DnsRecord shape
// (kept for API completeness / future multi-account views) but the current
// TUI table doesn't display them.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct DnsRecord {
    pub id: String,
    pub domain: String,
    pub subdomain: String,
    pub type_: String,
    pub value: String,
    pub ttl: i64,
    /// MX, SRV
    pub priority: Option<i64>,
    /// SRV
    pub weight: Option<i64>,
    /// SRV
    pub port: Option<i64>,
    /// CAA
    pub flags: Option<i64>,
    /// CAA: "issue" | "issuewild" | "iodef"
    pub tag: Option<String>,
    pub account_label: String,
    pub account_id: String,
}

#[derive(Debug, Clone, Default)]
pub struct DnsRecordInput {
    pub type_: String,
    pub subdomain: String,
    pub value: String,
    pub ttl: i64,
    pub priority: Option<i64>,
    pub weight: Option<i64>,
    pub port: Option<i64>,
    pub flags: Option<i64>,
    pub tag: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TestKeyResult {
    pub ok: bool,
    pub domains_count: usize,
    pub message: Option<String>,
}

/// Records-endpoint error that keeps the HTTP status/code so callers can
/// silently skip domains this key can't see (404 UNKNOWN_DOMAIN, 403).
pub struct RecordsError {
    pub status: u16,
    pub code: Option<String>,
    pub message: String,
}

fn client() -> Client {
    Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap_or_else(|_| Client::new())
}

fn auth_header(account: &AccountWithSecret) -> String {
    format!("sso-key {}:{}", account.api_key, account.api_secret)
}

fn to_api_name(subdomain: &str) -> String {
    let trimmed = subdomain.trim();
    if trimmed.is_empty() || trimmed == "@" {
        "@".to_string()
    } else {
        trimmed.to_string()
    }
}

/// GoDaddy's 422 schema-validation errors wrap the real reason in a
/// `fields` array (e.g. `[{"path": "ttl", "message": "ttl must be greater
/// than or equal to 600"}]`) and leave `message` as the generic "see
/// details in `fields`". Surface those field-level messages too, or the
/// actual cause is invisible to the user.
fn extract_error_detail(body: &str, fallback: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(body) {
        let mut parts = Vec::new();
        if let Some(msg) = v.get("message").and_then(|m| m.as_str()) {
            parts.push(msg.to_string());
        }
        if let Some(fields) = v.get("fields").and_then(|f| f.as_array()) {
            for field in fields {
                let path = field.get("path").and_then(|p| p.as_str()).unwrap_or("?");
                let fmsg = field
                    .get("message")
                    .and_then(|m| m.as_str())
                    .or_else(|| field.get("code").and_then(|c| c.as_str()))
                    .unwrap_or("invalid");
                parts.push(format!("{path}: {fmsg}"));
            }
        }
        if !parts.is_empty() {
            return parts.join(" — ");
        }
    }
    if body.trim().is_empty() {
        fallback.to_string()
    } else {
        body.to_string()
    }
}

/// Minimal percent-encoding for path segments (domain / record type / name).
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub fn get_domains(account: &AccountWithSecret) -> Result<Vec<String>, String> {
    let url = format!("{BASE_URL}/v1/domains?limit=500");
    let resp = client()
        .get(&url)
        .header("Authorization", auth_header(account))
        .header("Accept", "application/json")
        .send()
        .map_err(|e| e.to_string())?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        return Err(format!(
            "Domains request failed ({status}): {}",
            extract_error_detail(&body, "unknown error")
        ));
    }

    let payload: Value = resp.json().map_err(|e| e.to_string())?;
    let arr = if payload.is_array() {
        payload
    } else {
        payload.get("domains").cloned().unwrap_or(Value::Array(vec![]))
    };
    let domains = arr
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|d| d.get("domain").and_then(|v| v.as_str()).map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    Ok(domains)
}

pub fn get_domain_records(account: &AccountWithSecret, domain: &str) -> Result<Vec<DnsRecord>, RecordsError> {
    let url = format!("{BASE_URL}/v1/domains/{}/records", urlencode(domain));
    let resp = client()
        .get(&url)
        .header("Authorization", auth_header(account))
        .header("Accept", "application/json")
        .send()
        .map_err(|e| RecordsError { status: 0, code: None, message: e.to_string() })?;

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().unwrap_or_default();
        let code = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|v| v.get("code").and_then(|c| c.as_str()).map(|s| s.to_string()));
        let detail = extract_error_detail(&body, "unknown error");
        return Err(RecordsError {
            status,
            code,
            message: format!("Records request failed for {domain} ({status}): {detail}"),
        });
    }

    let payload: Value = resp
        .json()
        .map_err(|e| RecordsError { status: 0, code: None, message: e.to_string() })?;
    let arr = payload.as_array().cloned().unwrap_or_default();

    let records = arr
        .iter()
        .enumerate()
        .map(|(idx, r)| {
            let rtype = r.get("type").and_then(|v| v.as_str()).unwrap_or("UNKNOWN").to_string();
            let name = r
                .get("name")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or("@")
                .to_string();
            DnsRecord {
                id: format!("{}:{}:{}:{}:{}", account.id, domain, rtype, name, idx),
                domain: domain.to_string(),
                subdomain: name,
                type_: rtype,
                value: r.get("data").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                ttl: r.get("ttl").and_then(|v| v.as_i64()).unwrap_or(0),
                priority: r.get("priority").and_then(|v| v.as_i64()),
                weight: r.get("weight").and_then(|v| v.as_i64()),
                port: r.get("port").and_then(|v| v.as_i64()),
                flags: r.get("flags").and_then(|v| v.as_i64()),
                tag: r.get("tag").and_then(|v| v.as_str()).map(|s| s.to_string()),
                account_label: account.label.clone(),
                account_id: account.id.clone(),
            }
        })
        .collect();
    Ok(records)
}

fn record_set_url(domain: &str, rtype: &str, name: &str) -> String {
    let safe_name = to_api_name(name);
    format!(
        "{BASE_URL}/v1/domains/{}/records/{}/{}",
        urlencode(domain),
        urlencode(rtype),
        urlencode(&safe_name)
    )
}

fn get_record_set(account: &AccountWithSecret, domain: &str, rtype: &str, name: &str) -> Result<Vec<Value>, String> {
    let url = record_set_url(domain, rtype, name);
    let resp = client()
        .get(&url)
        .header("Authorization", auth_header(account))
        .send()
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        return Err(format!(
            "Failed to fetch records for {domain} ({status}): {}",
            extract_error_detail(&body, "unknown error")
        ));
    }
    let payload: Value = resp.json().map_err(|e| e.to_string())?;
    Ok(payload.as_array().cloned().unwrap_or_default())
}

/// Builds the GoDaddy record JSON object for a record input, including the
/// type-specific fields GoDaddy expects: `priority` for MX/SRV, `weight`
/// and `port` for SRV, `flags`/`tag` for CAA.
fn record_payload(record: &DnsRecordInput) -> Value {
    let mut obj = serde_json::json!({
        "type": record.type_,
        "name": to_api_name(&record.subdomain),
        "data": record.value,
        "ttl": record.ttl,
    });
    let map = obj.as_object_mut().expect("record_payload always builds an object");
    if let Some(v) = record.priority {
        map.insert("priority".to_string(), v.into());
    }
    if let Some(v) = record.weight {
        map.insert("weight".to_string(), v.into());
    }
    if let Some(v) = record.port {
        map.insert("port".to_string(), v.into());
    }
    if let Some(v) = record.flags {
        map.insert("flags".to_string(), v.into());
    }
    if let Some(v) = &record.tag {
        map.insert("tag".to_string(), v.clone().into());
    }
    obj
}

pub fn add_record(account: &AccountWithSecret, domain: &str, record: &DnsRecordInput) -> Result<(), String> {
    let url = format!("{BASE_URL}/v1/domains/{}/records", urlencode(domain));
    let body = serde_json::json!([record_payload(record)]);
    let resp = client()
        .patch(&url)
        .header("Authorization", auth_header(account))
        .json(&body)
        .send()
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        let status = resp.status();
        let b = resp.text().unwrap_or_default();
        return Err(format!(
            "Failed to add record for {domain} ({status}): {}",
            extract_error_detail(&b, "unknown error")
        ));
    }
    Ok(())
}

/// Deletes a single record value from a type+name record set. If other
/// values share that type+name, PUTs back the filtered set instead of
/// deleting the whole set (mirrors the Electron app's behaviour, including
/// its race-condition tradeoff between the GET and the PUT).
pub fn delete_record(account: &AccountWithSecret, domain: &str, rtype: &str, name: &str, value: &str) -> Result<(), String> {
    let url = record_set_url(domain, rtype, name);
    let current = get_record_set(account, domain, rtype, name)?;
    let remaining: Vec<Value> = current
        .iter()
        .filter(|r| r.get("data").and_then(|d| d.as_str()) != Some(value))
        .cloned()
        .collect();

    if remaining.len() == current.len() {
        return Err(format!("Record value was not found on GoDaddy for {domain}."));
    }

    if !remaining.is_empty() {
        let payload: Vec<Value> = remaining
            .into_iter()
            .map(|mut r| {
                if let Some(obj) = r.as_object_mut() {
                    obj.remove("type");
                    obj.remove("name");
                }
                r
            })
            .collect();
        let resp = client()
            .put(&url)
            .header("Authorization", auth_header(account))
            .json(&payload)
            .send()
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            let status = resp.status();
            let b = resp.text().unwrap_or_default();
            return Err(format!(
                "Failed to replace records for {domain} ({status}): {}",
                extract_error_detail(&b, "unknown error")
            ));
        }
        return Ok(());
    }

    let resp = client()
        .delete(&url)
        .header("Authorization", auth_header(account))
        .send()
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        let status = resp.status();
        let b = resp.text().unwrap_or_default();
        return Err(format!(
            "Failed to delete record for {domain} ({status}): {}",
            extract_error_detail(&b, "unknown error")
        ));
    }
    Ok(())
}

pub fn update_record(
    account: &AccountWithSecret,
    domain: &str,
    old: &DnsRecord,
    new: &DnsRecordInput,
) -> Result<(), String> {
    delete_record(account, domain, &old.type_, &old.subdomain, &old.value)
        .map_err(|e| format!("DELETE_STEP_FAILED: {e}"))?;
    add_record(account, domain, new).map_err(|e| format!("CREATE_STEP_FAILED: {e}"))
}

pub fn test_key(api_key: &str, api_secret: &str) -> TestKeyResult {
    let probe = AccountWithSecret {
        id: "probe".into(),
        label: "probe".into(),
        api_key: api_key.to_string(),
        api_secret: api_secret.to_string(),
    };
    match get_domains(&probe) {
        Ok(domains) => TestKeyResult { ok: true, domains_count: domains.len(), message: None },
        Err(e) => TestKeyResult { ok: false, domains_count: 0, message: Some(e) },
    }
}

/// Fetches every DNS record across every domain visible to this account.
/// Domains the key can't see (404 UNKNOWN_DOMAIN, 403) are skipped rather
/// than failing the whole fetch.
pub fn fetch_account_records(account: &AccountWithSecret) -> Result<(Vec<DnsRecord>, usize, usize), String> {
    let domains = get_domains(account)?;
    let mut records = Vec::new();
    let mut skipped = 0usize;
    for d in &domains {
        match get_domain_records(account, d) {
            Ok(mut r) => records.append(&mut r),
            Err(e) if e.status == 404 && e.code.as_deref() == Some("UNKNOWN_DOMAIN") => skipped += 1,
            Err(e) if e.status == 403 => skipped += 1,
            Err(e) => return Err(e.message),
        }
    }
    Ok((records, domains.len(), skipped))
}

/// Fetches every DNS record across every domain of every given account —
/// the "global search" data set. Accounts are queried in parallel (one
/// thread each); an account whose key fails entirely is reported in
/// `errors` rather than failing the whole fetch.
pub fn fetch_all_accounts(accounts: Vec<AccountWithSecret>) -> (Vec<DnsRecord>, usize, usize, Vec<(String, String)>) {
    let handles: Vec<_> = accounts
        .into_iter()
        .map(|account| {
            thread::spawn(move || {
                let label = account.label.clone();
                fetch_account_records(&account).map_err(|e| (label, e))
            })
        })
        .collect();

    let mut records = Vec::new();
    let mut domains_count = 0usize;
    let mut skipped_count = 0usize;
    let mut errors = Vec::new();

    for h in handles {
        match h.join() {
            Ok(Ok((recs, domains, skipped))) => {
                records.extend(recs);
                domains_count += domains;
                skipped_count += skipped;
            }
            Ok(Err((label, e))) => errors.push((label, e)),
            Err(_) => {} // account fetch thread panicked; skip it
        }
    }

    (records, domains_count, skipped_count, errors)
}
