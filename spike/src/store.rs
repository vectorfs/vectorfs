//! Storage backend: lancedb engine over object_store (file / s3-minio dual backend).
//!
//! 职责（M0 后续任务实现）：
//! - 唯一出现 lancedb API 的模块（design.md §5.7：lancedb 只经一个内部 crate/模块暴露）；
//! - file 后端（本地目录）与 s3 后端（MinIO 容器，storage_options 连接参数）；
//! - flat-first 布局与批量提交（design.md §5.3 / §5.4）。

/// TODO(M0): open a lancedb connection (file or s3 backend by URI + storage_options).
pub async fn open(_uri: &str, _storage_options: Option<std::collections::HashMap<String, String>>) -> Result<(), String> {
    let _ = (_uri, _storage_options);
    todo!("store open not implemented yet (M0 follow-up task)")
}
