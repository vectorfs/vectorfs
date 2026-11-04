// aws-sdk-go-v2 conformance check against an s3vectors-compatible server
// (M0 Task C; moved to repo-root conformance/ 2026-11-04). Talks to an
// already-running server; owns no server lifecycle.
// Steps 1-9 mirror conformance/boto3_check.py. Any FAIL -> exit 1.
//
// Usage:
//
//	go run ./conformance/go --endpoint http://127.0.0.1:8787 \
//	    --access-key test --secret-key testsecret [--region us-east-1] [--prefix XXX]
package main

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"errors"
	"flag"
	"fmt"
	"math"
	"os"
	"reflect"
	"strconv"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/config"
	"github.com/aws/aws-sdk-go-v2/credentials"
	"github.com/aws/aws-sdk-go-v2/service/s3vectors"
	s3vdoc "github.com/aws/aws-sdk-go-v2/service/s3vectors/document"
	"github.com/aws/aws-sdk-go-v2/service/s3vectors/types"
	smithydoc "github.com/aws/smithy-go/document"
)

var (
	failed int
	obs    []string // wire observations printed in the summary
)

// step prints PASS/FAIL per step; assertions inside fn use panic, which is
// recovered here so later steps still run and the exit code reflects failures.
func step(n int, name string, fn func() string) {
	fmt.Printf("[%d/9] %s ... ", n, name)
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
	)

	// Record which members the typed output actually carries (model says
	// ARN-only; spike double-emits nested objects — unknown members must be
	// silently ignored, which is itself a wire observation).
	outputMembers := func(v any) []string {
		var out []string
		rv := reflect.ValueOf(v)
		for i := 0; i < rv.NumField(); i++ {
			if rv.Type().Field(i).IsExported() && !rv.Field(i).IsZero() {
				out = append(out, rv.Type().Field(i).Name)
			}
		}
		return out
	}

	step(1, "CreateVectorBucket -> VectorBucketArn", func() string {
		resp, err := client.CreateVectorBucket(ctx, &s3vectors.CreateVectorBucketInput{
			VectorBucketName: aws.String(bucket),
		})
		if err != nil {
			panic(err)
		}
		if resp.VectorBucketArn == nil {
			panic("missing VectorBucketArn; parsed members: " + fmt.Sprint(outputMembers(*resp)))
		}
		createdBucketArn = *resp.VectorBucketArn
		obs = append(obs, fmt.Sprintf("CreateVectorBucketOutput populated members: %v (double-emitted vectorBucket{creationTime:epoch-float} ignored without error)", outputMembers(*resp)))
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
		obs = append(obs, fmt.Sprintf("conflict: errors.As ConflictException ok, ErrorCode=%q Message=%q", ce.ErrorCode(), aws.ToString(ce.Message)))
		return fmt.Sprintf("ErrorCode=%q", ce.ErrorCode())
	})

	step(3, "CreateIndex(float32/4/cosine) -> IndexArn", func() string {
		resp, err := client.CreateIndex(ctx, &s3vectors.CreateIndexInput{
			VectorBucketName: aws.String(bucket),
			IndexName:        aws.String(index),
			DataType:         types.DataTypeFloat32,
			Dimension:        aws.Int32(4),
			DistanceMetric:   types.DistanceMetricCosine,
		})
		if err != nil {
			panic(err)
		}
		if resp.IndexArn == nil {
			panic("missing IndexArn")
		}
		createdIndexArn = *resp.IndexArn
		return fmt.Sprintf("arn=%s members=%v", createdIndexArn, outputMembers(*resp))
	})

	f32 := func(xs ...float32) types.VectorData {
		return &types.VectorDataMemberFloat32{Value: xs}
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
		if err != nil {
			panic(err)
		}
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
		if err != nil {
			panic(err)
		}
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
		if err != nil {
			panic(err)
		}
		resp, err := client.QueryVectors(ctx, &s3vectors.QueryVectorsInput{
			VectorBucketName: aws.String(bucket),
			IndexName:        aws.String(index),
			TopK:             aws.Int32(3),
			QueryVector:      f32(0, 1, 0, 0),
			ReturnDistance:   true,
			ReturnMetadata:   true,
		})
		if err != nil {
			panic(err)
		}
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
		if err != nil {
			panic(err)
		}
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
		obs = append(obs, fmt.Sprintf("notfound: errors.As NotFoundException ok, ErrorCode=%q", nf.ErrorCode()))
		return fmt.Sprintf("ErrorCode=%q", nf.ErrorCode())
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
		obs = append(obs, fmt.Sprintf("validation: errors.As ValidationException ok, ErrorCode=%q Message=%q", ve.ErrorCode(), aws.ToString(ve.Message)))
		return fmt.Sprintf("ErrorCode=%q", ve.ErrorCode())
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
		obs = append(obs, fmt.Sprintf("denied: errors.As AccessDeniedException ok, ErrorCode=%q Message=%q", ad.ErrorCode(), aws.ToString(ad.Message)))
		return fmt.Sprintf("ErrorCode=%q", ad.ErrorCode())
	})

	fmt.Println("\n# wire observations (Go SDK side):")
	for _, o := range obs {
		fmt.Printf("#   - %s\n", o)
	}
	// step 10: no cleanup — spike implements no delete operations.
	if failed > 0 {
		fmt.Fprintf(os.Stderr, "\nRESULT: %d step(s) FAILED\n", failed)
		os.Exit(1)
	}
	fmt.Println("\nRESULT: all 9 steps PASS")
}
