// aws-sdk-go-v2 conformance check against an s3vectors-compatible server.
// Moved to repo-root conformance/ 2026-11-04 (M0 Task C); grown to the full
// 32-step matrix in M1 Task 7 — step numbers, names and assertion semantics
// mirror conformance/boto3_check.py 1:1 for side-by-side comparison:
//
//	 1- 9  core (create/put/query/upsert + the three error faces)
//	10-18  management plane (get/list, policy, tags, indexMode, error face)
//	19-25  vector data plane (get/list/segments/delete + metadata limits)
//	26-31  filter engine ($eq/$and/$or/$ne/range/$in/$nin/startsWith/$exists)
//	32     cleanup (delete indexes/buckets, verify prefix drained)
//
// Talks to an already-running server; owns no server lifecycle. Any FAIL ->
// exit 1. Like boto3, enum / maxResults-range negatives that the Go SDK would
// happily put on the wire are NOT driven here (parity with the boto3 scope —
// they live in the tower oneshot unit tests, crates/server/src/ops.rs).
//
// Assertion-scope adjustments forced by the Go SDK (vs the boto3 text):
//   - parsed members are TYPED, not dict keys: "member absent" checks become
//     nil-interface / non-nil *time.Time checks, and unknown members the
//     server double-emits (vectorBucket/index objects) are invisible to the
//     typed outputs (recorded as observations, like boto3's .get() probes);
//   - error HTTP status comes from *smithyhttp.ResponseError instead of
//     ResponseMetadata.HTTPStatusCode (same numbers asserted);
//   - metadata documents round-trip via UnmarshalSmithyDocument with numbers
//     normalized smithy Number -> float64 (docMap/norm below);
//   - ListVectorsInput.SegmentIndex is a NON-boxed int32 whose serializer
//     omits zero values, so "segmentIndex: 0" cannot be expressed through
//     the typed input at all (boto3 sends every provided param, 0 included).
//     A Build-stage middleware (withSegmentIndexZero) patches the serialized
//     JSON body — before the SigV4 payload hash is computed, so the signature
//     stays valid — used only by step 22's segment walk.
//
// Usage:
//
//	go run ./conformance/go --endpoint http://127.0.0.1:8787 \
//	    --access-key test --secret-key testsecret [--region us-east-1] [--prefix XXX]
package main

import (
	"bytes"
	"context"
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"math"
	"os"
	"reflect"
	"sort"
	"strconv"
	"strings"
	"time"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/config"
	"github.com/aws/aws-sdk-go-v2/credentials"
	"github.com/aws/aws-sdk-go-v2/service/s3vectors"
	s3vdoc "github.com/aws/aws-sdk-go-v2/service/s3vectors/document"
	"github.com/aws/aws-sdk-go-v2/service/s3vectors/types"
	smithydoc "github.com/aws/smithy-go/document"
	"github.com/aws/smithy-go/middleware"
	smithyhttp "github.com/aws/smithy-go/transport/http"
)

const totalSteps = 32

var (
	failed int
	obs    []string // wire observations printed in the summary
)

// step prints PASS/FAIL per step; assertions inside fn use panic, which is
// recovered here so later steps still run and the exit code reflects failures.
func step(n int, name string, fn func() string) {
	fmt.Printf("[%d/%d] %s ... ", n, totalSteps, name)
	var details string
	var err error
	func() {
		defer func() {
			if r := recover(); r != nil {
				err = fmt.Errorf("%v", r)
			}
		}()
		details = fn()
	}()
	if err != nil {
		failed++
		fmt.Printf("FAIL %v\n", err)
	} else {
		fmt.Printf("PASS %s\n", details)
	}
}

// must panics on transport errors (the step framework reports it as FAIL).
func must(err error) {
	if err != nil {
		panic(err)
	}
}

// httpStatus digs the HTTP status out of an SDK error (the Go analogue of
// botocore's ResponseMetadata.HTTPStatusCode).
func httpStatus(err error) int {
	var re *smithyhttp.ResponseError
	if errors.As(err, &re) {
		return re.Response.StatusCode
	}
	return 0
}

func expectStatus(err error, code int) {
	if got := httpStatus(err); got != code {
		panic(fmt.Sprintf("http status: want %d got %d (err %v)", code, got, err))
	}
}

// eqStrs compares two key slices (nil-normalized: [] and nil are equal here).
func eqStrs(got []string, want ...string) {
	if len(got) != len(want) {
		panic(fmt.Sprintf("got %v want %v", got, want))
	}
	for i := range got {
		if got[i] != want[i] {
			panic(fmt.Sprintf("got %v want %v", got, want))
		}
	}
}

func sortedCopy(in []string) []string {
	out := append([]string(nil), in...)
	sort.Strings(out)
	return out
}

// withSegmentIndexZero patches the serialized ListVectors JSON body so a
// segmentIndex of 0 reaches the wire (the typed input cannot express it —
// see the package doc). Build stage: the request stream lives on the
// smithyhttp.Request (not .Body, which is only materialized for the
// transport), so it is read/replaced via GetStream/SetStream; the SigV4
// payload hash and signature are computed later (Finalize), so they cover
// the patched body.
func withSegmentIndexZero(o *s3vectors.Options) {
	o.APIOptions = append(o.APIOptions, func(stack *middleware.Stack) error {
		return stack.Build.Add(middleware.BuildMiddlewareFunc(
			"SegmentIndexZeroWorkaround",
			func(ctx context.Context, in middleware.BuildInput, next middleware.BuildHandler) (
				middleware.BuildOutput, middleware.Metadata, error,
			) {
				req, ok := in.Request.(*smithyhttp.Request)
				if ok && strings.HasSuffix(req.URL.Path, "/ListVectors") && req.GetStream() != nil {
					if body, rerr := io.ReadAll(req.GetStream()); rerr == nil && len(body) > 0 {
						var m map[string]any
						if jerr := json.Unmarshal(body, &m); jerr == nil {
							if _, hasCount := m["segmentCount"]; hasCount {
								if _, hasIdx := m["segmentIndex"]; !hasIdx {
									m["segmentIndex"] = 0
									if b, merr := json.Marshal(m); merr == nil {
										body = b
									}
								}
							}
						}
						// reattach (possibly patched) body in every path
						if patched, serr := req.SetStream(bytes.NewReader(body)); serr == nil {
							*req = *patched
						} else {
							return middleware.BuildOutput{}, middleware.Metadata{},
								fmt.Errorf("segmentIndex workaround: %w", serr)
						}
					}
				}
				return next.HandleBuild(ctx, in)
			}), middleware.Before)
	})
}

func main() {
	endpoint := flag.String("endpoint", "http://127.0.0.1:8787", "server base endpoint")
	accessKey := flag.String("access-key", "", "static access key")
	secretKey := flag.String("secret-key", "", "static secret key")
	region := flag.String("region", "us-east-1", "signing region")
	prefix := flag.String("prefix", "", "resource name prefix (default: random)")
	flag.Parse()
	if *accessKey == "" || *secretKey == "" {
		fmt.Fprintln(os.Stderr, "--access-key and --secret-key are required")
		os.Exit(2)
	}
	if *prefix == "" {
		b := make([]byte, 4)
		_, _ = rand.Read(b)
		*prefix = "vf" + hex.EncodeToString(b)
	}
	bucket := *prefix + "-b"
	index := *prefix + "-i"
	fmt.Printf("# aws-sdk-go-v2 s3vectors endpoint=%s region=%s bucket=%s index=%s\n",
		*endpoint, *region, bucket, index)

	ctx := context.Background()
	newClient := func(secret string) *s3vectors.Client {
		c, err := config.LoadDefaultConfig(ctx,
			config.WithRegion(*region),
			config.WithCredentialsProvider(credentials.NewStaticCredentialsProvider(*accessKey, secret, "")),
		)
		if err != nil {
			panic(err)
		}
		return s3vectors.NewFromConfig(c, func(o *s3vectors.Options) {
			o.BaseEndpoint = aws.String(*endpoint)
		})
	}
	client := newClient(*secretKey)

	var (
		createdBucketArn string
		createdIndexArn  string
		extraBuckets     []string
		index2           string
		filterIndex      string
		vecKeys          []string
		listToken        string
	)

	// Record which typed members the SDK actually parsed (model says ARN-only
	// on creates; the server double-emits nested objects — unknown members
	// must be silently ignored, which is itself a wire observation).
	outputMembers := func(v any) []string {
		var out []string
		rv := reflect.ValueOf(v)
		for i := 0; i < rv.NumField(); i++ {
			if rv.Type().Field(i).IsExported() && !rv.Field(i).IsZero() {
				out = append(out, rv.Type().Field(i).Name)
			}
		}
		sort.Strings(out)
		return out
	}

	step(1, "CreateVectorBucket -> VectorBucketArn", func() string {
		resp, err := client.CreateVectorBucket(ctx, &s3vectors.CreateVectorBucketInput{
			VectorBucketName: aws.String(bucket),
		})
		must(err)
		if resp.VectorBucketArn == nil {
			panic("missing VectorBucketArn; parsed members: " + fmt.Sprint(outputMembers(*resp)))
		}
		createdBucketArn = *resp.VectorBucketArn
		if !strings.HasSuffix(createdBucketArn, "bucket/"+bucket) {
			panic(createdBucketArn)
		}
		obs = append(obs, fmt.Sprintf("CreateVectorBucketOutput populated members: %v (double-emitted vectorBucket{creationTime:epoch-float} ignored without error; boto3 additionally parses creationTime as datetime when present)", outputMembers(*resp)))
		return fmt.Sprintf("arn=%s members=%v", createdBucketArn, outputMembers(*resp))
	})

	step(2, "duplicate create -> ConflictException", func() string {
		_, err := client.CreateVectorBucket(ctx, &s3vectors.CreateVectorBucketInput{
			VectorBucketName: aws.String(bucket),
		})
		var ce *types.ConflictException
		if !errors.As(err, &ce) {
			panic(fmt.Sprintf("want ConflictException, got %v", err))
		}
		expectStatus(err, 409)
		obs = append(obs, fmt.Sprintf("conflict: errors.As ConflictException ok, ErrorCode=%q Message=%q http=%d", ce.ErrorCode(), aws.ToString(ce.Message), httpStatus(err)))
		return fmt.Sprintf("ErrorCode=%q http=%d", ce.ErrorCode(), httpStatus(err))
	})

	step(3, "CreateIndex(float32/4/cosine) -> IndexArn", func() string {
		resp, err := client.CreateIndex(ctx, &s3vectors.CreateIndexInput{
			VectorBucketName: aws.String(bucket),
			IndexName:        aws.String(index),
			DataType:         types.DataTypeFloat32,
			Dimension:        aws.Int32(4),
			DistanceMetric:   types.DistanceMetricCosine,
		})
		must(err)
		if resp.IndexArn == nil {
			panic("missing IndexArn")
		}
		createdIndexArn = *resp.IndexArn
		return fmt.Sprintf("arn=%s members=%v", createdIndexArn, outputMembers(*resp))
	})

	f32 := func(xs ...float32) types.VectorData {
		return &types.VectorDataMemberFloat32{Value: xs}
	}

	// []float32 out of a typed VectorData union member.
	f32Of := func(d types.VectorData) []float32 {
		m, ok := d.(*types.VectorDataMemberFloat32)
		if !ok {
			return nil // member absent on the wire
		}
		return m.Value
	}

	step(4, "PutVectors x3", func() string {
		_, err := client.PutVectors(ctx, &s3vectors.PutVectorsInput{
			VectorBucketName: aws.String(bucket),
			IndexName:        aws.String(index),
			Vectors: []types.PutInputVector{
				{Key: aws.String("a"), Data: f32(1, 0, 0, 0),
					Metadata: s3vdoc.NewLazyDocument(map[string]any{"tag": "a-old", "legacy": true})},
				{Key: aws.String("b"), Data: f32(0, 0, 0, 1)},
				{Key: aws.String("c"), Data: f32(1, 1, 0, 0),
					Metadata: s3vdoc.NewLazyDocument(map[string]any{"tag": "demo", "n": 7, "ok": true})},
			},
		})
		must(err)
		return "3 vectors (a/b/c, c has string+number+bool metadata)"
	})

	docMap := func(d interface{ UnmarshalSmithyDocument(any) error }) map[string]any {
		if d == nil {
			return nil
		}
		var m map[string]any
		if err := d.UnmarshalSmithyDocument(&m); err != nil {
			panic(err)
		}
		return m
	}

	// numbers come back as smithy document.Number (string); normalize to float64
	var norm func(v any) any
	norm = func(v any) any {
		switch t := v.(type) {
		case smithydoc.Number:
			f, err := strconv.ParseFloat(string(t), 64)
			if err != nil {
				panic(err)
			}
			return f
		case map[string]any:
			for k, vv := range t {
				t[k] = norm(vv)
			}
			return t
		case []any:
			for i, vv := range t {
				t[i] = norm(vv)
			}
			return t
		default:
			return v
		}
	}

	step(5, "QueryVectors topK=2 (order a<c, d~0 first, metadata roundtrip)", func() string {
		resp, err := client.QueryVectors(ctx, &s3vectors.QueryVectorsInput{
			VectorBucketName: aws.String(bucket),
			IndexName:        aws.String(index),
			TopK:             aws.Int32(2),
			QueryVector:      f32(1, 0, 0, 0),
			ReturnDistance:   true,
			ReturnMetadata:   true,
		})
		must(err)
		if len(resp.Vectors) != 2 {
			panic(fmt.Sprintf("expected 2 got %d", len(resp.Vectors)))
		}
		if k1, k2 := *resp.Vectors[0].Key, *resp.Vectors[1].Key; k1 != "a" || k2 != "c" {
			panic(fmt.Sprintf("order wrong: %s,%s (want a,c)", k1, k2))
		}
		if d := *resp.Vectors[0].Distance; math.Abs(float64(d)) > 1e-6 {
			panic(fmt.Sprintf("d(a)=%v want ~0", d))
		}
		got := norm(docMap(resp.Vectors[1].Metadata))
		want := map[string]any{"tag": "demo", "n": float64(7), "ok": true}
		if !reflect.DeepEqual(got, want) {
			panic(fmt.Sprintf("metadata mismatch: got %#v want %#v", got, want))
		}
		if resp.DistanceMetric != types.DistanceMetricCosine {
			panic(fmt.Sprintf("distanceMetric=%s", resp.DistanceMetric))
		}
		// Go 口径: typed outputs carry no time-ish members on QueryVectors
		// (Key/Data/Metadata/Distance only), so boto3's generic
		// "any *time* member must parse as datetime" walk has nothing to
		// visit here — covered instead by steps 10-12 creationTime checks.
		return fmt.Sprintf("keys=[a c] d(a)=%v meta(c)=%v metric=%s", *resp.Vectors[0].Distance, got, resp.DistanceMetric)
	})

	step(6, "upsert key=a -> overwrite effective", func() string {
		_, err := client.PutVectors(ctx, &s3vectors.PutVectorsInput{
			VectorBucketName: aws.String(bucket),
			IndexName:        aws.String(index),
			Vectors: []types.PutInputVector{
				{Key: aws.String("a"), Data: f32(0, 1, 0, 0),
					Metadata: s3vdoc.NewLazyDocument(map[string]any{"tag": "a-new", "note": "replaced"})},
			},
		})
		must(err)
		resp, err := client.QueryVectors(ctx, &s3vectors.QueryVectorsInput{
			VectorBucketName: aws.String(bucket),
			IndexName:        aws.String(index),
			TopK:             aws.Int32(3),
			QueryVector:      f32(0, 1, 0, 0),
			ReturnDistance:   true,
			ReturnMetadata:   true,
		})
		must(err)
		var a *types.QueryOutputVector
		for i := range resp.Vectors {
			if *resp.Vectors[i].Key == "a" {
				a = &resp.Vectors[i]
			}
		}
		if a == nil {
			panic("key a not found")
		}
		if d := *a.Distance; math.Abs(float64(d)) > 1e-6 {
			panic(fmt.Sprintf("d(a)=%v want ~0", d))
		}
		got := norm(docMap(a.Metadata))
		want := map[string]any{"tag": "a-new", "note": "replaced"}
		if !reflect.DeepEqual(got, want) {
			panic(fmt.Sprintf("metadata not whole-replaced (merge would keep legacy): %#v", got))
		}
		resp2, err := client.QueryVectors(ctx, &s3vectors.QueryVectorsInput{
			VectorBucketName: aws.String(bucket),
			IndexName:        aws.String(index),
			TopK:             aws.Int32(1),
			QueryVector:      f32(1, 0, 0, 0),
			ReturnDistance:   true,
		})
		must(err)
		if *resp2.Vectors[0].Key != "c" {
			panic(fmt.Sprintf("old vector still closest: %s", *resp2.Vectors[0].Key))
		}
		return "vector+metadata overwritten (whole-doc replace, no legacy residue)"
	})

	step(7, "query missing index -> NotFoundException", func() string {
		_, err := client.QueryVectors(ctx, &s3vectors.QueryVectorsInput{
			VectorBucketName: aws.String(bucket),
			IndexName:        aws.String(*prefix + "-nope"),
			TopK:             aws.Int32(1),
			QueryVector:      f32(1, 0, 0, 0),
		})
		var nf *types.NotFoundException
		if !errors.As(err, &nf) {
			panic(fmt.Sprintf("want NotFoundException, got %v", err))
		}
		expectStatus(err, 404)
		obs = append(obs, fmt.Sprintf("notfound: errors.As NotFoundException ok, ErrorCode=%q http=%d", nf.ErrorCode(), httpStatus(err)))
		return fmt.Sprintf("ErrorCode=%q http=%d", nf.ErrorCode(), httpStatus(err))
	})

	step(8, "dimension mismatch (len 3 vs 4) -> ValidationException", func() string {
		_, err := client.PutVectors(ctx, &s3vectors.PutVectorsInput{
			VectorBucketName: aws.String(bucket),
			IndexName:        aws.String(index),
			Vectors: []types.PutInputVector{
				{Key: aws.String("bad"), Data: f32(1, 2, 3)},
			},
		})
		var ve *types.ValidationException
		if !errors.As(err, &ve) {
			panic(fmt.Sprintf("want ValidationException, got %v", err))
		}
		expectStatus(err, 400)
		obs = append(obs, fmt.Sprintf("validation: errors.As ValidationException ok, ErrorCode=%q Message=%q http=%d", ve.ErrorCode(), aws.ToString(ve.Message), httpStatus(err)))
		return fmt.Sprintf("ErrorCode=%q http=%d", ve.ErrorCode(), httpStatus(err))
	})

	step(9, "wrong secret key -> AccessDeniedException", func() string {
		bad := newClient(*secretKey + "-WRONG")
		_, err := bad.CreateVectorBucket(ctx, &s3vectors.CreateVectorBucketInput{
			VectorBucketName: aws.String(*prefix + "-denied"),
		})
		var ad *types.AccessDeniedException
		if !errors.As(err, &ad) {
			panic(fmt.Sprintf("want AccessDeniedException, got %v", err))
		}
		expectStatus(err, 403)
		obs = append(obs, fmt.Sprintf("denied: errors.As AccessDeniedException ok, ErrorCode=%q Message=%q http=%d", ad.ErrorCode(), aws.ToString(ad.Message), httpStatus(err)))
		return fmt.Sprintf("ErrorCode=%q http=%d", ad.ErrorCode(), httpStatus(err))
	})

	// ---------------------------------------------------------------------
	// management plane (steps 10-18): get/list bucket+index, policy
	// roundtrip, tags (put/list/untag on bucket AND index ARNs), indexMode
	// knobs, and the 404/409 management error face — over the SAME
	// bucket/index the core section created.
	// ---------------------------------------------------------------------

	step(10, "get_vector_bucket (name + ARN) -> VectorBucket", func() string {
		resp, err := client.GetVectorBucket(ctx, &s3vectors.GetVectorBucketInput{
			VectorBucketName: aws.String(bucket),
		})
		must(err)
		vb := resp.VectorBucket
		if vb == nil {
			panic("missing vectorBucket member")
		}
		if aws.ToString(vb.VectorBucketName) != bucket || aws.ToString(vb.VectorBucketArn) != createdBucketArn {
			panic(fmt.Sprintf("bucket mismatch: %s / %s", aws.ToString(vb.VectorBucketName), aws.ToString(vb.VectorBucketArn)))
		}
		// Go 口径: creationTime parses into *time.Time (smithy json default
		// epoch-seconds) — non-nil is the datetime-type assertion.
		if vb.CreationTime == nil || vb.CreationTime.IsZero() {
			panic(fmt.Sprintf("creationTime not parsed: %v", vb.CreationTime))
		}
		obs = append(obs, fmt.Sprintf("get_vector_bucket typed members: %v", outputMembers(*vb)))
		resp2, err := client.GetVectorBucket(ctx, &s3vectors.GetVectorBucketInput{
			VectorBucketArn: aws.String(createdBucketArn),
		})
		must(err)
		if aws.ToString(resp2.VectorBucket.VectorBucketName) != bucket {
			panic("ARN locator returned a different bucket")
		}
		return fmt.Sprintf("members=%v byName+byArn creationTime=%v", outputMembers(*vb), vb.CreationTime.UTC().Format(time.RFC3339))
	})

	step(11, "list_vector_buckets paginate (3 buckets, maxResults=2)", func() string {
		x1, x2 := *prefix+"-x1", *prefix+"-x2"
		_, err := client.CreateVectorBucket(ctx, &s3vectors.CreateVectorBucketInput{VectorBucketName: aws.String(x1)})
		must(err)
		_, err = client.CreateVectorBucket(ctx, &s3vectors.CreateVectorBucketInput{VectorBucketName: aws.String(x2)})
		must(err)
		extraBuckets = []string{x1, x2}
		var seen []string
		token, pages := "", 0
		for {
			in := &s3vectors.ListVectorBucketsInput{
				MaxResults: aws.Int32(2),
				Prefix:     aws.String(*prefix + "-"),
			}
			if token != "" {
				in.NextToken = aws.String(token)
			}
			resp, err := client.ListVectorBuckets(ctx, in)
			must(err)
			names := make([]string, 0, len(resp.VectorBuckets))
			for _, b := range resp.VectorBuckets {
				names = append(names, aws.ToString(b.VectorBucketName))
			}
			if !sort.StringsAreSorted(names) {
				panic(fmt.Sprintf("page not name-sorted: %v", names))
			}
			seen = append(seen, names...)
			pages++
			if resp.NextToken == nil || *resp.NextToken == "" {
				break
			}
			token = *resp.NextToken
			if pages >= 10 {
				panic("pagination did not terminate")
			}
		}
		// bucket names under the prefix: {prefix}-b, {prefix}-x1, {prefix}-x2
		// ({prefix}-i is an index, not a bucket)
		want := sortedCopy([]string{bucket, x1, x2})
		if !reflect.DeepEqual(seen, want) {
			panic(fmt.Sprintf("pagination seen=%v want=%v pages=%d", seen, want, pages))
		}
		// summary shape: no dimension/indexMode members exist on the typed
		// VectorBucketSummary — record what the SDK parsed for one entry
		ls, err := client.ListVectorBuckets(ctx, &s3vectors.ListVectorBucketsInput{MaxResults: aws.Int32(500)})
		must(err)
		if len(ls.VectorBuckets) > 0 {
			obs = append(obs, fmt.Sprintf("ListVectorBuckets summary members: %v", outputMembers(ls.VectorBuckets[0])))
		}
		return fmt.Sprintf("pages=%d names=%v", pages, seen)
	})

	step(12, "get_index + list_indexes (summary shape)", func() string {
		resp, err := client.GetIndex(ctx, &s3vectors.GetIndexInput{
			VectorBucketName: aws.String(bucket), IndexName: aws.String(index),
		})
		must(err)
		ix := resp.Index
		if ix == nil {
			panic("missing index member")
		}
		if aws.ToString(ix.IndexName) != index || aws.ToString(ix.VectorBucketName) != bucket {
			panic(fmt.Sprintf("index locator mismatch: %v", ix))
		}
		if ix.DataType != types.DataTypeFloat32 || aws.ToInt32(ix.Dimension) != 4 {
			panic(fmt.Sprintf("dataType/dimension: %s/%d", ix.DataType, aws.ToInt32(ix.Dimension)))
		}
		if ix.DistanceMetric != types.DistanceMetricCosine {
			panic(fmt.Sprintf("distanceMetric=%s", ix.DistanceMetric))
		}
		if ix.CreationTime == nil || ix.CreationTime.IsZero() {
			panic(fmt.Sprintf("creationTime not parsed: %v", ix.CreationTime))
		}
		obs = append(obs, fmt.Sprintf("get_index typed members: %v", outputMembers(*ix)))
		resp2, err := client.GetIndex(ctx, &s3vectors.GetIndexInput{IndexArn: aws.String(createdIndexArn)})
		must(err)
		if aws.ToString(resp2.Index.IndexArn) != createdIndexArn {
			panic("ARN locator returned a different index")
		}
		ls, err := client.ListIndexes(ctx, &s3vectors.ListIndexesInput{
			VectorBucketArn: aws.String(createdBucketArn),
		})
		must(err)
		names := make([]string, 0, len(ls.Indexes))
		for _, i := range ls.Indexes {
			names = append(names, aws.ToString(i.IndexName))
		}
		eqStrs(names, index)
		// summary shape: exactly the four model members, nothing more
		// (typed IndexSummary has no dataType/dimension/indexMode fields —
		// the populated set must be precisely these)
		s0 := ls.Indexes[0]
		if got := outputMembers(s0); !reflect.DeepEqual(got,
			[]string{"CreationTime", "IndexArn", "IndexName", "VectorBucketName"}) {
			panic(fmt.Sprintf("summary shape: %v", got))
		}
		return fmt.Sprintf("index members=%v; summary members=%v", outputMembers(*ix), outputMembers(s0))
	})

	step(13, "put/get vector bucket policy (byte-faithful echo)", func() string {
		doc := `{ "Version": "2012-10-17", "Statement": [ {"Effect": "Allow"} ] }`
		_, err := client.PutVectorBucketPolicy(ctx, &s3vectors.PutVectorBucketPolicyInput{
			VectorBucketName: aws.String(bucket), Policy: aws.String(doc),
		})
		must(err)
		resp, err := client.GetVectorBucketPolicy(ctx, &s3vectors.GetVectorBucketPolicyInput{
			VectorBucketArn: aws.String(createdBucketArn),
		})
		must(err)
		if aws.ToString(resp.Policy) != doc {
			panic(fmt.Sprintf("policy not byte-faithful: %q", aws.ToString(resp.Policy)))
		}
		return fmt.Sprintf("policy roundtrip %dB byte-exact", len(doc))
	})

	step(14, "delete policy -> get 404; delete idempotent", func() string {
		_, err := client.DeleteVectorBucketPolicy(ctx, &s3vectors.DeleteVectorBucketPolicyInput{
			VectorBucketName: aws.String(bucket),
		})
		must(err)
		_, err = client.GetVectorBucketPolicy(ctx, &s3vectors.GetVectorBucketPolicyInput{
			VectorBucketName: aws.String(bucket),
		})
		var nf *types.NotFoundException
		if !errors.As(err, &nf) {
			panic(fmt.Sprintf("want NotFoundException after delete, got %v", err))
		}
		expectStatus(err, 404)
		// idempotent: deleting an absent policy is NOT an error
		_, err = client.DeleteVectorBucketPolicy(ctx, &s3vectors.DeleteVectorBucketPolicyInput{
			VectorBucketName: aws.String(bucket),
		})
		must(err)
		return "delete -> get 404; re-delete 200 (idempotent)"
	})

	step(15, "tag_resource (bucket + index ARN) -> list_tags_for_resource", func() string {
		_, err := client.TagResource(ctx, &s3vectors.TagResourceInput{
			ResourceArn: aws.String(createdBucketArn),
			Tags:        map[string]string{"env": "dev", "team": "infra"},
		})
		must(err)
		_, err = client.TagResource(ctx, &s3vectors.TagResourceInput{
			ResourceArn: aws.String(createdIndexArn),
			Tags:        map[string]string{"idx": "1", "sp ace+x/y:z": "punct"},
		})
		must(err)
		btResp, err := client.ListTagsForResource(ctx, &s3vectors.ListTagsForResourceInput{
			ResourceArn: aws.String(createdBucketArn)})
		must(err)
		itResp, err := client.ListTagsForResource(ctx, &s3vectors.ListTagsForResourceInput{
			ResourceArn: aws.String(createdIndexArn)})
		must(err)
		if !reflect.DeepEqual(btResp.Tags, map[string]string{"env": "dev", "team": "infra"}) {
			panic(fmt.Sprintf("bucket tags: %v", btResp.Tags))
		}
		if !reflect.DeepEqual(itResp.Tags, map[string]string{"idx": "1", "sp ace+x/y:z": "punct"}) {
			panic(fmt.Sprintf("index tags: %v", itResp.Tags))
		}
		// merge semantics: second put keeps unrelated keys
		_, err = client.TagResource(ctx, &s3vectors.TagResourceInput{
			ResourceArn: aws.String(createdBucketArn),
			Tags:        map[string]string{"env": "prod"},
		})
		must(err)
		bt2, err := client.ListTagsForResource(ctx, &s3vectors.ListTagsForResourceInput{
			ResourceArn: aws.String(createdBucketArn)})
		must(err)
		if !reflect.DeepEqual(bt2.Tags, map[string]string{"env": "prod", "team": "infra"}) {
			panic(fmt.Sprintf("merge semantics broken: %v", bt2.Tags))
		}
		return fmt.Sprintf("bucket tags=%v index tags=%v", bt2.Tags, itResp.Tags)
	})

	step(16, "untag_resource (repeated tagKeys query, encoded values)", func() string {
		// untag: plain key + punctuation-heavy key (the Go SDK query-encodes
		// values with '+' for spaces — both wire forms round-trip)
		_, err := client.UntagResource(ctx, &s3vectors.UntagResourceInput{
			ResourceArn: aws.String(createdIndexArn),
			TagKeys:     []string{"idx", "sp ace+x/y:z"},
		})
		must(err)
		it, err := client.ListTagsForResource(ctx, &s3vectors.ListTagsForResourceInput{
			ResourceArn: aws.String(createdIndexArn)})
		must(err)
		if len(it.Tags) != 0 {
			panic(fmt.Sprintf("untag leftover: %v", it.Tags))
		}
		// untagging absent keys again is idempotent (200)
		_, err = client.UntagResource(ctx, &s3vectors.UntagResourceInput{
			ResourceArn: aws.String(createdIndexArn),
			TagKeys:     []string{"idx"},
		})
		must(err)
		// bucket tags untouched by index untagging
		bt, err := client.ListTagsForResource(ctx, &s3vectors.ListTagsForResourceInput{
			ResourceArn: aws.String(createdBucketArn)})
		must(err)
		if !reflect.DeepEqual(bt.Tags, map[string]string{"env": "prod", "team": "infra"}) {
			panic(fmt.Sprintf("bucket scope leaked: %v", bt.Tags))
		}
		return "untag plain + punctuation keys (idempotent); bucket scope isolated"
	})

	step(17, "put_default_index_mode + update_index_mode (echo-only)", func() string {
		_, err := client.PutVectorBucketDefaultIndexMode(ctx, &s3vectors.PutVectorBucketDefaultIndexModeInput{
			VectorBucketName: aws.String(bucket),
			DefaultIndexMode: types.IndexModeClassic,
		})
		must(err)
		vb, err := client.GetVectorBucket(ctx, &s3vectors.GetVectorBucketInput{
			VectorBucketName: aws.String(bucket)})
		must(err)
		if vb.VectorBucket.DefaultIndexMode != types.IndexModeClassic {
			panic(fmt.Sprintf("defaultIndexMode=%q", vb.VectorBucket.DefaultIndexMode))
		}
		// NEW index inherits the (echo-only) mode; existing one is untouched
		index2 = *prefix + "-i2"
		_, err = client.CreateIndex(ctx, &s3vectors.CreateIndexInput{
			VectorBucketName: aws.String(bucket),
			IndexName:        aws.String(index2),
			DataType:         types.DataTypeFloat32,
			Dimension:        aws.Int32(2),
			DistanceMetric:   types.DistanceMetricEuclidean,
		})
		must(err)
		ix2, err := client.GetIndex(ctx, &s3vectors.GetIndexInput{
			VectorBucketName: aws.String(bucket), IndexName: aws.String(index2)})
		must(err)
		if ix2.Index.IndexMode != types.IndexModeClassic {
			panic(fmt.Sprintf("indexMode=%q (want inherited CLASSIC)", ix2.Index.IndexMode))
		}
		_, err = client.UpdateIndexMode(ctx, &s3vectors.UpdateIndexModeInput{
			IndexArn:  aws.String(createdBucketArn + "/index/" + index2),
			IndexMode: types.IndexModeEnhanced,
		})
		must(err)
		ix2b, err := client.GetIndex(ctx, &s3vectors.GetIndexInput{
			VectorBucketName: aws.String(bucket), IndexName: aws.String(index2)})
		must(err)
		if ix2b.Index.IndexMode != types.IndexModeEnhanced {
			panic(fmt.Sprintf("indexMode=%q (want ENHANCED)", ix2b.Index.IndexMode))
		}
		// queries still work identically under both stored modes (§4.4 echo-only)
		resp, err := client.QueryVectors(ctx, &s3vectors.QueryVectorsInput{
			VectorBucketName: aws.String(bucket),
			IndexName:        aws.String(index),
			TopK:             aws.Int32(1),
			QueryVector:      f32(1, 0, 0, 0),
		})
		must(err)
		if len(resp.Vectors) != 1 {
			panic(fmt.Sprintf("topK=1 returned %d", len(resp.Vectors)))
		}
		return "default CLASSIC -> inherited; UpdateIndexMode -> ENHANCED; query unaffected"
	})

	step(18, "errors: 404 gets, 409 delete-non-empty", func() string {
		_, err := client.GetVectorBucket(ctx, &s3vectors.GetVectorBucketInput{
			VectorBucketName: aws.String(*prefix + "-ghost"),
		})
		var nf *types.NotFoundException
		if !errors.As(err, &nf) {
			panic(fmt.Sprintf("ghost bucket: want NotFoundException, got %v", err))
		}
		expectStatus(err, 404)
		// non-empty bucket refuses to delete (409, S3 convention)
		_, err = client.DeleteVectorBucket(ctx, &s3vectors.DeleteVectorBucketInput{
			VectorBucketName: aws.String(bucket),
		})
		var ce *types.ConflictException
		if !errors.As(err, &ce) {
			panic(fmt.Sprintf("delete-non-empty: want ConflictException, got %v", err))
		}
		expectStatus(err, 409)
		obs = append(obs, fmt.Sprintf("delete-nonempty: ErrorCode=%q http=%d", ce.ErrorCode(), httpStatus(err)))
		_, err = client.DeleteIndex(ctx, &s3vectors.DeleteIndexInput{
			VectorBucketName: aws.String(bucket), IndexName: aws.String(*prefix + "-nope"),
		})
		if !errors.As(err, &nf) {
			panic(fmt.Sprintf("ghost index: want NotFoundException, got %v", err))
		}
		return "ghost bucket 404; non-empty delete 409; ghost index delete 404"
	})

	// ---------------------------------------------------------------------
	// vector data plane (steps 19-25): GetVectors roundtrip + missing-key
	// omission, ListVectors pagination + parallel segments, DeleteVectors +
	// verification, and PutVectors metadata-limit negatives.
	// ---------------------------------------------------------------------

	step(19, "get_vectors: roundtrip, missing key omitted, default members absent", func() string {
		// grow the key space: v00..v09 plus a punctuation/unicode key
		vectors := make([]types.PutInputVector, 0, 11)
		for i := 0; i < 10; i++ {
			vectors = append(vectors, types.PutInputVector{
				Key:  aws.String(fmt.Sprintf("%s-v%02d", *prefix, i)),
				Data: f32(float32(i), 1, 0, 0),
			})
		}
		hostile := "sp\"ace'/🎯\u00fc"
		vectors = append(vectors, types.PutInputVector{
			Key:      aws.String(hostile),
			Data:     f32(9, 9, 0, 0),
			Metadata: s3vdoc.NewLazyDocument(map[string]any{"kind": "hostile"}),
		})
		_, err := client.PutVectors(ctx, &s3vectors.PutVectorsInput{
			VectorBucketName: aws.String(bucket), IndexName: aws.String(index), Vectors: vectors,
		})
		must(err)
		// every key now in the index: a, b, c (core section) + the 11 new ones
		all := []string{"a", "b", "c"}
		for _, v := range vectors {
			all = append(all, *v.Key)
		}
		vecKeys = sortedCopy(all)
		// get roundtrip: found keys in request order with data+metadata, the
		// missing key OMITTED (the model has no per-entry status member)
		resp, err := client.GetVectors(ctx, &s3vectors.GetVectorsInput{
			VectorBucketName: aws.String(bucket), IndexName: aws.String(index),
			Keys:           []string{hostile, *prefix + "-ghost", "a", "b"},
			ReturnData:     true,
			ReturnMetadata: true,
		})
		must(err)
		obs = append(obs, fmt.Sprintf("GetVectors entry populated members: %v", outputMembers(resp.Vectors[0])))
		keys := make([]string, 0, len(resp.Vectors))
		for _, v := range resp.Vectors {
			keys = append(keys, aws.ToString(v.Key))
		}
		eqStrs(keys, hostile, "a", "b")
		for _, v := range resp.Vectors {
			switch aws.ToString(v.Key) {
			case "a":
				if got := f32Of(v.Data); !reflect.DeepEqual(got, []float32{0, 1, 0, 0}) {
					panic(fmt.Sprintf("a data: %v", got))
				}
				if got := norm(docMap(v.Metadata)); !reflect.DeepEqual(got,
					map[string]any{"tag": "a-new", "note": "replaced"}) {
					panic(fmt.Sprintf("a metadata: %#v", got))
				}
			case "b":
				if v.Metadata != nil {
					panic(fmt.Sprintf("b metadata should be absent: %#v", docMap(v.Metadata)))
				}
				if got := f32Of(v.Data); !reflect.DeepEqual(got, []float32{0, 0, 0, 1}) {
					panic(fmt.Sprintf("b data: %v", got))
				}
			case hostile:
				if got := norm(docMap(v.Metadata)); !reflect.DeepEqual(got,
					map[string]any{"kind": "hostile"}) {
					panic(fmt.Sprintf("hostile metadata: %#v", got))
				}
			}
		}
		// defaults: neither member present (Go 口径: typed fields left nil)
		plain, err := client.GetVectors(ctx, &s3vectors.GetVectorsInput{
			VectorBucketName: aws.String(bucket), IndexName: aws.String(index),
			Keys: []string{"a"},
		})
		must(err)
		p := plain.Vectors[0]
		if aws.ToString(p.Key) != "a" || p.Data != nil || p.Metadata != nil {
			panic(fmt.Sprintf("default get: key=%q data=%v metadata=%v", aws.ToString(p.Key), p.Data, p.Metadata))
		}
		return fmt.Sprintf("%d keys stocked; order+omission+defaults ok", len(vecKeys))
	})

	listKeys := func(resp *s3vectors.ListVectorsOutput) []string {
		keys := make([]string, 0, len(resp.Vectors))
		for _, v := range resp.Vectors {
			keys = append(keys, aws.ToString(v.Key))
		}
		return keys
	}

	step(20, "list_vectors page 1 (maxResults=2, key-ascending)", func() string {
		// page 1: maxResults=2 -> 2 sorted keys + nextToken
		resp, err := client.ListVectors(ctx, &s3vectors.ListVectorsInput{
			VectorBucketName: aws.String(bucket), IndexName: aws.String(index),
			MaxResults: aws.Int32(2),
		})
		must(err)
		keys := listKeys(resp)
		obs = append(obs, fmt.Sprintf("ListVectors entry populated members: %v", outputMembers(resp.Vectors[0])))
		eqStrs(keys, vecKeys[0], vecKeys[1])
		if resp.NextToken == nil || *resp.NextToken == "" {
			panic(fmt.Sprintf("missing nextToken: %+v", resp))
		}
		listToken = *resp.NextToken
		// default list carries only key (Go 口径: Data/Metadata nil)
		if resp.Vectors[0].Data != nil || resp.Vectors[0].Metadata != nil {
			panic("default list carries more than key")
		}
		return fmt.Sprintf("page1=%v", keys)
	})

	step(21, "list_vectors: paginate to end, order + returnData/Metadata", func() string {
		// walk to the end through nextToken; union must be every key, sorted
		seen := append([]string(nil), vecKeys[:2]...)
		token, pages := listToken, 1
		for token != "" {
			resp, err := client.ListVectors(ctx, &s3vectors.ListVectorsInput{
				VectorBucketName: aws.String(bucket), IndexName: aws.String(index),
				MaxResults: aws.Int32(2), NextToken: aws.String(token),
			})
			must(err)
			page := listKeys(resp)
			if !sort.StringsAreSorted(page) {
				panic(fmt.Sprintf("page not sorted: %v", page))
			}
			seen = append(seen, page...)
			if resp.NextToken != nil {
				token = *resp.NextToken
			} else {
				token = ""
			}
			pages++
			if pages >= 30 {
				panic("pagination did not terminate")
			}
		}
		if !reflect.DeepEqual(seen, vecKeys) {
			panic(fmt.Sprintf("union=%v want=%v", seen, vecKeys))
		}
		// returnData/returnMetadata members come back when asked for
		// (first key ascending is "a" — its current row is step 6's upsert)
		resp, err := client.ListVectors(ctx, &s3vectors.ListVectorsInput{
			VectorBucketName: aws.String(bucket), IndexName: aws.String(index),
			MaxResults: aws.Int32(1), ReturnData: true, ReturnMetadata: true,
		})
		must(err)
		v0 := resp.Vectors[0]
		if aws.ToString(v0.Key) != "a" {
			panic(fmt.Sprintf("first key: %q", aws.ToString(v0.Key)))
		}
		if got := f32Of(v0.Data); !reflect.DeepEqual(got, []float32{0, 1, 0, 0}) {
			panic(fmt.Sprintf("a data: %v", got))
		}
		if got := norm(docMap(v0.Metadata)); !reflect.DeepEqual(got,
			map[string]any{"tag": "a-new", "note": "replaced"}) {
			panic(fmt.Sprintf("a metadata: %#v", got))
		}
		return fmt.Sprintf("%d pages, %d keys, data+metadata members verified", pages, len(seen))
	})

	step(22, "list_vectors segments: union=full, disjoint, stable", func() string {
		// parallel segments: segmentCount=2, both workers paginate their own
		// segment; union == full set, disjoint, and a repeat call is identical
		seg := func(segIdx int32) []string {
			var out []string
			token := ""
			for {
				in := &s3vectors.ListVectorsInput{
					VectorBucketName: aws.String(bucket), IndexName: aws.String(index),
					SegmentCount: aws.Int32(2), SegmentIndex: segIdx,
					MaxResults: aws.Int32(3),
				}
				if token != "" {
					in.NextToken = aws.String(token)
				}
				// withSegmentIndexZero: segIdx==0 is a zero value the typed
				// serializer would drop (see package doc)
				resp, err := client.ListVectors(ctx, in, withSegmentIndexZero)
				must(err)
				out = append(out, listKeys(resp)...)
				if resp.NextToken != nil {
					token = *resp.NextToken
				} else {
					return out
				}
			}
		}
		seg0, seg1 := seg(0), seg(1)
		if got := sortedCopy(append(append([]string(nil), seg0...), seg1...)); !reflect.DeepEqual(got, vecKeys) {
			panic(fmt.Sprintf("union != full set: %v", got))
		}
		set := map[string]bool{}
		for _, k := range seg0 {
			set[k] = true
		}
		for _, k := range seg1 {
			if set[k] {
				panic(fmt.Sprintf("segments overlap at %q", k))
			}
		}
		if !reflect.DeepEqual(seg(0), seg0) {
			panic("segment assignment is not stable")
		}
		return fmt.Sprintf("seg0=%d seg1=%d union=%d disjoint+stable", len(seg0), len(seg1), len(seg0)+len(seg1))
	})

	step(23, "delete_vectors: special chars, idempotent, verified gone, rebuild", func() string {
		// delete the hostile key + b; deletion is idempotent and visible
		hostile := "sp\"ace'/🎯\u00fc"
		_, err := client.DeleteVectors(ctx, &s3vectors.DeleteVectorsInput{
			VectorBucketName: aws.String(bucket), IndexName: aws.String(index),
			Keys: []string{hostile, "b", *prefix + "-ghost"},
		})
		must(err)
		_, err = client.DeleteVectors(ctx, &s3vectors.DeleteVectorsInput{ // idempotent repeat
			VectorBucketName: aws.String(bucket), IndexName: aws.String(index),
			Keys: []string{hostile},
		})
		must(err)
		resp, err := client.GetVectors(ctx, &s3vectors.GetVectorsInput{
			VectorBucketName: aws.String(bucket), IndexName: aws.String(index),
			Keys: []string{hostile, "b", "c"},
		})
		must(err)
		getKeys := make([]string, 0, len(resp.Vectors))
		for _, v := range resp.Vectors {
			getKeys = append(getKeys, aws.ToString(v.Key))
		}
		eqStrs(getKeys, "c")
		ls, err := client.ListVectors(ctx, &s3vectors.ListVectorsInput{
			VectorBucketName: aws.String(bucket), IndexName: aws.String(index),
			MaxResults: aws.Int32(1000),
		})
		must(err)
		vecKeys = listKeys(ls)
		for _, k := range vecKeys {
			if k == hostile || k == "b" {
				panic(fmt.Sprintf("deleted key %q still listed", k))
			}
		}
		// delete -> put rebuilds the row normally
		_, err = client.PutVectors(ctx, &s3vectors.PutVectorsInput{
			VectorBucketName: aws.String(bucket), IndexName: aws.String(index),
			Vectors: []types.PutInputVector{{
				Key:      aws.String(hostile),
				Data:     f32(1, 2, 3, 4),
				Metadata: s3vdoc.NewLazyDocument(map[string]any{"round": 2}),
			}},
		})
		must(err)
		back, err := client.GetVectors(ctx, &s3vectors.GetVectorsInput{
			VectorBucketName: aws.String(bucket), IndexName: aws.String(index),
			Keys: []string{hostile}, ReturnData: true, ReturnMetadata: true,
		})
		must(err)
		if len(back.Vectors) != 1 {
			panic(fmt.Sprintf("rebuild not visible: %+v", back.Vectors))
		}
		if got := norm(docMap(back.Vectors[0].Metadata)); !reflect.DeepEqual(got, map[string]any{"round": float64(2)}) {
			panic(fmt.Sprintf("rebuilt metadata: %#v", got))
		}
		_, err = client.DeleteVectors(ctx, &s3vectors.DeleteVectorsInput{
			VectorBucketName: aws.String(bucket), IndexName: aws.String(index),
			Keys: []string{hostile},
		})
		must(err)
		return "deleted b+hostile (ghost no-op); rebuilt+redeleted hostile"
	})

	step(24, "put_vectors metadata >2KB filterable -> ValidationException", func() string {
		// filterable metadata > 2 KB -> server-side ValidationException
		_, err := client.PutVectors(ctx, &s3vectors.PutVectorsInput{
			VectorBucketName: aws.String(bucket), IndexName: aws.String(index),
			Vectors: []types.PutInputVector{{
				Key:      aws.String("big"),
				Data:     f32(1, 0, 0, 0),
				Metadata: s3vdoc.NewLazyDocument(map[string]any{"s": strings.Repeat("v", 2100)}),
			}},
		})
		var ve *types.ValidationException
		if !errors.As(err, &ve) {
			panic(fmt.Sprintf("want ValidationException, got %v", err))
		}
		expectStatus(err, 400)
		obs = append(obs, fmt.Sprintf("metadata-2kb: %s", truncate(aws.ToString(ve.Message), 90)))
		return "filterable 2.1 KB -> ValidationException 400"
	})

	step(25, "put_vectors metadata type validation (nested rejected, list ok)", func() string {
		// nested object metadata value -> ValidationException naming legal types
		_, err := client.PutVectors(ctx, &s3vectors.PutVectorsInput{
			VectorBucketName: aws.String(bucket), IndexName: aws.String(index),
			Vectors: []types.PutInputVector{{
				Key:      aws.String("nest"),
				Data:     f32(1, 0, 0, 0),
				Metadata: s3vdoc.NewLazyDocument(map[string]any{"nested": map[string]any{"deep": true}}),
			}},
		})
		var ve *types.ValidationException
		if !errors.As(err, &ve) {
			panic(fmt.Sprintf("want ValidationException, got %v", err))
		}
		expectStatus(err, 400)
		msg := aws.ToString(ve.Message)
		if !strings.Contains(msg, "string, number, boolean") {
			panic(fmt.Sprintf("message does not name legal types: %q", msg))
		}
		// flat list of primitives is legal and round-trips
		_, err = client.PutVectors(ctx, &s3vectors.PutVectorsInput{
			VectorBucketName: aws.String(bucket), IndexName: aws.String(index),
			Vectors: []types.PutInputVector{{
				Key:      aws.String("listy"),
				Data:     f32(0, 1, 0, 0),
				Metadata: s3vdoc.NewLazyDocument(map[string]any{"l": []any{"a", 2, true}}),
			}},
		})
		must(err)
		got, err := client.GetVectors(ctx, &s3vectors.GetVectorsInput{
			VectorBucketName: aws.String(bucket), IndexName: aws.String(index),
			Keys: []string{"listy"}, ReturnMetadata: true,
		})
		must(err)
		want := map[string]any{"l": []any{"a", float64(2), true}}
		if got := norm(docMap(got.Vectors[0].Metadata)); !reflect.DeepEqual(got, want) {
			panic(fmt.Sprintf("list metadata: got %#v want %#v", got, want))
		}
		return fmt.Sprintf("nested rejected (%s…); flat list round-trips", truncate(msg, 60))
	})

	// ---------------------------------------------------------------------
	// filter engine (steps 26-31): the full AWS operator set driven through
	// the SDK's typed `filter` document parameter against a stocked index —
	// eq / and / range / in / startsWith / exists, each asserting the exact
	// key set (plus any-element list semantics where they land).
	// ---------------------------------------------------------------------

	step(26, "filters: seed metadata-rich vectors", func() string {
		// dedicated index so the assertion universe is exactly these 6
		// vectors (the main index carries earlier steps' rows)
		filterIndex = *prefix + "-if"
		_, err := client.CreateIndex(ctx, &s3vectors.CreateIndexInput{
			VectorBucketName: aws.String(bucket),
			IndexName:        aws.String(filterIndex),
			DataType:         types.DataTypeFloat32,
			Dimension:        aws.Int32(4),
			DistanceMetric:   types.DistanceMetricCosine,
		})
		must(err)
		// 6 vectors with rich filterable metadata: numbers, strings, bools,
		// a list, and one row with NO metadata (missing-field semantics)
		_, err = client.PutVectors(ctx, &s3vectors.PutVectorsInput{
			VectorBucketName: aws.String(bucket), IndexName: aws.String(filterIndex),
			Vectors: []types.PutInputVector{
				{Key: aws.String("f1"), Data: f32(1, 0, 0, 0),
					Metadata: s3vdoc.NewLazyDocument(map[string]any{"genre": "documentary", "year": 2019, "ok": true})},
				{Key: aws.String("f2"), Data: f32(0.9, 0.1, 0, 0),
					Metadata: s3vdoc.NewLazyDocument(map[string]any{"genre": "comedy", "year": 2021, "ok": false})},
				{Key: aws.String("f3"), Data: f32(0.8, 0.2, 0, 0),
					Metadata: s3vdoc.NewLazyDocument(map[string]any{"genre": "documentary", "year": 2022, "ok": true, "tags": []any{"a", "b"}})},
				{Key: aws.String("f4"), Data: f32(0.7, 0.3, 0, 0),
					Metadata: s3vdoc.NewLazyDocument(map[string]any{"genre": "drama", "year": 2020})},
				{Key: aws.String("f5"), Data: f32(0.6, 0.4, 0, 0),
					Metadata: s3vdoc.NewLazyDocument(map[string]any{"s3_path": "/marketing/report.pdf"})},
				{Key: aws.String("f6"), Data: f32(0.5, 0.5, 0, 0)},
			},
		})
		must(err)
		return "6 vectors (scalars x3 kinds, list, one without metadata)"
	})

	// fkeys runs one filtered query and returns the sorted result keys.
	fkeys := func(flt any) []string {
		resp, err := client.QueryVectors(ctx, &s3vectors.QueryVectorsInput{
			VectorBucketName: aws.String(bucket),
			IndexName:        aws.String(filterIndex),
			TopK:             aws.Int32(10),
			QueryVector:      f32(1, 0, 0, 0),
			Filter:           s3vdoc.NewLazyDocument(flt),
		})
		must(err)
		if resp.DistanceMetric != types.DistanceMetricCosine {
			panic(fmt.Sprintf("distanceMetric=%s", resp.DistanceMetric))
		}
		keys := make([]string, 0, len(resp.Vectors))
		for _, v := range resp.Vectors {
			keys = append(keys, aws.ToString(v.Key))
		}
		sort.Strings(keys)
		return keys
	}

	step(27, "filter $eq (string/number, mismatch & missing = no match)", func() string {
		eq := func(flt any, want ...string) { eqStrs(fkeys(flt), want...) }
		// $eq: shorthand + explicit + a miss (type mismatch = no match)
		eq(map[string]any{"genre": "documentary"}, "f1", "f3")
		eq(map[string]any{"genre": map[string]any{"$eq": "comedy"}}, "f2")
		eq(map[string]any{"year": map[string]any{"$eq": 2020}}, "f4")
		eq(map[string]any{"year": "2020"}) // string-on-number must not match
		eq(map[string]any{"ghost": 1})     // missing field must not match
		return "eq shorthand/explicit/miss + mismatch + missing"
	})

	step(28, "filter $and/$or/$ne (implicit AND, nesting)", func() string {
		eq := func(flt any, want ...string) { eqStrs(fkeys(flt), want...) }
		// $and + implicit top-level AND + $ne boundary semantics
		eq(map[string]any{"$and": []any{
			map[string]any{"genre": map[string]any{"$eq": "documentary"}},
			map[string]any{"year": map[string]any{"$gte": 2020}},
		}}, "f3")
		eq(map[string]any{"genre": "documentary", "ok": true}, "f1", "f3")
		// $ne matches rows WITHOUT the field too (f6 has no metadata)
		eq(map[string]any{"genre": map[string]any{"$ne": "documentary"}}, "f2", "f4", "f5", "f6")
		// nested: $or inside $and
		eq(map[string]any{"$and": []any{
			map[string]any{"$or": []any{
				map[string]any{"genre": "comedy"},
				map[string]any{"year": 2019},
			}},
		}}, "f1", "f2")
		return "and/or nesting + ne matches missing"
	})

	step(29, "filter range $gt/$gte/$lt/$lte", func() string {
		eq := func(flt any, want ...string) { eqStrs(fkeys(flt), want...) }
		// numeric range incl. same-field multi-operator AND
		eq(map[string]any{"year": map[string]any{"$gt": 2020}}, "f2", "f3")
		eq(map[string]any{"year": map[string]any{"$gte": 2020}}, "f2", "f3", "f4")
		eq(map[string]any{"year": map[string]any{"$lt": 2020}}, "f1")
		eq(map[string]any{"year": map[string]any{"$lte": 2020}}, "f1", "f4")
		eq(map[string]any{"year": map[string]any{"$gte": 2019, "$lt": 2021}}, "f1", "f4")
		return "range gt/gte/lt/lte + same-field band"
	})

	step(30, "filter $in/$nin (+ list any-element)", func() string {
		eq := func(flt any, want ...string) { eqStrs(fkeys(flt), want...) }
		// $in with primitives + any-element against list metadata
		eq(map[string]any{"genre": map[string]any{"$in": []any{"comedy", "drama"}}}, "f2", "f4")
		eq(map[string]any{"year": map[string]any{"$in": []any{2019, 2022}}}, "f1", "f3")
		eq(map[string]any{"tags": map[string]any{"$in": []any{"b", "z"}}}, "f3") // list any-element
		eq(map[string]any{"tags": map[string]any{"$nin": []any{"b", "z"}}}, "f1", "f2", "f4", "f5", "f6")
		return "in/nin (mixed filter list + list-metadata any-element)"
	})

	step(31, "filter $startsWith/$exists", func() string {
		eq := func(flt any, want ...string) { eqStrs(fkeys(flt), want...) }
		// $startsWith (string) + $exists (bool), including missing-field rows
		eq(map[string]any{"s3_path": map[string]any{"$startsWith": "/marketing/"}}, "f5")
		eq(map[string]any{"s3_path": map[string]any{"$startsWith": "/sales/"}})
		eq(map[string]any{"genre": map[string]any{"$startsWith": "do"}}, "f1", "f3")
		eq(map[string]any{"genre": map[string]any{"$exists": true}}, "f1", "f2", "f3", "f4")
		// rows with NO metadata at all satisfy $exists:false
		eq(map[string]any{"genre": map[string]any{"$exists": false}}, "f5", "f6")
		return "startsWith + exists true/false (missing rows)"
	})

	step(32, "cleanup: delete indexes, buckets; verify prefix drained", func() string {
		for _, ix := range []string{index, index2, filterIndex} {
			_, err := client.DeleteIndex(ctx, &s3vectors.DeleteIndexInput{
				VectorBucketName: aws.String(bucket), IndexName: aws.String(ix),
			})
			must(err)
		}
		_, err := client.DeleteVectorBucket(ctx, &s3vectors.DeleteVectorBucketInput{
			VectorBucketName: aws.String(bucket),
		})
		must(err)
		for _, extra := range extraBuckets {
			_, err = client.DeleteVectorBucket(ctx, &s3vectors.DeleteVectorBucketInput{
				VectorBucketName: aws.String(extra),
			})
			must(err)
		}
		_, err = client.GetVectorBucket(ctx, &s3vectors.GetVectorBucketInput{
			VectorBucketName: aws.String(bucket),
		})
		var nf *types.NotFoundException
		if !errors.As(err, &nf) {
			panic(fmt.Sprintf("bucket still resolvable after delete: %v", err))
		}
		expectStatus(err, 404)
		rest, err := client.ListVectorBuckets(ctx, &s3vectors.ListVectorBucketsInput{
			MaxResults: aws.Int32(500), Prefix: aws.String(*prefix + "-"),
		})
		must(err)
		if len(rest.VectorBuckets) != 0 {
			names := make([]string, 0, len(rest.VectorBuckets))
			for _, b := range rest.VectorBuckets {
				names = append(names, aws.ToString(b.VectorBucketName))
			}
			panic(fmt.Sprintf("cleanup left buckets: %v", names))
		}
		return "index x2 + bucket x3 deleted; prefix drained"
	})

	fmt.Println("\n# wire observations (Go SDK side):")
	for _, o := range obs {
		fmt.Printf("#   - %s\n", o)
	}
	if failed > 0 {
		fmt.Fprintf(os.Stderr, "\nRESULT: %d step(s) FAILED\n", failed)
		os.Exit(1)
	}
	fmt.Printf("\nRESULT: all %d steps PASS\n", totalSteps)
}

// truncate clips s to n runes-ish for observation lines (byte-safe enough for
// ASCII error messages; a multibyte cut mid-rune just prints mojibake).
func truncate(s string, n int) string {
	if len(s) <= n {
		return s
	}
	return s[:n] + "…"
}
