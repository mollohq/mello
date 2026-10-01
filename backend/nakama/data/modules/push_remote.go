package main

// Remote push to offline users (spec: mello-backlog/specs/23-PUSH-SERVICE.md).
//
// Nakama owns the device-token registry and the delivery rules. The mello-push
// Cloudflare Worker is a stateless signer: it sends to the tokens Nakama gives
// it and reports dead tokens back. Push is best-effort: every failure is logged
// and never reaches the chat path.
//
// Not to be confused with push.go, which is realtime WebSocket delivery to
// connected sessions.

import (
	"bytes"
	"context"
	"crypto/sha256"
	"database/sql"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"os"
	"regexp"
	"strings"
	"time"

	"github.com/heroiclabs/nakama-common/runtime"
)

const (
	PushTokensCollection = "push_tokens"
	// PushTokenOwnersCollection holds system-owned records key -> current owner,
	// so a register can find the token's previous owner (spec §4.2).
	PushTokenOwnersCollection = "push_token_owners"

	// maxMentionPushes caps the users one message can notify (spec §5).
	maxMentionPushes = 20
	// maxTokensPerSend is the Worker's per-request limit (spec §7.2).
	maxTokensPerSend = 20
	// maxPushTokensPerUser bounds one StorageList page; a user has few devices.
	maxPushTokensPerUser = 100
	maxPushTokenChars    = 4096
	pushTitleMaxRunes    = 64
	pushBodyMaxRunes     = 178
	pushFanoutTimeout    = 10 * time.Second
)

var hexTokenRe = regexp.MustCompile(`^[0-9a-fA-F]+$`)

// pushWorkerHTTP is shared by all sends. The timeout sits above the Worker's
// own 4 s budget (spec §9) so the Worker, not this client, decides.
var pushWorkerHTTP = &http.Client{Timeout: 5 * time.Second}

// ---------------------------------------------------------------------------
// Token registry
// ---------------------------------------------------------------------------

type pushTokenRecord struct {
	Token       string `json:"token"`
	Platform    string `json:"platform"`
	Environment string `json:"environment"`
	UpdatedAt   int64  `json:"updated_at"`
}

// pushTokenKey is the storage key for a token: sha256 hex, 64 chars. Nakama
// keys allow 128 chars and FCM tokens are longer than that.
func pushTokenKey(token string) string {
	sum := sha256.Sum256([]byte(token))
	return hex.EncodeToString(sum[:])
}

type registerPushTokenRequest struct {
	Token       string `json:"token"`
	Platform    string `json:"platform"`
	Environment string `json:"environment"`
}

// validatePushToken checks a register request and fills the default
// environment. The error text is safe to return to the client.
func validatePushToken(req *registerPushTokenRequest) error {
	if req.Token == "" || len(req.Token) > maxPushTokenChars {
		return fmt.Errorf("token must be 1-%d chars", maxPushTokenChars)
	}
	switch req.Platform {
	case "ios":
		if !hexTokenRe.MatchString(req.Token) {
			return fmt.Errorf("ios token must be hex")
		}
	case "android":
	default:
		return fmt.Errorf(`platform must be "ios" or "android"`)
	}
	switch req.Environment {
	case "":
		req.Environment = "production"
	case "production", "sandbox":
	default:
		return fmt.Errorf(`environment must be "production" or "sandbox"`)
	}
	return nil
}

// RegisterPushTokenRPC stores the caller's device token (spec §4).
func RegisterPushTokenRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok || userID == "" {
		return "", runtime.NewError("authentication required", 16)
	}
	var req registerPushTokenRequest
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return "", runtime.NewError("invalid request", 3)
	}
	if err := validatePushToken(&req); err != nil {
		return "", runtime.NewError(err.Error(), 3)
	}

	key := pushTokenKey(req.Token)
	reassignPushToken(ctx, logger, nk, key, userID)

	data, err := json.Marshal(pushTokenRecord{
		Token:       req.Token,
		Platform:    req.Platform,
		Environment: req.Environment,
		UpdatedAt:   time.Now().UnixMilli(),
	})
	if err != nil {
		return "", runtime.NewError("failed to encode push token", 13)
	}
	if _, err := nk.StorageWrite(ctx, []*runtime.StorageWrite{{
		Collection:      PushTokensCollection,
		Key:             key,
		UserID:          userID,
		Value:           string(data),
		PermissionRead:  0,
		PermissionWrite: 0,
	}}); err != nil {
		logger.Error("push: store token for %s: %v", userID, err)
		return "", runtime.NewError("failed to store push token", 13)
	}
	if err := writePushTokenOwner(ctx, nk, key, userID); err != nil {
		// The token is stored; only a later reassign would miss this owner.
		logger.Warn("push: write token owner for %s: %v", userID, err)
	}
	logger.Info("push: token registered user=%s platform=%s env=%s", userID, req.Platform, req.Environment)
	return `{"success":true}`, nil
}

// reassignPushToken removes the token from its previous owner. A token belongs
// to one device, and a device to its current user: without this, user A's
// mention previews reach the device user B now uses (spec §4.2).
//
// Nakama Storage is keyed per user and has no "find this key for any user"
// call, so a system-owned owner record (push_token_owners/<key>) names the
// current owner. Only the storage API is used: no SQL against Nakama's tables,
// which are not a stable interface across Nakama upgrades.
func reassignPushToken(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule, key, userID string) {
	previous := readPushTokenOwner(ctx, logger, nk, key)
	if previous == "" || previous == userID {
		return
	}
	if err := nk.StorageDelete(ctx, []*runtime.StorageDelete{{
		Collection: PushTokensCollection, Key: key, UserID: previous,
	}}); err != nil {
		logger.Warn("push: reassign delete failed: %v", err)
		return
	}
	logger.Info("push: token moved to user=%s from user=%s", userID, previous)
}

type pushTokenOwner struct {
	UserID string `json:"user_id"`
}

// readPushTokenOwner returns the user that owns a token key, or "".
func readPushTokenOwner(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule, key string) string {
	objects, err := nk.StorageRead(ctx, []*runtime.StorageRead{{
		Collection: PushTokenOwnersCollection, Key: key, UserID: "",
	}})
	if err != nil {
		logger.Warn("push: read token owner: %v", err)
		return ""
	}
	if len(objects) == 0 {
		return ""
	}
	var owner pushTokenOwner
	if err := json.Unmarshal([]byte(objects[0].GetValue()), &owner); err != nil {
		return ""
	}
	return owner.UserID
}

func writePushTokenOwner(ctx context.Context, nk runtime.NakamaModule, key, userID string) error {
	data, err := json.Marshal(pushTokenOwner{UserID: userID})
	if err != nil {
		return err
	}
	_, err = nk.StorageWrite(ctx, []*runtime.StorageWrite{{
		Collection:      PushTokenOwnersCollection,
		Key:             key,
		UserID:          "", // system-owned
		Value:           string(data),
		PermissionRead:  0,
		PermissionWrite: 0,
	}})
	return err
}

// dropPushTokenOwner deletes the owner record when userID still owns the key.
// A newer owner's record is left alone.
func dropPushTokenOwner(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule, key, userID string) {
	if readPushTokenOwner(ctx, logger, nk, key) != userID {
		return
	}
	if err := nk.StorageDelete(ctx, []*runtime.StorageDelete{{
		Collection: PushTokenOwnersCollection, Key: key, UserID: "",
	}}); err != nil {
		logger.Warn("push: delete token owner: %v", err)
	}
}

// UnregisterPushTokenRPC removes the caller's token (logout).
func UnregisterPushTokenRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok || userID == "" {
		return "", runtime.NewError("authentication required", 16)
	}
	var req struct {
		Token string `json:"token"`
	}
	if err := json.Unmarshal([]byte(payload), &req); err != nil || req.Token == "" {
		return "", runtime.NewError("invalid request", 3)
	}
	key := pushTokenKey(req.Token)
	if err := nk.StorageDelete(ctx, []*runtime.StorageDelete{{
		Collection: PushTokensCollection, Key: key, UserID: userID,
	}}); err != nil {
		logger.Warn("push: unregister for %s: %v", userID, err)
		return "", runtime.NewError("failed to remove push token", 13)
	}
	dropPushTokenOwner(ctx, logger, nk, key, userID)
	logger.Info("push: token unregistered user=%s", userID)
	return `{"success":true}`, nil
}

// ---------------------------------------------------------------------------
// Worker contract (spec §7; fixtures in testdata/)
// ---------------------------------------------------------------------------

type pushAlert struct {
	Type      string `json:"type"`
	CrewID    string `json:"crew_id"`
	ChannelID string `json:"channel_id"`
	MessageID string `json:"message_id"`
	Title     string `json:"title"`
	Body      string `json:"body"`
}

type pushDevice struct {
	Token       string `json:"token"`
	Platform    string `json:"platform"`
	Environment string `json:"environment"`
}

type pushSendRequest struct {
	Notification pushAlert    `json:"notification"`
	Tokens       []pushDevice `json:"tokens"`
}

type pushTokenResult struct {
	Token  string `json:"token"`
	Status string `json:"status"`
	APNsID string `json:"apns_id,omitempty"`
	Reason string `json:"reason,omitempty"`
}

type pushSendResponse struct {
	Results []pushTokenResult `json:"results"`
	Prune   []string          `json:"prune"`
}

// postPushSend calls the Worker's POST /send.
func postPushSend(ctx context.Context, client *http.Client, baseURL, bearer string, req *pushSendRequest) (*pushSendResponse, error) {
	body, err := json.Marshal(req)
	if err != nil {
		return nil, err
	}
	httpReq, err := http.NewRequestWithContext(ctx, http.MethodPost,
		strings.TrimRight(baseURL, "/")+"/send", bytes.NewReader(body))
	if err != nil {
		return nil, err
	}
	httpReq.Header.Set("Authorization", "Bearer "+bearer)
	httpReq.Header.Set("Content-Type", "application/json")

	resp, err := client.Do(httpReq)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	raw, err := io.ReadAll(io.LimitReader(resp.Body, 64<<10))
	if err != nil {
		return nil, err
	}
	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("worker status %d: %s", resp.StatusCode, strings.TrimSpace(string(raw)))
	}
	var out pushSendResponse
	if err := json.Unmarshal(raw, &out); err != nil {
		return nil, fmt.Errorf("decode worker response: %w", err)
	}
	return &out, nil
}

// sendPushToUser sends one notification to every registered device of a user
// and deletes the tokens the Worker reports dead.
func sendPushToUser(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule, userID string, n pushAlert) {
	baseURL, bearer := os.Getenv("PUSH_WORKER_URL"), os.Getenv("PUSH_WORKER_TOKEN")
	if baseURL == "" || bearer == "" {
		logger.Debug("push: PUSH_WORKER_URL/PUSH_WORKER_TOKEN not set; skipping")
		return
	}

	objects, _, err := nk.StorageList(ctx, "", userID, PushTokensCollection, maxPushTokensPerUser, "")
	if err != nil {
		logger.Warn("push: list tokens for %s: %v", userID, err)
		return
	}
	var devices []pushDevice
	for _, o := range objects {
		var rec pushTokenRecord
		if err := json.Unmarshal([]byte(o.GetValue()), &rec); err != nil || rec.Token == "" {
			continue
		}
		devices = append(devices, pushDevice{Token: rec.Token, Platform: rec.Platform, Environment: rec.Environment})
	}
	if len(devices) == 0 {
		logger.Debug("push: user=%s has no registered device", userID)
		return
	}

	for start := 0; start < len(devices); start += maxTokensPerSend {
		end := start + maxTokensPerSend
		if end > len(devices) {
			end = len(devices)
		}
		resp, err := postPushSend(ctx, pushWorkerHTTP, baseURL, bearer,
			&pushSendRequest{Notification: n, Tokens: devices[start:end]})
		if err != nil {
			logger.Warn("push: send to user=%s failed: %v", userID, err)
			continue
		}
		sent := 0
		for _, r := range resp.Results {
			if r.Status == "sent" {
				sent++
			}
		}
		logger.Info("push: user=%s message=%s sent=%d/%d pruned=%d",
			userID, n.MessageID, sent, len(resp.Results), len(resp.Prune))
		prunePushTokens(ctx, logger, nk, userID, resp.Prune)
	}
}

func prunePushTokens(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule, userID string, tokens []string) {
	if len(tokens) == 0 {
		return
	}
	deletes := make([]*runtime.StorageDelete, 0, len(tokens))
	for _, t := range tokens {
		deletes = append(deletes, &runtime.StorageDelete{
			Collection: PushTokensCollection, Key: pushTokenKey(t), UserID: userID,
		})
	}
	if err := nk.StorageDelete(ctx, deletes); err != nil {
		logger.Warn("push: prune %d token(s) for %s: %v", len(tokens), userID, err)
		return
	}
	for _, t := range tokens {
		dropPushTokenOwner(ctx, logger, nk, pushTokenKey(t), userID)
	}
}

// ---------------------------------------------------------------------------
// Mention trigger (spec §5)
// ---------------------------------------------------------------------------

// mentionPushTargets returns the users a message may notify: the envelope's
// mentions without the sender, empty ids, or duplicates, capped.
func mentionPushTargets(mentions []string, senderID string) []string {
	seen := map[string]bool{}
	var out []string
	for _, id := range mentions {
		if id == "" || id == senderID || seen[id] {
			continue
		}
		seen[id] = true
		out = append(out, id)
		if len(out) == maxMentionPushes {
			break
		}
	}
	return out
}

// pushBody is "<sender>: <text>", bounded for the lock screen.
func pushBody(senderName, text string) string {
	if senderName == "" {
		return truncateRunes(text, pushBodyMaxRunes)
	}
	return truncateRunes(senderName+": "+text, pushBodyMaxRunes)
}

// queueMentionPushes starts the push fan-out for a chat message. It returns at
// once: the chat hook must not wait on storage or the Worker.
func queueMentionPushes(logger runtime.Logger, nk runtime.NakamaModule, senderID, senderName, crewID, messageID, content string) {
	var env messageEnvelope
	if err := json.Unmarshal([]byte(content), &env); err != nil || env.Type != "text" {
		return
	}
	targets := mentionPushTargets(env.Mentions, senderID)
	if len(targets) == 0 {
		return
	}
	logger.Debug("push: message=%s crew=%s mentions=%d", messageID, crewID, len(targets))
	body := env.Body
	go func() {
		defer func() {
			if r := recover(); r != nil {
				logger.Error("push: fan-out panic for message %s: %v", messageID, r)
			}
		}()
		ctx, cancel := context.WithTimeout(context.Background(), pushFanoutTimeout)
		defer cancel()
		deliverMentionPushes(ctx, logger, nk, senderName, crewID, messageID, body, targets)
	}()
}

func deliverMentionPushes(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule, senderName, crewID, messageID, body string, targets []string) {
	members := crewMemberSet(ctx, nk, crewID)
	var now, later []string
	for _, uid := range targets {
		if !members[uid] {
			continue // the client's mention list is not trusted
		}
		switch action := pushDecisionFor(uid); action {
		case pushNow:
			now = append(now, uid)
		case pushAfterGrace:
			later = append(later, uid)
		default:
			logger.Debug("push: skip user=%s (%s)", uid, action)
		}
	}
	logger.Debug("push: message=%s members=%d now=%d later=%d", messageID, len(members), len(now), len(later))
	if len(now) == 0 && len(later) == 0 {
		return
	}

	title := "Mello"
	if groups, err := nk.GroupsGetId(ctx, []string{crewID}); err == nil && len(groups) > 0 && groups[0].GetName() != "" {
		title = groups[0].GetName()
	}
	channelID, err := nk.ChannelIdBuild(ctx, "", crewID, runtime.Group)
	if err != nil {
		logger.Warn("push: channel id for crew %s: %v", crewID, err)
		return
	}
	n := pushAlert{
		Type:      "mention",
		CrewID:    crewID,
		ChannelID: channelID,
		MessageID: messageID,
		Title:     truncateRunes(title, pushTitleMaxRunes),
		Body:      pushBody(senderName, resolveMentionTokens(ctx, nk, body)),
	}
	for _, uid := range now {
		sendPushToUser(ctx, logger, nk, uid, n)
	}
	for _, uid := range later {
		schedulePushAfterGrace(logger, nk, uid, n)
	}
}

// schedulePushAfterGrace holds a push back while the user's desktop is open
// but inactive, then sends it unless the user became active (spec 23 §6.2).
// The timer lives in memory: a Nakama restart drops it (best-effort, §6.4).
func schedulePushAfterGrace(logger runtime.Logger, nk runtime.NakamaModule, userID string, n pushAlert) {
	grace := desktopGrace()
	logger.Debug("push: user=%s message=%s held for %s (desktop inactive)", userID, n.MessageID, grace)
	afterFunc(grace, func() {
		defer func() {
			if r := recover(); r != nil {
				logger.Error("push: delayed send panic for message %s: %v", n.MessageID, r)
			}
		}()
		if action := pushDecisionFor(userID); action == pushSkip {
			logger.Debug("push: drop held push user=%s message=%s (active again)", userID, n.MessageID)
			return
		}
		ctx, cancel := context.WithTimeout(context.Background(), pushFanoutTimeout)
		defer cancel()
		heldPushSend(ctx, logger, nk, userID, n)
	})
}

// heldPushSend is sendPushToUser; tests replace it to observe the recheck.
var heldPushSend = sendPushToUser

// crewMemberSet returns the crew's members (not pending join requests).
func crewMemberSet(ctx context.Context, nk runtime.NakamaModule, crewID string) map[string]bool {
	out := map[string]bool{}
	cursor := ""
	for {
		members, next, err := nk.GroupUsersList(ctx, crewID, 100, nil, cursor)
		if err != nil {
			return out
		}
		for _, m := range members {
			// 0 superadmin, 1 admin, 2 member, 3 join request.
			if m.GetState().GetValue() <= 2 {
				out[m.GetUser().GetId()] = true
			}
		}
		if next == "" {
			return out
		}
		cursor = next
	}
}
