#!/usr/bin/env python3
"""boto3 conformance check against an s3vectors-compatible server (M0 Task C
core 9 steps + M1 T3 management 10 steps = 19 steps).

Talks to an ALREADY-RUNNING server (default http://127.0.0.1:8787) — this script
owns no server lifecycle. Each step prints PASS/FAIL; exit code 1 on any FAIL.
Works against real AWS too by pointing --endpoint elsewhere (the management
section cleans up after itself via the delete operations).

Wire-observation notes are printed alongside (parsed field sets, error-code
mappings) — they feed NOTES.md 线格式观察 / 附录B对拍.

Usage:
    python3 boto3_check.py --access-key test --secret-key testsecret \
        [--endpoint http://127.0.0.1:8787] [--region us-east-1] [--prefix XXX]
"""

import argparse
import datetime
import secrets
import sys

import boto3
from botocore.exceptions import ClientError

FAILED = []
OBS = []  # wire observations, printed in the summary
STEPS = []  # (num, name, fn) — registered in definition order, run at the end


def step(num, name, fn):
    STEPS.append((num, name, fn))


def run_steps():
    total = len(STEPS)
    for num, name, fn in STEPS:
        print(f"[{num}/{total}] {name} ... ", end="", flush=True)
        try:
            details = fn() or ""
            print(f"PASS {details}".rstrip())
        except Exception as e:  # noqa: BLE001 — report any failure and continue
            FAILED.append((num, name, repr(e)))
            print(f"FAIL {e!r}")


def expect_client_error(client, exc_cls, fn):
    """Run fn, require exactly exc_cls; return the error for field inspection."""
    try:
        fn()
    except exc_cls as e:
        return e
    except ClientError as e:
        raise AssertionError(
            f"wrong error type: got {type(e).__name__} "
            f"code={e.response.get('Error', {}).get('Code')!r}: {e}"
        )
    raise AssertionError(f"expected {exc_cls.__name__} but call succeeded")


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--endpoint", default="http://127.0.0.1:8787")
    ap.add_argument("--access-key", required=True)
    ap.add_argument("--secret-key", required=True)
    ap.add_argument("--region", default="us-east-1")
    ap.add_argument("--prefix", default=None, help="resource name prefix (default: random)")
    args = ap.parse_args()

    prefix = args.prefix or ("vf" + secrets.token_hex(4))
    bucket = f"{prefix}-b"
    index = f"{prefix}-i"
    print(f"# boto3 {boto3.__version__} endpoint={args.endpoint} region={args.region} "
          f"bucket={bucket} index={index}")

    client = boto3.client(
        "s3vectors",
        endpoint_url=args.endpoint,
        aws_access_key_id=args.access_key,
        aws_secret_access_key=args.secret_key,
        region_name=args.region,
    )

    state = {}

    def s1():
        resp = client.create_vector_bucket(vectorBucketName=bucket)
        fields = {k: v for k, v in resp.items() if k != "ResponseMetadata"}
        OBS.append(f"create_vector_bucket parsed fields: {sorted(fields)}")
        assert "vectorBucketArn" in resp, f"missing vectorBucketArn, got {sorted(fields)}"
        assert resp["vectorBucketArn"].endswith(f"bucket/{bucket}"), resp["vectorBucketArn"]
        state["bucket_arn"] = resp["vectorBucketArn"]
        # spike double-emits a `vectorBucket` object (AWS CLI docs shape); boto3
        # (botocore 1.43 model knows only vectorBucketArn) drops unknown members
        # on the floor — record whether we saw it at all.
        vb = resp.get("vectorBucket")
        if vb is not None:
            ct = vb.get("creationTime")
            assert isinstance(ct, datetime.datetime), f"creationTime not datetime: {ct!r}"
            OBS.append(f"vectorBucket.creationTime parsed as {type(ct).__name__}: {ct.isoformat()}")
        return f"arn={resp['vectorBucketArn']} fields={sorted(fields)}"

    step(1, "create_vector_bucket -> vectorBucketArn", s1)

    def s2():
        err = expect_client_error(
            client, client.exceptions.ConflictException,
            lambda: client.create_vector_bucket(vectorBucketName=bucket),
        )
        code = err.response["Error"]["Code"]
        status = err.response["ResponseMetadata"]["HTTPStatusCode"]
        OBS.append(f"conflict: botocore Error.Code={code!r} http={status}")
        assert code == "ConflictException", code
        assert status == 409, status
        return f"code={code} http={status}"

    step(2, "duplicate create -> ConflictException", s2)

    def s3():
        resp = client.create_index(
            vectorBucketName=bucket,
            indexName=index,
            dataType="float32",
            dimension=4,
            distanceMetric="cosine",
        )
        fields = {k: v for k, v in resp.items() if k != "ResponseMetadata"}
        OBS.append(f"create_index parsed fields: {sorted(fields)}")
        assert "indexArn" in resp, f"missing indexArn, got {sorted(fields)}"
        state["index_arn"] = resp["indexArn"]
        ix = resp.get("index")
        if ix is not None:
            ct = ix.get("creationTime")
            assert isinstance(ct, datetime.datetime), f"creationTime not datetime: {ct!r}"
            OBS.append(f"index.creationTime parsed as {type(ct).__name__}: {ct.isoformat()}")
        return f"arn={resp['indexArn']}"

    step(3, "create_index(float32/4/cosine) -> indexArn", s3)

    def s4():
        client.put_vectors(
            vectorBucketName=bucket,
            indexName=index,
            vectors=[
                {"key": "a", "data": {"float32": [1.0, 0.0, 0.0, 0.0]},
                 "metadata": {"tag": "a-old", "legacy": True}},
                {"key": "b", "data": {"float32": [0.0, 0.0, 0.0, 1.0]}},
                {"key": "c", "data": {"float32": [1.0, 1.0, 0.0, 0.0]},
                 "metadata": {"tag": "demo", "n": 7, "ok": True}},
            ],
        )
        return "3 vectors (a/b/c, c has string+number+bool metadata)"

    step(4, "put_vectors x3", s4)

    def s5():
        resp = client.query_vectors(
            vectorBucketName=bucket, indexName=index, topK=2,
            queryVector={"float32": [1.0, 0.0, 0.0, 0.0]},
            returnDistance=True, returnMetadata=True,
        )
        vecs = resp["vectors"]
        assert len(vecs) == 2, f"expected 2 got {len(vecs)}: {vecs}"
        keys = [v["key"] for v in vecs]
        assert keys == ["a", "c"], f"order wrong: {keys} (want a<c, a first)"
        assert abs(vecs[0]["distance"]) < 1e-6, vecs[0]["distance"]
        assert vecs[1]["metadata"] == {"tag": "demo", "n": 7, "ok": True}, vecs[1]["metadata"]
        assert resp["distanceMetric"] == "cosine", resp["distanceMetric"]
        # any time-ish member the service returns must come back as datetime
        for v in vecs:
            for k, val in v.items():
                if "time" in k.lower():
                    assert isinstance(val, datetime.datetime), f"{k}={val!r}"
        state["c_meta_ok"] = True
        return (f"keys={keys} d(a)={vecs[0]['distance']} "
                f"meta(c)={vecs[1]['metadata']} metric={resp['distanceMetric']}")

    step(5, "query topK=2 (order a<c, d~0 first, metadata roundtrip)", s5)

    def s6():
        # re-put key=a: new vector + new metadata with a disjoint key set
        # ("legacy" from the original must vanish if metadata is replaced
        # whole-document, survive if the server merges).
        client.put_vectors(
            vectorBucketName=bucket, indexName=index,
            vectors=[{"key": "a", "data": {"float32": [0.0, 1.0, 0.0, 0.0]},
                      "metadata": {"tag": "a-new", "note": "replaced"}}],
        )
        resp = client.query_vectors(
            vectorBucketName=bucket, indexName=index, topK=3,
            queryVector={"float32": [0.0, 1.0, 0.0, 0.0]},
            returnDistance=True, returnMetadata=True,
        )
        a = next(v for v in resp["vectors"] if v["key"] == "a")
        assert abs(a["distance"]) < 1e-6, a
        assert a["metadata"] == {"tag": "a-new", "note": "replaced"}, \
            f"metadata not whole-replaced (merge would keep legacy): {a['metadata']}"
        # old vector is gone: [1,0,0,0] topK=1 must now be c, not a
        resp2 = client.query_vectors(
            vectorBucketName=bucket, indexName=index, topK=1,
            queryVector={"float32": [1.0, 0.0, 0.0, 0.0]},
            returnDistance=True,
        )
        assert resp2["vectors"][0]["key"] == "c", resp2["vectors"]
        return "vector+metadata overwritten (whole-doc replace, no legacy residue)"

    step(6, "upsert key=a -> overwrite effective", s6)

    def s7():
        err = expect_client_error(
            client, client.exceptions.NotFoundException,
            lambda: client.query_vectors(
                vectorBucketName=bucket, indexName=f"{prefix}-nope", topK=1,
                queryVector={"float32": [1.0, 0.0, 0.0, 0.0]},
            ),
        )
        code = err.response["Error"]["Code"]
        status = err.response["ResponseMetadata"]["HTTPStatusCode"]
        OBS.append(f"notfound: botocore Error.Code={code!r} http={status}")
        assert code == "NotFoundException", code
        assert status == 404, status
        return f"code={code} http={status}"

    step(7, "query missing index -> NotFoundException", s7)

    def s8():
        err = expect_client_error(
            client, client.exceptions.ValidationException,
            lambda: client.put_vectors(
                vectorBucketName=bucket, indexName=index,
                vectors=[{"key": "bad", "data": {"float32": [1.0, 2.0, 3.0]}}],
            ),
        )
        code = err.response["Error"]["Code"]
        status = err.response["ResponseMetadata"]["HTTPStatusCode"]
        OBS.append(f"validation: botocore Error.Code={code!r} http={status}")
        assert code == "ValidationException", code
        assert status == 400, status
        return f"code={code} http={status}"

    step(8, "dimension mismatch (len 3 vs 4) -> ValidationException", s8)

    def s9():
        bad = boto3.client(
            "s3vectors",
            endpoint_url=args.endpoint,
            aws_access_key_id=args.access_key,
            aws_secret_access_key=args.secret_key + "-WRONG",
            region_name=args.region,
        )
        err = expect_client_error(
            bad, bad.exceptions.AccessDeniedException,
            lambda: bad.create_vector_bucket(vectorBucketName=f"{prefix}-denied"),
        )
        code = err.response["Error"]["Code"]
        status = err.response["ResponseMetadata"]["HTTPStatusCode"]
        OBS.append(f"denied: botocore Error.Code={code!r} http={status}")
        assert code == "AccessDeniedException", code
        assert status == 403, status
        return f"code={code} http={status}"

    step(9, "wrong secret key -> AccessDeniedException", s9)

    # ------------------------------------------------------------------
    # M1 T3 management section (steps 10-19): the 14 management-plane
    # operations over the SAME bucket/index the core section created —
    # get/list bucket+index, policy roundtrip, tags (put/list/untag, bucket
    # AND index ARN), indexMode knobs, and full cleanup at the end.
    # Note: invalid-enum / maxResults-range ValidationException paths cannot
    # be driven through boto3 (botocore validates enums & ranges client-side
    # and raises ParamValidationError before sending) — those live in the
    # tower oneshot unit tests (vf/src/ops.rs).
    # ------------------------------------------------------------------

    def m10():
        resp = client.get_vector_bucket(vectorBucketName=bucket)
        vb = resp["vectorBucket"]
        assert vb["vectorBucketName"] == bucket, vb
        assert vb["vectorBucketArn"] == state["bucket_arn"], vb
        assert isinstance(vb["creationTime"], datetime.datetime), vb["creationTime"]
        OBS.append(f"get_vector_bucket fields: {sorted(k for k in vb)}")
        # same lookup via the ARN locator
        resp2 = client.get_vector_bucket(vectorBucketArn=state["bucket_arn"])
        assert resp2["vectorBucket"]["vectorBucketName"] == bucket
        return f"fields={sorted(vb)} byName+byArn"

    step(10, "get_vector_bucket (name + ARN) -> VectorBucket", m10)

    def m11():
        # 3 buckets under the prefix -> paginate maxResults=2 to the end
        x1, x2 = f"{prefix}-x1", f"{prefix}-x2"
        client.create_vector_bucket(vectorBucketName=x1)
        client.create_vector_bucket(vectorBucketName=x2)
        state["extra_buckets"] = [x1, x2]
        seen, token, pages = [], None, 0
        while True:
            kw = {"maxResults": 2, "prefix": f"{prefix}-"}
            if token:
                kw["nextToken"] = token
            resp = client.list_vector_buckets(**kw)
            names = [b["vectorBucketName"] for b in resp.get("vectorBuckets", [])]
            assert names == sorted(names), f"page not name-sorted: {names}"
            seen.extend(names)
            token = resp.get("nextToken")
            pages += 1
            if not token:
                break
            assert pages < 10, "pagination did not terminate"
        # bucket names under the prefix: {prefix}-b, {prefix}-x1, {prefix}-x2
        # ({prefix}-i is an index, not a bucket)
        want = sorted([bucket, x1, x2])
        assert seen == want, f"pagination seen={seen} want={want} pages={pages}"
        # summary shape: no dimension/indexMode on bucket summaries
        b0 = client.list_vector_buckets(maxResults=500)["vectorBuckets"][0]
        OBS.append(f"list_vector_buckets summary fields: {sorted(b0)}")
        return f"pages={pages} names={seen}"

    step(11, "list_vector_buckets paginate (3 buckets, maxResults=2)", m11)

    def m12():
        resp = client.get_index(vectorBucketName=bucket, indexName=index)
        ix = resp["index"]
        assert ix["indexName"] == index and ix["vectorBucketName"] == bucket, ix
        assert ix["dataType"] == "float32" and ix["dimension"] == 4, ix
        assert ix["distanceMetric"] == "cosine", ix
        assert isinstance(ix["creationTime"], datetime.datetime), ix["creationTime"]
        OBS.append(f"get_index fields: {sorted(ix)}")
        resp2 = client.get_index(indexArn=state["index_arn"])
        assert resp2["index"]["indexArn"] == state["index_arn"]
        ls = client.list_indexes(vectorBucketArn=state["bucket_arn"])
        names = [i["indexName"] for i in ls["indexes"]]
        assert names == [index], names
        s0 = ls["indexes"][0]
        assert set(s0) == {"vectorBucketName", "indexName", "indexArn", "creationTime"}, s0
        return f"index fields={sorted(ix)}; summary fields={sorted(s0)}"

    step(12, "get_index + list_indexes (summary shape)", m12)

    def m13():
        doc = '{ "Version": "2012-10-17", "Statement": [ {"Effect": "Allow"} ] }'
        client.put_vector_bucket_policy(vectorBucketName=bucket, policy=doc)
        resp = client.get_vector_bucket_policy(vectorBucketArn=state["bucket_arn"])
        assert isinstance(resp["policy"], str), type(resp["policy"])
        assert resp["policy"] == doc, f"policy not byte-faithful: {resp['policy']!r}"
        return f"policy roundtrip {len(doc)}B byte-exact"

    step(13, "put/get vector bucket policy (byte-faithful echo)", m13)

    def m14():
        client.delete_vector_bucket_policy(vectorBucketName=bucket)
        err = expect_client_error(
            client, client.exceptions.NotFoundException,
            lambda: client.get_vector_bucket_policy(vectorBucketName=bucket),
        )
        assert err.response["ResponseMetadata"]["HTTPStatusCode"] == 404
        # idempotent: deleting an absent policy is NOT an error
        client.delete_vector_bucket_policy(vectorBucketName=bucket)
        return "delete -> get 404; re-delete 200 (idempotent)"

    step(14, "delete policy -> get 404; delete idempotent", m14)

    def m15():
        client.tag_resource(resourceArn=state["bucket_arn"],
                            tags={"env": "dev", "team": "infra"})
        client.tag_resource(resourceArn=state["index_arn"],
                            tags={"idx": "1", "sp ace+x/y:z": "punct"})
        bt = client.list_tags_for_resource(resourceArn=state["bucket_arn"])["tags"]
        it = client.list_tags_for_resource(resourceArn=state["index_arn"])["tags"]
        assert bt == {"env": "dev", "team": "infra"}, bt
        assert it == {"idx": "1", "sp ace+x/y:z": "punct"}, it
        # merge semantics: second put keeps unrelated keys
        client.tag_resource(resourceArn=state["bucket_arn"], tags={"env": "prod"})
        bt2 = client.list_tags_for_resource(resourceArn=state["bucket_arn"])["tags"]
        assert bt2 == {"env": "prod", "team": "infra"}, bt2
        return f"bucket tags={sorted(bt2)} index tags={sorted(it)}"

    step(15, "tag_resource (bucket + index ARN) -> list_tags_for_resource", m15)

    def m16():
        # untag: plain key + punctuation-heavy key (boto3 percent-encodes the
        # query values — space/%2B/%2F/%3A forms all round-trip)
        client.untag_resource(resourceArn=state["index_arn"],
                              tagKeys=["idx", "sp ace+x/y:z"])
        it = client.list_tags_for_resource(resourceArn=state["index_arn"])["tags"]
        assert it == {}, f"untag leftover: {it}"
        # untagging absent keys again is idempotent (200)
        client.untag_resource(resourceArn=state["index_arn"], tagKeys=["idx"])
        # bucket tags untouched by index untagging
        bt = client.list_tags_for_resource(resourceArn=state["bucket_arn"])["tags"]
        assert bt == {"env": "prod", "team": "infra"}, bt
        return "untag plain + punctuation keys (idempotent); bucket scope isolated"

    step(16, "untag_resource (repeated tagKeys query, encoded values)", m16)

    def m17():
        client.put_vector_bucket_default_index_mode(
            vectorBucketName=bucket, defaultIndexMode="CLASSIC")
        vb = client.get_vector_bucket(vectorBucketName=bucket)["vectorBucket"]
        assert vb.get("defaultIndexMode") == "CLASSIC", vb
        # NEW index inherits the (echo-only) mode; existing one is untouched
        idx2 = f"{prefix}-i2"
        client.create_index(vectorBucketName=bucket, indexName=idx2, dataType="float32",
                            dimension=2, distanceMetric="euclidean")
        state["index2"] = idx2
        ix2 = client.get_index(vectorBucketName=bucket, indexName=idx2)["index"]
        assert ix2.get("indexMode") == "CLASSIC", ix2
        client.update_index_mode(
            indexArn=f"{state['bucket_arn']}/index/{idx2}", indexMode="ENHANCED")
        ix2b = client.get_index(vectorBucketName=bucket, indexName=idx2)["index"]
        assert ix2b.get("indexMode") == "ENHANCED", ix2b
        # queries still work identically under both stored modes (§4.4 echo-only)
        resp = client.query_vectors(
            vectorBucketName=bucket, indexName=index, topK=1,
            queryVector={"float32": [1.0, 0.0, 0.0, 0.0]})
        assert len(resp["vectors"]) == 1
        return "default CLASSIC -> inherited; UpdateIndexMode -> ENHANCED; query unaffected"

    step(17, "put_default_index_mode + update_index_mode (echo-only)", m17)

    def m18():
        err = expect_client_error(
            client, client.exceptions.NotFoundException,
            lambda: client.get_vector_bucket(vectorBucketName=f"{prefix}-ghost"),
        )
        assert err.response["Error"]["Code"] == "NotFoundException"
        # non-empty bucket refuses to delete (409, S3 convention)
        err = expect_client_error(
            client, client.exceptions.ConflictException,
            lambda: client.delete_vector_bucket(vectorBucketName=bucket),
        )
        code = err.response["Error"]["Code"]
        status = err.response["ResponseMetadata"]["HTTPStatusCode"]
        OBS.append(f"delete-nonempty: botocore Error.Code={code!r} http={status}")
        assert code == "ConflictException" and status == 409, (code, status)
        err = expect_client_error(
            client, client.exceptions.NotFoundException,
            lambda: client.delete_index(vectorBucketName=bucket, indexName=f"{prefix}-nope"),
        )
        assert err.response["Error"]["Code"] == "NotFoundException"
        return "ghost bucket 404; non-empty delete 409; ghost index delete 404"

    step(18, "errors: 404 gets, 409 delete-non-empty", m18)

    def m19():
        client.delete_index(vectorBucketName=bucket, indexName=index)
        client.delete_index(vectorBucketName=bucket, indexName=state["index2"])
        client.delete_vector_bucket(vectorBucketName=bucket)
        for extra in state.get("extra_buckets", []):
            client.delete_vector_bucket(vectorBucketName=extra)
        err = expect_client_error(
            client, client.exceptions.NotFoundException,
            lambda: client.get_vector_bucket(vectorBucketName=bucket),
        )
        assert err.response["ResponseMetadata"]["HTTPStatusCode"] == 404
        rest = client.list_vector_buckets(maxResults=500, prefix=f"{prefix}-")["vectorBuckets"]
        assert rest == [], f"cleanup left buckets: {[b['vectorBucketName'] for b in rest]}"
        return "index x2 + bucket x3 deleted; prefix drained"

    step(19, "cleanup: delete indexes, buckets; verify prefix drained", m19)

    run_steps()

    print("\n# wire observations (boto3-side):")
    for o in OBS:
        print(f"#   - {o}")
    if FAILED:
        print(f"\nRESULT: {len(FAILED)} step(s) FAILED", file=sys.stderr)
        return 1
    print(f"\nRESULT: all {len(STEPS)} steps PASS")
    return 0


if __name__ == "__main__":
    sys.exit(main())
