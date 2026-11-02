package main

import (
	"context"
	"encoding/json"
	"fmt"
	"net/url"
	"strings"
	"testing"
	"time"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/service/s3"
	"github.com/aws/aws-sdk-go-v2/service/s3/types"
)

const mib int64 = 1024 * 1024

// ---- post_clip ----

func TestValidatePostClipType(t *testing.T) {
	game := func(id string, w, h int) PostClipRequest {
		return PostClipRequest{ClipID: id, ClipType: clipTypeGame, Width: w, Height: h}
	}
	cases := []struct {
		name string
		req  PostClipRequest
		ok   bool
	}{
		{"voice", PostClipRequest{ClipID: "any id/with.dots", ClipType: clipTypeVoice}, true},
		{"game 720p", game("c1", 1280, 720), true},
		{"game min", game("c1", 1, 1), true},
		{"game max", game("c1", 7680, 4320), true},
		{"game width 0", game("c1", 0, 720), false},
		{"game width over", game("c1", 7681, 720), false},
		{"game height 0", game("c1", 1280, 0), false},
		{"game height over", game("c1", 1280, 4321), false},
		{"game negative", game("c1", -1, 720), false},
		{"game bad clip_id", game("../c1", 1280, 720), false},
		{"unknown type", PostClipRequest{ClipID: "c1", ClipType: "screen"}, false},
	}
	for _, c := range cases {
		got := validatePostClipType(c.req)
		if (got == "") != c.ok {
			t.Errorf("%s: got %q, want ok=%t", c.name, got, c.ok)
		}
	}
}

func TestGameClipIDPattern(t *testing.T) {
	valid := []string{"a", "Z", "0", "clip_01-AB", strings.Repeat("x", 64),
		"3f2b9c1e-6d4a-4b8e-9f0a-1c2d3e4f5a6b"}
	for _, id := range valid {
		if !validGameClipID(id) {
			t.Errorf("%q must be valid", id)
		}
	}
	invalid := []string{"", strings.Repeat("x", 65), "a/b", "a.b", "..", "a b",
		"a\\b", "clip%2F", "é", "a\n", "a?b"}
	for _, id := range invalid {
		if validGameClipID(id) {
			t.Errorf("%q must be invalid", id)
		}
	}
}

func TestExistingClipFindsTheClipByIDAndActor(t *testing.T) {
	clips := []StoredClip{
		{EventID: "e1", ClipID: "c1", ActorID: "alice"},
		{EventID: "e2", ClipID: "c2", ActorID: "bob"},
	}
	if got, taken := existingClip(clips, "c1", "alice"); got == nil || got.EventID != "e1" || taken {
		t.Fatalf("same actor: got %+v taken=%t, want e1", got, taken)
	}
	if got, taken := existingClip(clips, "c1", "bob"); got != nil || !taken {
		t.Fatalf("other actor: got %+v taken=%t, want taken", got, taken)
	}
	if got, taken := existingClip(clips, "c3", "alice"); got != nil || taken {
		t.Fatalf("new id: got %+v taken=%t, want none", got, taken)
	}
	// The pointer is into the slice, so a caller can update the stored clip.
	got, _ := existingClip(clips, "c2", "bob")
	got.MediaURL = "u"
	if clips[1].MediaURL != "u" {
		t.Fatal("existingClip must point into the slice")
	}
	// Old duplicate data: the actor's own clip wins over another actor's.
	dup := []StoredClip{
		{EventID: "e1", ClipID: "c1", ActorID: "bob"},
		{EventID: "e2", ClipID: "c1", ActorID: "alice"},
	}
	if got, taken := existingClip(dup, "c1", "alice"); got == nil || got.EventID != "e2" || taken {
		t.Fatalf("duplicate: got %+v taken=%t, want e2", got, taken)
	}
}

func gameClipFixture(crew string) *fakeGuestNk {
	nk := newFakeGuestNk()
	nk.addMember(crew, "gc-alice", "alice")
	nk.addMember(crew, "gc-bob", "bob")
	return nk
}

func postClip(t *testing.T, nk *fakeGuestNk, user, payload string) (map[string]interface{}, error) {
	t.Helper()
	out, err := PostClipRPC(ctxWithUser(user), testLogger(), nil, nk, payload)
	if err != nil {
		return nil, err
	}
	var resp map[string]interface{}
	if err := json.Unmarshal([]byte(out), &resp); err != nil {
		t.Fatalf("response: %v", err)
	}
	return resp, nil
}

func TestPostClipStoresTheGameFields(t *testing.T) {
	nk := gameClipFixture("gc-crew-1")
	p := `{"crew_id":"gc-crew-1","clip_id":"g1","clip_type":"game","duration_seconds":30,"game":"CS2","width":1920,"height":1080,"has_voice":true}`
	if _, err := postClip(t, nk, "gc-alice", p); err != nil {
		t.Fatal(err)
	}
	// A voice clip keeps no game fields, even when the client sends them.
	v := `{"crew_id":"gc-crew-1","clip_id":"v1","duration_seconds":10,"width":1920,"height":1080,"has_voice":true}`
	if _, err := postClip(t, nk, "gc-alice", v); err != nil {
		t.Fatal(err)
	}
	doc, _ := readClipsDoc(context.Background(), nk, "gc-crew-1")
	if len(doc.Clips) != 2 {
		t.Fatalf("clips: %+v", doc.Clips)
	}
	g := doc.Clips[0]
	if g.ClipType != clipTypeGame || g.Width != 1920 || g.Height != 1080 || !g.HasVoice || g.Game != "CS2" {
		t.Fatalf("game clip: %+v", g)
	}
	vc := doc.Clips[1]
	if vc.ClipType != clipTypeVoice || vc.Width != 0 || vc.Height != 0 || vc.HasVoice {
		t.Fatalf("voice clip: %+v", vc)
	}
	raw, _ := json.Marshal(vc)
	for _, f := range []string{"width", "height", "has_voice", "poster_url"} {
		if strings.Contains(string(raw), `"`+f+`"`) {
			t.Fatalf("voice clip JSON must omit %s: %s", f, raw)
		}
	}
}

func TestPostClipRejectsBadGameFields(t *testing.T) {
	nk := gameClipFixture("gc-crew-2")
	bad := []string{
		`{"crew_id":"gc-crew-2","clip_id":"g1","clip_type":"game","duration_seconds":30,"height":1080}`,
		`{"crew_id":"gc-crew-2","clip_id":"g/1","clip_type":"game","duration_seconds":30,"width":1920,"height":1080}`,
		`{"crew_id":"gc-crew-2","clip_id":"g1","clip_type":"screen","duration_seconds":30}`,
	}
	for _, p := range bad {
		if _, err := postClip(t, nk, "gc-alice", p); errCode(err) != 3 {
			t.Errorf("%s: got %v, want code 3", p, err)
		}
	}
}

func TestPostClipRetryReturnsTheStoredClip(t *testing.T) {
	nk := gameClipFixture("gc-crew-3")
	p := `{"crew_id":"gc-crew-3","clip_id":"g1","clip_type":"game","duration_seconds":30,"width":1280,"height":720}`
	first, err := postClip(t, nk, "gc-alice", p)
	if err != nil {
		t.Fatal(err)
	}
	again, err := postClip(t, nk, "gc-alice", p)
	if err != nil {
		t.Fatal(err)
	}
	if again["event_id"] != first["event_id"] || again["clip_id"] != "g1" || again["success"] != true {
		t.Fatalf("retry: got %+v, first %+v", again, first)
	}
	doc, _ := readClipsDoc(context.Background(), nk, "gc-crew-3")
	if len(doc.Clips) != 1 {
		t.Fatalf("a retry must append nothing, got %d clips", len(doc.Clips))
	}
	// The same clip_id from another actor is taken.
	if _, err := postClip(t, nk, "gc-bob", p); errCode(err) != 6 {
		t.Fatalf("other actor: got %v, want code 6", err)
	}
}

func TestPostClipRetryDoesNotCountAgainstTheDailyLimit(t *testing.T) {
	nk := gameClipFixture("gc-crew-4")
	for i := 0; i < ClipMaxPerUserPerDay; i++ {
		p := fmt.Sprintf(`{"crew_id":"gc-crew-4","clip_id":"v%d","duration_seconds":5}`, i)
		if _, err := postClip(t, nk, "gc-alice", p); err != nil {
			t.Fatalf("clip %d: %v", i, err)
		}
	}
	if _, err := postClip(t, nk, "gc-alice", `{"crew_id":"gc-crew-4","clip_id":"over","duration_seconds":5}`); errCode(err) != 8 {
		t.Fatalf("over the limit: got %v, want code 8", err)
	}
	if _, err := postClip(t, nk, "gc-alice", `{"crew_id":"gc-crew-4","clip_id":"v0","duration_seconds":5}`); err != nil {
		t.Fatalf("a retry at the limit must succeed: %v", err)
	}
}

// ---- clip_upload_url ----

func TestGameClipPartPlan(t *testing.T) {
	cases := []struct {
		bytes int64
		want  []int64
	}{
		{1, []int64{1}},
		{5 * mib, []int64{5 * mib}},
		{5*mib + 1, []int64{5 * mib, 1}},
		{64 * mib, []int64{5 * mib, 5 * mib, 5 * mib, 5 * mib, 5 * mib, 5 * mib,
			5 * mib, 5 * mib, 5 * mib, 5 * mib, 5 * mib, 5 * mib, 4 * mib}},
	}
	for _, c := range cases {
		got, ok := gameClipPartPlan(c.bytes)
		if !ok || fmt.Sprint(got) != fmt.Sprint(c.want) {
			t.Errorf("%d bytes: got %v ok=%t, want %v", c.bytes, got, ok, c.want)
		}
		sum := int64(0)
		for _, n := range got {
			sum += n
		}
		if sum != c.bytes {
			t.Errorf("%d bytes: parts sum to %d", c.bytes, sum)
		}
	}
	if len(mustPlan(t, 64*mib)) != gameClipMaxParts || gameClipMaxParts != 13 {
		t.Fatalf("gameClipMaxParts = %d, want 13", gameClipMaxParts)
	}
	for _, b := range []int64{0, -1, 64*mib + 1} {
		if got, ok := gameClipPartPlan(b); ok {
			t.Errorf("%d bytes must be rejected, got %v", b, got)
		}
	}
}

func mustPlan(t *testing.T, n int64) []int64 {
	t.Helper()
	p, ok := gameClipPartPlan(n)
	if !ok {
		t.Fatalf("plan %d rejected", n)
	}
	return p
}

func TestValidateGameClipUploadRequest(t *testing.T) {
	req := func(poster, video int64) ClipUploadURLRequest {
		return ClipUploadURLRequest{ClipID: "g1", CrewID: "c", ClipType: clipTypeGame, PosterBytes: poster, VideoBytes: video}
	}
	cases := []struct {
		name string
		req  ClipUploadURLRequest
		ok   bool
	}{
		{"min", req(1, 1), true},
		{"max", req(mib, 64*mib), true},
		{"poster 0", req(0, mib), false},
		{"poster over", req(mib+1, mib), false},
		{"video 0", req(1000, 0), false},
		{"video over", req(1000, 64*mib+1), false},
		{"bad clip_id", ClipUploadURLRequest{ClipID: "a/b", PosterBytes: 1, VideoBytes: 1}, false},
		{"long upload_id", ClipUploadURLRequest{ClipID: "g1", PosterBytes: 1, VideoBytes: 1, UploadID: strings.Repeat("u", 1025)}, false},
	}
	for _, c := range cases {
		if got := validateGameClipUploadRequest(c.req); (got == "") != c.ok {
			t.Errorf("%s: got %q, want ok=%t", c.name, got, c.ok)
		}
	}
}

func TestOwnGameClip(t *testing.T) {
	clips := []StoredClip{
		{ClipID: "g1", ActorID: "alice", ClipType: clipTypeGame},
		{ClipID: "v1", ActorID: "alice", ClipType: clipTypeVoice},
	}
	if c, err := ownGameClip(clips, "g1", "alice"); err != nil || c.ClipID != "g1" {
		t.Fatalf("own game clip: %+v %v", c, err)
	}
	for _, tc := range []struct {
		clipID, user string
		code         int
	}{
		{"g1", "bob", 7},
		{"nope", "alice", 5},
		{"v1", "alice", 3},
	} {
		if _, err := ownGameClip(clips, tc.clipID, tc.user); errCode(err) != tc.code {
			t.Errorf("%s by %s: got %v, want code %d", tc.clipID, tc.user, err, tc.code)
		}
	}
}

// testPresignClient signs for a dummy endpoint. Presigning makes no request.
func testPresignClient() *s3.PresignClient {
	return s3.NewPresignClient(newS3Client("http://127.0.0.1:9", "AKIDTEST", "secrettest"))
}

func signedHeaders(t *testing.T, raw string) (url.Values, []string) {
	t.Helper()
	u, err := url.Parse(raw)
	if err != nil {
		t.Fatalf("parse %q: %v", raw, err)
	}
	q := u.Query()
	return q, strings.Split(q.Get("X-Amz-SignedHeaders"), ";")
}

func hasAll(have []string, want ...string) bool {
	set := map[string]bool{}
	for _, h := range have {
		set[h] = true
	}
	for _, w := range want {
		if !set[w] {
			return false
		}
	}
	return true
}

// The design depends on this: a presigned part or poster URL accepts only a
// body of the signed length (plans/GAME-CLIPS.md 6.3, G6).
func TestPresignedURLsSignTheLength(t *testing.T) {
	p := testPresignClient()
	ctx := context.Background()

	partURL, err := presignUploadPart(ctx, p, "mello-clips", "crews/c/g1.mp4", "upload-1", 2, 5*mib, gameClipURLExpiry)
	if err != nil {
		t.Fatal(err)
	}
	q, signed := signedHeaders(t, partURL)
	if !hasAll(signed, "content-length", "host") {
		t.Fatalf("part URL must sign content-length, signed: %v", signed)
	}
	t.Logf("part URL signed headers: %v", signed)
	if q.Get("partNumber") != "2" || q.Get("uploadId") != "upload-1" || q.Get("X-Amz-Expires") != "3600" {
		t.Fatalf("part URL query: %v", q)
	}
	if !strings.Contains(partURL, "/mello-clips/crews/c/g1.mp4?") {
		t.Fatalf("part URL must be path style on the clip key: %s", partURL)
	}

	posterURL, err := presignPutSized(ctx, p, "mello-clips", "crews/c/g1.jpg", gameClipPosterType, gameClipCacheControl, 123456, gameClipURLExpiry)
	if err != nil {
		t.Fatal(err)
	}
	q, signed = signedHeaders(t, posterURL)
	if !hasAll(signed, "content-length", "content-type", "cache-control", "host") {
		t.Fatalf("poster URL must sign content-length, content-type, cache-control, signed: %v", signed)
	}
	t.Logf("poster URL signed headers: %v", signed)
	if q.Get("X-Amz-Expires") != "3600" {
		t.Fatalf("poster URL expiry: %v", q)
	}
	// A checksum the SDK computed over the empty presign body would reject
	// every real upload.
	for _, raw := range []string{partURL, posterURL} {
		lower := strings.ToLower(raw)
		if strings.Contains(lower, "x-amz-checksum") || strings.Contains(lower, "x-amz-sdk-checksum") {
			t.Fatalf("presigned URL carries a checksum: %s", raw)
		}
	}
}

func TestCheckClipObject(t *testing.T) {
	cases := []struct {
		name    string
		info    s3ObjectInfo
		max     int64
		want    string
		ok      bool
		overCap bool
	}{
		{"video ok", s3ObjectInfo{Size: 20 * mib, ContentType: "video/mp4"}, gameClipMaxVideoBytes, gameClipVideoType, true, false},
		{"video at cap", s3ObjectInfo{Size: 64 * mib, ContentType: "video/mp4"}, gameClipMaxVideoBytes, gameClipVideoType, true, false},
		{"video over cap", s3ObjectInfo{Size: 64*mib + 1, ContentType: "video/mp4"}, gameClipMaxVideoBytes, gameClipVideoType, false, true},
		{"poster ok", s3ObjectInfo{Size: 90_000, ContentType: "image/jpeg"}, gameClipMaxPosterBytes, gameClipPosterType, true, false},
		{"poster over cap", s3ObjectInfo{Size: mib + 1, ContentType: "image/jpeg"}, gameClipMaxPosterBytes, gameClipPosterType, false, true},
		{"empty", s3ObjectInfo{Size: 0, ContentType: "video/mp4"}, gameClipMaxVideoBytes, gameClipVideoType, false, false},
		{"wrong type", s3ObjectInfo{Size: 10, ContentType: "application/octet-stream"}, gameClipMaxVideoBytes, gameClipVideoType, false, false},
		{"no type", s3ObjectInfo{Size: 10}, gameClipMaxPosterBytes, gameClipPosterType, false, false},
		{"type with params", s3ObjectInfo{Size: 10, ContentType: "Video/MP4; codecs=avc1"}, gameClipMaxVideoBytes, gameClipVideoType, true, false},
	}
	for _, c := range cases {
		errStr, overCap := checkClipObject(c.info, c.max, c.want)
		if (errStr == "") != c.ok || overCap != c.overCap {
			t.Errorf("%s: got %q overCap=%t, want ok=%t overCap=%t", c.name, errStr, overCap, c.ok, c.overCap)
		}
	}
}

func TestCompletedClipParts(t *testing.T) {
	got, errStr := completedClipParts([]CompletedClipPart{
		{PartNumber: 3, ETag: `"c"`}, {PartNumber: 1, ETag: `"a"`}, {PartNumber: 2, ETag: `"b"`},
	})
	if errStr != "" || len(got) != 3 {
		t.Fatalf("got %v %q", got, errStr)
	}
	for i, p := range got {
		if aws.ToInt32(p.PartNumber) != int32(i+1) {
			t.Fatalf("parts must be sorted: %d at %d", aws.ToInt32(p.PartNumber), i)
		}
	}
	tooMany := make([]CompletedClipPart, gameClipMaxParts+1)
	for i := range tooMany {
		tooMany[i] = CompletedClipPart{PartNumber: int32(i + 1), ETag: "e"}
	}
	bad := map[string][]CompletedClipPart{
		"empty":     nil,
		"too many":  tooMany,
		"zero":      {{PartNumber: 0, ETag: "e"}},
		"over":      {{PartNumber: int32(gameClipMaxParts + 1), ETag: "e"}},
		"duplicate": {{PartNumber: 1, ETag: "e"}, {PartNumber: 1, ETag: "f"}},
		"no etag":   {{PartNumber: 1}},
	}
	for name, parts := range bad {
		if _, errStr := completedClipParts(parts); errStr == "" {
			t.Errorf("%s must be rejected", name)
		}
	}
}

func TestStaleMultipartUploads(t *testing.T) {
	now := time.Date(2026, time.November, 2, 12, 0, 0, 0, time.UTC)
	cutoff := now.Add(-gameClipUploadMaxAge)
	at := func(key string, ts time.Time) types.MultipartUpload {
		return types.MultipartUpload{Key: aws.String(key), UploadId: aws.String("u-" + key), Initiated: aws.Time(ts)}
	}
	uploads := []types.MultipartUpload{
		at("crews/c/old.mp4", now.Add(-25*time.Hour)),
		at("crews/c/young.mp4", now.Add(-23*time.Hour)),
		at("crews/c/edge.mp4", cutoff),
		at("other/old.mp4", now.Add(-48*time.Hour)),
		{Key: aws.String("crews/c/no-time.mp4"), UploadId: aws.String("u")},
	}
	got := staleMultipartUploads(uploads, gameClipUploadPrefix, cutoff)
	if len(got) != 1 || aws.ToString(got[0].Key) != "crews/c/old.mp4" {
		t.Fatalf("stale: %+v", got)
	}
}

// ---- RPCs without S3 (the test process has no S3 env) ----

func requireNoS3(t *testing.T) {
	t.Helper()
	if S3IsConfigured() {
		t.Skip("S3 env is set; these cases cover the unconfigured path")
	}
}

func TestGameClipUploadURLChecksTheClipThenAnswersEmptyWithoutS3(t *testing.T) {
	requireNoS3(t)
	nk := gameClipFixture("gc-crew-5")
	if _, err := postClip(t, nk, "gc-alice", `{"crew_id":"gc-crew-5","clip_id":"g1","clip_type":"game","duration_seconds":30,"width":1280,"height":720}`); err != nil {
		t.Fatal(err)
	}
	if _, err := postClip(t, nk, "gc-alice", `{"crew_id":"gc-crew-5","clip_id":"v1","duration_seconds":10}`); err != nil {
		t.Fatal(err)
	}
	call := func(user, p string) (string, error) {
		return ClipUploadURLRPC(ctxWithUser(user), testLogger(), nil, nk, p)
	}
	const ok = `{"crew_id":"gc-crew-5","clip_id":"g1","clip_type":"game","poster_bytes":90000,"video_bytes":20000000}`
	for _, tc := range []struct {
		user, payload string
		code          int
	}{
		{"gc-bob", ok, 7},
		{"gc-alice", `{"crew_id":"gc-crew-5","clip_id":"nope","clip_type":"game","poster_bytes":1,"video_bytes":1}`, 5},
		{"gc-alice", `{"crew_id":"gc-crew-5","clip_id":"v1","clip_type":"game","poster_bytes":1,"video_bytes":1}`, 3},
		{"gc-alice", `{"crew_id":"gc-crew-5","clip_id":"g1","clip_type":"game","poster_bytes":0,"video_bytes":1}`, 3},
		{"gc-alice", `{"crew_id":"gc-crew-5","clip_id":"g1","clip_type":"game","poster_bytes":1,"video_bytes":67108865}`, 3},
		{"gc-alice", `{"crew_id":"gc-crew-5","clip_id":"g1","clip_type":"screen"}`, 3},
		{"gc-outsider", ok, 7},
	} {
		if _, err := call(tc.user, tc.payload); errCode(err) != tc.code {
			t.Errorf("%s %s: got %v, want code %d", tc.user, tc.payload, err, tc.code)
		}
	}
	out, err := call("gc-alice", ok)
	if err != nil {
		t.Fatal(err)
	}
	want := `{"poster_upload_url":"","poster_url":"","upload_id":"","part_size":0,"parts":[],"media_url":"","cache_control":""}`
	if out != want {
		t.Fatalf("empty answer:\n got %s\nwant %s", out, want)
	}
	// The voice path answers as before.
	out, err = call("gc-alice", `{"crew_id":"gc-crew-5","clip_id":"v1"}`)
	if err != nil || out != `{"upload_url":"","media_url":""}` {
		t.Fatalf("voice: %s %v", out, err)
	}
}

func TestGameClipUploadCompleteChecksTheRequestAndTheClip(t *testing.T) {
	requireNoS3(t)
	nk := gameClipFixture("gc-crew-6")
	if _, err := postClip(t, nk, "gc-alice", `{"crew_id":"gc-crew-6","clip_id":"g1","clip_type":"game","duration_seconds":30,"width":1280,"height":720}`); err != nil {
		t.Fatal(err)
	}
	call := func(user, p string) error {
		_, err := ClipUploadCompleteRPC(ctxWithUser(user), testLogger(), nil, nk, p)
		return err
	}
	for _, tc := range []struct {
		user, payload string
		code          int
	}{
		{"gc-alice", `{"crew_id":"gc-crew-6","clip_id":"g1","clip_type":"game","part":"thumb"}`, 3},
		{"gc-alice", `{"crew_id":"gc-crew-6","clip_id":"g1","clip_type":"game","part":"video","parts":[{"part_number":1,"etag":"e"}]}`, 3},
		{"gc-alice", `{"crew_id":"gc-crew-6","clip_id":"g1","clip_type":"game","part":"video","upload_id":"u"}`, 3},
		{"gc-bob", `{"crew_id":"gc-crew-6","clip_id":"g1","clip_type":"game","part":"poster"}`, 7},
		{"gc-alice", `{"crew_id":"gc-crew-6","clip_id":"g2","clip_type":"game","part":"poster"}`, 5},
		// The clip is the caller's game clip, but there is no store to check.
		{"gc-alice", `{"crew_id":"gc-crew-6","clip_id":"g1","clip_type":"game","part":"poster"}`, 9},
	} {
		if err := call(tc.user, tc.payload); errCode(err) != tc.code {
			t.Errorf("%s %s: got %v, want code %d", tc.user, tc.payload, err, tc.code)
		}
	}
}
