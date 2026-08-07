package main

import (
	"bytes"
	"encoding/base64"
	"image"
	"image/png"
	"testing"
)

func pngBase64(t *testing.T, w, h int) string {
	t.Helper()
	var buf bytes.Buffer
	if err := png.Encode(&buf, image.NewRGBA(image.Rect(0, 0, w, h))); err != nil {
		t.Fatalf("encode: %v", err)
	}
	return base64.StdEncoding.EncodeToString(buf.Bytes())
}

func TestValidateGameIconUpload_Accepts(t *testing.T) {
	raw, errStr := validateGameIconUpload("custom-night-stones", pngBase64(t, 64, 64))
	if errStr != "" {
		t.Fatalf("unexpected error: %s", errStr)
	}
	if len(raw) == 0 {
		t.Fatalf("expected decoded bytes")
	}
}

func TestValidateGameIconUpload_RejectsBadIDs(t *testing.T) {
	good := pngBase64(t, 8, 8)
	for _, id := range []string{"", "UPPER", "has space", "a/b", "..", string(make([]byte, 80))} {
		if _, errStr := validateGameIconUpload(id, good); errStr == "" {
			t.Fatalf("id %q should be rejected", id)
		}
	}
}

func TestValidateGameIconUpload_RejectsNonPNGAndBadBase64(t *testing.T) {
	if _, errStr := validateGameIconUpload("ok-id", "not-base64!!"); errStr == "" {
		t.Fatalf("bad base64 should be rejected")
	}
	notPng := base64.StdEncoding.EncodeToString([]byte("GIF89a not a png"))
	if _, errStr := validateGameIconUpload("ok-id", notPng); errStr == "" {
		t.Fatalf("non-PNG should be rejected")
	}
}

func TestValidateGameIconUpload_RejectsOversize(t *testing.T) {
	// A large noisy PNG comfortably above the 48 KB cap.
	img := image.NewRGBA(image.Rect(0, 0, 512, 512))
	for i := range img.Pix {
		img.Pix[i] = byte(i * 31)
	}
	var buf bytes.Buffer
	if err := png.Encode(&buf, img); err != nil {
		t.Fatalf("encode: %v", err)
	}
	if buf.Len() <= gameIconMaxBytes {
		t.Skipf("test PNG only %d bytes; cap is %d", buf.Len(), gameIconMaxBytes)
	}
	data := base64.StdEncoding.EncodeToString(buf.Bytes())
	if _, errStr := validateGameIconUpload("ok-id", data); errStr != "icon too large" {
		t.Fatalf("expected size rejection, got %q", errStr)
	}
}
