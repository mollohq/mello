package main

import (
	"context"
	"errors"
	"fmt"
	"os"
	"sort"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/credentials"
	"github.com/aws/aws-sdk-go-v2/service/s3"
	"github.com/aws/aws-sdk-go-v2/service/s3/types"
)

var (
	s3Once      sync.Once
	s3Presigner *s3.PresignClient
	s3Bucket    string
	s3PublicURL string
	s3Available bool
	// s3Client talks to S3_ENDPOINT for the calls the backend makes itself
	// (multipart create, complete, abort, list, HEAD, delete). The presigner
	// signs for S3_PRESIGN_ENDPOINT, which the backend container cannot reach
	// in dev.
	s3Client *s3.Client

	snapshotsOnce      sync.Once
	snapshotsClient    *s3.Client
	snapshotsBucket    string
	snapshotsPublicURL string
)

func initS3() {
	s3Once.Do(func() {
		endpoint := os.Getenv("S3_ENDPOINT")
		bucket := os.Getenv("S3_BUCKET")
		accessKey := os.Getenv("S3_ACCESS_KEY")
		secretKey := os.Getenv("S3_SECRET_KEY")
		publicURL := os.Getenv("S3_PUBLIC_URL")

		if endpoint == "" || bucket == "" || accessKey == "" || secretKey == "" {
			return
		}

		s3Bucket = bucket
		s3PublicURL = publicURL
		s3Client = newS3Client(endpoint, accessKey, secretKey)

		presignEndpoint := os.Getenv("S3_PRESIGN_ENDPOINT")
		if presignEndpoint == "" {
			presignEndpoint = endpoint
		}

		presignClient := s3.New(s3.Options{
			Region:       "auto",
			BaseEndpoint: aws.String(presignEndpoint),
			Credentials:  credentials.NewStaticCredentialsProvider(accessKey, secretKey, ""),
			UsePathStyle: true,
		})

		s3Presigner = s3.NewPresignClient(presignClient)
		s3Available = true
	})
}

func initSnapshotsS3() {
	snapshotsOnce.Do(func() {
		endpoint := os.Getenv("S3_ENDPOINT")
		accessKey := os.Getenv("S3_ACCESS_KEY")
		secretKey := os.Getenv("S3_SECRET_KEY")

		if endpoint == "" || accessKey == "" || secretKey == "" {
			return
		}

		snapshotsBucket = os.Getenv("SNAPSHOTS_S3_BUCKET")
		if snapshotsBucket == "" {
			snapshotsBucket = "mello-snapshots"
		}
		snapshotsPublicURL = os.Getenv("SNAPSHOTS_S3_PUBLIC_URL")

		snapshotsClient = s3.New(s3.Options{
			Region:       "auto",
			BaseEndpoint: aws.String(endpoint),
			Credentials:  credentials.NewStaticCredentialsProvider(accessKey, secretKey, ""),
			UsePathStyle: true,
		})
	})
}

func S3IsConfigured() bool {
	initS3()
	return s3Available
}

func GeneratePresignedPUT(key, contentType string, expiry time.Duration) (string, error) {
	initS3()
	if !s3Available {
		return "", fmt.Errorf("S3 not configured")
	}

	result, err := s3Presigner.PresignPutObject(context.Background(), &s3.PutObjectInput{
		Bucket:      aws.String(s3Bucket),
		Key:         aws.String(key),
		ContentType: aws.String(contentType),
	}, s3.WithPresignExpires(expiry))
	if err != nil {
		return "", fmt.Errorf("presign PUT failed: %w", err)
	}
	return result.URL, nil
}

// DiagnosticsBucket returns the private bucket diagnostic logs are written to.
// Empty when unset so we never fall back to the public clips bucket.
func DiagnosticsBucket() string {
	return os.Getenv("DIAGNOSTICS_S3_BUCKET")
}

// DiagnosticsConfigured reports whether diagnostic-log upload is available
// (S3 credentials present AND a dedicated private bucket configured).
func DiagnosticsConfigured() bool {
	initS3()
	return s3Available && DiagnosticsBucket() != ""
}

// GeneratePresignedPUTToBucket presigns a PUT against an explicit bucket, reusing
// the shared S3 credentials/presigner. Used for diagnostics (private bucket)
// rather than the public clips bucket bound to GeneratePresignedPUT.
func GeneratePresignedPUTToBucket(bucket, key, contentType string, expiry time.Duration) (string, error) {
	initS3()
	if !s3Available {
		return "", fmt.Errorf("S3 not configured")
	}

	result, err := s3Presigner.PresignPutObject(context.Background(), &s3.PutObjectInput{
		Bucket:      aws.String(bucket),
		Key:         aws.String(key),
		ContentType: aws.String(contentType),
	}, s3.WithPresignExpires(expiry))
	if err != nil {
		return "", fmt.Errorf("presign PUT failed: %w", err)
	}
	return result.URL, nil
}

// newS3Client builds a path-style client for an S3-compatible endpoint (R2,
// MinIO), the same options as the presign and snapshots clients.
func newS3Client(endpoint, accessKey, secretKey string) *s3.Client {
	return s3.New(s3.Options{
		Region:       "auto",
		BaseEndpoint: aws.String(endpoint),
		Credentials:  credentials.NewStaticCredentialsProvider(accessKey, secretKey, ""),
		UsePathStyle: true,
	})
}

// presignPutSized presigns a PUT that signs Content-Type, Cache-Control and
// Content-Length. The store rejects a body of any other length or headers
// with other values.
func presignPutSized(ctx context.Context, p *s3.PresignClient, bucket, key, contentType, cacheControl string, length int64, expiry time.Duration) (string, error) {
	result, err := p.PresignPutObject(ctx, &s3.PutObjectInput{
		Bucket:        aws.String(bucket),
		Key:           aws.String(key),
		ContentType:   aws.String(contentType),
		CacheControl:  aws.String(cacheControl),
		ContentLength: aws.Int64(length),
	}, s3.WithPresignExpires(expiry))
	if err != nil {
		return "", fmt.Errorf("presign PUT failed: %w", err)
	}
	return result.URL, nil
}

// presignUploadPart presigns one UploadPart of a multipart upload with a
// signed Content-Length, so the part has exactly that length.
func presignUploadPart(ctx context.Context, p *s3.PresignClient, bucket, key, uploadID string, partNumber int32, length int64, expiry time.Duration) (string, error) {
	result, err := p.PresignUploadPart(ctx, &s3.UploadPartInput{
		Bucket:        aws.String(bucket),
		Key:           aws.String(key),
		UploadId:      aws.String(uploadID),
		PartNumber:    aws.Int32(partNumber),
		ContentLength: aws.Int64(length),
	}, s3.WithPresignExpires(expiry))
	if err != nil {
		return "", fmt.Errorf("presign UploadPart failed: %w", err)
	}
	return result.URL, nil
}

// createMultipartUpload starts a multipart upload. The object gets its
// Content-Type and Cache-Control here, not from the part PUTs.
func createMultipartUpload(ctx context.Context, c *s3.Client, bucket, key, contentType, cacheControl string) (string, error) {
	out, err := c.CreateMultipartUpload(ctx, &s3.CreateMultipartUploadInput{
		Bucket:       aws.String(bucket),
		Key:          aws.String(key),
		ContentType:  aws.String(contentType),
		CacheControl: aws.String(cacheControl),
	})
	if err != nil {
		return "", fmt.Errorf("CreateMultipartUpload failed: %w", err)
	}
	uploadID := aws.ToString(out.UploadId)
	if uploadID == "" {
		return "", fmt.Errorf("CreateMultipartUpload returned no upload id")
	}
	return uploadID, nil
}

// completeMultipartUpload joins the parts into the object. parts must be
// sorted by part number.
func completeMultipartUpload(ctx context.Context, c *s3.Client, bucket, key, uploadID string, parts []types.CompletedPart) error {
	_, err := c.CompleteMultipartUpload(ctx, &s3.CompleteMultipartUploadInput{
		Bucket:          aws.String(bucket),
		Key:             aws.String(key),
		UploadId:        aws.String(uploadID),
		MultipartUpload: &types.CompletedMultipartUpload{Parts: parts},
	})
	if err != nil {
		return fmt.Errorf("CompleteMultipartUpload failed: %w", err)
	}
	return nil
}

// s3ObjectInfo is what a HEAD request tells about an object.
type s3ObjectInfo struct {
	Size        int64
	ContentType string
}

// headObject returns the size and type of an object. found is false when
// the object does not exist; err is set only for other failures.
func headObject(ctx context.Context, c *s3.Client, bucket, key string) (info s3ObjectInfo, found bool, err error) {
	out, err := c.HeadObject(ctx, &s3.HeadObjectInput{
		Bucket: aws.String(bucket),
		Key:    aws.String(key),
	})
	if err != nil {
		switch s3ErrorCode(err) {
		case "NotFound", "NoSuchKey":
			return s3ObjectInfo{}, false, nil
		}
		return s3ObjectInfo{}, false, fmt.Errorf("HeadObject failed: %w", err)
	}
	return s3ObjectInfo{
		Size:        aws.ToInt64(out.ContentLength),
		ContentType: aws.ToString(out.ContentType),
	}, true, nil
}

// deleteObject removes one object. A missing object is not an error.
func deleteObject(ctx context.Context, c *s3.Client, bucket, key string) error {
	_, err := c.DeleteObject(ctx, &s3.DeleteObjectInput{
		Bucket: aws.String(bucket),
		Key:    aws.String(key),
	})
	if err != nil {
		return fmt.Errorf("DeleteObject failed: %w", err)
	}
	return nil
}

// staleMultipartUploads returns the uploads with a key under prefix that
// started before cutoff. An upload with no start time is kept, because its
// age is not known.
func staleMultipartUploads(uploads []types.MultipartUpload, prefix string, cutoff time.Time) []types.MultipartUpload {
	out := make([]types.MultipartUpload, 0)
	for _, u := range uploads {
		if u.Initiated == nil || u.Key == nil || u.UploadId == nil {
			continue
		}
		if strings.HasPrefix(*u.Key, prefix) && u.Initiated.Before(cutoff) {
			out = append(out, u)
		}
	}
	return out
}

// abortStaleMultipartUploads aborts each multipart upload under prefix that
// started before cutoff. It returns how many it aborted and how many aborts
// failed. A failed abort is tried again on the next run.
//
// The list request carries no prefix and the prefix is applied to the keys
// here: MinIO lists the uploads of one exact key only, so a list with the
// prefix "crews/" finds nothing there. R2 and S3 list the bucket the same way.
func abortStaleMultipartUploads(ctx context.Context, c *s3.Client, bucket, prefix string, cutoff time.Time) (aborted, failed int, err error) {
	var keyMarker, uploadIDMarker *string
	for {
		out, err := c.ListMultipartUploads(ctx, &s3.ListMultipartUploadsInput{
			Bucket:         aws.String(bucket),
			KeyMarker:      keyMarker,
			UploadIdMarker: uploadIDMarker,
		})
		if err != nil {
			return aborted, failed, fmt.Errorf("ListMultipartUploads failed: %w", err)
		}
		for _, u := range staleMultipartUploads(out.Uploads, prefix, cutoff) {
			_, err := c.AbortMultipartUpload(ctx, &s3.AbortMultipartUploadInput{
				Bucket:   aws.String(bucket),
				Key:      u.Key,
				UploadId: u.UploadId,
			})
			// NoSuchUpload: it completed or was aborted since the list.
			if err != nil && s3ErrorCode(err) != "NoSuchUpload" {
				failed++
				continue
			}
			aborted++
		}
		if !aws.ToBool(out.IsTruncated) || (out.NextKeyMarker == nil && out.NextUploadIdMarker == nil) {
			break
		}
		keyMarker, uploadIDMarker = out.NextKeyMarker, out.NextUploadIdMarker
	}
	return aborted, failed, nil
}

// s3ErrorCode returns the S3 error code of an API error ("NoSuchUpload",
// "NotFound"), or "" for other errors. Modeled and generic API errors both
// carry ErrorCode.
func s3ErrorCode(err error) string {
	var apiErr interface{ ErrorCode() string }
	if errors.As(err, &apiErr) {
		return apiErr.ErrorCode()
	}
	return ""
}

func S3PublicURL(key string) string {
	initS3()
	if s3PublicURL == "" {
		return ""
	}
	return s3PublicURL + "/" + key
}

type snapshotEntry struct {
	key       string
	timestamp int64
}

func ListSnapshotURLs(crewID, sessionID string) ([]string, error) {
	initSnapshotsS3()
	if snapshotsClient == nil {
		return nil, fmt.Errorf("snapshots S3 not configured")
	}

	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()

	prefix := fmt.Sprintf("snapshots/%s/%s/", crewID, sessionID)
	entries := make([]snapshotEntry, 0)
	var continuationToken *string
	for {
		output, err := snapshotsClient.ListObjectsV2(ctx, &s3.ListObjectsV2Input{
			Bucket:            aws.String(snapshotsBucket),
			Prefix:            aws.String(prefix),
			ContinuationToken: continuationToken,
		})
		if err != nil {
			return nil, fmt.Errorf("ListObjectsV2 failed: %w", err)
		}

		for _, obj := range output.Contents {
			if obj.Key == nil {
				continue
			}

			// Key format: snapshots/{crew_id}/{session_id}/{unix_timestamp_ms}.jpg
			key := *obj.Key
			if !strings.HasPrefix(key, prefix) {
				continue
			}
			filename := strings.TrimPrefix(key, prefix)
			if filename == "" || strings.Contains(filename, "/") || !strings.HasSuffix(filename, ".jpg") {
				continue
			}
			tsRaw := strings.TrimSuffix(filename, ".jpg")
			ts, err := strconv.ParseInt(tsRaw, 10, 64)
			if err != nil || ts <= 0 {
				continue
			}
			entries = append(entries, snapshotEntry{key: key, timestamp: ts})
		}

		if !aws.ToBool(output.IsTruncated) || output.NextContinuationToken == nil {
			break
		}
		continuationToken = output.NextContinuationToken
	}

	if len(entries) == 0 {
		return []string{}, nil
	}

	// Sort ascending by timestamp.
	sort.Slice(entries, func(i, j int) bool {
		return entries[i].timestamp < entries[j].timestamp
	})

	urls := make([]string, 0, len(entries))
	baseURL := strings.TrimRight(snapshotsPublicURL, "/")
	for _, e := range entries {
		if baseURL != "" {
			urls = append(urls, baseURL+"/"+e.key)
		} else {
			urls = append(urls, e.key)
		}
	}

	return urls, nil
}
