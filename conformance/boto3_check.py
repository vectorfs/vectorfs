#!/usr/bin/env python3
"""boto3 conformance check against an s3vectors-compatible server (M0 Task C
core 9 steps + M1 T3 management 9 steps + M1 T4 vector data plane 7 steps
+ M1 T5 filter engine 6 steps = 32 steps).

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

    # ------------------------------------------------------------------
    # M1 T4 vector data-plane section (steps 19-25): GetVectors roundtrip +
    # missing-key omission, ListVectors pagination + parallel segments,
    # DeleteVectors + verification, and PutVectors over-limit negatives.
    # Range violations that botocore itself validates client-side (keys>100
    # / >500, maxResults range, segment pairing is NOT client-checked) cannot
    # be driven through boto3 — those live in the tower oneshot unit tests
    # (vf/src/ops.rs). Document-typed metadata limits (2 KB filterable,
    # nested-object types) ARE drivable and checked server-side here.
    # ------------------------------------------------------------------

    def v20():
        # grow the key space: v00..v09 plus a punctuation/unicode key
        vectors = [{"key": f"{prefix}-v{i:02d}",
                    "data": {"float32": [float(i), 1.0, 0.0, 0.0]}}
                   for i in range(10)]
        hostile = "sp\"ace'/🎯\u00fc"
        vectors.append({"key": hostile, "data": {"float32": [9.0, 9.0, 0.0, 0.0]},
                        "metadata": {"kind": "hostile"}})
        client.put_vectors(vectorBucketName=bucket, indexName=index, vectors=vectors)
        # every key now in the index: a, b, c (core section) + the 11 new ones
        state["vec_keys"] = sorted(["a", "b", "c"] + [v["key"] for v in vectors])
        # get roundtrip: found keys in request order with data+metadata, the
        # missing key OMITTED (the model has no per-entry status member)
        resp = client.get_vectors(
            vectorBucketName=bucket, indexName=index,
            keys=[hostile, f"{prefix}-ghost", "a", "b"],
            returnData=True, returnMetadata=True,
        )
        OBS.append(f"get_vectors entry fields: {sorted(resp['vectors'][0])}")
        keys = [v["key"] for v in resp["vectors"]]
        assert keys == [hostile, "a", "b"], keys
        a = next(v for v in resp["vectors"] if v["key"] == "a")
        assert a["data"]["float32"] == [0.0, 1.0, 0.0, 0.0], a["data"]
        assert a["metadata"] == {"tag": "a-new", "note": "replaced"}, a["metadata"]
        b = next(v for v in resp["vectors"] if v["key"] == "b")
        assert "metadata" not in b and b["data"]["float32"] == [0.0, 0.0, 0.0, 1.0], b
        h = next(v for v in resp["vectors"] if v["key"] == hostile)
        assert h["metadata"] == {"kind": "hostile"}, h
        # defaults: neither member present
        plain = client.get_vectors(vectorBucketName=bucket, indexName=index,
                                   keys=["a"])["vectors"][0]
        assert plain == {"key": "a"}, plain
        return f"{len(state['vec_keys'])} keys stocked; order+omission+defaults ok"

    step(19, "get_vectors: roundtrip, missing key omitted, default members absent", v20)

    def v21():
        # page 1: maxResults=2 -> 2 sorted keys + nextToken
        resp = client.list_vectors(vectorBucketName=bucket, indexName=index, maxResults=2)
        keys = [v["key"] for v in resp["vectors"]]
        OBS.append(f"list_vectors entry fields: {sorted(resp['vectors'][0])}")
        assert keys == state["vec_keys"][:2], keys
        assert "nextToken" in resp, resp
        state["list_token"] = resp["nextToken"]
        assert sorted(resp["vectors"][0]) == ["key"], "default list carries only key"
        return f"page1={keys}"

    step(20, "list_vectors page 1 (maxResults=2, key-ascending)", v21)

    def v22():
        # walk to the end through nextToken; union must be every key, sorted
        seen, token, pages = list(state["vec_keys"][:2]), state["list_token"], 1
        while token:
            resp = client.list_vectors(vectorBucketName=bucket, indexName=index,
                                       maxResults=2, nextToken=token)
            page = [v["key"] for v in resp["vectors"]]
            assert page == sorted(page), f"page not sorted: {page}"
            seen.extend(page)
            token = resp.get("nextToken")
            pages += 1
            assert pages < 30, "pagination did not terminate"
        assert seen == state["vec_keys"], (seen, state["vec_keys"])
        # returnData/returnMetadata members come back when asked for
        # (first key ascending is "a" — its current row is s6's upsert)
        resp = client.list_vectors(vectorBucketName=bucket, indexName=index,
                                   maxResults=1, returnData=True, returnMetadata=True)
        v0 = resp["vectors"][0]
        assert v0["key"] == "a", v0
        assert v0["data"]["float32"] == [0.0, 1.0, 0.0, 0.0], v0
        assert v0["metadata"] == {"tag": "a-new", "note": "replaced"}, v0
        return f"{pages} pages, {len(seen)} keys, data+metadata members verified"

    step(21, "list_vectors: paginate to end, order + returnData/Metadata", v22)

    def v23():
        # parallel segments: segmentCount=2, both workers paginate their own
        # segment; union == full set, disjoint, and a repeat call is identical
        def seg(seg_idx):
            out, token = [], None
            while True:
                kw = {"vectorBucketName": bucket, "indexName": index, "segmentCount": 2,
                      "segmentIndex": seg_idx, "maxResults": 3}
                if token:
                    kw["nextToken"] = token
                resp = client.list_vectors(**kw)
                out.extend(v["key"] for v in resp["vectors"])
                token = resp.get("nextToken")
                if not token:
                    return out
        seg0 = seg(0)
        seg1 = seg(1)
        assert sorted(seg0 + seg1) == state["vec_keys"], (seg0, seg1)
        assert not set(seg0) & set(seg1), "segments overlap"
        assert seg(0) == seg0, "segment assignment is not stable"
        return (f"seg0={len(seg0)} seg1={len(seg1)} union={len(seg0)+len(seg1)} "
                f"disjoint+stable")

    step(22, "list_vectors segments: union=full, disjoint, stable", v23)

    def v24():
        # delete the hostile key + b; deletion is idempotent and visible
        hostile = "sp\"ace'/🎯\u00fc"
        client.delete_vectors(vectorBucketName=bucket, indexName=index,
                              keys=[hostile, "b", f"{prefix}-ghost"])
        client.delete_vectors(vectorBucketName=bucket, indexName=index,
                              keys=[hostile])  # idempotent repeat
        resp = client.get_vectors(vectorBucketName=bucket, indexName=index,
                                  keys=[hostile, "b", "c"])
        assert [v["key"] for v in resp["vectors"]] == ["c"], resp
        keys = [v["key"] for v in client.list_vectors(
            vectorBucketName=bucket, indexName=index, maxResults=1000)["vectors"]]
        assert hostile not in keys and "b" not in keys, keys
        state["vec_keys"] = keys
        # delete -> put rebuilds the row normally
        client.put_vectors(vectorBucketName=bucket, indexName=index,
                           vectors=[{"key": hostile, "data": {"float32": [1.0, 2.0, 3.0, 4.0]},
                                     "metadata": {"round": 2}}])
        back = client.get_vectors(vectorBucketName=bucket, indexName=index,
                                  keys=[hostile], returnData=True,
                                  returnMetadata=True)["vectors"]
        assert back and back[0]["metadata"] == {"round": 2}, back
        client.delete_vectors(vectorBucketName=bucket, indexName=index, keys=[hostile])
        return f"deleted b+hostile (ghost no-op); rebuilt+redeleted hostile"

    step(23, "delete_vectors: special chars, idempotent, verified gone, rebuild", v24)

    def v25():
        # filterable metadata > 2 KB -> server-side ValidationException
        err = expect_client_error(
            client, client.exceptions.ValidationException,
            lambda: client.put_vectors(
                vectorBucketName=bucket, indexName=index,
                vectors=[{"key": "big", "data": {"float32": [1.0, 0.0, 0.0, 0.0]},
                          "metadata": {"s": "v" * 2100}}],
            ),
        )
        assert err.response["ResponseMetadata"]["HTTPStatusCode"] == 400
        OBS.append(f"metadata-2kb: {err.response['Error'].get('Message', '')[:90]}")
        return "filterable 2.1 KB -> ValidationException 400"

    step(24, "put_vectors metadata >2KB filterable -> ValidationException", v25)

    def v26():
        # nested object metadata value -> ValidationException naming legal types
        err = expect_client_error(
            client, client.exceptions.ValidationException,
            lambda: client.put_vectors(
                vectorBucketName=bucket, indexName=index,
                vectors=[{"key": "nest", "data": {"float32": [1.0, 0.0, 0.0, 0.0]},
                          "metadata": {"nested": {"deep": True}}}],
            ),
        )
        assert err.response["ResponseMetadata"]["HTTPStatusCode"] == 400
        msg = err.response["Error"].get("Message", "")
        assert "string, number, boolean" in msg, msg
        # flat list of primitives is legal and round-trips
        client.put_vectors(
            vectorBucketName=bucket, indexName=index,
            vectors=[{"key": "listy", "data": {"float32": [0.0, 1.0, 0.0, 0.0]},
                      "metadata": {"l": ["a", 2, True]}}],
        )
        got = client.get_vectors(vectorBucketName=bucket, indexName=index,
                                 keys=["listy"], returnMetadata=True)["vectors"]
        assert got[0]["metadata"] == {"l": ["a", 2, True]}, got
        return f"nested rejected ({msg[:60]}…); flat list round-trips"

    step(25, "put_vectors metadata type validation (nested rejected, list ok)", v26)

    # ------------------------------------------------------------------
    # M1 T5 filter section (steps 26-31): the full AWS operator set driven
    # through boto3's typed `filter` document parameter against a stocked
    # index — eq / and / range / in / startsWith / exists, each asserting the
    # exact key set (plus any-element list semantics where they land).
    # Operator/validation negatives live in the tower oneshot unit tests
    # (vf/src/ops.rs + vf/src/filter.rs); these steps pin the WIRE path.
    # ------------------------------------------------------------------

    def f_seed():
        # dedicated index so the assertion universe is exactly these 6
        # vectors (the main index carries earlier steps' rows).
        fidx = f"{prefix}-if"
        state["filter_index"] = fidx
        client.create_index(vectorBucketName=bucket, indexName=fidx,
                            dataType="float32", dimension=4, distanceMetric="cosine")
        # 6 vectors with rich filterable metadata: numbers, strings, bools,
        # a list, and one row with NO metadata (missing-field semantics).
        client.put_vectors(
            vectorBucketName=bucket, indexName=fidx,
            vectors=[
                {"key": "f1", "data": {"float32": [1.0, 0.0, 0.0, 0.0]},
                 "metadata": {"genre": "documentary", "year": 2019, "ok": True}},
                {"key": "f2", "data": {"float32": [0.9, 0.1, 0.0, 0.0]},
                 "metadata": {"genre": "comedy", "year": 2021, "ok": False}},
                {"key": "f3", "data": {"float32": [0.8, 0.2, 0.0, 0.0]},
                 "metadata": {"genre": "documentary", "year": 2022, "ok": True,
                              "tags": ["a", "b"]}},
                {"key": "f4", "data": {"float32": [0.7, 0.3, 0.0, 0.0]},
                 "metadata": {"genre": "drama", "year": 2020}},
                {"key": "f5", "data": {"float32": [0.6, 0.4, 0.0, 0.0]},
                 "metadata": {"s3_path": "/marketing/report.pdf"}},
                {"key": "f6", "data": {"float32": [0.5, 0.5, 0.0, 0.0]}},
            ],
        )
        return "6 vectors (scalars x3 kinds, list, one without metadata)"

    step(26, "filters: seed metadata-rich vectors", f_seed)

    def fkeys(flt, **kw):
        resp = client.query_vectors(
            vectorBucketName=bucket, indexName=state["filter_index"], topK=10,
            queryVector={"float32": [1.0, 0.0, 0.0, 0.0]}, filter=flt, **kw)
        keys = sorted(v["key"] for v in resp["vectors"])
        assert resp["distanceMetric"] == "cosine", resp
        return keys

    def f27():
        # $eq: shorthand + explicit + a miss (type mismatch = no match)
        assert fkeys({"genre": "documentary"}) == ["f1", "f3"], fkeys({"genre": "documentary"})
        assert fkeys({"genre": {"$eq": "comedy"}}) == ["f2"]
        assert fkeys({"year": {"$eq": 2020}}) == ["f4"]
        assert fkeys({"year": "2020"}) == [], "string-on-number must not match"
        assert fkeys({"ghost": 1}) == [], "missing field must not match"
        return "eq shorthand/explicit/miss + mismatch + missing"

    step(27, "filter $eq (string/number, mismatch & missing = no match)", f27)

    def f28():
        # $and + implicit top-level AND + $ne boundary semantics
        assert fkeys({"$and": [{"genre": {"$eq": "documentary"}},
                               {"year": {"$gte": 2020}}]}) == ["f3"]
        assert fkeys({"genre": "documentary", "ok": True}) == ["f1", "f3"]
        # $ne matches rows WITHOUT the field too (f6 has no metadata)
        got = fkeys({"genre": {"$ne": "documentary"}})
        assert got == ["f2", "f4", "f5", "f6"], got
        # nested: $or inside $and
        assert fkeys({"$and": [{"$or": [{"genre": "comedy"}, {"year": 2019}]}]}) == ["f1", "f2"]
        return "and/or nesting + ne matches missing"

    step(28, "filter $and/$or/$ne (implicit AND, nesting)", f28)

    def f29():
        # numeric range incl. same-field multi-operator AND
        assert fkeys({"year": {"$gt": 2020}}) == ["f2", "f3"]
        assert fkeys({"year": {"$gte": 2020}}) == ["f2", "f3", "f4"]
        assert fkeys({"year": {"$lt": 2020}}) == ["f1"]
        assert fkeys({"year": {"$lte": 2020}}) == ["f1", "f4"]
        assert fkeys({"year": {"$gte": 2019, "$lt": 2021}}) == ["f1", "f4"]
        return "range gt/gte/lt/lte + same-field band"

    step(29, "filter range $gt/$gte/$lt/$lte", f29)

    def f30():
        # $in with primitives + any-element against list metadata
        assert fkeys({"genre": {"$in": ["comedy", "drama"]}}) == ["f2", "f4"]
        assert fkeys({"year": {"$in": [2019, 2022]}}) == ["f1", "f3"]
        assert fkeys({"tags": {"$in": ["b", "z"]}}) == ["f3"], "list any-element"
        assert fkeys({"tags": {"$nin": ["b", "z"]}}) == ["f1", "f2", "f4", "f5", "f6"]
        return "in/nin (mixed filter list + list-metadata any-element)"

    step(30, "filter $in/$nin (+ list any-element)", f30)

    def f31():
        # $startsWith (string) + $exists (bool), including missing-field rows
        assert fkeys({"s3_path": {"$startsWith": "/marketing/"}}) == ["f5"]
        assert fkeys({"s3_path": {"$startsWith": "/sales/"}}) == []
        assert fkeys({"genre": {"$startsWith": "do"}}) == ["f1", "f3"]
        assert fkeys({"genre": {"$exists": True}}) == ["f1", "f2", "f3", "f4"]
        # rows with NO metadata at all satisfy $exists:false
        got = fkeys({"genre": {"$exists": False}})
        assert got == ["f5", "f6"], got
        return "startsWith + exists true/false (missing rows)"

    step(31, "filter $startsWith/$exists", f31)

    def m27():
        client.delete_index(vectorBucketName=bucket, indexName=index)
        client.delete_index(vectorBucketName=bucket, indexName=state["index2"])
        client.delete_index(vectorBucketName=bucket, indexName=state["filter_index"])
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

    step(32, "cleanup: delete indexes, buckets; verify prefix drained", m27)

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
