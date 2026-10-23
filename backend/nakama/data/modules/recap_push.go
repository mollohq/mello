package main

// Monday push for the weekly recap (plans/next-gen-feed.md section 8).
//
// After a recap is written, one push per crew member is queued for Monday 18:00
// in the member's local time. The queue is a storage doc per crew, and the
// hourly recap job sends what is due. Storage, not a timer in memory, so a
// Nakama restart between Monday 00:00 UTC and 18:00 local drops nothing.
// Delivery uses the same device path as mention pushes: the member's push_tokens
// go to the push Worker (push_remote.go, sendPushToUser).

import (
	"context"
	"encoding/json"
	"fmt"
	"strings"
	"time"

	"github.com/heroiclabs/nakama-common/runtime"
)

const (
	// RecapPushCollection holds one system-owned doc per crew, key = crew id.
	RecapPushCollection = "recap_pushes"

	// recapPushLocalHour is the local hour of the Monday push.
	recapPushLocalHour = 18

	// recapPushMaxLateness is how long after its time a push may still go.
	// A recap written late (recapLookback) sends at the next tick inside this
	// window. After it the push is dropped: Monday evening news on Wednesday
	// is noise.
	recapPushMaxLateness = 24 * time.Hour

	recapPushType = "recap"
)

// RecapPushItem is one queued push.
//
// The belt and crew join pushes (event_push.go) use the same item. They set
// Type and MessageID, and a coalescing item keeps its Group, its Events and
// the time of its last send so the next one can wait out the window.
type RecapPushItem struct {
	UserID   string `json:"user_id"`
	SendAtMs int64  `json:"send_at_ms"`
	Body     string `json:"body"`
	Sent     bool   `json:"sent"`

	Type       string      `json:"type,omitempty"`         // empty means recap
	MessageID  string      `json:"message_id,omitempty"`   // collapse id, empty means the recap id
	Group      string      `json:"group,omitempty"`        // coalescing group, empty means none
	Events     []pushEvent `json:"events,omitempty"`       // what the coalesced body is built from
	Hold       int         `json:"hold,omitempty"`         // belts the member holds, for the belt body
	LastSentMs int64       `json:"last_sent_ms,omitempty"` // when the item last went out
}

// RecapPushDoc is the queue of one crew for one recap week.
type RecapPushDoc struct {
	CrewID string          `json:"crew_id"`
	Week   string          `json:"week"` // isoWeekKey of the recap week end
	Items  []RecapPushItem `json:"items"`
}

// pushTextClean removes what the copy rules forbid from a member name: the
// exclamation point and the long dashes.
var pushTextClean = strings.NewReplacer("!", "", "—", "-", "–", "-")

func cleanPushName(s string) string {
	return strings.TrimSpace(pushTextClean.Replace(s))
}

func beltTitleLabel(id string) string {
	if l := head2headTitleLabels[id]; l != "" {
		return l
	}
	return id
}

// heldCount is the number of titles a member holds in the recap.
func heldCount(r WeeklyRecapData, userID string) int {
	for _, h := range r.BeltsHeld {
		if h.UserID == userID {
			return h.Count
		}
	}
	return 0
}

// recapPushBody is the one line a member gets, from the belts of the recap.
// It is empty when the crew has no belts.
//
//  1. A belt moved and the member lost it: "<Name> took <Title>. You hold <n>."
//  2. The member took a belt: "You took <Title> from <Name>. You hold <n>."
//  3. The crew has belts: "Week <n> recap. <Leader> leads with <n>."
//
// With several moves in one rule the newest move wins.
func recapPushBody(r WeeklyRecapData, userID string) string {
	var lost, took *RecapBeltMoved
	for i := range r.BeltsMoved {
		m := &r.BeltsMoved[i]
		if m.PreviousUserID == userID && m.HolderUserID != userID && (lost == nil || m.MovedAtMs >= lost.MovedAtMs) {
			lost = m
		}
		if m.HolderUserID == userID && (took == nil || m.MovedAtMs >= took.MovedAtMs) {
			took = m
		}
	}
	switch {
	case lost != nil:
		return truncateRunes(fmt.Sprintf("%s took %s. You hold %d.",
			cleanPushName(lost.HolderName), beltTitleLabel(lost.TitleID), heldCount(r, userID)), pushBodyMaxRunes)
	case took != nil:
		from := cleanPushName(took.PreviousName)
		if from == "" {
			return truncateRunes(fmt.Sprintf("You took %s. You hold %d.",
				beltTitleLabel(took.TitleID), heldCount(r, userID)), pushBodyMaxRunes)
		}
		return truncateRunes(fmt.Sprintf("You took %s from %s. You hold %d.",
			beltTitleLabel(took.TitleID), from, heldCount(r, userID)), pushBodyMaxRunes)
	case len(r.BeltsHeld) > 0:
		lead := r.BeltsHeld[0]
		_, week := time.UnixMilli(r.WeekStart).UTC().ISOWeek()
		return truncateRunes(fmt.Sprintf("Week %d recap. %s leads with %d.",
			week, cleanPushName(lead.DisplayName), lead.Count), pushBodyMaxRunes)
	}
	return ""
}

// recapPushAt is Monday 18:00 in the member's local time, as unix ms. weekEnd
// is any instant on that Monday (UTC). tz is the member's offset in minutes
// east of UTC, nil when unknown: the push then goes at 18:00 UTC.
func recapPushAt(weekEnd time.Time, tz *int) int64 {
	at := mondayStartUTC(weekEnd).Add(recapPushLocalHour * time.Hour)
	if tz = validTzOffset(tz); tz != nil {
		at = at.Add(-time.Duration(*tz) * time.Minute)
	}
	return at.UnixMilli()
}

// buildRecapPushItems builds the queue of a recap. tzs maps a member to the
// offset on the activity doc. A member with an empty line gets no item.
func buildRecapPushItems(r WeeklyRecapData, memberIDs []string, tzs map[string]*int) []RecapPushItem {
	var items []RecapPushItem
	for _, uid := range memberIDs {
		body := recapPushBody(r, uid)
		if body == "" {
			continue
		}
		items = append(items, RecapPushItem{
			UserID:   uid,
			SendAtMs: recapPushAt(time.UnixMilli(r.WeekEnd), tzs[uid]),
			Body:     body,
		})
	}
	return items
}

// splitDueRecapPushes returns the items to send at now, and marks every item
// that is due or stale as sent. A stale item is dropped without a send.
// changed reports whether the doc was edited.
func splitDueRecapPushes(doc *RecapPushDoc, now time.Time) (due []RecapPushItem, changed bool) {
	nowMs := now.UnixMilli()
	for i := range doc.Items {
		it := &doc.Items[i]
		if it.Sent || it.SendAtMs > nowMs {
			continue
		}
		it.Sent = true
		changed = true
		if nowMs-it.SendAtMs <= recapPushMaxLateness.Milliseconds() {
			it.LastSentMs = nowMs
			due = append(due, *it)
		}
	}
	return due, changed
}

// scheduleRecapPushes queues the Monday pushes of a recap. Never fails the
// recap: push is best-effort.
func scheduleRecapPushes(ctx context.Context, nk runtime.NakamaModule, logger runtime.Logger, crewID string, r WeeklyRecapData) {
	ids, err := crewMemberIDs(ctx, nk, crewID)
	if err != nil || len(ids) == 0 {
		return
	}
	tzs := map[string]*int{}
	for uid, a := range readAllUserActivityStats(ctx, nk, ids) {
		tzs[uid] = validTzOffset(a.TzOffsetMin)
	}
	items := buildRecapPushItems(r, ids, tzs)
	if len(items) == 0 {
		return
	}
	data, err := json.Marshal(RecapPushDoc{CrewID: crewID, Week: isoWeekKey(time.UnixMilli(r.WeekEnd)), Items: items})
	if err != nil {
		return
	}
	// One doc per crew: a new recap replaces the queue of the week before.
	if _, err := nk.StorageWrite(ctx, []*runtime.StorageWrite{{
		Collection: RecapPushCollection, Key: crewID, UserID: SystemUserID,
		Value: string(data), PermissionRead: 0, PermissionWrite: 0,
	}}); err != nil {
		logger.Warn("recap push: queue for crew %s failed: %v", crewID, err)
		return
	}
	logger.Info("recap push: queued %d push(es) for crew %s", len(items), crewID)
}

// recapPushSend delivers one recap push. Tests replace it.
var recapPushSend = func(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule, crewID, week string, it RecapPushItem) {
	if pushDecisionFor(it.UserID) == pushSkip {
		logger.Debug("recap push: skip user=%s (active in the app)", it.UserID)
		return
	}
	title := "m3llo"
	if groups, err := nk.GroupsGetId(ctx, []string{crewID}); err == nil && len(groups) > 0 && groups[0].GetName() != "" {
		title = groups[0].GetName()
	}
	channelID, err := nk.ChannelIdBuild(ctx, "", crewID, runtime.Group)
	if err != nil {
		logger.Warn("recap push: channel id for crew %s: %v", crewID, err)
		return
	}
	typ, messageID := recapPushType, "recap-"+crewID+"-"+week // collapse id, under 64 bytes
	if it.Type != "" {
		typ = it.Type
	}
	if it.MessageID != "" {
		messageID = it.MessageID
	}
	sendPushToUser(ctx, logger, nk, it.UserID, pushAlert{
		Type:      typ,
		CrewID:    crewID,
		ChannelID: channelID,
		MessageID: messageID,
		Title:     truncateRunes(title, pushTitleMaxRunes),
		Body:      it.Body,
	})
}

// sendDueRecapPushes sends the queued recap pushes that are due at now. It
// marks an item sent before it sends, so a crash sends at most once.
func sendDueRecapPushes(ctx context.Context, nk runtime.NakamaModule, logger runtime.Logger, now time.Time) {
	cursor := ""
	for {
		objects, next, err := nk.StorageList(ctx, "", SystemUserID, RecapPushCollection, 100, cursor)
		if err != nil {
			logger.Warn("recap push: list queue failed: %v", err)
			return
		}
		for _, o := range objects {
			processPushQueueDoc(ctx, nk, logger, o.GetKey(), o.GetValue(), o.GetVersion(), now)
		}
		if next == "" || len(objects) == 0 {
			return
		}
		cursor = next
	}
}

// processPushQueueDoc claims and sends what is due in one queue doc.
func processPushQueueDoc(ctx context.Context, nk runtime.NakamaModule, logger runtime.Logger, key, value, version string, now time.Time) {
	var doc RecapPushDoc
	if json.Unmarshal([]byte(value), &doc) != nil {
		return
	}
	due, changed := splitDueRecapPushes(&doc, now)
	if !changed {
		return
	}
	data, err := json.Marshal(doc)
	if err != nil {
		return
	}
	if _, err := nk.StorageWrite(ctx, []*runtime.StorageWrite{{
		Collection: RecapPushCollection, Key: key, UserID: SystemUserID,
		Value: string(data), Version: version, PermissionRead: 0, PermissionWrite: 0,
	}}); err != nil {
		logger.Debug("recap push: claim failed for crew %s, next tick: %v", doc.CrewID, err)
		return
	}
	for _, it := range due {
		recapPushSend(ctx, logger, nk, doc.CrewID, doc.Week, it)
	}
}
