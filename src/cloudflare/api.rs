//! Cloudflare API v4 client for zones and DNS records. Auth is a single
//! scoped API Token (`Authorization: Bearer ...`) — Cloudflare's own
//! current guidance over the legacy Global API Key, which grants
//! full-account access with no scoping at all.
//!
//! Unlike GoDaddy, Cloudflare hands back a stable per-record `id` on every
//! DNS record, so update/delete here are real `PUT`/`DELETE` calls by ID —
//! no delete-then-recreate workaround needed.

use std::thread;
use std::time::Duration;

use reqwest::blocking::Client;
use serde_json::Value;

use super::config::AccountWithSecret;

const BASE_URL: &str = "https://api.cloudflare.com/client/v4";

#[derive(Debug, Clone)]
pub struct Zone {
    pub id: String,
    pub name: String,
}

// `account_label`/`account_id` mirror GoDaddy's DnsRecord shape (kept for
// API completeness / future multi-account views) but the current TUI table
// doesn't display them.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct DnsRecord {
    /// Cloudflare's own stable record ID — used directly for update/delete.
    pub id: String,
    pub zone_id: String,
    pub domain: String,
    pub subdomain: String,
    pub type_: String,
    pub value: String,
    pub ttl: i64,
    pub proxied: bool,
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
    pub proxied: bool,
    pub priority: Option<i64>,
    pub weight: Option<i64>,
    pub port: Option<i64>,
    pub flags: Option<i64>,
    pub tag: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TestKeyResult {
    pub ok: bool,
    pub zones_count: usize,
    pub message: Option<String>,
}

/// Records-endpoint error that keeps the HTTP status so callers can
/// silently skip zones this token can't see (403 — a scoped token that
/// only covers some zones on the account).
pub struct RecordsError {
    pub status: u16,
    pub message: String,
}

fn client() -> Client {
    Client::builder().timeout(Duration::from_secs(20)).build().unwrap_or_else(|_| Client::new())
}

fn auth_header(account: &AccountWithSecret) -> String {
    format!("Bearer {}", account.api_token)
}

/// Cloudflare's error envelope is `{"success": false, "errors": [{"code":
/// N, "message": "..."}], ...}` — join every message so a multi-error
/// response (e.g. both a validation error and a permission note) isn't
/// silently truncated to just the first one.
fn extract_error_detail(body: &str, fallback: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(body) {
        if let Some(errors) = v.get("errors").and_then(|e| e.as_array()) {
            let parts: Vec<String> = errors
                .iter()
                .filter_map(|e| {
                    let code = e.get("code").and_then(|c| c.as_i64());
                    let msg = e.get("message").and_then(|m| m.as_str())?;
                    Some(match code {
                        Some(c) => format!("{msg} (code {c})"),
                        None => msg.to_string(),
                    })
                })
                .collect();
            if !parts.is_empty() {
                return parts.join(" — ");
            }
        }
    }
    if body.trim().is_empty() {
        fallback.to_string()
    } else {
        body.to_string()
    }
}

/// Turns a relative name typed in the UI ("@", "", "www") plus the zone
/// name into the full DNS name Cloudflare's API expects ("example.com",
/// "www.example.com").
fn to_full_name(subdomain: &str, zone_name: &str) -> String {
    let trimmed = subdomain.trim();
    if trimmed.is_empty() || trimmed == "@" {
        zone_name.to_string()
    } else {
        format!("{trimmed}.{zone_name}")
    }
}

/// The inverse of `to_full_name` — turns Cloudflare's full record name back
/// into the relative form the UI shows ("@" for the zone apex).
fn to_relative_name(full_name: &str, zone_name: &str) -> String {
    if full_name == zone_name {
        "@".to_string()
    } else if let Some(rel) = full_name.strip_suffix(&format!(".{zone_name}")) {
        rel.to_string()
    } else {
        full_name.to_string()
    }
}

pub fn get_zones(account: &AccountWithSecret) -> Result<Vec<Zone>, String> {
    let mut zones = Vec::new();
    let mut page = 1u32;
    loop {
        let url = format!("{BASE_URL}/zones?page={page}&per_page=50");
        let resp = client()
            .get(&url)
            .header("Authorization", auth_header(account))
            .header("Accept", "application/json")
            .send()
            .map_err(|e| e.to_string())?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            return Err(format!("Zones request failed ({status}): {}", extract_error_detail(&body, "unknown error")));
        }

        let payload: Value = resp.json().map_err(|e| e.to_string())?;
        let arr = payload.get("result").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        for z in &arr {
            let id = z.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_string();
            let name = z.get("name").and_then(|v| v.as_str()).unwrap_or_default().to_string();
            if !id.is_empty() && !name.is_empty() {
                zones.push(Zone { id, name });
            }
        }

        let total_pages = payload
            .get("result_info")
            .and_then(|ri| ri.get("total_pages"))
            .and_then(|v| v.as_u64())
            .unwrap_or(1);
        if (page as u64) >= total_pages || arr.is_empty() {
            break;
        }
        page += 1;
    }
    Ok(zones)
}

/// Parses a single Cloudflare DNS record JSON object into our `DnsRecord`,
/// pulling type-specific fields (priority/weight/port/flags/tag) out of the
/// nested `data` object SRV/CAA records carry theirs in, falling back to
/// top-level `priority` for MX.
fn parse_record(r: &Value, account: &AccountWithSecret, zone_id: &str, zone_name: &str) -> DnsRecord {
    let rtype = r.get("type").and_then(|v| v.as_str()).unwrap_or("UNKNOWN").to_string();
    let full_name = r.get("name").and_then(|v| v.as_str()).unwrap_or(zone_name);
    let data = r.get("data");
    let priority = r
        .get("priority")
        .and_then(|v| v.as_i64())
        .or_else(|| data.and_then(|d| d.get("priority")).and_then(|v| v.as_i64()));
    DnsRecord {
        id: r.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
        zone_id: zone_id.to_string(),
        domain: zone_name.to_string(),
        subdomain: to_relative_name(full_name, zone_name),
        type_: rtype,
        value: r.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        ttl: r.get("ttl").and_then(|v| v.as_i64()).unwrap_or(1),
        proxied: r.get("proxied").and_then(|v| v.as_bool()).unwrap_or(false),
        priority,
        weight: data.and_then(|d| d.get("weight")).and_then(|v| v.as_i64()),
        port: data.and_then(|d| d.get("port")).and_then(|v| v.as_i64()),
        flags: data.and_then(|d| d.get("flags")).and_then(|v| v.as_i64()),
        tag: data.and_then(|d| d.get("tag")).and_then(|v| v.as_str()).map(|s| s.to_string()),
        account_label: account.label.clone(),
        account_id: account.id.clone(),
    }
}

pub fn get_zone_records(account: &AccountWithSecret, zone_id: &str, zone_name: &str) -> Result<Vec<DnsRecord>, RecordsError> {
    let mut records = Vec::new();
    let mut page = 1u32;
    loop {
        let url = format!("{BASE_URL}/zones/{zone_id}/dns_records?page={page}&per_page=100");
        let resp = client()
            .get(&url)
            .header("Authorization", auth_header(account))
            .header("Accept", "application/json")
            .send()
            .map_err(|e| RecordsError { status: 0, message: e.to_string() })?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().unwrap_or_default();
            return Err(RecordsError {
                status,
                message: format!("Records request failed for {zone_name} ({status}): {}", extract_error_detail(&body, "unknown error")),
            });
        }

        let payload: Value = resp.json().map_err(|e| RecordsError { status: 0, message: e.to_string() })?;
        let arr = payload.get("result").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        for r in &arr {
            records.push(parse_record(r, account, zone_id, zone_name));
        }

        let total_pages =
            payload.get("result_info").and_then(|ri| ri.get("total_pages")).and_then(|v| v.as_u64()).unwrap_or(1);
        if (page as u64) >= total_pages || arr.is_empty() {
            break;
        }
        page += 1;
    }
    Ok(records)
}

/// Builds the Cloudflare record JSON body for a record input. SRV and CAA
/// carry their type-specific fields in a nested `data` object instead of
/// top-level (unlike GoDaddy, which is flat for every type); `content` is
/// omitted for those two since Cloudflare derives it from `data` itself,
/// and providing a mismatched one is rejected.
fn record_payload(record: &DnsRecordInput, zone_name: &str) -> Value {
    let name = to_full_name(&record.subdomain, zone_name);
    let mut obj = serde_json::json!({
        "type": record.type_,
        "name": name,
        "ttl": record.ttl,
    });
    let map = obj.as_object_mut().expect("record_payload always builds an object");

    // Only A/AAAA/CNAME are "proxiable" — sending `proxied` on a type that
    // isn't is a hard validation error, not a harmless no-op.
    if matches!(record.type_.as_str(), "A" | "AAAA" | "CNAME") {
        map.insert("proxied".to_string(), record.proxied.into());
    }

    match record.type_.as_str() {
        "SRV" => {
            map.insert(
                "data".to_string(),
                serde_json::json!({
                    "priority": record.priority.unwrap_or(0),
                    "weight": record.weight.unwrap_or(0),
                    "port": record.port.unwrap_or(0),
                    "target": record.value,
                }),
            );
        }
        "CAA" => {
            map.insert(
                "data".to_string(),
                serde_json::json!({
                    "flags": record.flags.unwrap_or(0),
                    "tag": record.tag.clone().unwrap_or_else(|| "issue".to_string()),
                    "value": record.value,
                }),
            );
        }
        "MX" => {
            map.insert("content".to_string(), record.value.clone().into());
            map.insert("priority".to_string(), record.priority.unwrap_or(10).into());
        }
        _ => {
            map.insert("content".to_string(), record.value.clone().into());
        }
    }
    obj
}

pub fn add_record(account: &AccountWithSecret, zone_id: &str, zone_name: &str, record: &DnsRecordInput) -> Result<(), String> {
    let url = format!("{BASE_URL}/zones/{zone_id}/dns_records");
    let body = record_payload(record, zone_name);
    let resp = client().post(&url).header("Authorization", auth_header(account)).json(&body).send().map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        let status = resp.status();
        let b = resp.text().unwrap_or_default();
        return Err(format!("Failed to add record for {zone_name} ({status}): {}", extract_error_detail(&b, "unknown error")));
    }
    Ok(())
}

/// A real update-by-ID, unlike GoDaddy's delete-then-recreate — Cloudflare
/// records have a stable ID that survives a content/type change.
pub fn update_record(
    account: &AccountWithSecret,
    zone_id: &str,
    zone_name: &str,
    record_id: &str,
    new: &DnsRecordInput,
) -> Result<(), String> {
    let url = format!("{BASE_URL}/zones/{zone_id}/dns_records/{record_id}");
    let body = record_payload(new, zone_name);
    let resp = client().put(&url).header("Authorization", auth_header(account)).json(&body).send().map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        let status = resp.status();
        let b = resp.text().unwrap_or_default();
        return Err(format!("Failed to update record for {zone_name} ({status}): {}", extract_error_detail(&b, "unknown error")));
    }
    Ok(())
}

pub fn delete_record(account: &AccountWithSecret, zone_id: &str, zone_name: &str, record_id: &str) -> Result<(), String> {
    let url = format!("{BASE_URL}/zones/{zone_id}/dns_records/{record_id}");
    let resp = client().delete(&url).header("Authorization", auth_header(account)).send().map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        let status = resp.status();
        let b = resp.text().unwrap_or_default();
        return Err(format!("Failed to delete record for {zone_name} ({status}): {}", extract_error_detail(&b, "unknown error")));
    }
    Ok(())
}

pub fn test_token(api_token: &str) -> TestKeyResult {
    let probe = AccountWithSecret { id: "probe".into(), label: "probe".into(), api_token: api_token.to_string() };
    match get_zones(&probe) {
        Ok(zones) => TestKeyResult { ok: true, zones_count: zones.len(), message: None },
        Err(e) => TestKeyResult { ok: false, zones_count: 0, message: Some(e) },
    }
}

/// Fetches every DNS record across every zone visible to this account.
/// Zones a scoped token can't see records for (403) are skipped rather
/// than failing the whole fetch.
pub fn fetch_account_records(account: &AccountWithSecret) -> Result<(Vec<DnsRecord>, usize, usize), String> {
    let zones = get_zones(account)?;
    let mut records = Vec::new();
    let mut skipped = 0usize;
    for z in &zones {
        match get_zone_records(account, &z.id, &z.name) {
            Ok(mut r) => records.append(&mut r),
            Err(e) if e.status == 403 => skipped += 1,
            Err(e) => return Err(e.message),
        }
    }
    Ok((records, zones.len(), skipped))
}

/// Fetches every DNS record across every zone of every given account — the
/// "global search" data set. Accounts are queried in parallel (one thread
/// each); an account whose token fails entirely is reported in `errors`
/// rather than failing the whole fetch.
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
    let mut zones_count = 0usize;
    let mut skipped_count = 0usize;
    let mut errors = Vec::new();

    for h in handles {
        match h.join() {
            Ok(Ok((recs, zones, skipped))) => {
                records.extend(recs);
                zones_count += zones;
                skipped_count += skipped;
            }
            Ok(Err((label, e))) => errors.push((label, e)),
            Err(_) => {} // account fetch thread panicked; skip it
        }
    }

    (records, zones_count, skipped_count, errors)
}
