package main

// Event pushes on the recap push queue: belt moved and friend landed
// (specs/features/HEAD2HEAD.md section 8, push service spec 23).
//
// They reuse the queue doc, the hourly sender and the device path of the recap
// push (recap_push.go). Each type has its own queue doc per crew, so a new
// recap never replaces a pending belt push. Three rules apply to both:
//
//   - Quiet hours: nothing goes before 09:00 or from 22:00 in the member's
//     local time (tz_offset_min on the activity doc, else UTC). The push is
//     queued for the next 09:00.
//   - Coalescing: a type with a window sends the first push at once and holds
//     the next ones until the window since the last send has passed. The held
//     events leave as one push.
//   - Best effort: a failed queue write drops the push, never the caller.

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"sort"
	"time"

	"github.com/heroiclabs/nakama-common/runtime"
)

const (
	beltPushType = "belt"
	beltPushKey  = "belt_" // queue doc key prefix, then the crew id

	// beltPushWindow is the minimum time between two belt pushes to a member
	// of one crew.
	beltPushWindow = 6 * time.Hour

	// pushQuietUntilHour and pushQuietFromHour bound the local hours in which
	// an event push may go: from 09:00 up to, not including, 22:00.
	pushQuietUntilHour = 9
	pushQuietFromHour  = 22

	pushQueueTries = 3
)

// pushEvent is one thing a coalesced push tells about.
type pushEvent struct {
	Name  string `json:"name"`            // the other member
	Title string `json:"title,omitempty"` // belt title label
	Took  bool   `json:"took,omitempty"`  // belt: the recipient took it
}

// eventPushClock and eventPushGo are replaced by tests.
var (
	eventPushClock = time.Now
	eventPushGo    = func(f func()) { go f() }
)

// pushTzOffset is the offset used for a member's local time, nil when unknown.
func pushTzOffset(a *UserActivityStats) *int {
	if a == nil {
		return nil
	}
	return validTzOffset(a.TzOffsetMin)
}

// pushQuietAdjust returns at when it is outside quiet hours in the member's
// local time, else the next 09:00 local. tz is minutes east of UTC.
func pushQuietAdjust(at time.Time, tz *int) time.Time {
	var off time.Duration
	if tz = validTzOffset(tz); tz != nil {
		off = time.Duration(*tz) * time.Minute
	}
	local := at.UTC().Add(off) // the UTC fields of local read as the local clock
	h := local.Hour()
	if h >= pushQuietUntilHour && h < pushQuietFromHour {
		return at
	}
	next := time.Date(local.Year(), local.Month(), local.Day(), pushQuietUntilHour, 0, 0, 0, time.UTC)
	if h >= pushQuietFromHour {
		next = next.AddDate(0, 0, 1)
	}
	return next.Add(-off)
}

// pushSendAt is when a coalescing push may go: not before the window since the
// last send has passed, and not in quiet hours.
func pushSendAt(now time.Time, lastSentMs int64, window time.Duration, tz *int) time.Time {
	at := now
	if lastSentMs > 0 {
		if earliest := time.UnixMilli(lastSentMs).Add(window); earliest.After(at) {
			at = earliest
		}
	}
	return pushQuietAdjust(at, tz)
}

// shortPushID is a stable 12 character id for a collapse id. A Nakama id is 36
// characters and APNs allows 64 bytes in all.
func shortPushID(id string) string {
	sum := sha256.Sum256([]byte(id))
	return hex.EncodeToString(sum[:6])
}

// updatePushQueue reads one queue doc, lets mutate edit it, and writes it with
// the version it read. It retries on a conflict. mutate returns false for no
// change.
func updatePushQueue(ctx context.Context, nk runtime.NakamaModule, logger runtime.Logger, key, crewID string, mutate func(doc *RecapPushDoc) bool) bool {
	for attempt := 0; attempt < pushQueueTries; attempt++ {
		doc := RecapPushDoc{CrewID: crewID}
		version := "*" // write only if the doc does not exist yet
		objects, err := nk.StorageRead(ctx, []*runtime.StorageRead{{Collection: RecapPushCollection, Key: key, UserID: SystemUserID}})
		if err != nil {
			logger.Warn("push queue: read %s failed: %v", key, err)
			return false
		}
		if len(objects) > 0 {
			if json.Unmarshal([]byte(objects[0].GetValue()), &doc) != nil {
				doc = RecapPushDoc{CrewID: crewID}
			}
			version = objects[0].GetVersion()
		}
		if !mutate(&doc) {
			return false
		}
		data, err := json.Marshal(doc)
		if err != nil {
			return false
		}
		if _, err := nk.StorageWrite(ctx, []*runtime.StorageWrite{{
			Collection: RecapPushCollection, Key: key, UserID: SystemUserID,
			Value: string(data), Version: version, PermissionRead: 0, PermissionWrite: 0,
		}}); err != nil {
			logger.Debug("push queue: write %s retry: %v", key, err)
			continue
		}
		return true
	}
	logger.Warn("push queue: write %s failed after %d tries", key, pushQueueTries)
	return false
}

// flushPushQueue sends what is due in one queue doc at now. The queue
// functions call it after a write, so a push that may go at once does not wait
// for the hourly tick.
func flushPushQueue(ctx context.Context, nk runtime.NakamaModule, logger runtime.Logger, key string) {
	objects, err := nk.StorageRead(ctx, []*runtime.StorageRead{{Collection: RecapPushCollection, Key: key, UserID: SystemUserID}})
	if err != nil || len(objects) == 0 {
		return
	}
	processPushQueueDoc(ctx, nk, logger, key, objects[0].GetValue(), objects[0].GetVersion(), eventPushClock())
}

// flushPushQueueAsync flushes in a goroutine with its own deadline, so the
// caller (an RPC) does not wait for the Worker.
func flushPushQueueAsync(nk runtime.NakamaModule, logger runtime.Logger, key string) {
	eventPushGo(func() {
		ctx, cancel := context.WithTimeout(context.Background(), pushFanoutTimeout)
		defer cancel()
		flushPushQueue(ctx, nk, logger, key)
	})
}

// pushDisplayNames maps user ids to the name shown in a push: the display
// name, else the username, cleaned for the copy rules.
func pushDisplayNames(ctx context.Context, nk runtime.NakamaModule, ids []string) map[string]string {
	out := make(map[string]string, len(ids))
	if len(ids) == 0 {
		return out
	}
	users, err := nk.UsersGetId(ctx, ids, nil)
	if err != nil {
		return out
	}
	for _, u := range users {
		name := cleanPushName(u.GetDisplayName())
		if name == "" {
			name = cleanPushName(u.GetUsername())
		}
		out[u.GetId()] = name
	}
	return out
}

// pushCrewName is the crew name, "m3llo" when it cannot be read.
func pushCrewName(ctx context.Context, nk runtime.NakamaModule, crewID string) string {
	if groups, err := nk.GroupsGetId(ctx, []string{crewID}); err == nil && len(groups) > 0 {
		if n := cleanPushName(groups[0].GetName()); n != "" {
			return n
		}
	}
	return "m3llo"
}

// ---------------------------------------------------------------------------
// Belt moved
// ---------------------------------------------------------------------------

// ctxKeyH2hActor carries the user whose action caused a recompute.
type ctxKeyH2hActor struct{}

// withHead2headActor marks ctx as a recompute caused by userID.
func withHead2headActor(ctx context.Context, userID string) context.Context {
	return context.WithValue(ctx, ctxKeyH2hActor{}, userID)
}

func head2headActor(ctx context.Context) string {
	s, _ := ctx.Value(ctxKeyH2hActor{}).(string)
	return s
}

// beltMove is one title that changed holder in a recompute.
type beltMove struct {
	TitleID    string
	HolderID   string
	PreviousID string
}

// beltMovesOf lists the titles that moved in the recompute that wrote doc.
// computeHead2head stamps moved_at_ms with the recompute time, and doc.UpdatedAt
// is the same instant.
func beltMovesOf(doc *Head2headDoc) []beltMove {
	if doc == nil {
		return nil
	}
	var moves []beltMove
	for _, t := range doc.Titles {
		if t.PreviousHolderUserID != "" && t.MovedAtMs == doc.UpdatedAt && t.PreviousHolderUserID != t.HolderUserID {
			moves = append(moves, beltMove{TitleID: t.TitleID, HolderID: t.HolderUserID, PreviousID: t.PreviousHolderUserID})
		}
	}
	return moves
}

// heldTitles counts the titles each member holds in doc.
func heldTitles(doc *Head2headDoc) map[string]int {
	out := map[string]int{}
	for _, t := range doc.Titles {
		out[t.HolderUserID]++
	}
	return out
}

// beltPushBody is the line of a belt push. The newest event leads.
//
//	lost one:   "<Name> took <Title> from you. You hold <n>."
//	lost many:  "<Name> took <Title> and <m> more. You hold <n>."
//	took one:   "You took <Title> from <Name>. You hold <n>."
//	took many:  "You took <Title> and <m> more. You hold <n>."
func beltPushBody(events []pushEvent, hold int) string {
	if len(events) == 0 {
		return ""
	}
	last := events[len(events)-1]
	more := len(events) - 1
	var s string
	switch {
	case last.Took && more == 0:
		s = fmt.Sprintf("You took %s from %s. You hold %d.", last.Title, last.Name, hold)
	case last.Took:
		s = fmt.Sprintf("You took %s and %d more. You hold %d.", last.Title, more, hold)
	case more == 0:
		s = fmt.Sprintf("%s took %s from you. You hold %d.", last.Name, last.Title, hold)
	default:
		s = fmt.Sprintf("%s took %s and %d more. You hold %d.", last.Name, last.Title, more, hold)
	}
	return truncateRunes(s, pushBodyMaxRunes)
}

// beltPushEvents builds, for each recipient, the events of the moves. The new
// holder gets none when they are the actor. A recipient outside members gets
// none.
func beltPushEvents(moves []beltMove, names map[string]string, members map[string]bool, actor string) map[string][]pushEvent {
	out := map[string][]pushEvent{}
	for _, m := range moves {
		label := beltTitleLabel(m.TitleID)
		if members[m.PreviousID] {
			out[m.PreviousID] = append(out[m.PreviousID], pushEvent{Name: names[m.HolderID], Title: label})
		}
		if m.HolderID != actor && members[m.HolderID] {
			out[m.HolderID] = append(out[m.HolderID], pushEvent{Name: names[m.PreviousID], Title: label, Took: true})
		}
	}
	return out
}

// addBeltEvents folds new events into the queue of one crew and returns
// whether the doc changed. One coalescing item per member: a pending item takes
// the events, a sent item starts a new pending one that waits for the window.
func addBeltEvents(doc *RecapPushDoc, crewID string, events map[string][]pushEvent, hold map[string]int, tzs map[string]*int, now time.Time) bool {
	users := make([]string, 0, len(events))
	for uid := range events {
		users = append(users, uid)
	}
	sort.Strings(users)
	for _, uid := range users {
		evs := events[uid]
		var it *RecapPushItem
		for i := range doc.Items {
			if doc.Items[i].UserID == uid && doc.Items[i].Group == beltPushType {
				it = &doc.Items[i]
			}
		}
		if it == nil {
			doc.Items = append(doc.Items, RecapPushItem{UserID: uid, Group: beltPushType, Type: beltPushType, Sent: true})
			it = &doc.Items[len(doc.Items)-1]
		}
		if it.Sent {
			it.Sent = false
			it.Events = nil
		}
		it.Events = append(it.Events, evs...)
		it.Hold = hold[uid]
		it.Body = beltPushBody(it.Events, it.Hold)
		it.MessageID = "belt-" + shortPushID(crewID) + "-" + shortPushID(uid)
		it.SendAtMs = pushSendAt(now, it.LastSentMs, beltPushWindow, tzs[uid]).UnixMilli()
	}
	return len(users) > 0
}

// queueBeltPushes queues the belt pushes of the recompute that wrote doc. The
// actor, from ctx, gets no push for a belt they took. Never fails the caller.
func queueBeltPushes(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule, doc *Head2headDoc) {
	moves := beltMovesOf(doc)
	if len(moves) == 0 {
		return
	}
	ids, err := crewMemberIDs(ctx, nk, doc.CrewID)
	if err != nil {
		return
	}
	members := make(map[string]bool, len(ids))
	for _, id := range ids {
		members[id] = true
	}
	want := map[string]bool{}
	for _, m := range moves {
		want[m.HolderID], want[m.PreviousID] = true, true
	}
	var nameIDs []string
	for id := range want {
		nameIDs = append(nameIDs, id)
	}
	names := pushDisplayNames(ctx, nk, nameIDs)
	events := beltPushEvents(moves, names, members, head2headActor(ctx))
	if len(events) == 0 {
		return
	}
	tzs := map[string]*int{}
	for uid, a := range readAllUserActivityStats(ctx, nk, ids) {
		tzs[uid] = pushTzOffset(a)
	}
	held := heldTitles(doc)
	key := beltPushKey + doc.CrewID
	now := eventPushClock()
	if updatePushQueue(ctx, nk, logger, key, doc.CrewID, func(q *RecapPushDoc) bool {
		return addBeltEvents(q, doc.CrewID, events, held, tzs, now)
	}) {
		logger.Info("belt push: queued for %d member(s) of crew %s", len(events), doc.CrewID)
		flushPushQueueAsync(nk, logger, key)
	}
}
