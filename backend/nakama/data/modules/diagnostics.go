package main

import (
	"context"
	"database/sql"
	"encoding/json"
	"fmt"
	"strings"
	"time"

	"github.com/heroiclabs/nakama-common/runtime"
)

// DiagnosticLogUploadURLRequest is the client payload for diagnostic_log_upload_url.
type DiagnosticLogUploadURLRequest struct {
	CaptureID string `json:"capture_id"`
}

// DiagnosticLogUploadURLResponse returns a presigned PUT URL (empty when
// diagnostics storage is not configured) and the object key it maps to.
type DiagnosticLogUploadURLResponse struct {
	UploadURL string `json:"upload_url"`
	Key       string `json:"key"`
}

// DiagnosticLogUploadURLRPC mints a presigned PUT URL so the client can upload a
// short diagnostic log capture directly to a PRIVATE bucket. The key is always
// scoped to the authenticated caller so a client cannot write under another
// user. Returns an empty URL (client skips) when storage is unconfigured.
func DiagnosticLogUploadURLRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok || userID == "" {
		return "", runtime.NewError("authentication required", 16)
	}

	var req DiagnosticLogUploadURLRequest
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}

	captureID := sanitizeCaptureID(req.CaptureID)
	if captureID == "" {
		return "", runtime.NewError("capture_id required", 3)
	}

	if !DiagnosticsConfigured() {
		logger.Warn("diagnostic_log_upload_url: diagnostics storage not configured")
		data, _ := json.Marshal(DiagnosticLogUploadURLResponse{})
		return string(data), nil
	}

	key := fmt.Sprintf("diagnostics/%s/%s.log", userID, captureID)
	uploadURL, err := GeneratePresignedPUTToBucket(DiagnosticsBucket(), key, "text/plain", 15*time.Minute)
	if err != nil {
		logger.Error("diagnostic_log_upload_url: presign failed: %v", err)
		return "", runtime.NewError("failed to generate upload URL", 13)
	}

	logger.Info("diagnostic_log_upload_url: issued for user=%s key=%s", userID, key)
	data, _ := json.Marshal(DiagnosticLogUploadURLResponse{UploadURL: uploadURL, Key: key})
	return string(data), nil
}

// sanitizeCaptureID keeps only filename-safe characters so the caller can't
// escape their key prefix (e.g. via "../"). Returns "" if nothing valid remains.
func sanitizeCaptureID(raw string) string {
	raw = strings.TrimSpace(raw)
	var b strings.Builder
	for _, r := range raw {
		switch {
		case r >= 'a' && r <= 'z', r >= 'A' && r <= 'Z', r >= '0' && r <= '9', r == '-', r == '_':
			b.WriteRune(r)
		}
	}
	out := b.String()
	if len(out) > 128 {
		out = out[:128]
	}
	return out
}
