package main

import (
	"bytes"
	"context"
	"fmt"
	"io"
	"net/http"
	"os"
	"testing"
	"time"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/service/s3"
)

// TestGameClipUploadLiveMinIO runs the game clip upload against the local
// MinIO of the dev stack: the signed lengths, the multipart complete, the
// HEAD check and the stale-upload abort. It needs MELLO_S3_LIVE=1.
func TestGameClipUploadLiveMinIO(t *testing.T) {
	if os.Getenv("MELLO_S3_LIVE") != "1" {
		t.Skip("set MELLO_S3_LIVE=1 with the local MinIO up")
	}
	const bucket = "mello-clips"
	c := newS3Client("http://localhost:9000", "minioadmin", "minioadmin")
	p := s3.NewPresignClient(c)
	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Minute)
	defer cancel()

	prefix := fmt.Sprintf("crews/live-test-%d/", time.Now().UnixNano())
	posterKey, videoKey := prefix+"g1.jpg", prefix+"g1.mp4"
	t.Cleanup(func() {
		for _, k := range []string{posterKey, videoKey} {
			if err := deleteObject(context.Background(), c, bucket, k); err != nil {
				t.Logf("cleanup %s: %v", k, err)
			}
		}
	})

	put := func(url string, body []byte, header map[string]string) (int, string) {
		t.Helper()
		req, err := http.NewRequestWithContext(ctx, http.MethodPut, url, bytes.NewReader(body))
		if err != nil {
			t.Fatal(err)
		}
		for k, v := range header {
			req.Header.Set(k, v)
		}
		resp, err := http.DefaultClient.Do(req)
		if err != nil {
			t.Fatalf("PUT: %v", err)
		}
		defer resp.Body.Close()
		_, _ = io.Copy(io.Discard, resp.Body)
		return resp.StatusCode, resp.Header.Get("ETag")
	}

	// The poster: one PUT with signed length, type and cache control.
	poster := bytes.Repeat([]byte{0xFF}, 2000)
	posterURL, err := presignPutSized(ctx, p, bucket, posterKey, gameClipPosterType, gameClipCacheControl, int64(len(poster)), gameClipURLExpiry)
	if err != nil {
		t.Fatal(err)
	}
	posterHeaders := map[string]string{"Content-Type": gameClipPosterType, "Cache-Control": gameClipCacheControl}
	if code, _ := put(posterURL, poster[:1999], posterHeaders); code < 400 || code >= 500 {
		t.Fatalf("poster with a wrong length: got %d, want 4xx", code)
	}
	if code, _ := put(posterURL, poster, map[string]string{"Content-Type": gameClipPosterType}); code < 400 || code >= 500 {
		t.Fatalf("poster without the signed Cache-Control: got %d, want 4xx", code)
	}
	if code, _ := put(posterURL, poster, posterHeaders); code != http.StatusOK {
		t.Fatalf("poster PUT: got %d, want 200", code)
	}

	// The video: two parts, the first at the 5 MiB part size.
	sizes, _ := gameClipPartPlan(gameClipPartSize + 1000)
	uploadID, err := createMultipartUpload(ctx, c, bucket, videoKey, gameClipVideoType, gameClipCacheControl)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		_, err := c.AbortMultipartUpload(context.Background(), &s3.AbortMultipartUploadInput{
			Bucket: aws.String(bucket), Key: aws.String(videoKey), UploadId: aws.String(uploadID),
		})
		if err != nil && s3ErrorCode(err) != "NoSuchUpload" {
			t.Logf("cleanup abort: %v", err)
		}
	})

	var completed []CompletedClipPart
	for i, n := range sizes {
		num := int32(i + 1)
		partURL, err := presignUploadPart(ctx, p, bucket, videoKey, uploadID, num, n, gameClipURLExpiry)
		if err != nil {
			t.Fatal(err)
		}
		body := bytes.Repeat([]byte{byte(num)}, int(n))
		if code, _ := put(partURL, body[:n-1], nil); code < 400 || code >= 500 {
			t.Fatalf("part %d short by one byte: got %d, want 4xx", num, code)
		}
		if code, _ := put(partURL, append(body, 0), nil); code < 400 || code >= 500 {
			t.Fatalf("part %d long by one byte: got %d, want 4xx", num, code)
		}
		code, etag := put(partURL, body, nil)
		if code != http.StatusOK || etag == "" {
			t.Fatalf("part %d PUT: got %d etag %q, want 200", num, code, etag)
		}
		completed = append(completed, CompletedClipPart{PartNumber: num, ETag: etag})
	}

	parts, errStr := completedClipParts(completed)
	if errStr != "" {
		t.Fatal(errStr)
	}
	if err := completeMultipartUpload(ctx, c, bucket, videoKey, uploadID, parts); err != nil {
		t.Fatal(err)
	}
	// A second complete is a lost-answer retry: NoSuchUpload, and the
	// object is there.
	if err := completeMultipartUpload(ctx, c, bucket, videoKey, uploadID, parts); s3ErrorCode(err) != "NoSuchUpload" {
		t.Fatalf("second complete: got %v (code %q), want NoSuchUpload", err, s3ErrorCode(err))
	}

	for _, o := range []struct {
		key, typ string
		size     int64
		max      int64
	}{
		{posterKey, gameClipPosterType, int64(len(poster)), gameClipMaxPosterBytes},
		{videoKey, gameClipVideoType, gameClipPartSize + 1000, gameClipMaxVideoBytes},
	} {
		info, found, err := headObject(ctx, c, bucket, o.key)
		if err != nil || !found {
			t.Fatalf("HEAD %s: found=%t err=%v", o.key, found, err)
		}
		if info.Size != o.size || info.ContentType != o.typ {
			t.Fatalf("HEAD %s: %+v, want %d bytes of %s", o.key, info, o.size, o.typ)
		}
		if errStr, _ := checkClipObject(info, o.max, o.typ); errStr != "" {
			t.Fatalf("check %s: %s", o.key, errStr)
		}
		out, err := c.HeadObject(ctx, &s3.HeadObjectInput{Bucket: aws.String(bucket), Key: aws.String(o.key)})
		if err != nil || aws.ToString(out.CacheControl) != gameClipCacheControl {
			t.Fatalf("HEAD %s Cache-Control: %q err=%v", o.key, aws.ToString(out.CacheControl), err)
		}
	}
	if _, found, err := headObject(ctx, c, bucket, prefix+"missing.mp4"); err != nil || found {
		t.Fatalf("HEAD of a missing key: found=%t err=%v, want not found", found, err)
	}

	// The cleanup job: an upload started before the cutoff is aborted. The
	// cutoff is in the future, so the upload just made counts as stale.
	staleKey := prefix + "stale.mp4"
	staleID, err := createMultipartUpload(ctx, c, bucket, staleKey, gameClipVideoType, gameClipCacheControl)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		_, err := c.AbortMultipartUpload(context.Background(), &s3.AbortMultipartUploadInput{
			Bucket: aws.String(bucket), Key: aws.String(staleKey), UploadId: aws.String(staleID),
		})
		if err != nil && s3ErrorCode(err) != "NoSuchUpload" {
			t.Logf("cleanup abort: %v", err)
		}
	})
	aborted, failed, err := abortStaleMultipartUploads(ctx, c, bucket, prefix, time.Now().Add(time.Minute))
	if err != nil || aborted != 1 || failed != 0 {
		t.Fatalf("abort: aborted=%d failed=%d err=%v, want 1 aborted", aborted, failed, err)
	}
	if left := uploadsUnder(ctx, t, c, bucket, prefix); len(left) != 0 {
		t.Fatalf("uploads left under the prefix: %v", left)
	}
}

// uploadsUnder lists the keys of the multipart uploads under prefix. The list
// has no prefix, because MinIO lists one exact key only.
func uploadsUnder(ctx context.Context, t *testing.T, c *s3.Client, bucket, prefix string) []string {
	t.Helper()
	out, err := c.ListMultipartUploads(ctx, &s3.ListMultipartUploadsInput{Bucket: aws.String(bucket)})
	if err != nil {
		t.Fatal(err)
	}
	keys := make([]string, 0)
	for _, u := range staleMultipartUploads(out.Uploads, prefix, time.Now().Add(time.Hour)) {
		keys = append(keys, aws.ToString(u.Key))
	}
	return keys
}
