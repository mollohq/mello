# i-slint-backend-winit 1.18.1 with slint#13568

This is a temporary copy. Remove it when a Slint release has the fix
(expected 1.18.2). Tracking issue: https://github.com/mollohq/mello/issues/159

## Why

On Windows, Slint 1.18.1 repaints only the changed parts of the window after a
minimize and restore. The rest of the window shows as transparent.

- Bug report: https://github.com/slint-ui/slint/issues/13537
- Fix: https://github.com/slint-ui/slint/pull/13568 (Slint master `08a297b`)

## What is here

- The `i-slint-backend-winit` 1.18.1 crate from crates.io (upstream commit
  `372cf0e`, see `.cargo_vcs_info.json`), without `Cargo.lock` and
  `Cargo.toml.orig`.
- The only change: `08a297b` applied to `winitwindowadapter.rs`
  (`mark_windows_update_region_dirty`, 33 lines).

The root `Cargo.toml` puts this copy in place with `[patch.crates-io]` and
excludes it from the workspace.
