//! Server-side AWS Signature Version 4 verification (hand-rolled, design.md §5.7:
//! no aws-* SDK crates) + a minimal test-only signer used by unit tests and as the
//! pre-Task-C self check.
//!
//! Scope (M0 spike):
//! - single-header Authorization: `AWS4-HMAC-SHA256 Credential=AK/date/region/service/aws4_request,
//!   SignedHeaders=..., Signature=...`
//! - presigned URLs (X-Amz-Algorithm query param) NOT supported -> 403 (SDKs don't
//!   use them for s3vectors operations; design.md §4.2).
//! - payload hash: `x-amz-content-sha256` header value if present (covers the
//!   UNSIGNED-PAYLOAD form naturally), else hex SHA-256 of received body.
//! - service MUST be `s3vectors`; region MUST equal server config; X-Amz-Date skew <= 15 min.
//!
//! TODO(AWS semantics, uncertain — recorded in NOTES.md):
//! - header value normalization beyond trim + whitespace collapse (multi-value join
//!   separator, quoting) not verified against real AWS.
//! - real AWS returns InvalidClientTokenId/SignatureDoesNotMatch/InvalidSignatureException
//!   sub-cases; spike collapses every failure into AccessDeniedException (403) per task spec.
//! - SigV4 also allows `Date` header fallback for X-Amz-Date; spike requires X-Amz-Date.
//!
//! M1 harvest note (2026-11-04): carried over VERBATIM from the verified M0
//! spike (spike/src/sigv4.rs, 22/22 signatures verified against two official
//! SDKs). Since then only formatting-level changes: rustfmt (edition 2024) and
//! three clippy-only nits (two redundant `as i64` casts removed, one nested
//! if/if-let collapsed to an edition-2024 let-chain) plus
//! `clippy::too_many_arguments` allows matching the spike's own practice.
//! Zero logic changes; all 7 unit tests carried and kept green.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone)]
pub struct Credentials {
    pub access_key: String,
    pub secret_key: String,
}

const MAX_SKEW_SECS: i64 = 15 * 60;

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

fn sha256(data: &[u8]) -> Vec<u8> {
    Sha256::digest(data).to_vec()
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// Hex sha256 of a request payload — the `x-amz-content-sha256` value (the
/// S3 admin egress in the server signs it explicitly, s3admin.rs; the wire
/// gate reads the header when present). Public since v3.1: the outbound S3
/// admin calls share this canonicalization with the inbound gate.
pub fn payload_hash_hex(body: &[u8]) -> String {
    hex(&sha256(body))
}

/// Percent-encode per general (non-S3) SigV4 rules: keep unreserved `A-Za-z0-9-_.~`
/// and (optionally) `/`; everything else as `%XX` uppercase hex.
fn uri_encode(s: &str, encode_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        let keep = b.is_ascii_alphanumeric()
            || matches!(b, b'-' | b'_' | b'.' | b'~')
            || (b == b'/' && !encode_slash);
        if keep {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        // (M1 clippy pass: collapsed to an edition-2024 let-chain — behavior
        // identical to the spike's nested if/if-let.)
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(hv), Some(lv)) = (hexval(b[i + 1]), hexval(b[i + 2]))
        {
            out.push(hv * 16 + lv);
            i += 3;
            continue;
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hexval(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Canonical query string: strict-encoded, sorted by (key, value). Empty input -> "".
/// (Our four operations never carry a query string; kept correct anyway.)
fn canonical_query(raw: Option<&str>) -> String {
    let Some(q) = raw else { return String::new() };
    if q.is_empty() {
        return String::new();
    }
    let mut pairs: Vec<(String, String)> = q
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (percent_decode(k), percent_decode(v)),
            None => (percent_decode(p), String::new()),
        })
        .collect();
    pairs.sort();
    pairs
        .into_iter()
        .map(|(k, v)| {
            let mut s = uri_encode(&k, true);
            s.push('=');
            s.push_str(&uri_encode(&v, true));
            s
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// Canonical URI: encode each path segment with general (non-S3) rules.
fn canonical_uri(path: &str) -> String {
    if path.is_empty() {
        return "/".to_string();
    }
    uri_encode(path, false)
}

/// Normalize a header value for canonical headers: trim + collapse internal
/// whitespace runs to a single space (SigV4 general reference behavior).
fn normalize_header_value(v: &str) -> String {
    v.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// days_from_civil (Howard Hinnant) — UTC calendar math without external crates.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = if m > 2 { m as i64 - 3 } else { m as i64 + 9 }; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d as i64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

/// Parse `YYYYMMDDTHHMMSSZ` into unix epoch seconds (UTC).
fn parse_amz_date(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 16 || b[8] != b'T' || b[15] != b'Z' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> { s.get(r)?.parse::<i64>().ok() };
    let (y, mo, d) = (num(0..4)?, num(4..6)?, num(6..8)?);
    let (h, mi, se) = (num(9..11)?, num(11..13)?, num(13..15)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    Some(days_from_civil(y, mo as u32, d as u32) * 86400 + h * 3600 + mi * 60 + se)
}

/// Constant-time string equality (XOR fold over bytes; length differences fold in).
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut acc: u8 = (a.len() ^ b.len()) as u8;
    for i in 0..a.len().max(b.len()) {
        let x = *a.get(i).unwrap_or(&0);
        let y = *b.get(i).unwrap_or(&0);
        acc |= x ^ y;
    }
    acc == 0
}

/// Verify one request. `headers` = raw request headers as (name, value) pairs in
/// received order (duplicates allowed; names matched case-insensitively).
/// `now_unix` is server time. Err(message) => AccessDeniedException (403).
// (M1 clippy pass: allow — the parameter set mirrors the SigV4 spec inputs.)
#[allow(clippy::too_many_arguments)]
pub fn verify(
    method: &str,
    path: &str,
    query: Option<&str>,
    headers: &[(String, String)],
    body: &[u8],
    creds: &Credentials,
    expected_region: &str,
    now_unix: i64,
) -> Result<(), String> {
    // ---- gather headers (lowercased names) ----
    let get_header = |name: &str| -> Vec<String> {
        headers
            .iter()
            .filter(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
            .collect()
    };
    let one = |name: &str| -> Result<String, String> {
        get_header(name)
            .into_iter()
            .next()
            .ok_or_else(|| format!("missing required header {name}"))
    };

    let authorization = one("authorization")?;
    let x_amz_date = one("x-amz-date")?;

    // ---- parse Authorization ----
    let rest = authorization
        .strip_prefix("AWS4-HMAC-SHA256")
        .ok_or_else(|| "unsupported authorization scheme (expected AWS4-HMAC-SHA256)".to_string())?
        .trim();
    let mut credential = None;
    let mut signed_headers = None;
    let mut signature = None;
    for part in rest.split(',') {
        let part = part.trim();
        if let Some(v) = part.strip_prefix("Credential=") {
            credential = Some(v.to_string());
        } else if let Some(v) = part.strip_prefix("SignedHeaders=") {
            signed_headers = Some(v.to_string());
        } else if let Some(v) = part.strip_prefix("Signature=") {
            signature = Some(v.to_string());
        }
    }
    let credential = credential.ok_or("Authorization missing Credential")?;
    let signed_headers = signed_headers.ok_or("Authorization missing SignedHeaders")?;
    let signature = signature.ok_or("Authorization missing Signature")?;

    // ---- credential scope checks ----
    let cparts: Vec<&str> = credential.split('/').collect();
    if cparts.len() != 5 {
        return Err(format!("malformed Credential scope: {credential}"));
    }
    let (ak, cdate, cregion, cservice, cterm) =
        (cparts[0], cparts[1], cparts[2], cparts[3], cparts[4]);
    if ak != creds.access_key {
        return Err(format!("unknown access key id: {ak}"));
    }
    if cservice != "s3vectors" {
        return Err(format!(
            "credential scope service must be s3vectors, got {cservice}"
        ));
    }
    if cregion != expected_region {
        return Err(format!(
            "credential scope region {cregion} does not match server region {expected_region}"
        ));
    }
    if cterm != "aws4_request" {
        return Err(format!(
            "credential scope terminator must be aws4_request, got {cterm}"
        ));
    }

    // ---- time checks ----
    let ts = parse_amz_date(&x_amz_date)
        .ok_or_else(|| format!("unparseable X-Amz-Date: {x_amz_date}"))?;
    if (now_unix - ts).abs() > MAX_SKEW_SECS {
        return Err(format!(
            "request time {x_amz_date} outside allowed clock skew of {MAX_SKEW_SECS}s"
        ));
    }
    // Scope date must match the signing date (SigV4 general reference).
    if !x_amz_date.starts_with(cdate) {
        return Err("credential scope date does not match X-Amz-Date".to_string());
    }

    // ---- signed headers sanity ----
    let mut sh: Vec<String> = signed_headers
        .split(';')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    sh.sort();
    sh.dedup();
    if !sh.iter().any(|h| h == "host") {
        return Err("SignedHeaders must include host".to_string());
    }
    if !sh.iter().any(|h| h == "x-amz-date") {
        return Err("SignedHeaders must include x-amz-date".to_string());
    }

    // ---- canonical headers (only the signed ones) ----
    let mut canonical_headers = String::new();
    for name in &sh {
        let vals = get_header(name);
        if vals.is_empty() {
            return Err(format!("signed header {name} not present in request"));
        }
        let joined = vals
            .iter()
            .map(|v| normalize_header_value(v))
            .collect::<Vec<_>>()
            .join(",");
        canonical_headers.push_str(name);
        canonical_headers.push(':');
        canonical_headers.push_str(&joined);
        canonical_headers.push('\n');
    }
    let signed_headers_joined = sh.join(";");

    // ---- payload hash ----
    let payload_hash = match get_header("x-amz-content-sha256").into_iter().next() {
        Some(v) => v.trim().to_string(),
        None => hex(&sha256(body)),
    };

    // ---- canonical request / string to sign / key chain ----
    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        method,
        canonical_uri(path),
        canonical_query(query),
        canonical_headers,
        signed_headers_joined,
        payload_hash
    );
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{}/{}/{}/aws4_request\n{}",
        x_amz_date,
        cdate,
        cregion,
        cservice,
        hex(&sha256(canonical_request.as_bytes()))
    );
    let k_date = hmac_sha256(
        format!("AWS4{}", creds.secret_key).as_bytes(),
        cdate.as_bytes(),
    );
    let k_region = hmac_sha256(&k_date, cregion.as_bytes());
    let k_service = hmac_sha256(&k_region, cservice.as_bytes());
    let k_signing = hmac_sha256(&k_service, b"aws4_request");
    let expected = hex(&hmac_sha256(&k_signing, string_to_sign.as_bytes()));

    if !constant_time_eq(&expected, &signature) {
        return Err(
            "The request signature we calculated does not match the signature you provided. \
             Check your AWS Secret Access Key and signing method."
                .to_string(),
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Client-side signer — written against the AWS SigV4 general reference,
// mirroring what an SDK client does. Born as the pre-Task-C test-only self
// check; since M2 T6 it is ALSO the `vf` client's production signer (appendix
// E.1: "vf CLI 复用同一自研签名栈") — same canonicalization as verify(),
// so client and gate stay byte-exact by construction.
// ---------------------------------------------------------------------------
pub mod signer {
    use super::*;

    pub struct Signed {
        pub authorization: String,
        pub x_amz_date: String,
    }

    /// `extra_headers`: additional headers to include in SignedHeaders (caller
    /// supplies host / x-amz-content-sha256 etc.). x-amz-date is always signed.
    // (M1 clippy pass: allow — mirrors the verify() parameter set.)
    #[allow(clippy::too_many_arguments)]
    pub fn sign(
        method: &str,
        path: &str,
        query: Option<&str>,
        host: &str,
        body: &[u8],
        creds: &Credentials,
        region: &str,
        service: &str,
        amz_date: &str, // "YYYYMMDDTHHMMSSZ"
        extra_headers: &[(&str, &str)],
    ) -> Signed {
        let mut names: Vec<String> = vec!["host".into(), "x-amz-date".into()];
        for (n, _) in extra_headers {
            let n = n.to_ascii_lowercase();
            if !names.contains(&n) {
                names.push(n);
            }
        }
        names.sort();
        names.dedup();

        let vals_for = |name: &str| -> String {
            match name {
                "host" => host.to_string(),
                "x-amz-date" => amz_date.to_string(),
                n => extra_headers
                    .iter()
                    .find(|(en, _)| en.eq_ignore_ascii_case(n))
                    .map(|(_, v)| v.to_string())
                    .unwrap_or_default(),
            }
        };
        let mut canonical_headers = String::new();
        for n in &names {
            canonical_headers.push_str(&format!(
                "{}:{}\n",
                n,
                normalize_header_value(&vals_for(n))
            ));
        }
        let payload_hash = match extra_headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("x-amz-content-sha256"))
        {
            Some((_, v)) => v.to_string(),
            None => hex(&sha256(body)),
        };
        let canonical_request = format!(
            "{}\n{}\n{}\n{}\n{}\n{}",
            method,
            canonical_uri(path),
            canonical_query(query),
            canonical_headers,
            names.join(";"),
            payload_hash
        );
        let cdate = &amz_date[0..8];
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{}\n{}/{}/{}/aws4_request\n{}",
            amz_date,
            cdate,
            region,
            service,
            hex(&sha256(canonical_request.as_bytes()))
        );
        let k_date = hmac_sha256(
            format!("AWS4{}", creds.secret_key).as_bytes(),
            cdate.as_bytes(),
        );
        let k_region = hmac_sha256(&k_date, region.as_bytes());
        let k_service = hmac_sha256(&k_region, service.as_bytes());
        let k_signing = hmac_sha256(&k_service, b"aws4_request");
        let signature = hex(&hmac_sha256(&k_signing, string_to_sign.as_bytes()));
        Signed {
            authorization: format!(
                "AWS4-HMAC-SHA256 Credential={}/{}/{}/{}/aws4_request, SignedHeaders={}, Signature={}",
                creds.access_key,
                cdate,
                region,
                service,
                names.join(";"),
                signature
            ),
            x_amz_date: amz_date.to_string(),
        }
    }

    /// Current time formatted for X-Amz-Date.
    pub fn now_amz_date() -> String {
        amz_date_from_epoch(std::time::SystemTime::now())
    }

    /// Epoch (secs) -> "YYYYMMDDTHHMMSSZ" using civil-from-days.
    pub fn amz_date_from_epoch(t: std::time::SystemTime) -> String {
        let secs = t
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let days = secs.div_euclid(86400);
        let sod = secs.rem_euclid(86400);
        let (y, m, d) = civil_from_days(days);
        format!(
            "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
            y,
            m,
            d,
            sod / 3600,
            (sod % 3600) / 60,
            sod % 60
        )
    }

    fn civil_from_days(z: i64) -> (i64, u32, u32) {
        let z = z + 719468;
        let era = if z >= 0 { z } else { z - 146096 } / 146097;
        let doe = z - era * 146097;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
        (if m <= 2 { y + 1 } else { y }, m, d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn creds() -> Credentials {
        Credentials {
            access_key: "AKIDEXAMPLE".into(),
            secret_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
        }
    }

    fn base_headers(auth: &str, date: &str) -> Vec<(String, String)> {
        vec![
            ("Host".into(), "127.0.0.1:8787".into()),
            ("Content-Type".into(), "application/json".into()),
            ("X-Amz-Date".into(), date.into()),
            ("Authorization".into(), auth.into()),
        ]
    }

    fn now_for(date: &str) -> i64 {
        parse_amz_date(date).unwrap()
    }

    const DATE: &str = "20261103T120000Z";

    #[test]
    fn verify_accepts_valid_signature() {
        let c = creds();
        let body = br#"{"vectorBucketName":"abc"}"#;
        let s = signer::sign(
            "POST",
            "/CreateVectorBucket",
            None,
            "127.0.0.1:8787",
            body,
            &c,
            "us-east-1",
            "s3vectors",
            DATE,
            &[("content-type", "application/json")],
        );
        let headers = base_headers(&s.authorization, &s.x_amz_date);
        assert!(
            verify(
                "POST",
                "/CreateVectorBucket",
                None,
                &headers,
                body,
                &c,
                "us-east-1",
                now_for(DATE)
            )
            .is_ok()
        );
    }

    #[test]
    fn verify_accepts_unsigned_payload_form() {
        let c = creds();
        let body = br#"{}"#;
        let s = signer::sign(
            "POST",
            "/PutVectors",
            None,
            "127.0.0.1:8787",
            body,
            &c,
            "us-east-1",
            "s3vectors",
            DATE,
            &[("x-amz-content-sha256", "UNSIGNED-PAYLOAD")],
        );
        // the signed x-amz-content-sha256 header must also be present on the request
        let mut headers = base_headers(&s.authorization, &s.x_amz_date);
        headers.push(("x-amz-content-sha256".into(), "UNSIGNED-PAYLOAD".into()));
        assert!(
            verify(
                "POST",
                "/PutVectors",
                None,
                &headers,
                body,
                &c,
                "us-east-1",
                now_for(DATE)
            )
            .is_ok()
        );
    }

    #[test]
    fn verify_rejects_tampered_body() {
        let c = creds();
        let signed_body = br#"{"vectorBucketName":"abc"}"#;
        let sent_body = br#"{"vectorBucketName":"xyz"}"#; // one-char change
        let s = signer::sign(
            "POST",
            "/CreateVectorBucket",
            None,
            "127.0.0.1:8787",
            signed_body,
            &c,
            "us-east-1",
            "s3vectors",
            DATE,
            &[],
        );
        let headers = base_headers(&s.authorization, &s.x_amz_date);
        let err = verify(
            "POST",
            "/CreateVectorBucket",
            None,
            &headers,
            sent_body,
            &c,
            "us-east-1",
            now_for(DATE),
        )
        .unwrap_err();
        assert!(err.contains("signature"), "{err}");
    }

    #[test]
    fn verify_rejects_wrong_access_key() {
        let good = creds();
        let evil = Credentials {
            access_key: "AKIDATTACKER".into(),
            secret_key: "somethingelse".into(),
        };
        let body = br#"{}"#;
        let s = signer::sign(
            "POST",
            "/PutVectors",
            None,
            "127.0.0.1:8787",
            body,
            &evil,
            "us-east-1",
            "s3vectors",
            DATE,
            &[],
        );
        let headers = base_headers(&s.authorization, &s.x_amz_date);
        let err = verify(
            "POST",
            "/PutVectors",
            None,
            &headers,
            body,
            &good,
            "us-east-1",
            now_for(DATE),
        )
        .unwrap_err();
        assert!(err.contains("unknown access key"));
    }

    #[test]
    fn verify_rejects_stale_date() {
        let c = creds();
        let body = br#"{}"#;
        let s = signer::sign(
            "POST",
            "/PutVectors",
            None,
            "127.0.0.1:8787",
            body,
            &c,
            "us-east-1",
            "s3vectors",
            DATE,
            &[],
        );
        let headers = base_headers(&s.authorization, &s.x_amz_date);
        // server clock 2h ahead of the signed request
        let err = verify(
            "POST",
            "/PutVectors",
            None,
            &headers,
            body,
            &c,
            "us-east-1",
            now_for(DATE) + 2 * 3600,
        )
        .unwrap_err();
        assert!(err.contains("clock skew"));
    }

    #[test]
    fn verify_rejects_wrong_region_and_service() {
        let c = creds();
        let body = br#"{}"#;
        // signed for another service
        let s = signer::sign(
            "POST",
            "/PutVectors",
            None,
            "h",
            body,
            &c,
            "us-east-1",
            "s3",
            DATE,
            &[],
        );
        let headers = base_headers(&s.authorization, &s.x_amz_date);
        assert!(
            verify(
                "POST",
                "/PutVectors",
                None,
                &headers,
                body,
                &c,
                "us-east-1",
                now_for(DATE)
            )
            .unwrap_err()
            .contains("service must be s3vectors")
        );
        // signed for another region
        let s = signer::sign(
            "POST",
            "/PutVectors",
            None,
            "h",
            body,
            &c,
            "eu-west-1",
            "s3vectors",
            DATE,
            &[],
        );
        let headers = base_headers(&s.authorization, &s.x_amz_date);
        assert!(
            verify(
                "POST",
                "/PutVectors",
                None,
                &headers,
                body,
                &c,
                "us-east-1",
                now_for(DATE)
            )
            .unwrap_err()
            .contains("does not match server region")
        );
    }

    #[test]
    fn amz_date_roundtrip() {
        // 2026-11-03T12:00:00Z == 1793707200 (checked against `date -u -d ...`)
        assert_eq!(parse_amz_date("20261103T120000Z").unwrap(), 1793707200);
        assert_eq!(
            signer::amz_date_from_epoch(std::time::UNIX_EPOCH),
            "19700101T000000Z"
        );
    }

    #[test]
    fn constant_time_eq_basics() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "ab"));
    }
}
