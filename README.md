# vectorfs

Single-binary, self-hosted vector storage speaking the **AWS S3 Vectors API** —
a thin protocol layer over any S3-compatible object store (MinIO, SeaweedFS, Ceph,
R2, AWS S3), a local directory, or memory for dev/CI.

Point your AWS SDK's endpoint at it and go. Strong read-after-write consistency.
No bundled embeddings — it stores the vectors your client sends.

> Status: pre-M0 (design phase). See [design.md](design.md). Not usable yet.

## Non-goals

- No own object-storage backend or storage engine — persistence is always an
  existing object store or a local directory.
- No bundled embedding models, no chat UI, no write-time entity extraction,
  no plugin marketplace, no AWS-console UI clone, no distributed multi-writer
  coordination.
