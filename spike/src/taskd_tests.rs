//! Task D tests: (1) conditional-write matrix rows we can test in-process,
//! (2) Q10/Q4 experiment — merge_insert x dynamic schema evolution.
//!
//! The S3 rows are `#[ignore]`d (need a live S3-compatible server) and run
//! manually: `VF_S3CP_URI='s3+http://127.0.0.1:9000/vf-spike' cargo test -- --ignored`

use std::sync::Arc;

use lancedb::arrow::arrow_array::types::{Float32Type, Int64Type};
use lancedb::arrow::arrow_array::{
    Array, FixedSizeListArray, Float32Array, Int64Array, RecordBatch, RecordBatchIterator,
    StringArray, StructArray,
};
use lancedb::arrow::arrow_schema::{ArrowError, DataType, Field, Fields, Schema};
use lancedb::query::{ExecutableQuery, QueryBase};
use lancedb::{connect, DistanceType};

use object_store::{ObjectStore as _, ObjectStoreExt as _}; // put_opts / get+delete

// ---------------------------------------------------------------------------
// conditional writes (design §5.2, 必答 2)
// ---------------------------------------------------------------------------

/// LocalFileSystem: PutMode::Create must succeed once and reject the second
/// write with AlreadyExists (object_store 0.14 local impl uses O_EXCL).
#[tokio::test]
async fn conditional_put_local() {
    use object_store::local::LocalFileSystem;
    use object_store::path::Path;
    use object_store::{PutMode, PutOptions};

    let dir = tempfile::tempdir().unwrap();
    let store = LocalFileSystem::new_with_prefix(dir.path()).unwrap();
    let path = Path::from("cond/cat.json");

    store
        .put_opts(&path, b"first".to_vec().into(), PutOptions { mode: PutMode::Create, ..Default::default() })
        .await
        .expect("first conditional put must succeed");
    let bytes = store.get(&path).await.unwrap().bytes().await.unwrap();
    assert_eq!(&bytes[..], b"first");

    let second = store
        .put_opts(&path, b"second".to_vec().into(), PutOptions { mode: PutMode::Create, ..Default::default() })
        .await;
    match second {
        Err(object_store::Error::AlreadyExists { .. }) => {}
        other => panic!("expected AlreadyExists, got {other:?}"),
    }
}

/// S3-compatible server row of the conditional-write matrix.
/// Env: VF_S3CP_URI='s3+http://127.0.0.1:9000/vf-spike' (+ optional
/// VF_S3_ACCESS_KEY/VF_S3_SECRET_KEY). Exercises the same code path Lance's
/// ConditionalPutCommitHandler uses for manifest commits (PutMode::Create).
#[tokio::test]
#[ignore = "needs live S3 server; set VF_S3CP_URI"]
async fn conditional_put_s3() {
    let uri = std::env::var("VF_S3CP_URI").expect("VF_S3CP_URI=s3+http://host:port/bucket[/prefix]");
    let cfg = crate::s3::S3BackendConfig::parse(&uri).expect("parse s3+ uri");
    let cfg = crate::s3::S3BackendConfig {
        access_key: std::env::var("VF_S3_ACCESS_KEY").ok().or(cfg.access_key),
        secret_key: std::env::var("VF_S3_SECRET_KEY").ok().or(cfg.secret_key),
        ..cfg
    };
    let store = cfg.build_catalog_store().expect("build store");
    let path = cfg.catalog_key("cond-test/taskd-probe.json");

    let _ = store.delete(&path).await; // idempotent rerun
    store
        .put_opts(&path, b"first".to_vec().into(), object_store::PutOptions { mode: object_store::PutMode::Create, ..Default::default() })
        .await
        .expect("first conditional put must succeed (If-None-Match:*)");

    let second = store
        .put_opts(&path, b"second".to_vec().into(), object_store::PutOptions { mode: object_store::PutMode::Create, ..Default::default() })
        .await;
    match second {
        Err(object_store::Error::AlreadyExists { .. }) => {}
        Err(object_store::Error::Precondition { .. }) => {}
        other => panic!("expected AlreadyExists/Precondition, got {other:?}"),
    }
    let _ = store.delete(&path).await;
}

// ---------------------------------------------------------------------------
// Q10 / 必答 4: merge_insert x dynamic schema evolution
// ---------------------------------------------------------------------------

const DIM: i32 = 4;

fn metadata_struct_schema(with_tag: bool) -> Arc<Schema> {
    let mut fields = vec![Field::new("i", DataType::Int64, true)];
    if with_tag {
        fields.push(Field::new("tag", DataType::Utf8, true));
    }
    Arc::new(Schema::new(vec![
        Field::new("key", DataType::Utf8, false),
        Field::new(
            "vector",
            DataType::FixedSizeList(Arc::new(Field::new("item", DataType::Float32, true)), DIM),
            false,
        ),
        Field::new("metadata", DataType::Struct(Fields::from(fields)), true),
    ]))
}

fn batch_with_meta(schema: Arc<Schema>, with_tag: bool, keys: &[&str]) -> RecordBatch {
    let n = keys.len();
    let key_arr: StringArray = keys.iter().map(|k| Some(*k)).collect();
    let vecs = FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
        (0..n).map(|i| {
            Some(
                (0..DIM as usize)
                    .map(|d| Some((i as f32 + d as f32) / 10.0))
                    .collect::<Vec<_>>(),
            )
        }),
        DIM,
    );
    let i_arr: Int64Array = (0..n as i64).map(|i| Some(i)).collect();
    let tag_arr: StringArray = (0..n).map(|i| Some(format!("t{i}"))).collect();
    let meta = if with_tag {
        StructArray::new(
            Fields::from(vec![
                Field::new("i", DataType::Int64, true),
                Field::new("tag", DataType::Utf8, true),
            ]),
            vec![Arc::new(i_arr) as Arc<dyn Array>, Arc::new(tag_arr)],
            None,
        )
    } else {
        StructArray::new(
            Fields::from(vec![Field::new("i", DataType::Int64, true)]),
            vec![Arc::new(i_arr) as Arc<dyn Array>],
            None,
        )
    };
    RecordBatch::try_new(
        schema,
        vec![
            Arc::new(key_arr) as Arc<dyn Array>,
            Arc::new(vecs) as Arc<dyn Array>,
            Arc::new(meta) as Arc<dyn Array>,
        ],
    )
    .unwrap()
}

async fn merge_put(table: &lancedb::Table, with_tag: bool, keys: &[&str]) -> lancedb::Result<()> {
    let schema = metadata_struct_schema(with_tag);
    let batch = batch_with_meta(schema.clone(), with_tag, keys);
    let reader = RecordBatchIterator::new(vec![Ok::<_, ArrowError>(batch)], schema);
    let mut m = table.merge_insert(&["key"]);
    m.when_matched_update_all(None).when_not_matched_insert_all();
    m.execute(Box::new(reader)).await.map(|_| ())
}

/// The actual Q10 experiment (file backend). Steps:
/// 1. table with metadata struct {i}
/// 2. merge_insert batch1 ({i})            -> must work
/// 3. merge_insert batch2 ({i, tag})       -> expected: schema mismatch error (recorded)
/// 4. evolve schema (add nested field via NewColumnTransform::AllNulls)
/// 5. merge_insert batch2 again            -> the question: works?
/// 6. read back: old rows tag=NULL, new rows tagged, merge_insert not broken.
#[tokio::test]
async fn merge_insert_with_schema_evolution() {
    let dir = tempfile::tempdir().unwrap();
    let uri = dir.path().join("data");
    let uri = uri.to_str().unwrap().to_string();
    let conn = connect(&uri).execute().await.unwrap();
    let table = conn
        .create_empty_table("vectors", metadata_struct_schema(false))
        .execute()
        .await
        .unwrap();

    // batch 1: {i} only
    merge_put(&table, false, &["a", "b"])
        .await
        .expect("batch1 (declared schema) must merge cleanly");

    // batch 2: new metadata key "tag" appears — struct has extra field
    let err = merge_put(&table, true, &["c", "d"])
        .await
        .expect_err("mismatched struct schema must be rejected by merge_insert");
    eprintln!("Q10 step3 error (expected): {err}");

    // evolve: add nested field metadata.tag as all-nulls. "metadata.tag" as a
    // flat name is rejected ("cannot contain `.`"), so pass the full evolved
    // struct as the "metadata" field — lance merges the new child in.
    use lance::dataset::NewColumnTransform;
    let evolved_struct = Arc::new(Schema::new(vec![Field::new(
        "metadata",
        DataType::Struct(Fields::from(vec![Field::new("tag", DataType::Utf8, true)])),
        true,
    )]));
    let evolved = table
        .add_columns()
        .transform(NewColumnTransform::AllNulls(evolved_struct))
        .execute()
        .await;
    match evolved {
        Ok(_) => eprintln!("Q10 step4: AllNulls(full struct i+tag) evolution OK"),
        Err(e) => panic!("schema evolution via AllNulls full-struct failed: {e}"),
    }

    // batch 2 retry after evolution
    let table = conn.open_table("vectors").execute().await.unwrap();
    merge_put(&table, true, &["c", "d"])
        .await
        .expect("Q10: merge_insert must work after schema evolution");

    // read back
    let mut stream = table
        .query()
        .nearest_to(vec![0.05, 0.15, 0.25, 0.35])
        .unwrap()
        .distance_type(DistanceType::Cosine)
        .execute()
        .await
        .unwrap();
    use futures_util::StreamExt;
    let mut rows: Vec<(String, Option<i64>, Option<String>)> = Vec::new();
    while let Some(b) = stream.next().await {
        let b = b.unwrap();
        let keys = b.column_by_name("key").unwrap().as_any().downcast_ref::<StringArray>().unwrap();
        let meta = b
            .column_by_name("metadata")
            .unwrap()
            .as_any()
            .downcast_ref::<StructArray>()
            .unwrap();
        let i_col = meta.column_by_name("i").map(|c| c.as_any().downcast_ref::<Int64Array>().unwrap().clone());
        let tag_col = meta.column_by_name("tag").map(|c| c.as_any().downcast_ref::<StringArray>().unwrap().clone());
        for r in 0..b.num_rows() {
            rows.push((
                keys.value(r).to_string(),
                i_col.as_ref().map(|c| c.value(r)),
                tag_col.as_ref().and_then(|c| if c.is_null(r) { None } else { Some(c.value(r).to_string()) }),
            ));
        }
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    eprintln!("Q10 readback: {rows:?}");
    assert_eq!(rows.len(), 4, "all four keys must survive");
    let by_key: std::collections::HashMap<String, (Option<i64>, Option<String>)> = rows
        .into_iter()
        .map(|(k, i, t)| (k, (i, t)))
        .collect();
    // pre-evolution rows: tag NULL; post-evolution rows: tag present
    assert_eq!(by_key["a"].1, None, "old row a must have tag=NULL");
    assert_eq!(by_key["c"].1.as_deref(), Some("t0"), "new row c must carry tag");
    assert_eq!(by_key["a"].0, Some(0));
    assert_eq!(by_key["c"].0, Some(0));
}
