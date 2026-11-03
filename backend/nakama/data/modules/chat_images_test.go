package main

import (
	"strings"
	"testing"
)

const chatBase = "http://localhost:9000/mello-media"

func chatImg(url string) chatImage {
	return chatImage{URL: url, Width: 1600, Height: 900, Bytes: 120000, Mime: "image/jpeg"}
}

func TestChatImageKey_OnlyThreeCleanSegments(t *testing.T) {
	for _, key := range []string{"c1/u1/a.jpg", "crew-x/user-y/01ABC.png"} {
		if !ownChatImageKey(key, strings.Split(key, "/")[1]) {
			t.Fatalf("%q is the user's own key", key)
		}
	}
	for _, key := range []string{"", "a.jpg", "c1/a.jpg", "c1/u1/x/a.jpg", "c1//a.jpg", "c1/../a.jpg", "/c1/u1/a.jpg"} {
		if _, _, _, ok := parseChatImageKey(key); ok {
			t.Fatalf("%q must not parse as a chat image key", key)
		}
	}
	if ownChatImageKey("c1/u1/a.jpg", "u2") {
		t.Fatal("another user's key is not yours")
	}
}

func TestChatImageExt_AcceptsThreeTypes(t *testing.T) {
	for mime, want := range map[string]string{"image/jpeg": "jpg", "image/png": "png", "image/gif": "gif"} {
		ext, ok := chatImageExt(mime)
		if !ok || ext != want {
			t.Fatalf("%s -> %q %v", mime, ext, ok)
		}
	}
	for _, mime := range []string{"", "image/webp", "image/svg+xml", "text/html"} {
		if _, ok := chatImageExt(mime); ok {
			t.Fatalf("%q must be refused", mime)
		}
	}
}

func TestValidateChatImages_RefusesWhatIsNotOurs(t *testing.T) {
	good := chatImg(chatBase + "/c1/u1/a.jpg")
	cases := []struct {
		name   string
		images []chatImage
		base   string
		want   string
	}{
		{"one good image", []chatImage{good}, chatBase, ""},
		{"four images", []chatImage{good, good, good, good}, chatBase, ""},
		{"bucket off", []chatImage{good}, "", "images are off on this server"},
		{"no images", nil, chatBase, "image type requires images"},
		{"five images", []chatImage{good, good, good, good, good}, chatBase, "at most 4 images per message"},
		{"other host", []chatImage{chatImg("https://evil.example/c1/u1/a.jpg")}, chatBase, "image url is not on the chat bucket"},
		{"not a key", []chatImage{chatImg(chatBase + "/a.jpg")}, chatBase, "image url is not a chat image"},
		{"bad mime", []chatImage{{URL: good.URL, Width: 10, Height: 10, Bytes: 1, Mime: "image/webp"}}, chatBase, "unsupported image type"},
		{"zero size", []chatImage{{URL: good.URL, Width: 0, Height: 10, Bytes: 1, Mime: "image/png"}}, chatBase, "image size out of range"},
		{"too many bytes", []chatImage{{URL: good.URL, Width: 10, Height: 10, Bytes: maxChatImageBytes + 1, Mime: "image/png"}}, chatBase, "image too large"},
	}
	for _, c := range cases {
		if got := validateChatImages(c.images, c.base); got != c.want {
			t.Errorf("%s: got %q, want %q", c.name, got, c.want)
		}
	}
}

func TestValidateChatContent_ImageEnvelope(t *testing.T) {
	ok := `{"v":1,"type":"image","body":"look","images":[{"url":"` + chatBase + `/c1/u1/a.jpg","width":800,"height":600,"bytes":1000,"mime":"image/jpeg"}]}`
	if err := validateChatContent(ok, chatBase); err != nil {
		t.Fatalf("a good image message is accepted: %v", err)
	}
	if err := validateChatContent(ok, ""); err == nil {
		t.Fatal("with no chat bucket an image message is refused")
	}
	noImages := `{"v":1,"type":"image","body":"look"}`
	if err := validateChatContent(noImages, chatBase); err == nil {
		t.Fatal("an image message without images is refused")
	}
	text := `{"v":1,"type":"text","body":"hi"}`
	if err := validateChatContent(text, ""); err != nil {
		t.Fatalf("text does not need the bucket: %v", err)
	}
}
