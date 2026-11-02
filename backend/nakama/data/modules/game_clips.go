package main

import (
	"context"
	"crypto/rand"
	"encoding/json"
	"fmt"
	"math/big"
	"regexp"
	"sort"
	"strings"
	"time"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/service/s3/types"
	"github.com/heroiclabs/nakama-common/runtime"
)

// ---------------------------------------------------------------------------
// Game clips (F8): the upload calls of plans/GAME-CLIPS.md section 6.5. The
// poster is one presigned PUT, the MP4 a multipart upload of 5 MiB parts.
// Every presigned URL signs its Content-Length, so a client cannot upload
// more bytes than it asked for.
// ---------------------------------------------------------------------------

const (
	gameClipMaxWidth  = 7680
	gameClipMaxHeight = 4320

	// gameClipPartSize is the size of each part except the last. R2 needs
	// equal parts, and 5 MiB is the S3 minimum part size.
	gameClipPartSize       int64 = 5 * 1024 * 1024
	gameClipMaxVideoBytes  int64 = 64 * 1024 * 1024
	gameClipMaxPosterBytes int64 = 1024 * 1024
	// gameClipMaxParts is the part count of a video at the cap (13).
	gameClipMaxParts = int((gameClipMaxVideoBytes + gameClipPartSize - 1) / gameClipPartSize)

	gameClipVideoType  = "video/mp4"
	gameClipPosterType = "image/jpeg"
	// gameClipCacheControl is set on both objects. A clip object never
	// changes, so the CDN can keep its ranges (section 6.2).
	gameClipCacheControl = "public, max-age=31536000, immutable"
	gameClipURLExpiry    = 1 * time.Hour
	gameClipS3Timeout    = 15 * time.Second

	gameClipPartPoster = "poster"
	gameClipPartVideo  = "video"

	// A multipart upload under gameClipUploadPrefix that is older than
	// gameClipUploadMaxAge is aborted by the cleanup job.
	gameClipUploadPrefix        = "crews/"
	gameClipUploadMaxAge        = 24 * time.Hour
	gameClipUploadCleanupPeriod = 1 * time.Hour
	gameClipUploadCleanupBudget = 2 * time.Minute

	// gameClipMaxUploadIDLen bounds the client's upload_id. R2 and MinIO
	// ids are well under it.
	gameClipMaxUploadIDLen = 1024
)

// gameClipIDPattern: the clip_id is part of the object keys.
var gameClipIDPattern = regexp.MustCompile(`^[A-Za-z0-9_-]{1,64}$`)

// validGameClipID reports whether a clip_id is safe in an object key.
func validGameClipID(clipID string) bool {
	return gameClipIDPattern.MatchString(clipID)
}

// validatePostClipType checks clip_type and, for a game clip, the fields a
// game clip needs. clip_type is already defaulted to voice. Returns a
// non-empty error string on failure.
func validatePostClipType(req PostClipRequest) string {
	switch req.ClipType {
	case clipTypeVoice:
		return ""
	case clipTypeGame:
		if !validGameClipID(req.ClipID) {
			return "clip_id must match [A-Za-z0-9_-]{1,64}"
		}
		if req.Width < 1 || req.Width > gameClipMaxWidth {
			return fmt.Sprintf("width must be 1-%d", gameClipMaxWidth)
		}
		if req.Height < 1 || req.Height > gameClipMaxHeight {
			return fmt.Sprintf("height must be 1-%d", gameClipMaxHeight)
		}
		return ""
	default:
		return "clip_type must be voice or game"
	}
}

// gameClipKeys returns the object keys of a game clip's poster and video.
func gameClipKeys(crewID, clipID string) (posterKey, videoKey string) {
	base := fmt.Sprintf("crews/%s/%s", crewID, clipID)
	return base + ".jpg", base + ".mp4"
}

// gameClipPartPlan returns the byte size of each part of a video of
// videoBytes: gameClipPartSize each, the last part the rest. ok is false for
// 0 bytes or more than gameClipMaxVideoBytes.
func gameClipPartPlan(videoBytes int64) (sizes []int64, ok bool) {
	if videoBytes < 1 || videoBytes > gameClipMaxVideoBytes {
		return nil, false
	}
	n := (videoBytes + gameClipPartSize - 1) / gameClipPartSize
	sizes = make([]int64, n)
	for i := range sizes {
		sizes[i] = gameClipPartSize
	}
	sizes[n-1] = videoBytes - (n-1)*gameClipPartSize
	return sizes, true
}

// validateGameClipUploadRequest checks the clip_id, the sizes and the
// upload_id of a game clip_upload_url request. Returns a non-empty error
// string on failure.
func validateGameClipUploadRequest(req ClipUploadURLRequest) string {
	if !validGameClipID(req.ClipID) {
		return "clip_id must match [A-Za-z0-9_-]{1,64}"
	}
	if req.PosterBytes < 1 || req.PosterBytes > gameClipMaxPosterBytes {
		return fmt.Sprintf("poster_bytes must be 1-%d", gameClipMaxPosterBytes)
	}
	if _, ok := gameClipPartPlan(req.VideoBytes); !ok {
		return fmt.Sprintf("video_bytes must be 1-%d", gameClipMaxVideoBytes)
	}
	if len(req.UploadID) > gameClipMaxUploadIDLen {
		return "upload_id too long"
	}
	return ""
}

// ownGameClip finds the caller's game clip in a crew's clips. The error is a
// Nakama error: 5 when the clip is not there, 7 when it is another actor's,
// 3 when it is not a game clip. Pure, so the ownership rule is testable.
func ownGameClip(clips []StoredClip, clipID, userID string) (*StoredClip, error) {
	clip, taken := existingClip(clips, clipID, userID)
	switch {
	case clip == nil && taken:
		return nil, runtime.NewError("not the clipper", 7)
	case clip == nil:
		return nil, runtime.NewError("clip not found", 5)
	case clip.ClipType != clipTypeGame:
		return nil, runtime.NewError("not a game clip", 3)
	}
	return clip, nil
}

// CompletedClipPart is one uploaded part of a game clip video, with the ETag
// the part PUT answered.
type CompletedClipPart struct {
	PartNumber int32  `json:"part_number"`
	ETag       string `json:"etag"`
}

// completedClipParts checks the parts of a video upload and sorts them by
// part number, as CompleteMultipartUpload needs. Returns a non-empty error
// string on failure.
func completedClipParts(parts []CompletedClipPart) ([]types.CompletedPart, string) {
	if len(parts) == 0 || len(parts) > gameClipMaxParts {
		return nil, fmt.Sprintf("parts must hold 1-%d parts", gameClipMaxParts)
	}
	sorted := append([]CompletedClipPart(nil), parts...)
	sort.Slice(sorted, func(i, j int) bool { return sorted[i].PartNumber < sorted[j].PartNumber })
	out := make([]types.CompletedPart, 0, len(sorted))
	for i, p := range sorted {
		if p.PartNumber < 1 || int(p.PartNumber) > gameClipMaxParts {
			return nil, fmt.Sprintf("part_number must be 1-%d", gameClipMaxParts)
		}
		if i > 0 && p.PartNumber == sorted[i-1].PartNumber {
			return nil, "duplicate part_number"
		}
		if p.ETag == "" {
			return nil, "etag required"
		}
		out = append(out, types.CompletedPart{
			PartNumber: aws.Int32(p.PartNumber),
			ETag:       aws.String(p.ETag),
		})
	}
	return out, ""
}

// checkClipObject checks the HEAD of an uploaded clip object: 1 to maxBytes
// long and of type wantType. overCap is true when the object is too big, so
// the caller deletes it. Returns a non-empty error string on failure.
func checkClipObject(info s3ObjectInfo, maxBytes int64, wantType string) (errStr string, overCap bool) {
	if info.Size > maxBytes {
		return fmt.Sprintf("object is %d bytes, cap is %d", info.Size, maxBytes), true
	}
	if info.Size < 1 {
		return "object is empty", false
	}
	mediaType := strings.TrimSpace(strings.SplitN(info.ContentType, ";", 2)[0])
	if !strings.EqualFold(mediaType, wantType) {
		return fmt.Sprintf("object type is %q, want %q", info.ContentType, wantType), false
	}
	return "", false
}

// ---------------------------------------------------------------------------
// clip_upload_url, clip_type game
// ---------------------------------------------------------------------------

// GameClipUploadURLResponse is the clip_upload_url answer for a game clip.
// Every field is empty when S3 is not configured.
type GameClipUploadURLResponse struct {
	PosterUploadURL string            `json:"poster_upload_url"`
	PosterURL       string            `json:"poster_url"`
	UploadID        string            `json:"upload_id"`
	PartSize        int64             `json:"part_size"`
	Parts           []GameClipPartURL `json:"parts"`
	MediaURL        string            `json:"media_url"`
	CacheControl    string            `json:"cache_control"`
}

// GameClipPartURL is one presigned UploadPart. The PUT body must be exactly
// Bytes long, because the length is signed.
type GameClipPartURL struct {
	PartNumber int32  `json:"part_number"`
	URL        string `json:"url"`
	Bytes      int64  `json:"bytes"`
}

func gameClipUploadURL(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule, userID string, req ClipUploadURLRequest) (string, error) {
	if errStr := validateGameClipUploadRequest(req); errStr != "" {
		return "", runtime.NewError(errStr, 3)
	}
	if !isCrewMember(ctx, nk, req.CrewID, userID) {
		return "", runtime.NewError("not a crew member", 7)
	}
	doc, _ := readClipsDoc(ctx, nk, req.CrewID)
	if _, err := ownGameClip(doc.Clips, req.ClipID, userID); err != nil {
		return "", err
	}

	if !S3IsConfigured() {
		logger.Warn("clip_upload_url: S3 not configured, returning empty URLs")
		data, _ := json.Marshal(GameClipUploadURLResponse{Parts: []GameClipPartURL{}})
		return string(data), nil
	}

	sizes, _ := gameClipPartPlan(req.VideoBytes)
	posterKey, videoKey := gameClipKeys(req.CrewID, req.ClipID)
	s3ctx, cancel := context.WithTimeout(ctx, gameClipS3Timeout)
	defer cancel()

	posterUploadURL, err := presignPutSized(s3ctx, s3Presigner, s3Bucket, posterKey,
		gameClipPosterType, gameClipCacheControl, req.PosterBytes, gameClipURLExpiry)
	if err != nil {
		logger.Error("clip_upload_url: poster presign failed: %v", err)
		return "", runtime.NewError("failed to generate upload URL", 13)
	}

	// A set upload_id is a resume, or a refresh of expired URLs: presign new
	// part URLs for it and make no new upload.
	uploadID := req.UploadID
	if uploadID == "" {
		uploadID, err = createMultipartUpload(s3ctx, s3Client, s3Bucket, videoKey,
			gameClipVideoType, gameClipCacheControl)
		if err != nil {
			logger.Error("clip_upload_url: %v", err)
			return "", runtime.NewError("failed to start upload", 13)
		}
	}

	parts := make([]GameClipPartURL, 0, len(sizes))
	for i, n := range sizes {
		num := int32(i + 1)
		partURL, err := presignUploadPart(s3ctx, s3Presigner, s3Bucket, videoKey, uploadID, num, n, gameClipURLExpiry)
		if err != nil {
			logger.Error("clip_upload_url: part %d presign failed: %v", num, err)
			return "", runtime.NewError("failed to generate upload URL", 13)
		}
		parts = append(parts, GameClipPartURL{PartNumber: num, URL: partURL, Bytes: n})
	}

	logger.Info("clip_upload_url: game clip crew=%s clip=%s upload_id=%s parts=%d resume=%t",
		req.CrewID, req.ClipID, uploadID, len(parts), req.UploadID != "")
	data, _ := json.Marshal(GameClipUploadURLResponse{
		PosterUploadURL: posterUploadURL,
		PosterURL:       S3PublicURL(posterKey),
		UploadID:        uploadID,
		PartSize:        gameClipPartSize,
		Parts:           parts,
		MediaURL:        S3PublicURL(videoKey),
		CacheControl:    gameClipCacheControl,
	})
	return string(data), nil
}

// ---------------------------------------------------------------------------
// clip_upload_complete, clip_type game
// ---------------------------------------------------------------------------

func gameClipUploadComplete(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule, userID string, req ClipUploadCompleteRequest) (string, error) {
	if !validGameClipID(req.ClipID) {
		return "", runtime.NewError("clip_id must match [A-Za-z0-9_-]{1,64}", 3)
	}
	var completed []types.CompletedPart
	switch req.Part {
	case gameClipPartPoster:
	case gameClipPartVideo:
		if req.UploadID == "" || len(req.UploadID) > gameClipMaxUploadIDLen {
			return "", runtime.NewError("upload_id required", 3)
		}
		parts, errStr := completedClipParts(req.Parts)
		if errStr != "" {
			return "", runtime.NewError(errStr, 3)
		}
		completed = parts
	default:
		return "", runtime.NewError("part must be poster or video", 3)
	}
	if !isCrewMember(ctx, nk, req.CrewID, userID) {
		return "", runtime.NewError("not a crew member", 7)
	}
	doc, _ := readClipsDoc(ctx, nk, req.CrewID)
	if _, err := ownGameClip(doc.Clips, req.ClipID, userID); err != nil {
		return "", err
	}
	if !S3IsConfigured() {
		return "", runtime.NewError("clip storage not configured", 9)
	}

	posterKey, videoKey := gameClipKeys(req.CrewID, req.ClipID)
	key, maxBytes, wantType := posterKey, gameClipMaxPosterBytes, gameClipPosterType
	if req.Part == gameClipPartVideo {
		key, maxBytes, wantType = videoKey, gameClipMaxVideoBytes, gameClipVideoType
	}

	s3ctx, cancel := context.WithTimeout(ctx, gameClipS3Timeout)
	defer cancel()

	if req.Part == gameClipPartVideo {
		if err := completeMultipartUpload(s3ctx, s3Client, s3Bucket, videoKey, req.UploadID, completed); err != nil {
			switch s3ErrorCode(err) {
			case "NoSuchUpload":
				// A retry after a complete whose answer was lost: the
				// HEAD below finds the object. No object means the upload
				// is gone.
				logger.Info("clip_upload_complete: no upload %s for clip %s, checking the object", req.UploadID, req.ClipID)
			case "InvalidPart", "InvalidPartOrder", "EntityTooSmall":
				logger.Warn("clip_upload_complete: parts rejected for clip %s: %v", req.ClipID, err)
				return "", runtime.NewError("parts rejected: "+s3ErrorCode(err), 3)
			default:
				logger.Error("clip_upload_complete: %v", err)
				return "", runtime.NewError("failed to complete upload", 13)
			}
		}
	}

	info, found, err := headObject(s3ctx, s3Client, s3Bucket, key)
	if err != nil {
		logger.Error("clip_upload_complete: %v", err)
		return "", runtime.NewError("failed to check upload", 13)
	}
	if !found {
		return "", runtime.NewError("uploaded object not found", 5)
	}
	if errStr, overCap := checkClipObject(info, maxBytes, wantType); errStr != "" {
		logger.Warn("clip_upload_complete: %s rejected for clip %s: %s", req.Part, req.ClipID, errStr)
		if overCap {
			if err := deleteObject(s3ctx, s3Client, s3Bucket, key); err != nil {
				logger.Error("clip_upload_complete: delete of %s failed: %v", key, err)
			}
		}
		return "", runtime.NewError(errStr, 9)
	}

	url := S3PublicURL(key)
	if err := setGameClipURL(ctx, nk, req.CrewID, req.ClipID, userID, req.Part, url); err != nil {
		logger.Error("clip_upload_complete: %v", err)
		return "", runtime.NewError("failed to update clip", 13)
	}

	logger.Info("Game clip %s upload complete: crew=%s clip=%s url=%s", req.Part, req.CrewID, req.ClipID, url)
	PushFeedChanged(ctx, logger, nk, req.CrewID, "clip")

	field := "media_url"
	if req.Part == gameClipPartPoster {
		field = "poster_url"
	}
	resp, _ := json.Marshal(map[string]interface{}{
		"success": true,
		field:     url,
	})
	return string(resp), nil
}

// setGameClipURL sets poster_url or media_url on the caller's clip, with the
// same version-retry loop as the voice clip path.
func setGameClipURL(ctx context.Context, nk runtime.NakamaModule, crewID, clipID, userID, part, url string) error {
	for attempt := 0; attempt < 3; attempt++ {
		doc, version := readClipsDoc(ctx, nk, crewID)
		clip, _ := existingClip(doc.Clips, clipID, userID)
		if clip == nil {
			return fmt.Errorf("clip %s not found in crew %s", clipID, crewID)
		}
		if part == gameClipPartPoster {
			clip.PosterURL = url
		} else {
			clip.MediaURL = url
		}
		doc.UpdatedAt = time.Now().UnixMilli()
		if err := writeClipsDoc(ctx, nk, crewID, doc, version); err == nil {
			return nil
		}
		jitter, _ := rand.Int(rand.Reader, big.NewInt(50))
		time.Sleep(time.Duration(50*(attempt+1)+int(jitter.Int64())) * time.Millisecond)
	}
	return fmt.Errorf("crew_clips write failed after 3 retries for crew %s", crewID)
}

// ---------------------------------------------------------------------------
// Multipart cleanup job
// ---------------------------------------------------------------------------

// StartGameClipUploadCleanupJob runs at start and then every hour. Each run
// aborts the multipart uploads under crews/ that started more than 24 h ago,
// so the parts of a game clip upload that never completes do not stay in
// the bucket.
func StartGameClipUploadCleanupJob(ctx context.Context, logger runtime.Logger) {
	if !S3IsConfigured() {
		logger.Info("Game clip upload cleanup disabled (S3 not configured)")
		return
	}
	runGameClipUploadCleanup(ctx, logger, time.Now())

	ticker := time.NewTicker(gameClipUploadCleanupPeriod)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case t := <-ticker.C:
			runGameClipUploadCleanup(ctx, logger, t)
		}
	}
}

// runGameClipUploadCleanup is one pass of the job at now.
func runGameClipUploadCleanup(ctx context.Context, logger runtime.Logger, now time.Time) {
	defer func() {
		if r := recover(); r != nil {
			logger.Error("Game clip upload cleanup panic: %v", r)
		}
	}()
	s3ctx, cancel := context.WithTimeout(ctx, gameClipUploadCleanupBudget)
	defer cancel()
	aborted, failed, err := abortStaleMultipartUploads(s3ctx, s3Client, s3Bucket,
		gameClipUploadPrefix, now.Add(-gameClipUploadMaxAge))
	if err != nil {
		logger.Error("Game clip upload cleanup: %v", err)
	}
	if aborted > 0 || failed > 0 {
		logger.Info("Game clip upload cleanup: aborted=%d failed=%d", aborted, failed)
	}
}
