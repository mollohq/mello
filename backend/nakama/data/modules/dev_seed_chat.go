package main

// dev_seed_chat: chat history to test the chat with. Gamers gets four days of
// messages (day separators, two pages of history, grouped runs, mentions,
// markdown, emoji-only rows, a GIF, image messages, replies, reactions) and
// a read cursor for every member a few messages behind, so the feed opens
// at the unread rule. Devs gets a short burst today for the sidebar badge.
//
// Every seeded message carries `"seed": true`, so a rerun finds them and
// does nothing; `{"reset": true}` deletes them first. Dev only.

import (
	"bytes"
	"context"
	"database/sql"
	"encoding/json"
	"fmt"
	"image"
	"image/color"
	"image/png"
	"strings"
	"time"

	"github.com/heroiclabs/nakama-common/runtime"
)

type seedChatKind int

const (
	seedText seedChatKind = iota
	seedGif
	seedImage
)

// seedChatMsg is one planned message. `at` is the offset from "now",
// `reply` the index of the message it answers, `reactions` emoji -> sender
// names, `mentions` sender names to write as `<@user_id>` tokens.
type seedChatMsg struct {
	sender    string
	body      string
	at        time.Duration
	kind      seedChatKind
	reply     int
	reactions map[string][]string
}

const seedGifURL = "https://upload.wikimedia.org/wikipedia/commons/2/2c/Rotating_earth_%28large%29.gif"

// seedChatPlan is the Gamers history: alice, bob, diana over four days.
// Pure, so a test can check its shape.
func seedChatPlan() []seedChatMsg {
	day := 24 * time.Hour
	h := time.Hour
	m := time.Minute
	var plan []seedChatMsg
	add := func(sender, body string, at time.Duration) int {
		plan = append(plan, seedChatMsg{sender: sender, body: body, at: at, reply: -1})
		return len(plan) - 1
	}

	// Three days ago: the plan for the week.
	add("alice", "ok crew, ranked push this week?", -3*day-2*h)
	add("bob", "in. what time", -3*day-2*h+3*m)
	add("alice", "8 every night, **no excuses**", -3*day-2*h+4*m)
	add("diana", "can do 8 on wed and thu, fri is dinner", -3*day-2*h+9*m)
	add("bob", "<@alice> did you see the patch notes https://www.counter-strike.net/news", -3*day-1*h)
	add("alice", "reading now", -3*day-1*h+2*m)
	add("alice", "they nerfed the mp9 again", -3*day-1*h+3*m)
	add("diana", "finally", -3*day-1*h+10*m)

	// Two days ago: a long session, two pages of history.
	for i := 0; i < 34; i++ {
		sender := []string{"bob", "alice", "diana"}[i%3]
		body := []string{
			"round %d, nice site take",
			"round %d, I need a flash on long",
			"round %d, rotating",
			"round %d gg",
			"round %d, they are saving",
			"round %d, eco",
		}[i%6]
		add(sender, fmt.Sprintf(body, i+1), -2*day-3*h+time.Duration(i)*2*m)
	}
	add("alice", "that was a good one", -2*day-1*h)
	add("bob", "mp9 still fine btw", -2*day-1*h+1*m)

	// Yesterday: the fun stuff.
	add("diana", "🔥🔥", -1*day-5*h)
	add("bob", "💀", -1*day-5*h+1*m)
	add("alice", "😂😂😂", -1*day-5*h+2*m)
	gif := add("bob", "", -1*day-4*h)
	plan[gif].kind = seedGif
	add("diana", "not that one again", -1*day-4*h+1*m)
	img := add("alice", "look at this one", -1*day-3*h)
	plan[img].kind = seedImage
	r := add("bob", "that is actually clean", -1*day-3*h+2*m)
	plan[r].reply = img
	add("diana", "frame it", -1*day-3*h+3*m)
	add("alice", "this is a longer message to see how the row wraps when someone writes a lot of text in one go, with *italic* and **bold** and a link https://m3llo.app in the middle of it all", -1*day-2*h)
	add("bob", "🎮🎮🎮🎮", -1*day-2*h+5*m)

	// Today: the unread tail. The cursor sits before "gg ez".
	add("bob", "anyone on?", -2*h)
	add("alice", "ten minutes", -2*h+1*m)
	add("diana", "same", -2*h+2*m)
	add("bob", "gg ez", -40*m)
	add("alice", "<@bob> <@diana> one more?", -38*m)
	img2 := add("diana", "", -35*m)
	plan[img2].kind = seedImage
	add("bob", "ok that is the thumbnail", -34*m)
	add("alice", "👀", -30*m)
	add("diana", "🇸🇪 vs 🇳🇴 next", -20*m)
	add("bob", "lets go", -5*m)

	// Reactions: emoji -> who.
	plan[2].reactions = map[string][]string{"💀": {"bob", "diana"}}
	plan[gif].reactions = map[string][]string{"😂": {"alice", "diana"}, "🔥": {"alice"}}
	plan[img].reactions = map[string][]string{"🔥": {"bob", "diana"}, "👀": {"bob"}}
	plan[len(plan)-7].reactions = map[string][]string{"😂": {"alice", "diana"}}
	return plan
}

// seedChatCursorIndex is the plan index every member has read up to:
// the message before "gg ez", so the unread rule sits at "gg ez".
func seedChatCursorIndex(plan []seedChatMsg) int {
	for i, p := range plan {
		if p.body == "gg ez" {
			return i - 1
		}
	}
	return len(plan) - 1
}

// seedDevsPlan is a short burst today in Devs, read by alice only, so bob
// and charlie get a badge.
func seedDevsPlan() []seedChatMsg {
	m := time.Minute
	return []seedChatMsg{
		{sender: "charlie", body: "the ci lane is green again", at: -50 * m, reply: -1},
		{sender: "alice", body: "which one", at: -48 * m, reply: -1},
		{sender: "charlie", body: "linux, the sccache one", at: -47 * m, reply: -1},
		{sender: "bob", body: "nice", at: -30 * m, reply: -1},
		{sender: "charlie", body: "<@bob> can you check the windows runner", at: -12 * m, reply: -1},
		{sender: "alice", body: "👍", at: -10 * m, reply: -1},
	}
}

// seedPNG draws a gradient with a few bars, so two images tell apart.
func seedPNG(w, h int, seed uint8) ([]byte, error) {
	img := image.NewRGBA(image.Rect(0, 0, w, h))
	for y := 0; y < h; y++ {
		for x := 0; x < w; x++ {
			r := uint8(20 + (x*120/w+int(seed))%140)
			g := uint8(30 + (y*110/h)%120)
			b := uint8(50 + ((x+y)*90/(w+h)+int(seed)*3)%150)
			if (x/80+y/80)%7 == 0 {
				r, g, b = r/2, g/2, b/2
			}
			img.Set(x, y, color.RGBA{r, g, b, 255})
		}
	}
	var buf bytes.Buffer
	if err := png.Encode(&buf, img); err != nil {
		return nil, err
	}
	return buf.Bytes(), nil
}

type DevSeedChatRequest struct {
	Reset bool `json:"reset"`
}

func DevSeedChatRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	var req DevSeedChatRequest
	if payload != "" && payload != `""` {
		_ = json.Unmarshal([]byte(payload), &req)
	}

	type seedUser struct{ id, name string }
	users := map[string]seedUser{}
	for _, uname := range []string{"alice", "bob", "charlie", "diana"} {
		var id, display string
		if err := db.QueryRowContext(ctx, "SELECT id, display_name FROM users WHERE username = $1", uname).Scan(&id, &display); err != nil {
			return "", runtime.NewError("seed user "+uname+" not found; run seed.sh first", 9)
		}
		if display == "" {
			display = uname
		}
		users[uname] = seedUser{id: id, name: display}
	}
	crews := map[string]string{}
	for _, name := range []string{"Gamers", "Devs"} {
		var id string
		if err := db.QueryRowContext(ctx, "SELECT id FROM groups WHERE name = $1", name).Scan(&id); err != nil {
			return "", runtime.NewError("seed crew "+name+" not found; run seed.sh first", 9)
		}
		crews[name] = id
	}

	if req.Reset {
		// `content` is jsonb, so match the marker as JSON, not as text.
		res, err := db.ExecContext(ctx, `DELETE FROM message WHERE content @> '{"seed": true}'::jsonb`)
		if err != nil {
			return "", runtime.NewError("reset failed: "+err.Error(), 13)
		}
		n, _ := res.RowsAffected()
		logger.Info("dev_seed_chat: removed %d seeded messages", n)
	}

	now := time.Now().UTC()
	report := map[string]interface{}{}

	seedCrew := func(crewName string, plan []seedChatMsg, readers []string, cursorIdx int) error {
		crewID := crews[crewName]
		channelID, err := nk.ChannelIdBuild(ctx, "", crewID, runtime.Group)
		if err != nil {
			return fmt.Errorf("channel of %s: %w", crewName, err)
		}
		// Already seeded? Look for the marker in the newest page.
		if msgs, _, _, err := nk.ChannelMessagesList(ctx, channelID, 20, false, ""); err == nil {
			for _, m := range msgs {
				if strings.Contains(m.GetContent(), `"seed":true`) {
					report[crewName] = "already seeded"
					return nil
				}
			}
		}

		ids := make([]string, len(plan))
		imageN := 0
		for i, p := range plan {
			u := users[p.sender]
			body := p.body
			for name, su := range users {
				body = strings.ReplaceAll(body, "<@"+name+">", "<@"+su.id+">")
			}
			content := map[string]interface{}{"v": 1, "type": "text", "body": body, "seed": true}
			if p.reply >= 0 && ids[p.reply] != "" {
				content["reply_to"] = ids[p.reply]
			}
			switch p.kind {
			case seedGif:
				content["type"] = "gif"
				content["gif"] = map[string]interface{}{
					"id": "seed-earth", "url": seedGifURL, "preview": seedGifURL,
					"width": 400, "height": 400, "alt": "rotating earth",
				}
			case seedImage:
				if !ChatImagesConfigured() {
					logger.Warn("dev_seed_chat: chat bucket off, image message sent as text")
					content["body"] = "(an image would be here; the chat bucket is off)"
					break
				}
				imageN++
				var imgs []map[string]interface{}
				for n, dims := range [][2]int{{1200, 800}, {600, 900}} {
					w, h := dims[0], dims[1]
					data, err := seedPNG(w, h, uint8(imageN*40+n*90))
					if err != nil {
						return err
					}
					key := chatImageKey(crewID, u.id, fmt.Sprintf("seed-%d-%d", imageN, n), "png")
					if err := PutChatObject(ctx, key, "image/png", data); err != nil {
						return fmt.Errorf("put %s: %w", key, err)
					}
					imgs = append(imgs, map[string]interface{}{
						"url": ChatPublicURL(key), "width": w, "height": h,
						"bytes": len(data), "mime": "image/png",
					})
					if imageN == 2 {
						break // one image on the second message
					}
				}
				content["type"] = "image"
				content["images"] = imgs
			}
			ack, err := nk.ChannelMessageSend(ctx, channelID, content, u.id, u.name, true)
			if err != nil {
				return fmt.Errorf("send %d: %w", i, err)
			}
			ids[i] = ack.GetMessageId()
			at := now.Add(p.at)
			if _, err := db.ExecContext(ctx, "UPDATE message SET create_time = $1, update_time = $1 WHERE id = $2", at, ids[i]); err != nil {
				return fmt.Errorf("backdate %d: %w", i, err)
			}
		}

		for i, p := range plan {
			if len(p.reactions) == 0 {
				continue
			}
			doc := &ReactionsDoc{CrewID: crewID, Reactions: map[string][]string{}}
			for emoji, names := range p.reactions {
				for _, n := range names {
					doc.Reactions[emoji] = append(doc.Reactions[emoji], users[n].id)
				}
			}
			if err := writeReactionsDoc(ctx, nk, ids[i], doc, ""); err != nil {
				return fmt.Errorf("reactions %d: %w", i, err)
			}
		}

		if cursorIdx >= 0 && cursorIdx < len(plan) {
			cursor := map[string]string{
				"message_id": ids[cursorIdx],
				"time":       now.Add(plan[cursorIdx].at).Format(time.RFC3339),
			}
			value, _ := json.Marshal(cursor)
			for _, name := range readers {
				if _, err := nk.StorageWrite(ctx, []*runtime.StorageWrite{{
					Collection: "chat_read", Key: crewID, UserID: users[name].id,
					Value: string(value), PermissionRead: 1, PermissionWrite: 1,
				}}); err != nil {
					return fmt.Errorf("cursor %s: %w", name, err)
				}
			}
		}
		report[crewName] = fmt.Sprintf("%d messages", len(plan))
		return nil
	}

	plan := seedChatPlan()
	if err := seedCrew("Gamers", plan, []string{"alice", "bob", "diana"}, seedChatCursorIndex(plan)); err != nil {
		logger.Error("dev_seed_chat: %v", err)
		return "", runtime.NewError(err.Error(), 13)
	}
	devs := seedDevsPlan()
	if err := seedCrew("Devs", devs, []string{"alice"}, len(devs)-1); err != nil {
		logger.Error("dev_seed_chat: %v", err)
		return "", runtime.NewError(err.Error(), 13)
	}

	data, _ := json.Marshal(report)
	return string(data), nil
}
