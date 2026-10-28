//! The About page: the Third-party notices asset.
//!
//! `build.rs` writes the asset (see `build_notices.rs` for the format). It is
//! embedded as one string and parsed on the first open of the notices view.

use mello_core::testers::{Lookup, TesterAvatars, TESTERS};
use tokio::sync::mpsc::UnboundedSender;

use crate::app_context::AppContext;
use crate::{MainWindow, NoticeEntryData, TesterData};

static NOTICES: &str = include_str!(concat!(env!("OUT_DIR"), "/third_party_notices.txt"));

const RECORD: char = '\u{1e}';
const FIELD: char = '\u{1f}';

/// One parsed record.
#[derive(Debug, PartialEq, Eq)]
pub struct Notice<'a> {
    pub names: &'a str,
    pub license: &'a str,
    pub text: &'a str,
}

/// Parse an asset. A record with a missing field is skipped.
pub fn parse(asset: &str) -> Vec<Notice<'_>> {
    asset
        .split(RECORD)
        .filter_map(|record| {
            let mut fields = record.splitn(3, FIELD);
            Some(Notice {
                names: fields.next()?,
                license: fields.next()?,
                text: fields.next()?,
            })
        })
        .collect()
}

/// Show the version, the commit and the build day on the About tab and in the
/// settings sidebar.
pub fn show_build(app: &MainWindow) {
    app.set_settings_build_version(format!("v{}", env!("CARGO_PKG_VERSION")).into());
    app.set_settings_build_commit(crate::build_info::commit_label().into());
    app.set_settings_build_date(crate::build_info::built_date().into());
}

/// The embedded notices, for the Slint model.
pub fn entries() -> Vec<NoticeEntryData> {
    parse(NOTICES)
        .into_iter()
        .map(|n| NoticeEntryData {
            names: n.names.into(),
            license: n.license.into(),
            text: n.text.into(),
        })
        .collect()
}

/// A tester's avatar from disk. A file that does not decode is removed, so
/// the next open asks again.
fn decode_avatar(path: &std::path::Path) -> Option<slint::Image> {
    match image::open(path) {
        Ok(img) => {
            let rgba = img.to_rgba8();
            let (w, h) = rgba.dimensions();
            Some(crate::avatar::rgba_to_image(rgba.as_raw(), w, h))
        }
        Err(_) => {
            let _ = std::fs::remove_file(path);
            None
        }
    }
}

/// The seven tiles. A tile with no avatar on disk shows the monogram.
pub fn tester_rows(store: &TesterAvatars) -> Vec<TesterData> {
    TESTERS
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let image = match store.lookup(t.slug) {
                Lookup::Fresh(p) | Lookup::Stale(p) => decode_avatar(&p),
                Lookup::Negative | Lookup::Unknown => None,
            };
            TesterData {
                name: t.name.into(),
                initials: t
                    .name
                    .chars()
                    .next()
                    .map_or(String::new(), |c| c.to_uppercase().collect())
                    .into(),
                url: t.profile_url().into(),
                color_index: (i % 5) as i32,
                has_avatar: image.is_some(),
                avatar: image.unwrap_or_default(),
            }
        })
        .collect()
}

/// The ABOUT tab was opened: show the testers from the cache and ask for the
/// avatars that are missing or older than a week. Nothing is asked before
/// this, and a fresh cache asks nothing.
pub fn on_opened(
    app: &MainWindow,
    store: &TesterAvatars,
    cmd: &UnboundedSender<mello_core::Command>,
) {
    app.set_settings_testers(slint::ModelRc::new(slint::VecModel::from(tester_rows(
        store,
    ))));
    let want: Vec<String> = TESTERS
        .iter()
        .filter(|t| matches!(store.lookup(t.slug), Lookup::Unknown | Lookup::Stale(_)))
        .map(|t| t.slug.to_string())
        .collect();
    if !want.is_empty() {
        log::info!("[about] asking for {} tester avatars", want.len());
        let _ = cmd.send(mello_core::Command::FetchTesterAvatars { slugs: want });
    }
}

/// An avatar reached the disk: build the tiles again.
pub fn on_avatar_loaded(ctx: &AppContext) {
    ctx.app
        .set_settings_testers(slint::ModelRc::new(slint::VecModel::from(tester_rows(
            &ctx.testers,
        ))));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_has_names_license_and_text() {
        let asset = "a 1.0\nb 2.0\u{1f}MIT\u{1f}Copyright (c) A\u{1e}c 3.0\u{1f}OFL-1.1\u{1f}text";
        let parsed = parse(asset);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].names, "a 1.0\nb 2.0");
        assert_eq!(parsed[0].license, "MIT");
        assert_eq!(parsed[1].text, "text");
    }

    #[test]
    fn a_record_with_a_missing_field_is_skipped() {
        assert!(parse("only a name").is_empty());
    }

    #[test]
    fn the_asset_stays_inside_the_install_budget() {
        assert!(NOTICES.len() < 1_000_000, "{} bytes", NOTICES.len());
    }
}
