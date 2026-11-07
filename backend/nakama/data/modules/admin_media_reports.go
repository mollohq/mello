package main

// The admin side of the media reports (report_crew_media in crew_gifts.go):
// a list for the admin Reports page, a takedown and a dismiss. All three are
// server-to-server only, as the other admin RPCs (spec 04 section 4).

import (
	"context"
	"database/sql"
	"encoding/json"
	"sort"
	"strings"
	"time"

	"github.com/heroiclabs/nakama-common/runtime"
)

// maxMediaReportsListed bounds one list. Reports are rare; the page shows
// the open ones first.
const maxMediaReportsListed = 500

// mediaDelete deletes media bucket objects. A var so a test sees the keys.
var mediaDelete = func(ctx context.Context, keys []string) error {
	if !ChatImagesConfigured() {
		return nil
	}
	return DeleteChatObjects(ctx, keys)
}

// AdminMediaReport is one report in the admin_media_reports_list response.
// Live says the reported image is still the crew's banner or emoji.
type AdminMediaReport struct {
	ID string `json:"id"`
	MediaReport
	CrewName string `json:"crew_name"`
	Live     bool   `json:"live"`
}

// readMediaReport reads one report and its storage version.
func readMediaReport(ctx context.Context, nk runtime.NakamaModule, id string) (*MediaReport, string, error) {
	objects, err := nk.StorageRead(ctx, []*runtime.StorageRead{
		{Collection: MediaReportsCollection, Key: id, UserID: SystemUserID},
	})
	if err != nil {
		return nil, "", err
	}
	if len(objects) == 0 {
		return nil, "", nil
	}
	var r MediaReport
	if err := json.Unmarshal([]byte(objects[0].GetValue()), &r); err != nil {
		return nil, "", err
	}
	return &r, objects[0].GetVersion(), nil
}

// listMediaReports reads every report, newest first, up to the bound.
func listMediaReports(ctx context.Context, nk runtime.NakamaModule) ([]AdminMediaReport, error) {
	var out []AdminMediaReport
	cursor := ""
	for len(out) < maxMediaReportsListed {
		objects, next, err := nk.StorageList(ctx, "", SystemUserID, MediaReportsCollection, 100, cursor)
		if err != nil {
			return nil, err
		}
		for _, o := range objects {
			var r MediaReport
			if json.Unmarshal([]byte(o.GetValue()), &r) == nil {
				out = append(out, AdminMediaReport{ID: o.GetKey(), MediaReport: r})
			}
		}
		if next == "" || len(objects) == 0 {
			break
		}
		cursor = next
	}
	sort.SliceStable(out, func(i, j int) bool { return out[i].Ts > out[j].Ts })
	if len(out) > maxMediaReportsListed {
		out = out[:maxMediaReportsListed]
	}
	return out, nil
}

// mediaLive reports whether the image of r is still the crew's banner or
// one of its emoji.
func mediaLive(p *CrewProfile, e *CrewEmojiDoc, r MediaReport) bool {
	switch r.Kind {
	case "banner":
		return p != nil && p.Banner != nil && p.Banner.Kind == "upload" && p.Banner.URL == r.URL
	case "emoji":
		if e == nil {
			return false
		}
		for _, x := range e.Emoji {
			if x.Name == r.Name && x.URL == r.URL {
				return true
			}
		}
	}
	return false
}

// AdminMediaReportsListRPC lists the reports, newest first. Request
// {"status": "open"} lists the open ones only; anything else lists all.
func AdminMediaReportsListRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	if err := requireServerCaller(ctx, "admin_media_reports_list"); err != nil {
		return "", err
	}
	var req struct {
		Status string `json:"status"`
	}
	if payload != "" {
		if err := json.Unmarshal([]byte(payload), &req); err != nil {
			return "", runtime.NewError("invalid request", 3)
		}
	}
	all, err := listMediaReports(ctx, nk)
	if err != nil {
		logger.Error("admin_media_reports_list: %v", err)
		return "", runtime.NewError("reports read failed", 13)
	}
	reports := make([]AdminMediaReport, 0, len(all))
	for _, r := range all {
		if req.Status == "open" && r.Status != "" {
			continue
		}
		reports = append(reports, r)
	}

	// One read per crew for the name, the banner and the emoji.
	crewIDs := map[string]bool{}
	for _, r := range reports {
		crewIDs[r.CrewID] = true
	}
	ids := make([]string, 0, len(crewIDs))
	for id := range crewIDs {
		ids = append(ids, id)
	}
	names := map[string]string{}
	if len(ids) > 0 {
		if groups, err := nk.GroupsGetId(ctx, ids); err == nil {
			for _, g := range groups {
				names[g.GetId()] = g.GetName()
			}
		}
	}
	profiles := map[string]*CrewProfile{}
	emoji := map[string]*CrewEmojiDoc{}
	for _, id := range ids {
		profiles[id], _ = readCrewProfile(ctx, nk, id)
		emoji[id], _ = readCrewEmoji(ctx, nk, id)
	}
	for i := range reports {
		r := &reports[i]
		r.CrewName = names[r.CrewID]
		r.Live = mediaLive(profiles[r.CrewID], emoji[r.CrewID], r.MediaReport)
	}
	out, _ := json.Marshal(map[string]interface{}{"reports": reports})
	return string(out), nil
}

// AdminMediaReportActionRequest is the request of admin_media_takedown and
// admin_media_report_dismiss. Actor is the admin, for the record.
type AdminMediaReportActionRequest struct {
	ReportID string `json:"report_id"`
	Reason   string `json:"reason"`
	Actor    string `json:"actor"`
}

func parseMediaReportAction(payload string) (AdminMediaReportActionRequest, error) {
	var req AdminMediaReportActionRequest
	if err := json.Unmarshal([]byte(payload), &req); err != nil {
		return req, runtime.NewError("invalid request", 3)
	}
	req.Reason = strings.TrimSpace(req.Reason)
	req.Actor = strings.TrimSpace(req.Actor)
	if req.ReportID == "" || req.Reason == "" || req.Actor == "" {
		return req, runtime.NewError("report_id, reason and actor required", 3)
	}
	if len(req.Reason) > 500 {
		req.Reason = req.Reason[:500]
	}
	return req, nil
}

// closeMediaReports sets status on the report id and on every other open
// report of the same image. It returns how many it closed.
func closeMediaReports(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule, id string, target MediaReport, status string, req AdminMediaReportActionRequest) int {
	ids := []string{id}
	if all, err := listMediaReports(ctx, nk); err == nil {
		for _, r := range all {
			if r.ID != id && r.Status == "" && r.CrewID == target.CrewID && r.Kind == target.Kind && r.URL == target.URL {
				ids = append(ids, r.ID)
			}
		}
	}
	now := time.Now().UnixMilli()
	closed := 0
	for _, rid := range ids {
		r, version, err := readMediaReport(ctx, nk, rid)
		if err != nil || r == nil || r.Status != "" {
			continue
		}
		r.Status, r.Reason, r.Actor, r.DoneMs = status, req.Reason, req.Actor, now
		data, _ := json.Marshal(r)
		if _, err := nk.StorageWrite(ctx, []*runtime.StorageWrite{{
			Collection: MediaReportsCollection, Key: rid, UserID: SystemUserID,
			Value: string(data), Version: version, PermissionRead: 0, PermissionWrite: 0,
		}}); err != nil {
			logger.Warn("media report %s: close failed: %v", rid, err)
			continue
		}
		closed++
	}
	return closed
}

// AdminMediaTakedownRPC removes the reported image when it is still the
// crew's banner or emoji, deletes its object from the media bucket, and
// closes every open report of it. An image that is gone already only closes
// the reports.
func AdminMediaTakedownRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	if err := requireServerCaller(ctx, "admin_media_takedown"); err != nil {
		return "", err
	}
	req, err := parseMediaReportAction(payload)
	if err != nil {
		return "", err
	}
	r, _, err := readMediaReport(ctx, nk, req.ReportID)
	if err != nil {
		logger.Error("admin_media_takedown: %v", err)
		return "", runtime.NewError("report read failed", 13)
	}
	if r == nil {
		return "", runtime.NewError("no such report", 5)
	}

	key := ""
	switch r.Kind {
	case "banner":
		_, err = updateCrewProfile(ctx, nk, r.CrewID, func(p *CrewProfile) error {
			key = ""
			if p.Banner != nil && p.Banner.Kind == "upload" && p.Banner.URL == r.URL {
				key = p.Banner.Key
				p.Banner = nil
			}
			return nil
		})
	case "emoji":
		_, err = updateCrewEmoji(ctx, nk, r.CrewID, func(d *CrewEmojiDoc) error {
			key = ""
			for i, e := range d.Emoji {
				if e.Name == r.Name && e.URL == r.URL {
					key = e.Key
					d.Emoji = append(d.Emoji[:i], d.Emoji[i+1:]...)
					return nil
				}
			}
			return nil
		})
	default:
		return "", runtime.NewError("unknown report kind", 3)
	}
	if err != nil {
		logger.Error("admin_media_takedown: %v", err)
		return "", runtime.NewError("takedown failed", 13)
	}
	if key != "" {
		if err := mediaDelete(ctx, []string{key}); err != nil {
			logger.Warn("admin_media_takedown: delete %s: %v", key, err)
		}
		InvalidateCrewState(r.CrewID)
		pushCrewGiftsChanged(ctx, logger, nk, r.CrewID, r.Kind)
	}
	closed := closeMediaReports(ctx, logger, nk, req.ReportID, *r, "taken_down", req)
	logger.Info("admin_media_takedown: report=%s crew=%s kind=%s removed=%v closed=%d actor=%s",
		req.ReportID, r.CrewID, r.Kind, key != "", closed, req.Actor)
	out, _ := json.Marshal(map[string]interface{}{"removed": key != "", "closed": closed})
	return string(out), nil
}

// AdminMediaReportDismissRPC closes a report and every other open report of
// the same image, and leaves the image.
func AdminMediaReportDismissRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	if err := requireServerCaller(ctx, "admin_media_report_dismiss"); err != nil {
		return "", err
	}
	req, err := parseMediaReportAction(payload)
	if err != nil {
		return "", err
	}
	r, _, err := readMediaReport(ctx, nk, req.ReportID)
	if err != nil {
		logger.Error("admin_media_report_dismiss: %v", err)
		return "", runtime.NewError("report read failed", 13)
	}
	if r == nil {
		return "", runtime.NewError("no such report", 5)
	}
	closed := closeMediaReports(ctx, logger, nk, req.ReportID, *r, "dismissed", req)
	logger.Info("admin_media_report_dismiss: report=%s crew=%s closed=%d actor=%s", req.ReportID, r.CrewID, closed, req.Actor)
	out, _ := json.Marshal(map[string]interface{}{"closed": closed})
	return string(out), nil
}
