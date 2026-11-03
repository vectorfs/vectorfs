//! SigV4 verification (server-side AWS Signature Version 4).
//!
//! 职责（M0 后续任务实现）：
//! - 解析 `Authorization` 头（Credential / SignedHeaders / Signature）与
//!   `X-Amz-Date` / `X-Amz-Content-Sha256`；
//! - 重建 canonical request 与 string-to-sign，HMAC-SHA256 派生链比对签名；
//! - 时钟偏移检查（InvalidSignatureException / SignatureDoesNotMatch / AccessDenied 映射）。
//! 设计约束（design.md §5.7）：自研，不引入 aws-sigv4 / 任何 aws-* SDK crate。

/// TODO(M0): verify a request's SigV4 authorization against static credentials.
/// Returns Ok(canon) or Err(structured error code + message) in product; spike stub.
pub fn verify(_authorization_header: &str, _x_amz_date: &str, _payload_hash: &str) -> Result<(), String> {
    let _ = (_authorization_header, _x_amz_date, _payload_hash);
    todo!("SigV4 verification not implemented yet (M0 follow-up task)")
}
