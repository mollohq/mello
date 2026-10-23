package main

// Head2head holders doc (plans/next-gen-feed.md sections 4 and 10, spec
// features/HEAD2HEAD.md).
//
// Each crew has one doc that says which member holds each title. The server
// computes it from the members' library docs and per-game stats, and writes
// it. The feed reads it and trims it for the viewer (see Head2headFeedData).
//
// Storage permission: read 0 and write 0, so only the server touches the doc.
// Nakama has no "group members" read level, and a public-read doc would let
// any client read it by crew id. The feed RPC reads it as the system user,
// after it checks crew membership, and returns the trimmed payload.

import (
	"context"
	"encoding/json"
	"fmt"
	"math"
	"sort"
	"sync"
	"time"

	"github.com/heroiclabs/nakama-common/runtime"
)

const (
	// CrewHead2headCollection holds one doc per crew under head2headKey(crew).
	CrewHead2headCollection = "crew_head2head"
	head2headKeyPrefix      = "holders_"

	// head2headSeats is the number of seats head2head shows. A crew with fewer
	// members shows the rest as open seats.
	head2headSeats = 5

	// head2headSessionDebounceMs is the minimum age of the doc before a session
	// end recomputes it. A library report always recomputes.
	head2headSessionDebounceMs = 60 * 1000

	// head2headMaxMembers bounds the member list read for one crew, as in
	// isCrewMember.
	head2headMaxMembers = 100
)

// Title ids. The client owns the copy for each id.
const (
	titleTheCarry   = "the_carry"
	titleLockedIn   = "locked_in"
	titleHasALife   = "has_a_life"
	titleNightOwl   = "night_owl"
	titleTheCollect = "the_collector"
	titleSampler    = "sampler"
	titleOneTrick   = "one_trick"

	// Session titles. They read the per-user activity doc
	// (user_activity_stats.go).
	titleMouth     = "mouth"
	titleMarathon  = "marathon"
	titleLastOneUp = "last_one_up"
	titleClipper   = "clipper"
	titleRageQuit  = "rage_quit"
	titleWinner    = "winner"
)

// head2headNightHr is the local hour where the night_owl day starts. 02:00
// ranks later than 23:00.
const head2headNightHr = 18

// Recompute triggers.
const (
	head2headTriggerLibrary = "library"
	head2headTriggerSession = "session"
)

// Head2headTitle is one contested title.
//
// Value is a JSON number for every title except night_owl and last_one_up,
// where it is the string "HH:MM" (local clock). Unit is "h", "installed",
// "games", "%", "min", "clips", "sessions", "wins", or "" for the two clock
// titles. SinceMs is when the current holder took the title. When the
// title changed hands, PreviousHolderUserID and MovedAtMs say from whom and
// when. Both are absent for a title that never changed hands.
type Head2headTitle struct {
	TitleID              string      `json:"title_id"`
	HolderUserID         string      `json:"holder_user_id"`
	Value                interface{} `json:"value"`
	Unit                 string      `json:"unit"`
	SinceMs              int64       `json:"since_ms"`
	PreviousHolderUserID string      `json:"previous_holder_user_id,omitempty"`
	MovedAtMs            int64       `json:"moved_at_ms,omitempty"`
}

// Head2headPairValue is what the two members of a pair hold in one title that
// both contend for. AValue and BValue use the same encoding as
// Head2headTitle.Value, so the client can compute the gap and draw the
// numbers. TitleID is in the order of the computation, not the display order.
type Head2headPairValue struct {
	TitleID string      `json:"title_id"`
	AValue  interface{} `json:"a_value"`
	BValue  interface{} `json:"b_value"`
}

// Head2headPair is the tally of two members, A before B in id order. A count
// is the number of contested titles that member would hold if only the two
// of them played. Values lists, for each of those titles, what each member
// holds. Its length is ACount + BCount. It is never null.
type Head2headPair struct {
	A      string               `json:"a"`
	B      string               `json:"b"`
	ACount int                  `json:"a_count"`
	BCount int                  `json:"b_count"`
	Values []Head2headPairValue `json:"values"`
}

// Head2headDoc is the per-crew holders doc. UpdatedAt is Unix milliseconds.
// Members are the crew members with library or stats data. MemberCount is the
// size of the whole crew, and OpenSeats is max(0, 5 - MemberCount).
type Head2headDoc struct {
	CrewID      string           `json:"crew_id"`
	Members     []string         `json:"members"`
	Titles      []Head2headTitle `json:"titles"`
	Pairs       []Head2headPair  `json:"pairs"`
	MemberCount int              `json:"member_count"`
	OpenSeats   int              `json:"open_seats"`
	UpdatedAt   int64            `json:"updated_at"`
}

func head2headKey(crewID string) string { return head2headKeyPrefix + crewID }

func head2headOpenSeats(memberCount int) int {
	if memberCount >= head2headSeats {
		return 0
	}
	if memberCount < 0 {
		return head2headSeats
	}
	return head2headSeats - memberCount
}

// h2hMemberData is what the computation reads for one member.
type h2hMemberData struct {
	UserID  string
	Library *LibraryDoc     // nil when the member does not share a library
	Stats   []UserGameStats // all per-game stats docs of the member
	// Activity is the per-user activity doc, nil when the member has none.
	Activity *UserActivityStats
}

func (m h2hMemberData) hasData() bool {
	return (m.Library != nil && len(m.Library.Games) > 0) || len(m.Stats) > 0 || m.Activity != nil
}

// h2hInput is the whole input of the pure computation.
type h2hInput struct {
	CrewID      string
	MemberCount int
	Members     []h2hMemberData
	Prev        *Head2headDoc
	NowMs       int64
}

// h2hResult is one title before the holder bookkeeping: each contender's
// integer score, and the direction.
type h2hResult struct {
	titleID    string
	unit       string
	higherWins bool
	scores     map[string]int64
}

// pickWinner returns the winner among the given users. On equal scores the
// previous holder keeps the title, else the lowest user id wins, so the
// result does not depend on map order.
func pickWinner(scores map[string]int64, users []string, higherWins bool, prevHolder string) string {
	best, bestScore, found := "", int64(0), false
	for _, u := range users {
		s, ok := scores[u]
		if !ok {
			continue
		}
		better := false
		switch {
		case !found:
			better = true
		case s == bestScore:
			better = u == prevHolder || (best != prevHolder && u < best)
		case higherWins:
			better = s > bestScore
		default:
			better = s < bestScore
		}
		if better {
			best, bestScore, found = u, s, true
		}
	}
	return best
}

func (r h2hResult) users() []string {
	out := make([]string, 0, len(r.scores))
	for u := range r.scores {
		out = append(out, u)
	}
	sort.Strings(out)
	return out
}

func (r h2hResult) value(score int64) interface{} {
	switch r.titleID {
	case titleTheCarry, titleLockedIn, titleHasALife:
		return math.Round(float64(score)/60*10) / 10 // minutes to hours, one decimal
	case titleOneTrick:
		return int(math.Round(float64(score) / 100)) // basis points to percent
	case titleNightOwl, titleLastOneUp:
		m := (int(score) + head2headNightHr*60) % 1440
		return fmt.Sprintf("%02d:%02d", m/60, m%60)
	}
	return int(score)
}

// gameMinutes is the lifetime minutes of one game for a member: the library
// figure when there is one, else the minutes m3llo observed.
func gameMinutes(lib *LibraryGame, stats *UserGameStats) int {
	if lib != nil && lib.PlaytimeMin != nil {
		return *lib.PlaytimeMin
	}
	if stats != nil {
		return stats.PlayedMinTotal
	}
	return 0
}

// memberGameMinutes returns lifetime minutes per game id for one member, from
// the library and the observed stats.
func memberGameMinutes(m h2hMemberData) map[string]int {
	libByID := map[string]*LibraryGame{}
	if m.Library != nil {
		for i := range m.Library.Games {
			libByID[m.Library.Games[i].GameID] = &m.Library.Games[i]
		}
	}
	statByID := map[string]*UserGameStats{}
	for i := range m.Stats {
		statByID[m.Stats[i].GameID] = &m.Stats[i]
	}
	out := map[string]int{}
	for id, l := range libByID {
		out[id] = gameMinutes(l, statByID[id])
	}
	for id, s := range statByID {
		if _, ok := libByID[id]; !ok {
			out[id] = gameMinutes(nil, s)
		}
	}
	return out
}

// recentMinutes sums game minutes on or after the UTC date cutoff.
func recentMinutes(stats []UserGameStats, cutoff string) int {
	total := 0
	for _, s := range stats {
		for _, d := range s.RecentDays {
			if d.Date >= cutoff {
				total += d.WallMin
			}
		}
	}
	return total
}

func cutoffDate(nowMs int64, days int) string {
	return time.UnixMilli(nowMs).UTC().AddDate(0, 0, -(days - 1)).Format("2006-01-02")
}

// computeTheCarry: the top shared game is the one installed by the most
// members, ties by the total minutes of its owners. The score is the member's
// minutes in that game. Members with no figure for it do not contend.
func computeTheCarry(members []h2hMemberData) h2hResult {
	r := h2hResult{titleID: titleTheCarry, unit: "h", higherWins: true, scores: map[string]int64{}}
	owners := map[string]int{}
	totals := map[string]int{}
	for _, m := range members {
		if m.Library == nil {
			continue
		}
		mins := memberGameMinutes(m)
		for _, g := range m.Library.Games {
			owners[g.GameID]++
			totals[g.GameID] += mins[g.GameID]
		}
	}
	top := ""
	for id, n := range owners {
		switch {
		case top == "":
			top = id
		case n > owners[top], n == owners[top] && totals[id] > totals[top],
			n == owners[top] && totals[id] == totals[top] && id < top:
			top = id
		}
	}
	if top == "" {
		return r
	}
	for _, m := range members {
		if m.Library == nil {
			continue
		}
		for i := range m.Library.Games {
			if m.Library.Games[i].GameID != top {
				continue
			}
			var st *UserGameStats
			for j := range m.Stats {
				if m.Stats[j].GameID == top {
					st = &m.Stats[j]
				}
			}
			l := m.Library.Games[i]
			if l.PlaytimeMin == nil && (st == nil || st.PlayedMinTotal == 0) {
				continue // no figure
			}
			r.scores[m.UserID] = int64(gameMinutes(&l, st))
		}
	}
	return r
}

func computeWeekTitles(members []h2hMemberData, nowMs int64) (locked, life h2hResult) {
	locked = h2hResult{titleID: titleLockedIn, unit: "h", higherWins: true, scores: map[string]int64{}}
	life = h2hResult{titleID: titleHasALife, unit: "h", higherWins: false, scores: map[string]int64{}}
	cut := cutoffDate(nowMs, 7)
	for _, m := range members {
		if min := recentMinutes(m.Stats, cut); min > 0 {
			locked.scores[m.UserID] = int64(min)
			life.scores[m.UserID] = int64(min) // has a session in the window
		}
	}
	return locked, life
}

func computeCollector(members []h2hMemberData) h2hResult {
	r := h2hResult{titleID: titleTheCollect, unit: "installed", higherWins: true, scores: map[string]int64{}}
	for _, m := range members {
		if m.Library != nil && len(m.Library.Games) > 0 {
			r.scores[m.UserID] = int64(len(m.Library.Games))
		}
	}
	return r
}

func computeSampler(members []h2hMemberData, nowMs int64) h2hResult {
	r := h2hResult{titleID: titleSampler, unit: "games", higherWins: true, scores: map[string]int64{}}
	cut := cutoffDate(nowMs, 14)
	for _, m := range members {
		n := 0
		for _, s := range m.Stats {
			for _, d := range s.RecentDays {
				if d.Date >= cut && d.WallMin > 0 {
					n++
					break
				}
			}
		}
		if n > 0 {
			r.scores[m.UserID] = int64(n)
		}
	}
	return r
}

func computeOneTrick(members []h2hMemberData) h2hResult {
	r := h2hResult{titleID: titleOneTrick, unit: "%", higherWins: true, scores: map[string]int64{}}
	for _, m := range members {
		total, top := 0, 0
		for _, min := range memberGameMinutes(m) {
			total += min
			if min > top {
				top = min
			}
		}
		if total > 0 {
			r.scores[m.UserID] = int64(top) * 10000 / int64(total) // basis points
		}
	}
	return r
}

// nightOwlKey is minutes after 18:00 local, so a later night scores higher.
func nightOwlKey(lastPlayedMs int64, tzOffsetMin int) int64 {
	local := time.UnixMilli(lastPlayedMs).UTC().Add(time.Duration(tzOffsetMin) * time.Minute)
	m := local.Hour()*60 + local.Minute()
	return int64((m - head2headNightHr*60 + 1440) % 1440)
}

// computeNightOwl uses the last session of each game that carried a timezone
// offset and ended in the last 14 days. A member with no such session does not
// contend.
func computeNightOwl(members []h2hMemberData, nowMs int64) h2hResult {
	r := h2hResult{titleID: titleNightOwl, unit: "", higherWins: true, scores: map[string]int64{}}
	oldest := nowMs - 14*24*60*60*1000
	for _, m := range members {
		for _, s := range m.Stats {
			if s.LastTzOffsetMin == nil || s.LastPlayed < oldest {
				continue
			}
			k := nightOwlKey(s.LastPlayed, *s.LastTzOffsetMin)
			if cur, ok := r.scores[m.UserID]; !ok || k > cur {
				r.scores[m.UserID] = k
			}
		}
	}
	return r
}

// activityWindow returns the day buckets inside the last `days` UTC days.
func activityWindow(a *UserActivityStats, nowMs int64, days int) []ActivityDay {
	if a == nil {
		return nil
	}
	cut := cutoffDate(nowMs, days)
	var out []ActivityDay
	for _, d := range a.Days {
		if d.Date >= cut {
			out = append(out, d)
		}
	}
	return out
}

// computeActivityTitles computes the six session titles from the activity
// docs. A member contends for a title only with a score above zero (for
// last_one_up, with a session end that carried a time zone offset).
func computeActivityTitles(members []h2hMemberData, nowMs int64) []h2hResult {
	mk := func(id, unit string) h2hResult {
		return h2hResult{titleID: id, unit: unit, higherWins: true, scores: map[string]int64{}}
	}
	mouth := mk(titleMouth, "min")
	marathon := mk(titleMarathon, "min")
	lastUp := mk(titleLastOneUp, "")
	clipper := mk(titleClipper, "clips")
	rage := mk(titleRageQuit, "sessions")
	winner := mk(titleWinner, "wins")

	for _, m := range members {
		a := m.Activity
		if a == nil {
			continue
		}
		voice, short, wins := 0, 0, 0
		bestNight, hasEnd := 0, false
		for _, d := range activityWindow(a, nowMs, 14) {
			voice += d.VoiceMin
			short += d.ShortSessions
			wins += d.WinMoments
			if d.LatestEndLocalMin != nil {
				if k := activityNightKey(*d.LatestEndLocalMin); !hasEnd || k > bestNight {
					bestNight, hasEnd = k, true
				}
			}
		}
		if voice > 0 {
			mouth.scores[m.UserID] = int64(voice)
		}
		if a.LongestSessionMin > 0 {
			marathon.scores[m.UserID] = int64(a.LongestSessionMin)
		}
		if hasEnd {
			lastUp.scores[m.UserID] = int64(bestNight)
		}
		if a.Clips > 0 {
			clipper.scores[m.UserID] = int64(a.Clips)
		}
		if short > 0 {
			rage.scores[m.UserID] = int64(short)
		}
		if wins > 0 {
			winner.scores[m.UserID] = int64(wins)
		}
	}
	return []h2hResult{mouth, marathon, lastUp, clipper, rage, winner}
}

// computeHead2head is the pure computation: results to titles, holder
// bookkeeping against the previous doc, and pair tallies.
func computeHead2head(in h2hInput) *Head2headDoc {
	members := make([]h2hMemberData, 0, len(in.Members))
	for _, m := range in.Members {
		if m.hasData() {
			members = append(members, m)
		}
	}
	sort.Slice(members, func(i, j int) bool { return members[i].UserID < members[j].UserID })
	ids := make([]string, 0, len(members))
	for _, m := range members {
		ids = append(ids, m.UserID)
	}

	locked, life := computeWeekTitles(members, in.NowMs)
	results := []h2hResult{
		computeTheCarry(members),
		locked,
		life,
		computeNightOwl(members, in.NowMs),
		computeCollector(members),
		computeSampler(members, in.NowMs),
		computeOneTrick(members),
	}
	results = append(results, computeActivityTitles(members, in.NowMs)...)

	prevByTitle := map[string]Head2headTitle{}
	if in.Prev != nil {
		for _, t := range in.Prev.Titles {
			prevByTitle[t.TitleID] = t
		}
	}

	titles := make([]Head2headTitle, 0, len(results))
	contested := make([]h2hResult, 0, len(results))
	for _, r := range results {
		if len(r.scores) < 2 {
			continue // not contested
		}
		prev, hadPrev := prevByTitle[r.titleID]
		users := r.users()
		holder := pickWinner(r.scores, users, r.higherWins, prev.HolderUserID)
		t := Head2headTitle{
			TitleID:      r.titleID,
			HolderUserID: holder,
			Value:        r.value(r.scores[holder]),
			Unit:         r.unit,
			SinceMs:      in.NowMs,
		}
		switch {
		case hadPrev && prev.HolderUserID == holder:
			t.SinceMs = prev.SinceMs
			t.PreviousHolderUserID = prev.PreviousHolderUserID
			t.MovedAtMs = prev.MovedAtMs
		case hadPrev:
			t.PreviousHolderUserID = prev.HolderUserID
			t.MovedAtMs = in.NowMs
		}
		titles = append(titles, t)
		contested = append(contested, r)
	}

	pairs := make([]Head2headPair, 0, len(ids)*(len(ids)-1)/2)
	for i := 0; i < len(ids); i++ {
		for j := i + 1; j < len(ids); j++ {
			p := Head2headPair{A: ids[i], B: ids[j], Values: []Head2headPairValue{}}
			for _, r := range contested {
				aScore, aIn := r.scores[p.A]
				bScore, bIn := r.scores[p.B]
				if !aIn || !bIn {
					continue
				}
				p.Values = append(p.Values, Head2headPairValue{
					TitleID: r.titleID,
					AValue:  r.value(aScore),
					BValue:  r.value(bScore),
				})
				if pickWinner(r.scores, []string{p.A, p.B}, r.higherWins, prevByTitle[r.titleID].HolderUserID) == p.A {
					p.ACount++
				} else {
					p.BCount++
				}
			}
			pairs = append(pairs, p)
		}
	}

	return &Head2headDoc{
		CrewID:      in.CrewID,
		Members:     ids,
		Titles:      titles,
		Pairs:       pairs,
		MemberCount: in.MemberCount,
		OpenSeats:   head2headOpenSeats(in.MemberCount),
		UpdatedAt:   in.NowMs,
	}
}

// ---------------------------------------------------------------------------
// Storage and recompute
// ---------------------------------------------------------------------------

func readHead2headDoc(ctx context.Context, nk runtime.NakamaModule, crewID string) (*Head2headDoc, string) {
	objects, err := nk.StorageRead(ctx, []*runtime.StorageRead{
		{Collection: CrewHead2headCollection, Key: head2headKey(crewID), UserID: SystemUserID},
	})
	if err != nil || len(objects) == 0 {
		return nil, ""
	}
	var doc Head2headDoc
	if json.Unmarshal([]byte(objects[0].GetValue()), &doc) != nil {
		return nil, ""
	}
	return &doc, objects[0].GetVersion()
}

func writeHead2headDoc(ctx context.Context, nk runtime.NakamaModule, doc *Head2headDoc, version string) error {
	data, err := json.Marshal(doc)
	if err != nil {
		return err
	}
	_, err = nk.StorageWrite(ctx, []*runtime.StorageWrite{{
		Collection:      CrewHead2headCollection,
		Key:             head2headKey(doc.CrewID),
		UserID:          SystemUserID,
		Value:           string(data),
		Version:         version,
		PermissionRead:  0, // server only: the feed RPC reads it as system
		PermissionWrite: 0, // server only
	}})
	return err
}

// crewMemberIDs lists the user ids of the crew members, joined members only.
func crewMemberIDs(ctx context.Context, nk runtime.NakamaModule, crewID string) ([]string, error) {
	members, _, err := nk.GroupUsersList(ctx, crewID, head2headMaxMembers, nil, "")
	if err != nil {
		return nil, err
	}
	ids := make([]string, 0, len(members))
	for _, m := range members {
		if st := m.GetState(); st != nil && st.GetValue() > 2 {
			continue // join request or ban, not a member
		}
		if id := m.GetUser().GetId(); id != "" {
			ids = append(ids, id)
		}
	}
	return ids, nil
}

// readAllUserGameStats reads every per-game stats doc of the given users as
// the system.
func readAllUserGameStats(ctx context.Context, nk runtime.NakamaModule, userIDs []string) map[string][]UserGameStats {
	out := make(map[string][]UserGameStats, len(userIDs))
	for _, uid := range userIDs {
		cursor := ""
		for {
			objects, next, err := nk.StorageList(ctx, "", uid, UserGameStatsCollection, 100, cursor)
			if err != nil {
				break
			}
			for _, o := range objects {
				var s UserGameStats
				if json.Unmarshal([]byte(o.GetValue()), &s) == nil {
					out[uid] = append(out[uid], s)
				}
			}
			if next == "" {
				break
			}
			cursor = next
		}
	}
	return out
}

// readAllUserActivityStats reads the activity doc of each given user as the
// system. A user with no doc is absent from the result.
func readAllUserActivityStats(ctx context.Context, nk runtime.NakamaModule, userIDs []string) map[string]*UserActivityStats {
	out := make(map[string]*UserActivityStats, len(userIDs))
	if len(userIDs) == 0 {
		return out
	}
	reads := make([]*runtime.StorageRead, 0, len(userIDs))
	for _, uid := range userIDs {
		reads = append(reads, &runtime.StorageRead{Collection: UserActivityStatsCollection, Key: userActivityStatsKey, UserID: uid})
	}
	objects, err := nk.StorageRead(ctx, reads)
	if err != nil {
		return out
	}
	for _, o := range objects {
		var a UserActivityStats
		if json.Unmarshal([]byte(o.GetValue()), &a) == nil {
			out[o.GetUserId()] = &a
		}
	}
	return out
}

// recomputeHead2head recomputes and stores the holders doc of one crew. A
// session trigger skips when the doc is under head2headSessionDebounceMs old.
// A library trigger never skips. It returns the doc written, or nil when it
// skipped or failed.
func recomputeHead2head(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule, crewID, trigger string, nowMs int64) *Head2headDoc {
	ids, err := crewMemberIDs(ctx, nk, crewID)
	if err != nil {
		logger.Warn("head2head: member list failed for crew %s: %v", crewID, err)
		return nil
	}
	libs := readLibraryDocs(ctx, nk, ids)
	stats := readAllUserGameStats(ctx, nk, ids)
	activity := readAllUserActivityStats(ctx, nk, ids)
	members := make([]h2hMemberData, 0, len(ids))
	for _, id := range ids {
		members = append(members, h2hMemberData{UserID: id, Library: libs[id], Stats: stats[id], Activity: activity[id]})
	}

	for attempt := 0; attempt < 3; attempt++ {
		prev, version := readHead2headDoc(ctx, nk, crewID)
		if trigger == head2headTriggerSession && prev != nil && nowMs-prev.UpdatedAt < head2headSessionDebounceMs {
			// The stats behind this trigger are newer than the doc. Mark the
			// crew so the dirty loop recomputes it once the debounce has run
			// out, or the last session of a burst would never show.
			markHead2headDirty(crewID)
			return nil
		}
		doc := computeHead2head(h2hInput{CrewID: crewID, MemberCount: len(ids), Members: members, Prev: prev, NowMs: nowMs})
		if err := writeHead2headDoc(ctx, nk, doc, version); err != nil {
			logger.Debug("head2head: write retry for crew %s: %v", crewID, err)
			continue
		}
		clearHead2headDirty(crewID)
		queueBeltPushes(ctx, logger, nk, doc)
		return doc
	}
	logger.Warn("head2head: write failed for crew %s after 3 tries", crewID)
	return nil
}

// ---------------------------------------------------------------------------
// Trailing recompute
// ---------------------------------------------------------------------------

// head2headDirtyInterval is how often the dirty loop looks for crews whose
// session trigger the debounce skipped. It equals the debounce, so a skipped
// crew waits one more tick at most.
const head2headDirtyInterval = 60 * time.Second

var head2headDirty = struct {
	mu    sync.Mutex
	crews map[string]bool
}{crews: map[string]bool{}}

func markHead2headDirty(crewID string) {
	head2headDirty.mu.Lock()
	head2headDirty.crews[crewID] = true
	head2headDirty.mu.Unlock()
}

func clearHead2headDirty(crewID string) {
	head2headDirty.mu.Lock()
	delete(head2headDirty.crews, crewID)
	head2headDirty.mu.Unlock()
}

// takeHead2headDirty returns the dirty crews and empties the set.
func takeHead2headDirty() []string {
	head2headDirty.mu.Lock()
	defer head2headDirty.mu.Unlock()
	out := make([]string, 0, len(head2headDirty.crews))
	for id := range head2headDirty.crews {
		out = append(out, id)
	}
	head2headDirty.crews = map[string]bool{}
	return out
}

// flushHead2headDirty recomputes each dirty crew as a session trigger. A crew
// still inside the debounce marks itself again and waits for the next tick.
// The set lives in memory: after a restart the next trigger or feed read of
// the crew recomputes it.
func flushHead2headDirty(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule, nowMs int64) {
	for _, crewID := range takeHead2headDirty() {
		recomputeHead2head(ctx, logger, nk, crewID, head2headTriggerSession, nowMs)
	}
}

// StartHead2headDirtyLoop runs flushHead2headDirty on its own ticker. The only
// other ticker on the recap path runs hourly, which is too slow for a feed.
func StartHead2headDirtyLoop(ctx context.Context, nk runtime.NakamaModule, logger runtime.Logger, interval time.Duration) {
	ticker := time.NewTicker(interval)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case t := <-ticker.C:
			func() {
				defer func() {
					if r := recover(); r != nil {
						logger.Error("head2head: dirty loop panic: %v", r)
					}
				}()
				flushHead2headDirty(ctx, logger, nk, t.UnixMilli())
			}()
		}
	}
}

// recomputeHead2headForUser recomputes the doc of every crew the user is in.
// Never fails the caller: head2head is derived data.
func recomputeHead2headForUser(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule, userID, trigger string) {
	groups, _, err := nk.UserGroupsList(ctx, userID, 100, nil, "")
	if err != nil {
		logger.Warn("head2head: crew list failed for user %s: %v", userID, err)
		return
	}
	now := time.Now().UnixMilli()
	// The user is the actor: a belt they take is no news to them.
	ctx = withHead2headActor(ctx, userID)
	for _, g := range groups {
		if st := g.GetState(); st != nil && st.GetValue() > 2 {
			continue
		}
		if id := g.GetGroup().GetId(); id != "" {
			recomputeHead2head(ctx, logger, nk, id, trigger, now)
		}
	}
}

// validTzOffset returns the offset when it is a real UTC offset (-14 h to
// +14 h), else nil.
func validTzOffset(v *int) *int {
	if v == nil || *v < -840 || *v > 840 {
		return nil
	}
	return v
}

// ---------------------------------------------------------------------------
// Feed payload
// ---------------------------------------------------------------------------

// head2headFeedID is the id and type of the head2head entry in this_week.
const head2headFeedID = "head2head"

// Head2headFeedData is the data of the head2head entry in this_week. The feed
// builds it for every crew. It is not a ledger event. The client renders it.
//
//   - Titles: every contested title, so the client can draw the belt row.
//   - Pairs: only the pairs that include the viewer, with the viewer as A.
//     The counts and the values follow the swap. Values lists the contested
//     titles of the pair with both members' values, for the deciding chip.
//   - MemberCount and OpenSeats come from the live crew size, not the doc.
//   - BeltMovedRecent: the title with the newest MovedAtMs, when that is less
//     than 24 h old. Nil otherwise. The same title also sits in Titles.
//   - UpdatedAt: when the doc was computed. Zero when no doc exists yet.
//   - OwnValues: what the viewer holds in every title their own data can
//     compute, contested or not, so a crew of one still has chips to show. A
//     title with no data for the viewer is absent. Never null.
type Head2headFeedData struct {
	CrewID          string              `json:"crew_id"`
	MemberCount     int                 `json:"member_count"`
	OpenSeats       int                 `json:"open_seats"`
	Titles          []Head2headTitle    `json:"titles"`
	Pairs           []Head2headPair     `json:"pairs"`
	BeltMovedRecent *Head2headTitle     `json:"belt_moved_recent,omitempty"`
	UpdatedAt       int64               `json:"updated_at"`
	OwnValues       []Head2headOwnValue `json:"own_values"`
}

// Head2headOwnValue is the viewer's value in one title. Value and Unit use the
// encoding of Head2headTitle. Display is the value as text, ready to draw:
// whole hours from 100 up, one decimal below, thousands separated.
type Head2headOwnValue struct {
	TitleID string      `json:"title_id"`
	Value   interface{} `json:"value"`
	Unit    string      `json:"unit"`
	Display string      `json:"display"`
}

// ownValueOrder is the order of own_values. It follows the display order of
// the client card, then the session titles.
var ownValueOrder = []string{
	titleTheCarry, titleNightOwl, titleTheCollect, titleLockedIn, titleHasALife,
	titleSampler, titleOneTrick,
	titleMouth, titleMarathon, titleLastOneUp, titleClipper, titleRageQuit, titleWinner,
}

// groupedInt writes 4167 as "4,167".
func groupedInt(n int64) string {
	neg := n < 0
	if neg {
		n = -n
	}
	digits := fmt.Sprintf("%d", n)
	out := make([]byte, 0, len(digits)+len(digits)/3+1)
	for i := 0; i < len(digits); i++ {
		if i > 0 && (len(digits)-i)%3 == 0 {
			out = append(out, ',')
		}
		out = append(out, digits[i])
	}
	if neg {
		return "-" + string(out)
	}
	return string(out)
}

// ownDisplay is the text of one value.
func ownDisplay(value interface{}, unit string) string {
	switch v := value.(type) {
	case string:
		return v
	case float64:
		if unit == "h" && math.Abs(v) < 100 {
			return fmt.Sprintf("%.1f", v)
		}
		return groupedInt(int64(math.Round(v)))
	case int:
		return groupedInt(int64(v))
	}
	return ""
}

// computeOwnValues runs every title computation for the one member and keeps
// the member's own score. It reuses the contested computations, so a value
// here is the value the member would hold in a crew of any size.
func computeOwnValues(m h2hMemberData, nowMs int64) []Head2headOwnValue {
	out := []Head2headOwnValue{}
	if !m.hasData() {
		return out
	}
	ms := []h2hMemberData{m}
	locked, life := computeWeekTitles(ms, nowMs)
	results := []h2hResult{
		computeTheCarry(ms),
		locked,
		life,
		computeNightOwl(ms, nowMs),
		computeCollector(ms),
		computeSampler(ms, nowMs),
		computeOneTrick(ms),
	}
	results = append(results, computeActivityTitles(ms, nowMs)...)
	byID := map[string]h2hResult{}
	for _, r := range results {
		byID[r.titleID] = r
	}
	for _, id := range ownValueOrder {
		r, ok := byID[id]
		if !ok {
			continue
		}
		score, has := r.scores[m.UserID]
		if !has {
			continue
		}
		v := r.value(score)
		out = append(out, Head2headOwnValue{TitleID: id, Value: v, Unit: r.unit, Display: ownDisplay(v, r.unit)})
	}
	return out
}

// readMemberData reads the library, the per-game stats and the activity doc of
// one user.
func readMemberData(ctx context.Context, nk runtime.NakamaModule, userID string) h2hMemberData {
	ids := []string{userID}
	return h2hMemberData{
		UserID:   userID,
		Library:  readLibraryDocs(ctx, nk, ids)[userID],
		Stats:    readAllUserGameStats(ctx, nk, ids)[userID],
		Activity: readAllUserActivityStats(ctx, nk, ids)[userID],
	}
}

// trimHead2headForViewer builds the feed payload from the doc. doc may be nil.
func trimHead2headForViewer(doc *Head2headDoc, crewID, viewerID string, memberCount int, nowMs int64) *Head2headFeedData {
	out := &Head2headFeedData{
		CrewID:      crewID,
		MemberCount: memberCount,
		OpenSeats:   head2headOpenSeats(memberCount),
		Titles:      []Head2headTitle{},
		Pairs:       []Head2headPair{},
		OwnValues:   []Head2headOwnValue{},
	}
	if doc == nil {
		return out
	}
	out.UpdatedAt = doc.UpdatedAt
	out.Titles = append(out.Titles, doc.Titles...)
	for _, p := range doc.Pairs {
		switch viewerID {
		case p.A:
			if p.Values == nil {
				p.Values = []Head2headPairValue{} // doc written before values existed
			}
			out.Pairs = append(out.Pairs, p)
		case p.B:
			swapped := Head2headPair{A: p.B, B: p.A, ACount: p.BCount, BCount: p.ACount, Values: make([]Head2headPairValue, 0, len(p.Values))}
			for _, v := range p.Values {
				swapped.Values = append(swapped.Values, Head2headPairValue{TitleID: v.TitleID, AValue: v.BValue, BValue: v.AValue})
			}
			out.Pairs = append(out.Pairs, swapped)
		}
	}
	var newest *Head2headTitle
	for i := range out.Titles {
		t := &out.Titles[i]
		if t.MovedAtMs > 0 && nowMs-t.MovedAtMs <= feedHeroFreshMs && (newest == nil || t.MovedAtMs > newest.MovedAtMs) {
			newest = t
		}
	}
	if newest != nil {
		c := *newest
		out.BeltMovedRecent = &c
	}
	return out
}

// crewHead2headForViewer reads or, for a crew with no doc yet, computes the
// holders doc and trims it for the viewer. It returns nil only when the member
// list cannot be read.
func crewHead2headForViewer(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule, crewID, viewerID string, nowMs int64) *Head2headFeedData {
	ids, err := crewMemberIDs(ctx, nk, crewID)
	if err != nil {
		return nil
	}
	doc, _ := readHead2headDoc(ctx, nk, crewID)
	if doc == nil {
		// A crew that existed before head2head shipped. Build the doc once.
		doc = recomputeHead2head(ctx, logger, nk, crewID, head2headTriggerLibrary, nowMs)
	}
	out := trimHead2headForViewer(doc, crewID, viewerID, len(ids), nowMs)
	out.OwnValues = computeOwnValues(readMemberData(ctx, nk, viewerID), nowMs)
	return out
}
