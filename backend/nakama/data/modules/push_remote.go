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
	reassignPushToken(ctx, logger, db, nk, key, userID)

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
	logger.Info("push: token registered user=%s platform=%s env=%s", userID, req.Platform, req.Environment)
	return `{"success":true}`, nil
}

// reassignPushToken removes the token from every other user. A token belongs
// to one device, and a device to its current user: without this, user A's
// mention previews reach the device user B now uses (spec §4.2).
func reassignPushToken(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, key, userID string) {
	if db == nil {
		return
	}
	rows, err := db.QueryContext(ctx,
		`SELECT user_id FROM storage WHERE collection = $1 AND key = $2 AND user_id <> $3`,
		PushTokensCollection, key, userID)
	if err != nil {
		logger.Warn("push: reassign lookup failed: %v", err)
		return
	}
	defer rows.Close()

	var deletes []*runtime.StorageDelete
	for rows.Next() {
		var other string
		if err := rows.Scan(&other); err != nil {
			logger.Warn("push: reassign scan failed: %v", err)
			return
		}
		deletes = append(deletes, &runtime.StorageDelete{
			Collection: PushTokensCollection, Key: key, UserID: other,
		})
	}
	if len(deletes) == 0 {
		return
	}
	if err := nk.StorageDelete(ctx, deletes); err != nil {
		logger.Warn("push: reassign delete failed: %v", err)
		return
	}
	logger.Info("push: token moved to user=%s from %d other user(s)", userID, len(deletes))
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
	if err := nk.StorageDelete(ctx, []*runtime.StorageDelete{{
		Collection: PushTokensCollection, Key: pushTokenKey(req.Token), UserID: userID,
	}}); err != nil {
		logger.Warn("push: unregister for %s: %v", userID, err)
		return "", runtime.NewError("failed to remove push token", 13)
	}
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

// shouldPushNow is the delivery rule (spec §6.1): push only to a user with no
// connected session.
func shouldPushNow(userID string) bool {
	return !HasActiveSessions(userID)
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
	var recipients []string
	for _, uid := range targets {
		if !members[uid] {
			continue // the client's mention list is not trusted
		}
		if !shouldPushNow(uid) {
			logger.Debug("push: skip user=%s (connected)", uid)
			continue
		}
		recipients = append(recipients, uid)
	}
	if len(recipients) == 0 {
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
	for _, uid := range recipients {
		sendPushToUser(ctx, logger, nk, uid, n)
	}
}

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
