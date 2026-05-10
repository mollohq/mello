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
	s3Once        sync.Once
	s3Presigner   *s3.PresignClient
	s3Bucket      string
	s3PublicURL   string
	s3Available   bool

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
