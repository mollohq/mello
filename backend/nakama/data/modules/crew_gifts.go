package main

import (
	"context"
	"crypto/rand"
	"database/sql"
	"encoding/json"
	"fmt"
	"math/big"
	"os"
	"regexp"
	"strings"
	"time"

	"github.com/heroiclabs/nakama-common/runtime"
)

// The gifts of the crew quests (plans/crew-quests.md section 4):
//
//	banner  crew made     always open
//	link    a friend joins
//	emoji   first call
//	week    first clip    the founding week never locks in Crew history
//	plus    first session m3llo+ for 30 days for every member
//
// A crew made before quests shipped has no record. It gets the banner, the
// link and the emoji at once, and no week and no m3llo+.

const (
	giftBanner = "banner"
	giftLink   = "link"
	giftEmoji  = "emoji"
	giftWeek   = "week"
	giftPlus   = "plus"
)

// giftOpen reports whether a gift is unlocked for a crew with record q (nil
// when the crew has none).
func giftOpen(q *CrewQuests, gift string) bool {
	switch gift {
	case giftBanner:
		return true
	case giftLink:
		return q == nil || q.Steps.Friend != nil
	case giftEmoji:
		return q == nil || q.Steps.Call != nil
	case giftWeek:
		return q != nil && q.Steps.Clip != nil
	case giftPlus:
		return q != nil && q.Steps.Session != nil
	}
	return false
}

// requireGift reads the record and refuses a locked gift.
func requireGift(ctx context.Context, nk runtime.NakamaModule, crewID, gift string) error {
	q, _ := readCrewQuests(ctx, nk, crewID)
	if !giftOpen(q, gift) {
		return runtime.NewError("gift not unlocked", 9)
	}
	return nil
}

// isCrewOwner is the superadmin of the crew: the member who made it.
func isCrewOwner(ctx context.Context, nk runtime.NakamaModule, crewID, userID string) bool {
	state, err := callerCrewState(ctx, nk, userID, crewID)
	return err == nil && state != nil && *state == 0
}

// retryWait is the back-off of a version conflict, as the ledger uses.
func retryWait(attempt int) {
	jitter, _ := rand.Int(rand.Reader, big.NewInt(50))
	time.Sleep(time.Duration(50*(attempt+1)+int(jitter.Int64())) * time.Millisecond)
}

// ---------------------------------------------------------------------------
// The crew profile: the banner and the custom invite name
// ---------------------------------------------------------------------------

const CrewProfileCollection = "crew_profile"

// CrewProfile is crew_profile/{crew_id}, system-owned.
type CrewProfile struct {
	CrewID     string      `json:"crew_id"`
	Banner     *CrewBanner `json:"banner,omitempty"`
	InviteName string      `json:"invite_name,omitempty"`
}

// CrewBanner is the art behind the crew name. Kind game is the wide hero art
// of a game from the art Worker; kind upload is a JPEG on the media bucket.
type CrewBanner struct {
	Kind    string `json:"kind"`
	GameID  string `json:"game_id,omitempty"`
	Key     string `json:"key,omitempty"`
	URL     string `json:"url"`
	Version int64  `json:"version"`
}

func readCrewProfile(ctx context.Context, nk runtime.NakamaModule, crewID string) (*CrewProfile, string) {
	objects, err := nk.StorageRead(ctx, []*runtime.StorageRead{
		{Collection: CrewProfileCollection, Key: crewID, UserID: SystemUserID},
	})
	if err != nil || len(objects) == 0 {
		return &CrewProfile{CrewID: crewID}, ""
	}
	var p CrewProfile
	if err := json.Unmarshal([]byte(objects[0].GetValue()), &p); err != nil {
		return &CrewProfile{CrewID: crewID}, ""
	}
	p.CrewID = crewID
	return &p, objects[0].GetVersion()
}

// updateCrewProfile applies change to the profile and writes it with the
// version check, three tries. change returns an error to stop.
func updateCrewProfile(ctx context.Context, nk runtime.NakamaModule, crewID string, change func(*CrewProfile) error) (*CrewProfile, error) {
	for attempt := 0; attempt < 3; attempt++ {
		p, version := readCrewProfile(ctx, nk, crewID)
		if err := change(p); err != nil {
			return nil, err
		}
		if version == "" {
			version = "*"
		}
		data, _ := json.Marshal(p)
		_, err := nk.StorageWrite(ctx, []*runtime.StorageWrite{{
			Collection: CrewProfileCollection, Key: crewID, UserID: SystemUserID,
			Value: string(data), Version: version, PermissionRead: 0, PermissionWrite: 0,
		}})
		if err == nil {
			return p, nil
		}
		retryWait(attempt)
	}
	return nil, fmt.Errorf("crew_profile write failed after 3 retries for crew %s", crewID)
}

// ---------------------------------------------------------------------------
// Gift 1: the crew banner
// ---------------------------------------------------------------------------

const (
	maxBannerBytes  = 400 << 10
	mediaUploadTTL  = 15 * time.Minute
	mediaImmutable  = "public, max-age=31536000, immutable"
	artBaseDefault  = "https://art.m3llo.app"
	bannerKeyPrefix = "crew-banners/"
	emojiKeyPrefix  = "crew-emoji/"
)

var bannerGameIDRe = regexp.MustCompile(`^(igdb|steam|epic|gog)-[A-Za-z0-9_.-]{1,64}$`)

// artBase is the art Worker, as the client reads it (GAME-ART.md section 3).
func artBase() string {
	if b := strings.TrimRight(os.Getenv("MELLO_ART_BASE"), "/"); b != "" {
		return b
	}
	return artBaseDefault
}

// mediaURLFor is the public URL of a media bucket key, "" when the bucket is
// off. A var so a test can give the gifts a bucket.
var mediaURLFor = ChatPublicURL

// newMediaID is 16 random bytes as hex.
func newMediaID() (string, error) {
	return newChatImageID()
}

// mediaKeyOf reports whether key is one object under prefix/{crew}/ with
// one of the extensions: no extra segment, no path trick.
func mediaKeyOf(key, prefix, crewID string, exts ...string) bool {
	rest, ok := strings.CutPrefix(key, prefix+crewID+"/")
	if !ok || rest == "" || strings.Contains(rest, "/") || strings.Contains(rest, "..") {
		return false
	}
	for _, e := range exts {
		if strings.HasSuffix(rest, "."+e) && len(rest) > len(e)+1 {
			return true
		}
	}
	return false
}

// presignMedia is a presigned PUT on the media bucket with a signed length.
func presignMedia(ctx context.Context, key, mime string, size int64) (string, error) {
	if !ChatImagesConfigured() {
		return "", fmt.Errorf("media bucket not configured")
	}
	return presignPutSized(ctx, chatPresigner, chatBucket, key, mime, mediaImmutable, size, mediaUploadTTL)
}

type MediaUploadURLRequest struct {
	CrewID string `json:"crew_id"`
	Mime   string `json:"mime"`
	Bytes  int64  `json:"bytes"`
}

type MediaUploadURLResponse struct {
	UploadURL string `json:"upload_url"`
	Key       string `json:"key"`
}

// CrewBannerUploadURLRPC gives the owner a slot for a banner JPEG.
func CrewBannerUploadURLRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}
	var req MediaUploadURLRequest
	if err := json.Unmarshal([]byte(payload), &req); err != nil || req.CrewID == "" {
		return "", runtime.NewError("crew_id required", 3)
	}
	if req.Mime != "image/jpeg" {
		return "", runtime.NewError("a banner is a JPEG", 3)
	}
	if req.Bytes <= 0 || req.Bytes > maxBannerBytes {
		return "", runtime.NewError("banner too large", 3)
	}
	if !isCrewOwner(ctx, nk, req.CrewID, userID) {
		return "", runtime.NewError("only the owner sets the banner", 7)
	}
	id, err := newMediaID()
	if err != nil {
		return "", runtime.NewError("failed to generate upload URL", 13)
	}
	key := bannerKeyPrefix + req.CrewID + "/" + id + ".jpg"
	url, err := presignMedia(ctx, key, req.Mime, req.Bytes)
	if err != nil {
		logger.Error("crew_banner_upload_url: %v", err)
		return "", runtime.NewError("failed to generate upload URL", 13)
	}
	data, _ := json.Marshal(MediaUploadURLResponse{UploadURL: url, Key: key})
	return string(data), nil
}

type SetCrewBannerRequest struct {
	CrewID string `json:"crew_id"`
	Kind   string `json:"kind"` // game | upload
	GameID string `json:"game_id,omitempty"`
	Key    string `json:"key,omitempty"`
}

// bannerFor validates a request and returns the banner it sets.
func bannerFor(req SetCrewBannerRequest, nowMs int64) (*CrewBanner, error) {
	switch req.Kind {
	case "game":
		if !bannerGameIDRe.MatchString(req.GameID) {
			return nil, runtime.NewError("invalid game_id", 3)
		}
		return &CrewBanner{Kind: "game", GameID: req.GameID, URL: artBase() + "/" + req.GameID + "/hero", Version: nowMs}, nil
	case "upload":
		if !mediaKeyOf(req.Key, bannerKeyPrefix, req.CrewID, "jpg") {
			return nil, runtime.NewError("invalid key", 3)
		}
		url := mediaURLFor(req.Key)
		if url == "" {
			return nil, runtime.NewError("media bucket not configured", 9)
		}
		return &CrewBanner{Kind: "upload", Key: req.Key, URL: url, Version: nowMs}, nil
	}
	return nil, runtime.NewError("kind must be game or upload", 3)
}

// SetCrewBannerRPC sets the banner. Owner only. An uploaded banner it
// replaces is deleted from the bucket.
func SetCrewBannerRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}
	var req SetCrewBannerRequest
	if err := json.Unmarshal([]byte(payload), &req); err != nil || req.CrewID == "" {
		return "", runtime.NewError("crew_id required", 3)
	}
	banner, err := bannerFor(req, time.Now().UnixMilli())
	if err != nil {
		return "", err
	}
	if !isCrewOwner(ctx, nk, req.CrewID, userID) {
		return "", runtime.NewError("only the owner sets the banner", 7)
	}
	if err := requireGift(ctx, nk, req.CrewID, giftBanner); err != nil {
		return "", err
	}
	return storeBanner(ctx, logger, nk, req.CrewID, banner)
}

// ClearCrewBannerRPC removes the banner. Owner only.
func ClearCrewBannerRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}
	var req struct {
		CrewID string `json:"crew_id"`
	}
	if err := json.Unmarshal([]byte(payload), &req); err != nil || req.CrewID == "" {
		return "", runtime.NewError("crew_id required", 3)
	}
	if !isCrewOwner(ctx, nk, req.CrewID, userID) {
		return "", runtime.NewError("only the owner sets the banner", 7)
	}
	return storeBanner(ctx, logger, nk, req.CrewID, nil)
}

// storeBanner writes the banner (nil clears it) and deletes an uploaded
// object it replaces.
func storeBanner(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule, crewID string, banner *CrewBanner) (string, error) {
	oldKey := ""
	p, err := updateCrewProfile(ctx, nk, crewID, func(p *CrewProfile) error {
		oldKey = ""
		if p.Banner != nil && p.Banner.Kind == "upload" {
			oldKey = p.Banner.Key
		}
		p.Banner = banner
		return nil
	})
	if err != nil {
		logger.Error("crew banner: %v", err)
		return "", runtime.NewError("failed to store banner", 13)
	}
	if oldKey != "" && (banner == nil || banner.Key != oldKey) && ChatImagesConfigured() {
		if err := DeleteChatObjects(ctx, []string{oldKey}); err != nil {
			logger.Warn("crew banner: delete %s: %v", oldKey, err)
		}
	}
	InvalidateCrewState(crewID)
	pushCrewGiftsChanged(ctx, logger, nk, crewID, "banner")
	data, _ := json.Marshal(map[string]interface{}{"banner": p.Banner})
	return string(data), nil
}

// ---------------------------------------------------------------------------
// Gift 2: the custom invite link
// ---------------------------------------------------------------------------

const inviteNameHoldMs = 30 * dayMs

var inviteNameRe = regexp.MustCompile(`^[a-z0-9][a-z0-9-]{1,22}[a-z0-9]$`)

// inviteNamesNotAllowed are names a crew cannot take.
var inviteNamesNotAllowed = map[string]bool{
	"m3llo": true, "mello": true, "mollo": true, "admin": true, "support": true,
	"help": true, "api": true, "join": true, "invite": true, "official": true,
	"staff": true, "mod": true, "moderator": true, "system": true, "lounge": true,
}

// inviteCodeObject is the value of an invite_codes object. A custom name has
// Custom set. HeldUntil is set when the crew moved to another name: the old
// name still leads to the crew until then, and after it any crew can take it.
type inviteCodeObject struct {
	CrewID        string `json:"crew_id"`
	InviterUserID string `json:"inviter_user_id,omitempty"`
	Custom        bool   `json:"custom,omitempty"`
	HeldUntil     int64  `json:"held_until,omitempty"`
}

// normalizeInviteName lower-cases a name as the crew typed it.
func normalizeInviteName(name string) string {
	return strings.ToLower(strings.TrimSpace(name))
}

// inviteCodeShapeRe is the shape of a generated code with or without its
// dash. A name in that shape is refused: a client turns "abcd1234" into
// the code "ABCD-1234", which is another key (deep_link.rs).
var inviteCodeShapeRe = regexp.MustCompile(`^[a-z0-9]{4}-?[a-z0-9]{4}$`)

// inviteNameAllowed checks the form and the list of names not allowed.
func inviteNameAllowed(name string) bool {
	return inviteNameRe.MatchString(name) && !strings.Contains(name, "--") &&
		!inviteNamesNotAllowed[name] && !inviteCodeShapeRe.MatchString(name)
}

// inviteNameState is free, taken or not_allowed for crewID, and the storage
// version to write the name with when it is free.
func inviteNameState(ctx context.Context, nk runtime.NakamaModule, crewID, name string, nowMs int64) (string, string) {
	if !inviteNameAllowed(name) {
		return "not_allowed", ""
	}
	objects, err := nk.StorageRead(ctx, []*runtime.StorageRead{
		{Collection: InviteCodeCollection, Key: strings.ToUpper(name), UserID: SystemUserID},
	})
	if err != nil {
		return "taken", ""
	}
	if len(objects) == 0 {
		return "free", "*"
	}
	var o inviteCodeObject
	_ = json.Unmarshal([]byte(objects[0].GetValue()), &o)
	if o.CrewID == crewID {
		return "free", objects[0].GetVersion()
	}
	if o.Custom && o.HeldUntil > 0 && o.HeldUntil <= nowMs {
		return "free", objects[0].GetVersion()
	}
	return "taken", ""
}

// CheckInviteNameRPC says if a name is free for the crew.
func CheckInviteNameRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}
	var req struct {
		CrewID string `json:"crew_id"`
		Name   string `json:"name"`
	}
	if err := json.Unmarshal([]byte(payload), &req); err != nil || req.CrewID == "" {
		return "", runtime.NewError("crew_id required", 3)
	}
	if !isCrewMember(ctx, nk, req.CrewID, userID) {
		return "", runtime.NewError("not a crew member", 7)
	}
	state, _ := inviteNameState(ctx, nk, req.CrewID, normalizeInviteName(req.Name), time.Now().UnixMilli())
	data, _ := json.Marshal(map[string]string{"state": state})
	return string(data), nil
}

// SetInviteNameRPC gives the crew its custom invite name. Owner only, gift
// open. The old name stays the crew's for inviteNameHoldMs.
func SetInviteNameRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}
	var req struct {
		CrewID string `json:"crew_id"`
		Name   string `json:"name"`
	}
	if err := json.Unmarshal([]byte(payload), &req); err != nil || req.CrewID == "" {
		return "", runtime.NewError("crew_id required", 3)
	}
	name := normalizeInviteName(req.Name)
	if !isCrewOwner(ctx, nk, req.CrewID, userID) {
		return "", runtime.NewError("only the owner sets the invite link", 7)
	}
	if err := requireGift(ctx, nk, req.CrewID, giftLink); err != nil {
		return "", err
	}
	nowMs := time.Now().UnixMilli()
	state, version := inviteNameState(ctx, nk, req.CrewID, name, nowMs)
	switch state {
	case "not_allowed":
		return "", runtime.NewError("name not allowed", 3)
	case "taken":
		return "", runtime.NewError("name taken", 6)
	}
	value, _ := json.Marshal(inviteCodeObject{CrewID: req.CrewID, Custom: true})
	if _, err := nk.StorageWrite(ctx, []*runtime.StorageWrite{{
		Collection: InviteCodeCollection, Key: strings.ToUpper(name), UserID: SystemUserID,
		Value: string(value), Version: version, PermissionRead: 2, PermissionWrite: 0,
	}}); err != nil {
		return "", runtime.NewError("name taken", 6)
	}
	old := ""
	if _, err := updateCrewProfile(ctx, nk, req.CrewID, func(p *CrewProfile) error {
		old = p.InviteName
		p.InviteName = name
		return nil
	}); err != nil {
		logger.Error("set_invite_name: %v", err)
		return "", runtime.NewError("failed to store invite link", 13)
	}
	if old != "" && old != name {
		holdInviteName(ctx, logger, nk, req.CrewID, old, nowMs+inviteNameHoldMs)
	}
	InvalidateCrewState(req.CrewID)
	pushCrewGiftsChanged(ctx, logger, nk, req.CrewID, "link")
	logger.Info("crew %s invite name %q (was %q)", req.CrewID, name, old)
	data, _ := json.Marshal(map[string]string{"invite_name": name})
	return string(data), nil
}

// holdInviteName keeps an old name with the crew until heldUntil.
func holdInviteName(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule, crewID, name string, heldUntil int64) {
	key := strings.ToUpper(name)
	objects, err := nk.StorageRead(ctx, []*runtime.StorageRead{{Collection: InviteCodeCollection, Key: key, UserID: SystemUserID}})
	if err != nil || len(objects) == 0 {
		return
	}
	var o inviteCodeObject
	if json.Unmarshal([]byte(objects[0].GetValue()), &o) != nil || o.CrewID != crewID {
		return
	}
	o.HeldUntil = heldUntil
	value, _ := json.Marshal(o)
	if _, err := nk.StorageWrite(ctx, []*runtime.StorageWrite{{
		Collection: InviteCodeCollection, Key: key, UserID: SystemUserID,
		Value: string(value), Version: objects[0].GetVersion(), PermissionRead: 2, PermissionWrite: 0,
	}}); err != nil {
		logger.Warn("hold invite name %s: %v", name, err)
	}
}

// ---------------------------------------------------------------------------
// Gift 3: crew emoji
// ---------------------------------------------------------------------------

const (
	CrewEmojiCollection = "crew_emoji"
	crewEmojiSlots      = 5
	maxCrewEmojiBytes   = 256 << 10
)

var crewEmojiNameRe = regexp.MustCompile(`^[a-z0-9_]{2,24}$`)

// CrewEmoji is one emoji of the crew.
type CrewEmoji struct {
	ID          string `json:"id"`
	Name        string `json:"name"`
	URL         string `json:"url"`
	Key         string `json:"key"`
	AddedBy     string `json:"added_by"`
	AddedByName string `json:"added_by_name,omitempty"`
	AddedMs     int64  `json:"added_ms"`
}

// CrewEmojiDoc is crew_emoji/{crew_id}, system-owned.
type CrewEmojiDoc struct {
	CrewID string      `json:"crew_id"`
	Emoji  []CrewEmoji `json:"emoji"`
}

func readCrewEmoji(ctx context.Context, nk runtime.NakamaModule, crewID string) (*CrewEmojiDoc, string) {
	objects, err := nk.StorageRead(ctx, []*runtime.StorageRead{
		{Collection: CrewEmojiCollection, Key: crewID, UserID: SystemUserID},
	})
	if err != nil || len(objects) == 0 {
		return &CrewEmojiDoc{CrewID: crewID, Emoji: []CrewEmoji{}}, ""
	}
	var d CrewEmojiDoc
	if err := json.Unmarshal([]byte(objects[0].GetValue()), &d); err != nil {
		return &CrewEmojiDoc{CrewID: crewID, Emoji: []CrewEmoji{}}, ""
	}
	if d.Emoji == nil {
		d.Emoji = []CrewEmoji{}
	}
	return &d, objects[0].GetVersion()
}

func updateCrewEmoji(ctx context.Context, nk runtime.NakamaModule, crewID string, change func(*CrewEmojiDoc) error) (*CrewEmojiDoc, error) {
	for attempt := 0; attempt < 3; attempt++ {
		d, version := readCrewEmoji(ctx, nk, crewID)
		if err := change(d); err != nil {
			return nil, err
		}
		if version == "" {
			version = "*"
		}
		d.CrewID = crewID
		data, _ := json.Marshal(d)
		_, err := nk.StorageWrite(ctx, []*runtime.StorageWrite{{
			Collection: CrewEmojiCollection, Key: crewID, UserID: SystemUserID,
			Value: string(data), Version: version, PermissionRead: 0, PermissionWrite: 0,
		}})
		if err == nil {
			return d, nil
		}
		retryWait(attempt)
	}
	return nil, fmt.Errorf("crew_emoji write failed after 3 retries for crew %s", crewID)
}

// crewEmojiToken is the name in a `:name:` token, or "".
func crewEmojiToken(s string) string {
	if len(s) < 4 || s[0] != ':' || s[len(s)-1] != ':' {
		return ""
	}
	name := s[1 : len(s)-1]
	if !crewEmojiNameRe.MatchString(name) {
		return ""
	}
	return name
}

func (d *CrewEmojiDoc) has(name string) bool {
	for _, e := range d.Emoji {
		if e.Name == name {
			return true
		}
	}
	return false
}

// CrewEmojiUploadURLRPC gives a member a slot for one emoji image.
func CrewEmojiUploadURLRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}
	var req MediaUploadURLRequest
	if err := json.Unmarshal([]byte(payload), &req); err != nil || req.CrewID == "" {
		return "", runtime.NewError("crew_id required", 3)
	}
	ext, ok := chatImageExt(req.Mime)
	if !ok {
		return "", runtime.NewError("unsupported image type", 3)
	}
	if req.Bytes <= 0 || req.Bytes > maxCrewEmojiBytes {
		return "", runtime.NewError("emoji too large", 3)
	}
	if !isCrewMember(ctx, nk, req.CrewID, userID) {
		return "", runtime.NewError("not a crew member", 7)
	}
	if err := requireGift(ctx, nk, req.CrewID, giftEmoji); err != nil {
		return "", err
	}
	if d, _ := readCrewEmoji(ctx, nk, req.CrewID); len(d.Emoji) >= crewEmojiSlots {
		return "", runtime.NewError("no free emoji slot", 9)
	}
	id, err := newMediaID()
	if err != nil {
		return "", runtime.NewError("failed to generate upload URL", 13)
	}
	key := emojiKeyPrefix + req.CrewID + "/" + id + "." + ext
	url, err := presignMedia(ctx, key, req.Mime, req.Bytes)
	if err != nil {
		logger.Error("crew_emoji_upload_url: %v", err)
		return "", runtime.NewError("failed to generate upload URL", 13)
	}
	data, _ := json.Marshal(MediaUploadURLResponse{UploadURL: url, Key: key})
	return string(data), nil
}

// CrewEmojiAddRPC adds an uploaded image as a crew emoji. Any member.
func CrewEmojiAddRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}
	var req struct {
		CrewID string `json:"crew_id"`
		Name   string `json:"name"`
		Key    string `json:"key"`
	}
	if err := json.Unmarshal([]byte(payload), &req); err != nil || req.CrewID == "" {
		return "", runtime.NewError("crew_id required", 3)
	}
	name := strings.ToLower(strings.TrimSpace(req.Name))
	if !crewEmojiNameRe.MatchString(name) {
		return "", runtime.NewError("name is 2 to 24 letters, digits or underscores", 3)
	}
	if !mediaKeyOf(req.Key, emojiKeyPrefix, req.CrewID, "jpg", "png", "gif") {
		return "", runtime.NewError("invalid key", 3)
	}
	if !isCrewMember(ctx, nk, req.CrewID, userID) {
		return "", runtime.NewError("not a crew member", 7)
	}
	if err := requireGift(ctx, nk, req.CrewID, giftEmoji); err != nil {
		return "", err
	}
	url := mediaURLFor(req.Key)
	if url == "" {
		return "", runtime.NewError("media bucket not configured", 9)
	}
	id, err := newMediaID()
	if err != nil {
		return "", runtime.NewError("failed to add emoji", 13)
	}
	e := CrewEmoji{ID: id, Name: name, URL: url, Key: req.Key, AddedBy: userID, AddedByName: resolveUsername(ctx, nk, userID), AddedMs: time.Now().UnixMilli()}
	d, err := updateCrewEmoji(ctx, nk, req.CrewID, func(d *CrewEmojiDoc) error {
		if len(d.Emoji) >= crewEmojiSlots {
			return runtime.NewError("no free emoji slot", 9)
		}
		if d.has(name) {
			return runtime.NewError("name taken", 6)
		}
		d.Emoji = append(d.Emoji, e)
		return nil
	})
	if err != nil {
		if _, ok := err.(*runtime.Error); ok {
			return "", err
		}
		logger.Error("crew_emoji_add: %v", err)
		return "", runtime.NewError("failed to add emoji", 13)
	}
	InvalidateCrewState(req.CrewID)
	pushCrewGiftsChanged(ctx, logger, nk, req.CrewID, "emoji")
	data, _ := json.Marshal(map[string]interface{}{"emoji": d.Emoji})
	return string(data), nil
}

// CrewEmojiRemoveRPC removes an emoji. The member who added it, or the owner.
func CrewEmojiRemoveRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}
	var req struct {
		CrewID string `json:"crew_id"`
		Name   string `json:"name"`
	}
	if err := json.Unmarshal([]byte(payload), &req); err != nil || req.CrewID == "" {
		return "", runtime.NewError("crew_id required", 3)
	}
	name := strings.ToLower(strings.TrimSpace(req.Name))
	owner := isCrewOwner(ctx, nk, req.CrewID, userID)
	removedKey := ""
	d, err := updateCrewEmoji(ctx, nk, req.CrewID, func(d *CrewEmojiDoc) error {
		for i, e := range d.Emoji {
			if e.Name != name {
				continue
			}
			if e.AddedBy != userID && !owner {
				return runtime.NewError("only the adder or the owner removes an emoji", 7)
			}
			removedKey = e.Key
			d.Emoji = append(d.Emoji[:i], d.Emoji[i+1:]...)
			return nil
		}
		return runtime.NewError("no such emoji", 5)
	})
	if err != nil {
		if _, ok := err.(*runtime.Error); ok {
			return "", err
		}
		logger.Error("crew_emoji_remove: %v", err)
		return "", runtime.NewError("failed to remove emoji", 13)
	}
	if removedKey != "" && ChatImagesConfigured() {
		if err := DeleteChatObjects(ctx, []string{removedKey}); err != nil {
			logger.Warn("crew_emoji_remove: delete %s: %v", removedKey, err)
		}
	}
	InvalidateCrewState(req.CrewID)
	pushCrewGiftsChanged(ctx, logger, nk, req.CrewID, "emoji")
	data, _ := json.Marshal(map[string]interface{}{"emoji": d.Emoji})
	return string(data), nil
}

// ---------------------------------------------------------------------------
// Gift 4: the founding week
// ---------------------------------------------------------------------------

// foundingWeeks are the starts of the Crew history weeks that overlap the
// first 7 days of the crew, once its first clip is done.
func foundingWeeks(q *CrewQuests) []int64 {
	if !giftOpen(q, giftWeek) {
		return nil
	}
	first := weekStartOf(q.MadeMs)
	last := weekStartOf(q.MadeMs + 7*dayMs - 1)
	if last == first {
		return []int64{first}
	}
	return []int64{first, last}
}

// ---------------------------------------------------------------------------
// Gift 5: m3llo+ for 30 days for every member
// ---------------------------------------------------------------------------

// questsGrantPlus grants m3llo+ to every member of the crew when step 5 is
// done, and keeps who got it here on the record.
func questsGrantPlus(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule, crewID string) {
	members, _, err := nk.GroupUsersList(ctx, crewID, MaxCrewMembers, nil, "")
	if err != nil {
		logger.Warn("quests m3llo+: members of crew %s: %v", crewID, err)
		return
	}
	nowMs := time.Now().UnixMilli()
	var granted []string
	for _, m := range members {
		// 0 superadmin, 1 admin, 2 member; 3 is a join request.
		if m.GetState().GetValue() > 2 {
			continue
		}
		uid := m.GetUser().GetId()
		ok, err := grantQuestPlus(ctx, nk, uid, crewID, nowMs)
		if err != nil {
			logger.Warn("quests m3llo+: %v", err)
			continue
		}
		if ok {
			granted = append(granted, uid)
		}
	}
	if len(granted) == 0 {
		return
	}
	questsMark(ctx, logger, nk, crewID, "plus", func(q *CrewQuests) bool {
		q.PlusGranted = append(q.PlusGranted, granted...)
		return true
	})
	logger.Info("quests m3llo+: crew %s granted %d members", crewID, len(granted))
}

// pushCrewGiftsChanged tells the members who have the crew open that a gift
// changed: the banner, the invite name or the crew emoji. It is a crew event
// (code 111), which makes the client load crew state and the feed again. Only
// the members with the crew open get it: the gifts draw in the open crew, and
// a sidebar crew has nothing to load again.
func pushCrewGiftsChanged(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule, crewID, gift string) {
	content := map[string]interface{}{
		"type":    "crew_event",
		"crew_id": crewID,
		"event":   "crew_gifts",
		"data":    map[string]interface{}{"gift": gift},
	}
	seen := map[string]bool{}
	for _, sub := range getActiveSubscribersForCrew(crewID) {
		if seen[sub.UserID] {
			continue
		}
		seen[sub.UserID] = true
		pushNotification(ctx, nk, sub.UserID, NotifyCrewEvent, content)
	}
	logger.Debug("crew gifts: %s changed for crew %s, %d open", gift, crewID, len(seen))
}

// ---------------------------------------------------------------------------
// Reports and the invite copied mark
// ---------------------------------------------------------------------------

const MediaReportsCollection = "media_reports"

// MediaReport is media_reports/{id}: a report of a banner or an emoji, for
// the takedown in mello-admin.
type MediaReport struct {
	CrewID     string `json:"crew_id"`
	Kind       string `json:"kind"` // banner | emoji
	Name       string `json:"name,omitempty"`
	URL        string `json:"url"`
	ReporterID string `json:"reporter_id,omitempty"`
	Ts         int64  `json:"ts"`
	// Set by the admin takedown or dismiss (admin_media_reports.go). An
	// open report has no Status.
	Status string `json:"status,omitempty"` // taken_down | dismissed
	Reason string `json:"reason,omitempty"`
	Actor  string `json:"actor,omitempty"`
	DoneMs int64  `json:"done_ms,omitempty"`
}

// ReportCrewMediaRPC records a report. A signed-in user, or the join page
// through the http key (no user).
func ReportCrewMediaRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, _ := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	var req struct {
		CrewID string `json:"crew_id"`
		Kind   string `json:"kind"`
		Name   string `json:"name,omitempty"`
	}
	if err := json.Unmarshal([]byte(payload), &req); err != nil || req.CrewID == "" {
		return "", runtime.NewError("crew_id required", 3)
	}
	r := MediaReport{CrewID: req.CrewID, Kind: req.Kind, ReporterID: userID, Ts: time.Now().UnixMilli()}
	switch req.Kind {
	case "banner":
		p, _ := readCrewProfile(ctx, nk, req.CrewID)
		if p.Banner == nil || p.Banner.Kind != "upload" {
			return "", runtime.NewError("no uploaded banner", 5)
		}
		r.URL = p.Banner.URL
	case "emoji":
		d, _ := readCrewEmoji(ctx, nk, req.CrewID)
		name := strings.ToLower(req.Name)
		for _, e := range d.Emoji {
			if e.Name == name {
				r.Name, r.URL = e.Name, e.URL
			}
		}
		if r.URL == "" {
			return "", runtime.NewError("no such emoji", 5)
		}
	default:
		return "", runtime.NewError("kind must be banner or emoji", 3)
	}
	id, err := newMediaID()
	if err != nil {
		return "", runtime.NewError("failed to store report", 13)
	}
	data, _ := json.Marshal(r)
	if _, err := nk.StorageWrite(ctx, []*runtime.StorageWrite{{
		Collection: MediaReportsCollection, Key: id, UserID: SystemUserID,
		Value: string(data), PermissionRead: 0, PermissionWrite: 0,
	}}); err != nil {
		logger.Error("report_crew_media: %v", err)
		return "", runtime.NewError("failed to store report", 13)
	}
	logger.Info("media report %s: crew %s %s", id, req.CrewID, req.Kind)
	return `{"success":true}`, nil
}

// ReportInviteCopiedRPC marks the first copy of an invite link, for the
// funnel. It is not a step and unlocks nothing.
func ReportInviteCopiedRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return "", runtime.NewError("authentication required", 16)
	}
	var req struct {
		CrewID string `json:"crew_id"`
	}
	if err := json.Unmarshal([]byte(payload), &req); err != nil || req.CrewID == "" {
		return "", runtime.NewError("crew_id required", 3)
	}
	if !isCrewMember(ctx, nk, req.CrewID, userID) {
		return "", runtime.NewError("not a crew member", 7)
	}
	nowMs := time.Now().UnixMilli()
	questsMark(ctx, logger, nk, req.CrewID, "invite_copied", func(q *CrewQuests) bool {
		if q.InviteCopiedMs != 0 {
			return false
		}
		q.InviteCopiedMs = nowMs
		return true
	})
	return `{"success":true}`, nil
}

// ---------------------------------------------------------------------------
// The gifts in the zones payload
// ---------------------------------------------------------------------------

// ZoneQuestGifts is what each gift holds now, for the open tiles.
type ZoneQuestGifts struct {
	BannerURL string `json:"banner_url,omitempty"`
	// BannerGameID is set for a banner of game art, so the client draws it
	// from its art cache (GAME-ART.md).
	BannerGameID  string   `json:"banner_game_id,omitempty"`
	InviteName    string   `json:"invite_name,omitempty"`
	EmojiCount    int      `json:"emoji_count"`
	FirstEmojiURL string   `json:"first_emoji_url,omitempty"`
	FoundingWeeks []int64  `json:"founding_weeks,omitempty"`
	PlusGranted   []string `json:"plus_granted,omitempty"`
}

func zoneQuestGifts(q *CrewQuests, p *CrewProfile, e *CrewEmojiDoc) ZoneQuestGifts {
	g := ZoneQuestGifts{FoundingWeeks: foundingWeeks(q)}
	if q != nil {
		g.PlusGranted = q.PlusGranted
	}
	if p != nil {
		if p.Banner != nil {
			g.BannerURL = p.Banner.URL
			g.BannerGameID = p.Banner.GameID
		}
		g.InviteName = p.InviteName
	}
	if e != nil {
		g.EmojiCount = len(e.Emoji)
		if len(e.Emoji) > 0 {
			g.FirstEmojiURL = e.Emoji[0].URL
		}
	}
	return g
}
