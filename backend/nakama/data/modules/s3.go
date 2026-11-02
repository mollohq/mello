package main

import (
	"context"
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
)

var (
	s3Once      sync.Once
	s3Presigner *s3.PresignClient
	s3Bucket    string
	s3PublicURL string
	s3Available bool

	snapshotsOnce      sync.Once
	snapshotsClient    *s3.Client
	snapshotsBucket    string
	snapshotsPublicURL string

	chatOnce      sync.Once
	chatClient    *s3.Client
	chatPresigner *s3.PresignClient
	chatBucket    string
	chatPublicURL string
)

// initChatS3 sets up the chat images bucket: its own bucket (CHAT_S3_BUCKET,
// default mello-chat) and public base (CHAT_S3_PUBLIC_URL), on the shared
// credentials and endpoints. Without the public base, image messages are
// refused: the validation hook needs it to pin image URLs to this bucket.
func initChatS3() {
	chatOnce.Do(func() {
		endpoint := os.Getenv("S3_ENDPOINT")
		accessKey := os.Getenv("S3_ACCESS_KEY")
		secretKey := os.Getenv("S3_SECRET_KEY")
		chatPublicURL = strings.TrimRight(os.Getenv("CHAT_S3_PUBLIC_URL"), "/")
		if endpoint == "" || accessKey == "" || secretKey == "" || chatPublicURL == "" {
			chatPublicURL = ""
			return
		}
		chatBucket = os.Getenv("CHAT_S3_BUCKET")
		if chatBucket == "" {
			chatBucket = "mello-chat"
		}
		creds := credentials.NewStaticCredentialsProvider(accessKey, secretKey, "")
		chatClient = s3.New(s3.Options{
			Region:       "auto",
			BaseEndpoint: aws.String(endpoint),
			Credentials:  creds,
			UsePathStyle: true,
		})
		presignEndpoint := os.Getenv("S3_PRESIGN_ENDPOINT")
		if presignEndpoint == "" {
			presignEndpoint = endpoint
		}
		chatPresigner = s3.NewPresignClient(s3.New(s3.Options{
			Region:       "auto",
			BaseEndpoint: aws.String(presignEndpoint),
			Credentials:  creds,
			UsePathStyle: true,
		}))
	})
}

// ChatImagesConfigured reports whether chat image upload is available.
func ChatImagesConfigured() bool {
	initChatS3()
	return chatClient != nil
}

// ChatPublicBase is the public base URL of the chat bucket, "" when off.
func ChatPublicBase() string {
	initChatS3()
	return chatPublicURL
}

// ChatPublicURL is the public URL of a chat image key.
func ChatPublicURL(key string) string {
	base := ChatPublicBase()
	if base == "" {
		return ""
	}
	return base + "/" + key
}

func GeneratePresignedPUTChat(key, contentType string, expiry time.Duration) (string, error) {
	if !ChatImagesConfigured() {
		return "", fmt.Errorf("chat S3 not configured")
	}
	result, err := chatPresigner.PresignPutObject(context.Background(), &s3.PutObjectInput{
		Bucket:      aws.String(chatBucket),
		Key:         aws.String(key),
		ContentType: aws.String(contentType),
	}, s3.WithPresignExpires(expiry))
	if err != nil {
		return "", fmt.Errorf("presign PUT failed: %w", err)
	}
	return result.URL, nil
}

// DeleteChatObjects removes the given keys from the chat bucket. A missing
// key is not an error: a delete that ran twice has nothing left to do.
func DeleteChatObjects(ctx context.Context, keys []string) error {
	if !ChatImagesConfigured() {
		return fmt.Errorf("chat S3 not configured")
	}
	ctx, cancel := context.WithTimeout(ctx, 10*time.Second)
	defer cancel()
	for _, key := range keys {
		if _, err := chatClient.DeleteObject(ctx, &s3.DeleteObjectInput{
			Bucket: aws.String(chatBucket),
			Key:    aws.String(key),
		}); err != nil {
			return fmt.Errorf("delete %s: %w", key, err)
		}
	}
	return nil
}

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
