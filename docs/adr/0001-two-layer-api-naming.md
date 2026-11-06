---
status: accepted
---

# 两层 API 命名：s3vectors API 与 vectorfs API

产品有上下两张 API 面，命名须成对且互不混淆：底层 AWS 兼容面沿用 AWS 协议名 **s3vectors API**（21 操作）；高层原生面以产品名命名 **vectorfs API**（8 操作，wire 位于 `/x-vf/<Op>`）。取厂商自命名惯例（MinIO 原生 API 之于 S3 兼容面）：标准协议名 + 厂商原生名，两张脸零词根纠缠。2026-11-06 用户裁决。

## Considered Options

- **vf API**（wire 前缀扶正）——不自释，第一眼看不懂，出局
- **vectorized / vectorization API**——分词语法反向（"被向量化的 API"），且撞 Cloudflare Vectorize，出局
- **file vectorization API / 文件向量化 API**——自释但长、个性不足，出局
- **file2vec**——短且有个性，但 slogan 式造词当 API 面名不稳，出局
- **filevec**——Rust 生态已有 file-vec crate（内存映射向量存储），撞名，出局
- **file API / sources API**——file 撞 file 后端，sources 盖不住 Search，出局

## Consequences

- 「文件即向量化」保持为产品口号与能力描述，不作 API 名
- wire 前缀 `/x-vf/`、CLI `vf`、元数据 `x-vf-*`、桶后缀 `-vf` 均不变——前缀是命名空间约定，不约束文档名（`x-amz-` 先例）
- 子系统名「文件层 / file layer」不变
