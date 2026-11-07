package main

import (
	"context"
	"database/sql"
	"encoding/json"
	"testing"

	"github.com/heroiclabs/nakama-common/api"
	"github.com/heroiclabs/nakama-common/runtime"
)

// adminT calls an admin RPC as the admin Worker does: no user, the http key.
func adminT(t *testing.T, fn func(context.Context, runtime.Logger, *sql.DB, runtime.NakamaModule, string) (string, error), nk runtime.NakamaModule, payload string) (string, error) {
	t.Helper()
	return fn(context.Background(), testLogger(), nil, nk, payload)
}

// deletedMedia records the keys the takedown deletes.
func deletedMedia(t *testing.T) *[]string {
	t.Helper()
	var keys []string
	old := mediaDelete
	mediaDelete = func(_ context.Context, k []string) error {
		keys = append(keys, k...)
		return nil
	}
	t.Cleanup(func() { mediaDelete = old })
	return &keys
}

// reportedBanner is the gifts crew with an uploaded banner and two reports
// of it: one from a member, one from the join page.
func reportedBanner(t *testing.T) *fakeQuestsNk {
	t.Helper()
	nk := giftsCrew(t)
	if _, err := rpcT(t, SetCrewBannerRPC, nk, "own", `{"crew_id":"gc","kind":"upload","key":"crew-banners/gc/b.jpg"}`); err != nil {
		t.Fatal(err)
	}
	if _, err := rpcT(t, ReportCrewMediaRPC, nk, "ida", `{"crew_id":"gc","kind":"banner"}`); err != nil {
		t.Fatal(err)
	}
	if _, err := ReportCrewMediaRPC(context.Background(), testLogger(), nil, nk, `{"crew_id":"gc","kind":"banner"}`); err != nil {
		t.Fatal(err)
	}
	return nk
}

func listReportsT(t *testing.T, nk runtime.NakamaModule, payload string) []AdminMediaReport {
	t.Helper()
	out, err := adminT(t, AdminMediaReportsListRPC, nk, payload)
	if err != nil {
		t.Fatal(err)
	}
	var resp struct {
		Reports []AdminMediaReport `json:"reports"`
	}
	if err := json.Unmarshal([]byte(out), &resp); err != nil {
		t.Fatal(err)
	}
	return resp.Reports
}

func TestAdminMediaRPCsRejectAClientSession(t *testing.T) {
	nk := reportedBanner(t)
	for name, fn := range map[string]func(context.Context, runtime.Logger, *sql.DB, runtime.NakamaModule, string) (string, error){
		"list":     AdminMediaReportsListRPC,
		"takedown": AdminMediaTakedownRPC,
		"dismiss":  AdminMediaReportDismissRPC,
	} {
		if _, err := rpcT(t, fn, nk, "own", `{}`); errCode(err) != 7 {
			t.Fatalf("%s from a client session: %v, want PERMISSION_DENIED", name, err)
		}
	}
}

func TestAdminMediaReportsListShowsTheCrewAndTheLiveImage(t *testing.T) {
	nk := reportedBanner(t)
	nk.groups["gc"] = &api.Group{Id: "gc", Name: "Brawl Bros"}
	reports := listReportsT(t, nk, `{"status":"open"}`)
	if len(reports) != 2 {
		t.Fatalf("reports = %d, want 2", len(reports))
	}
	r := reports[0]
	if r.ID == "" || r.CrewID != "gc" || r.CrewName != "Brawl Bros" || r.Kind != "banner" || !r.Live || r.Status != "" {
		t.Fatalf("report = %+v", r)
	}
	if r.URL != "https://media.test/crew-banners/gc/b.jpg" {
		t.Fatalf("url = %s", r.URL)
	}
}

func TestAdminMediaTakedownRemovesTheBannerAndClosesItsReports(t *testing.T) {
	nk := reportedBanner(t)
	deleted := deletedMedia(t)
	id := listReportsT(t, nk, `{"status":"open"}`)[0].ID
	if _, err := adminT(t, AdminMediaTakedownRPC, nk, `{"report_id":"`+id+`","reason":"","actor":"bob"}`); errCode(err) != 3 {
		t.Fatalf("a takedown with no reason: %v", err)
	}
	out, err := adminT(t, AdminMediaTakedownRPC, nk, `{"report_id":"`+id+`","reason":"nudity","actor":"bob"}`)
	if err != nil {
		t.Fatal(err)
	}
	if out != `{"closed":2,"removed":true}` {
		t.Fatalf("takedown = %s", out)
	}
	if p, _ := readCrewProfile(context.Background(), nk, "gc"); p.Banner != nil {
		t.Fatalf("banner = %+v, want none", p.Banner)
	}
	if len(*deleted) != 1 || (*deleted)[0] != "crew-banners/gc/b.jpg" {
		t.Fatalf("deleted = %v", *deleted)
	}
	if open := listReportsT(t, nk, `{"status":"open"}`); len(open) != 0 {
		t.Fatalf("open reports = %d after the takedown", len(open))
	}
	all := listReportsT(t, nk, `{}`)
	for _, r := range all {
		if r.Status != "taken_down" || r.Reason != "nudity" || r.Actor != "bob" || r.DoneMs == 0 || r.Live {
			t.Fatalf("closed report = %+v", r)
		}
	}
}

func TestAdminMediaTakedownLeavesANewBanner(t *testing.T) {
	// The owner set another banner after the report. The takedown removes
	// only the reported image: here none, so it only closes the reports.
	nk := reportedBanner(t)
	deleted := deletedMedia(t)
	if _, err := rpcT(t, SetCrewBannerRPC, nk, "own", `{"crew_id":"gc","kind":"upload","key":"crew-banners/gc/c.jpg"}`); err != nil {
		t.Fatal(err)
	}
	id := listReportsT(t, nk, `{}`)[0].ID
	out, err := adminT(t, AdminMediaTakedownRPC, nk, `{"report_id":"`+id+`","reason":"nudity","actor":"bob"}`)
	if err != nil {
		t.Fatal(err)
	}
	if out != `{"closed":2,"removed":false}` {
		t.Fatalf("takedown = %s", out)
	}
	if p, _ := readCrewProfile(context.Background(), nk, "gc"); p.Banner == nil || p.Banner.Key != "crew-banners/gc/c.jpg" {
		t.Fatalf("banner = %+v, want the new one", p.Banner)
	}
	if len(*deleted) != 0 {
		t.Fatalf("deleted = %v, want nothing", *deleted)
	}
}

func TestAdminMediaTakedownRemovesOneEmoji(t *testing.T) {
	nk := giftsCrew(t)
	deleted := deletedMedia(t)
	markStep(t, nk, "gc", func(q *CrewQuests) { q.Steps.Call = &QuestCall{DoneMs: 1} })
	for _, name := range []string{"gg", "ez"} {
		if _, err := addEmoji(t, nk, "ida", name); err != nil {
			t.Fatal(err)
		}
	}
	if _, err := rpcT(t, ReportCrewMediaRPC, nk, "own", `{"crew_id":"gc","kind":"emoji","name":"ez"}`); err != nil {
		t.Fatal(err)
	}
	id := listReportsT(t, nk, `{}`)[0].ID
	if _, err := adminT(t, AdminMediaTakedownRPC, nk, `{"report_id":"`+id+`","reason":"slur","actor":"bob"}`); err != nil {
		t.Fatal(err)
	}
	d, _ := readCrewEmoji(context.Background(), nk, "gc")
	if len(d.Emoji) != 1 || d.Emoji[0].Name != "gg" {
		t.Fatalf("emoji = %+v, want gg only", d.Emoji)
	}
	if len(*deleted) != 1 || (*deleted)[0] != "crew-emoji/gc/ez.png" {
		t.Fatalf("deleted = %v", *deleted)
	}
}

func TestAdminMediaDismissKeepsTheImage(t *testing.T) {
	nk := reportedBanner(t)
	deleted := deletedMedia(t)
	id := listReportsT(t, nk, `{}`)[0].ID
	out, err := adminT(t, AdminMediaReportDismissRPC, nk, `{"report_id":"`+id+`","reason":"fine","actor":"bob"}`)
	if err != nil {
		t.Fatal(err)
	}
	if out != `{"closed":2}` {
		t.Fatalf("dismiss = %s", out)
	}
	if p, _ := readCrewProfile(context.Background(), nk, "gc"); p.Banner == nil {
		t.Fatal("a dismiss removed the banner")
	}
	if len(*deleted) != 0 {
		t.Fatalf("deleted = %v", *deleted)
	}
	for _, r := range listReportsT(t, nk, `{}`) {
		if r.Status != "dismissed" || !r.Live {
			t.Fatalf("report = %+v", r)
		}
	}
	if _, err := adminT(t, AdminMediaReportDismissRPC, nk, `{"report_id":"nope","reason":"x","actor":"bob"}`); errCode(err) != 5 {
		t.Fatalf("an unknown report: %v", err)
	}
}
