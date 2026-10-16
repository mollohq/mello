# Discover Curation

> **Component:** Crew discovery (Backend)
> **Status:** Implemented
> **Related:** [04-BACKEND.md](../04-BACKEND.md) §4.3 (the contract), §6 (storage)

---

## 1. Overview

The admin chooses which crews Discover features and which crews it hides.

- Featured crews come first, in the order that the admin sets.
- Hidden crews never show in Discover.
- All other crews are "listed": they show in the normal order.

The `discover_crews` response shape does not change. The client needs no change.

## 2. Problem

Before this feature:

- `discover_crews` returned every open crew in the order that Nakama gave.
- The client puts the first crew in the large "featured" slot. Position was the only rule.
  See `client/src/handlers/crew.rs` (`ONBOARDING_CREW_CARDS`) and the bento sets in `discover_panel.slint`.
- The admin could not hide a crew from Discover. The admin could not pick the featured crew.
- The only control was the open or invite-only flag. That flag belongs to the crew owner.

## 3. Decisions

| # | Decision | Reason |
|---|---|---|
| 1 | Curation is one system storage object: collection `discover`, key `curation`, owner `SystemUserID`, read 0, write 0. | One read per `discover_crews` call. A version-conditional write, the same as the capture policy. Crew owners cannot change it. |
| 2 | The value is `{featured: [crew_id, ...], hidden: [crew_id, ...], updated_by, updated_at}`. The `featured` list is ordered. | The first featured crew gets the large slot. The admin sets the order. |
| 3 | Each crew has one of three states: **featured**, **listed** (on no list, the default), **hidden**. | |
| 4 | Group metadata is not used. | An ordered list in metadata needs a scan of all groups. `update_crew` also writes metadata. |
| 5 | A featured crew shows first, on the first page only, in list order. | |
| 6 | A featured crew does not show if it is invite-only, deleted, or the caller is a member. | Discover shows open crews only. Featured does not override the owner. |
| 7 | A hidden crew never shows in `discover_crews`. Invite codes and invite links still work. | Hidden is not a ban. |
| 8 | A crew on both lists is refused. Featured holds at most 8 crews. | One state per crew. Page 1 holds at most 58 crews. |
| 9 | A featured id must be an existing crew when the admin saves. A hidden id is not checked. | This catches a typo in the featured list. A hidden id for a deleted crew does no harm. |
| 10 | If the curation read fails, `discover_crews` fails (code 13). | A hidden crew must never show. Onboarding already shows an error, a Retry and the Create Crew card. |
| 11 | The `discover_crews` response shape does not change. | Old clients get the curated order. |

## 4. Backend

`backend/nakama/data/modules/discover_curation.go`:

- `readDiscoverCuration(ctx, nk)`: returns the lists and the storage version. Returns empty lists when nothing is stored.
  A stored value that does not parse is an error.
- `normalizeDiscoverCurationWrite(w)`: checks UUIDs, removes duplicates, keeps the featured order, applies decisions 8 and 9.
- `admin_discover_curation_get`: server-to-server only (`requireServerCaller`).
  Returns the lists, the version, and a summary for each id: `name`, `open`, `member_count`, `exists`.
- `admin_discover_curation_set`: server-to-server only. Body `{featured, hidden, version, actor}`.
  The write is conditional on `version`. A conflict returns code 10 (ABORTED).
  Returns the new lists, the new version and a diff (`featured_added`, `featured_removed`, `hidden_added`, `hidden_removed`, `order_changed`).

`DiscoverCrewsRPC` in `crews.go`:

1. Read the curation.
2. First page (no cursor): get the featured crews with `nk.GroupsGetId`. Keep the order. Drop the crews in decision 6.
3. Every page: call `GroupsList` as before. Skip joined crews, hidden crews and featured crews. Page 1 already has the featured crews.
4. Return `{crews, cursor}` as before.

Both admin RPCs are registered in `main.go`, in the admin block. The full contract and the error table are in [04-BACKEND.md](../04-BACKEND.md) §4.3.

## 5. Admin Tool

The admin tool calls the two admin RPCs with the http key. It sets `actor` to
the signed-in admin and keeps each `diff` in its audit log. The admin tool is
not in this repository.

## 6. Tests

Go unit tests in `discover_curation_test.go`:

- Normalize: duplicates, a crew on both lists, more than 8 featured, a bad UUID, a missing list, a missing actor.
- Set: a client session is refused. An unknown featured crew is refused. A stale version returns code 10.
  A write that lands between the read and the write returns code 10. The diff is correct.
- Get: a deleted crew stays on its list with `exists: false`.
- Discover, with a fake `GroupsList` and `UserGroupsList`:
  - No curation stored: the same result as before.
  - A hidden crew never shows, on any page.
  - Featured crews come first on page 1, in list order, and do not show again on page 2.
  - A featured crew that is invite-only, deleted or joined does not show.
  - A failed or unparseable curation read fails the RPC.

Each discover test fails on the code before this feature, or under a mutation of the rule it covers.

Local run against local Nakama: feature two crews, hide one, walk every page of
`discover_crews` with the http key, and check the order. A featured crew that
goes invite-only drops out of Discover.

## 7. Out of Scope

- Client changes. A `featured` flag in the response can come later if the UI needs it.
- Automatic ranking (activity, member count).
- Hiding a crew from user search or from invite codes.
