package main

// Chat images: the client resizes and encodes an image, asks for a slot on
// the chat bucket (chat_upload_url), PUTs it there, and sends an `image`
// message whose URLs point at that bucket. Keys are
// `{crew_id}/{user_id}/{id}.{ext}`, so the uploader is in the key and
// chat_delete_images can accept only the caller's own objects.

import (
	"context"
	"crypto/rand"
	"database/sql"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"strings"
	"time"

	"github.com/heroiclabs/nakama-common/runtime"
)

const (
	maxChatImageBytes   = 8 << 20 // after the client's re-encode
	maxChatImagesPerMsg = 4
	maxChatImageSide    = 4096
	chatUploadExpiry    = 15 * time.Minute
)

// chatImage mirrors `ImageData` in mello-core/src/chat.rs.
type chatImage struct {
	URL    string `json:"url"`
	Width  int    `json:"width"`
	Height int    `json:"height"`
	Bytes  int64  `json:"bytes"`
	Mime   string `json:"mime"`
}

// chatImageExt is the file extension for an accepted image MIME type.
func chatImageExt(mime string) (string, bool) {
	switch mime {
	case "image/jpeg":
		return "jpg", true
	case "image/png":
		return "png", true
	case "image/gif":
		return "gif", true
	}
	return "", false
}

// newChatImageID is 16 random bytes as hex: unguessable, and no new module.
func newChatImageID() (string, error) {
	var b [16]byte
	if _, err := rand.Read(b[:]); err != nil {
		return "", err
	}
	return hex.EncodeToString(b[:]), nil
}

func chatImageKey(crewID, userID, id, ext string) string {
	return fmt.Sprintf("%s/%s/%s.%s", crewID, userID, id, ext)
}

// parseChatImageKey splits `{crew}/{user}/{file}`. Anything else is not a
// chat image key: no extra segments, no empty segment, no path tricks.
func parseChatImageKey(key string) (crew, user, file string, ok bool) {
	parts := strings.Split(key, "/")
	if len(parts) != 3 {
		return "", "", "", false
	}
	for _, p := range parts {
		if p == "" || p == "." || p == ".." {
			return "", "", "", false
		}
	}
	return parts[0], parts[1], parts[2], true
}

// ownChatImageKey is true when `key` names an object `userID` uploaded.
func ownChatImageKey(key, userID string) bool {
	_, user, _, ok := parseChatImageKey(key)
	return ok && user == userID
}

// validateChatImages checks the images of an `image` envelope against the
// chat bucket's public base. It returns the reason to refuse, or "".
func validateChatImages(images []chatImage, publicBase string) string {
	if publicBase == "" {
		return "images are off on this server"
	}
	if len(images) == 0 {
		return "image type requires images"
	}
	if len(images) > maxChatImagesPerMsg {
		return fmt.Sprintf("at most %d images per message", maxChatImagesPerMsg)
	}
	base := strings.TrimRight(publicBase, "/") + "/"
	for _, img := range images {
		if !strings.HasPrefix(img.URL, base) {
			return "image url is not on the chat bucket"
		}
		if _, _, _, ok := parseChatImageKey(strings.TrimPrefix(img.URL, base)); !ok {
			return "image url is not a chat image"
		}
		if _, ok := chatImageExt(img.Mime); !ok {
			return "unsupported image type"
		}
		if img.Width <= 0 || img.Height <= 0 || img.Width > maxChatImageSide || img.Height > maxChatImageSide {
			return "image size out of range"
		}
		if img.Bytes < 0 || img.Bytes > maxChatImageBytes {
			return "image too large"
		}
	}
	return ""
}

// ---------------------------------------------------------------------------
// chat_upload_url RPC: a presigned PUT on the chat bucket for a crew member
// ---------------------------------------------------------------------------

type ChatUploadURLRequest struct {
	CrewID string `json:"crew_id"`
	Mime   string `json:"mime"`
	Bytes  int64  `json:"bytes"`
}

type ChatUploadURLResponse struct {
	UploadURL string `json:"upload_url"`
	MediaURL  string `json:"media_url"`
	Key       string `json:"key"`
}

func ChatUploadURLRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}

	var req ChatUploadURLRequest
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}
	if req.CrewID == "" {
		return "", runtime.NewError("crew_id required", 3)
	}
	ext, ok := chatImageExt(req.Mime)
	if !ok {
		return "", runtime.NewError("unsupported image type", 3)
	}
	if req.Bytes <= 0 || req.Bytes > maxChatImageBytes {
		return "", runtime.NewError("image too large", 3)
	}
	if !isCrewMember(ctx, nk, req.CrewID, userID) {
		return "", runtime.NewError("not a crew member", 7)
	}

	if !ChatImagesConfigured() {
		logger.Warn("chat_upload_url: chat bucket not configured, returning empty URLs")
		data, _ := json.Marshal(ChatUploadURLResponse{})
		return string(data), nil
	}

	id, err := newChatImageID()
	if err != nil {
		logger.Error("chat_upload_url: id failed: %v", err)
		return "", runtime.NewError("failed to generate upload URL", 13)
	}
	key := chatImageKey(req.CrewID, userID, id, ext)
	uploadURL, err := GeneratePresignedPUTChat(key, req.Mime, chatUploadExpiry)
	if err != nil {
		logger.Error("chat_upload_url: presign failed: %v", err)
		return "", runtime.NewError("failed to generate upload URL", 13)
	}

	resp := ChatUploadURLResponse{
		UploadURL: uploadURL,
		MediaURL:  ChatPublicURL(key),
		Key:       key,
	}
	data, _ := json.Marshal(resp)
	return string(data), nil
}

// ---------------------------------------------------------------------------
// chat_delete_images RPC: remove the caller's own objects
// ---------------------------------------------------------------------------

type ChatDeleteImagesRequest struct {
	Keys []string `json:"keys"`
}

func ChatDeleteImagesRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}

	var req ChatDeleteImagesRequest
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}
	if len(req.Keys) == 0 || len(req.Keys) > maxChatImagesPerMsg {
		return "", runtime.NewError("keys required (1 to 4)", 3)
	}
	for _, key := range req.Keys {
		if !ownChatImageKey(key, userID) {
			return "", runtime.NewError("not your image", 7)
		}
	}

	if !ChatImagesConfigured() {
		return "{}", nil
	}
	if err := DeleteChatObjects(ctx, req.Keys); err != nil {
		logger.Error("chat_delete_images: delete failed: %v", err)
		return "", runtime.NewError("failed to delete images", 13)
	}
	return "{}", nil
}
