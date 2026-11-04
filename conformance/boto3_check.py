#!/usr/bin/env python3
"""boto3 conformance check against an s3vectors-compatible server (M0 Task C).

Talks to an ALREADY-RUNNING server (default http://127.0.0.1:8787) — this script
owns no server lifecycle. 9 steps, each printing PASS/FAIL; exit code 1 on any
FAIL. Works unchanged against real AWS by pointing --endpoint elsewhere (that
is the point of the prefix arg: no cleanup needed, spike has no delete ops).

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


def step(num, name, fn):
    print(f"[{num}/9] {name} ... ", end="", flush=True)
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

    print("\n# wire observations (boto3-side):")
    for o in OBS:
        print(f"#   - {o}")
    # step 10: no cleanup — spike implements no delete operations.
    if FAILED:
        print(f"\nRESULT: {len(FAILED)} step(s) FAILED", file=sys.stderr)
        return 1
    print("\nRESULT: all 9 steps PASS")
    return 0


if __name__ == "__main__":
    sys.exit(main())
