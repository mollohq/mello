package main

import (
	"context"
	"fmt"
	"os"
	"sync"
	"time"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/credentials"
	"github.com/aws/aws-sdk-go-v2/service/s3"
)

var (
	s3Once       sync.Once
	s3Presigner  *s3.PresignClient
	s3Bucket     string
	s3PublicURL  string
	s3Available  bool
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

		// Presigned URLs must be reachable by the client (outside Docker).
		// S3_PRESIGN_ENDPOINT overrides S3_ENDPOINT for URL generation only.
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
