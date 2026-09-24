//! Every control in the UI must be addressable by a role and a label.
//!
//! The e2e driver and the QA agents find controls by the text a user reads
//! ("Join crew", "Invite"), through Slint's accessibility properties. A
//! `TouchArea` without them can only be found by element ID or position, which
//! breaks on refactors (plans/E2E-QA.md §4).
//!
//! The rule, checked statically over every `.slint` file so it also covers
//! screens no test renders: a `TouchArea` carries `accessible-role` and
//! `accessible-label` itself, or its direct parent element does. A `TouchArea`
//! that is not a control — a backdrop or an input blocker — says so with a
//! `// a11y: none (<reason>)` comment inside its block.
//!
//! Text fields follow the same idea: a `TextInput` or `MelloTextInput` instance
//! declares `accessible-label`, and a `MelloInputField` instance sets `label`.
//! The label is the field's visible caption, or its placeholder when it has none.

use std::path::{Path, PathBuf};

/// A `TouchArea` that breaks the rule.
#[derive(Debug, PartialEq)]
struct Violation {
    file: String,
    line: usize,
}

/// One `{ … }` element block found while scanning.
struct Block {
    /// Byte offset of the opening `{`.
    open: usize,
    /// Byte offset one past the matching `}`.
    close: usize,
    /// Index of the enclosing block, if any.
    parent: Option<usize>,
    /// What the rule needs to know about the element that opens this block.
    kind: Kind,
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Other,
    /// A `TouchArea` instance, or a component inheriting `TouchArea`.
    TouchArea,
    /// A `TextInput` or `MelloTextInput` instance (not a component definition).
    TextInput,
    /// A `MelloInputField` instance.
    InputField,
}

/// Replace the contents of string literals and comments with spaces, keeping
/// offsets and newlines, so braces inside them do not count. `// a11y: none`
/// markers are kept verbatim so the exemption survives.
fn mask(src: &str) -> String {
    let b = src.as_bytes();
    let mut out = src.as_bytes().to_vec();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'"' {
            i += 1;
            while i < b.len() && b[i] != b'"' {
                if b[i] == b'\\' {
                    out[i] = b' ';
                    i += 1;
                }
                if i < b.len() && b[i] != b'\n' {
                    out[i] = b' ';
                }
                i += 1;
            }
            i += 1;
        } else if b[i] == b'/' && b.get(i + 1) == Some(&b'/') {
            let end = src[i..].find('\n').map_or(b.len(), |n| i + n);
            if !src[i..end].contains("a11y: none") {
                out[i..end].fill(b' ');
            }
            i = end;
        } else if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
            let end = src[i + 2..].find("*/").map_or(b.len(), |n| i + 2 + n + 2);
            for c in &mut out[i..end] {
                if *c != b'\n' {
                    *c = b' ';
                }
            }
            i = end;
        } else {
            i += 1;
        }
    }
    String::from_utf8(out).expect("masking keeps UTF-8 boundaries")
}

/// The identifier just before `{`, skipping whitespace: `TouchArea` in
/// `t := TouchArea {`, `Rectangle` in `if x : Rectangle {`.
fn element_before(masked: &str, open: usize) -> &str {
    let head = masked[..open].trim_end();
    let start = head
        .rfind(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_'))
        .map_or(0, |i| i + 1);
    &head[start..]
}

fn blocks(masked: &str) -> Vec<Block> {
    let mut out: Vec<Block> = Vec::new();
    let mut stack: Vec<usize> = Vec::new();
    for (i, c) in masked.char_indices() {
        match c {
            '{' => {
                let element = element_before(masked, i);
                let definition = masked[..i]
                    .trim_end()
                    .ends_with(&format!("inherits {element}"));
                // A component inheriting `TouchArea` is itself a touch area; a
                // component inheriting `TextInput` is a definition, and its
                // instances are checked where they are used.
                let kind = match element {
                    "TouchArea" => Kind::TouchArea,
                    "TextInput" | "MelloTextInput" if !definition => Kind::TextInput,
                    "MelloInputField" if !definition => Kind::InputField,
                    _ => Kind::Other,
                };
                out.push(Block {
                    open: i,
                    close: masked.len(),
                    parent: stack.last().copied(),
                    kind,
                });
                stack.push(out.len() - 1);
            }
            '}' => {
                if let Some(idx) = stack.pop() {
                    out[idx].close = i + 1;
                }
            }
            _ => {}
        }
    }
    out
}

/// The text of a block at its own depth: nested `{ … }` removed.
fn own_text(masked: &str, all: &[Block], idx: usize) -> String {
    let b = &all[idx];
    let mut text = String::new();
    let mut pos = b.open + 1;
    let mut children: Vec<&Block> = all.iter().filter(|c| c.parent == Some(idx)).collect();
    children.sort_by_key(|c| c.open);
    for c in children {
        text.push_str(&masked[pos..c.open]);
        pos = c.close;
    }
    text.push_str(&masked[pos..b.close.saturating_sub(1).max(pos)]);
    text
}

fn declares(text: &str, property: &str) -> bool {
    text.lines().any(|l| {
        let l = l.trim_start();
        l.starts_with(property) && l[property.len()..].trim_start().starts_with(':')
    })
}

fn labelled(text: &str) -> bool {
    declares(text, "accessible-role") && declares(text, "accessible-label")
}

fn check_source(file: &str, src: &str) -> Vec<Violation> {
    let masked = mask(src);
    let all = blocks(&masked);
    let mut out = Vec::new();
    for (idx, b) in all.iter().enumerate() {
        let own = || own_text(&masked, &all, idx);
        let ok = match b.kind {
            Kind::Other => true,
            Kind::TextInput => declares(&own(), "accessible-label"),
            Kind::InputField => declares(&own(), "label"),
            Kind::TouchArea => {
                let own = own();
                own.contains("a11y: none")
                    || labelled(&own)
                    || b.parent
                        .is_some_and(|p| labelled(&own_text(&masked, &all, p)))
            }
        };
        if ok {
            continue;
        }
        out.push(Violation {
            file: file.to_string(),
            line: masked[..b.open].lines().count(),
        });
    }
    out
}

fn slint_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            slint_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "slint") {
            out.push(p);
        }
    }
}

#[test]
fn every_touch_area_has_a_role_and_a_label() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("ui");
    let mut files = Vec::new();
    slint_files(&root, &mut files);
    files.sort();
    assert!(
        !files.is_empty(),
        "no .slint files under {}",
        root.display()
    );

    let mut violations = Vec::new();
    for f in &files {
        let src = std::fs::read_to_string(f).expect("read .slint file");
        let rel = f.strip_prefix(&root).unwrap_or(f).display().to_string();
        violations.extend(check_source(&rel, &src));
    }

    assert!(
        violations.is_empty(),
        "{} control(s) have no label. A TouchArea needs accessible-role + \
         accessible-label on itself or its parent (or `// a11y: none (<reason>)` \
         when it is not a control). A TextInput needs accessible-label; a \
         MelloInputField needs label. The label is the text the user reads. \
         See plans/E2E-QA.md §4.\n{}",
        violations.len(),
        violations
            .iter()
            .map(|v| format!("  client/ui/{}:{}", v.file, v.line))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// The static rule above cannot see a component whose `label` property an
/// instance never sets: the declaration passes, and the control renders with
/// an empty label. This renders the real screens through the headless harness
/// and checks every control that is on screen.
mod runtime {
    use i_slint_backend_testing::{AccessibleRole, ElementQuery};
    use mello_core::Event;

    use crate::testkit::{Harness, MainWindow};

    const CONTROL_ROLES: [AccessibleRole; 6] = [
        AccessibleRole::Button,
        AccessibleRole::TextInput,
        AccessibleRole::Switch,
        AccessibleRole::Tab,
        AccessibleRole::Slider,
        AccessibleRole::Combobox,
    ];

    fn crews(n: usize) -> Vec<mello_core::crew::Crew> {
        (0..n)
            .map(|i| mello_core::crew::Crew {
                id: format!("crew-{i}"),
                name: format!("Crew {i}"),
                description: String::new(),
                member_count: 3,
                max_members: 10,
                open: true,
                avatar_url: None,
            })
            .collect()
    }

    /// Controls on screen: (role, label) for each element with a control role.
    fn controls(app: &MainWindow) -> Vec<(AccessibleRole, String)> {
        ElementQuery::from_root(app)
            .match_descendants()
            .find_all()
            .into_iter()
            .filter_map(|e| {
                let role = e.accessible_role()?;
                CONTROL_ROLES.contains(&role).then(|| {
                    let label = e.accessible_label().unwrap_or_default().to_string();
                    (role, label)
                })
            })
            .collect()
    }

    fn check(state: &str, h: &mut Harness, failures: &mut Vec<String>) {
        h.pump();
        let found = controls(h.app());
        if found.is_empty() {
            failures.push(format!(
                "{state}: no controls rendered (the state is not set up)"
            ));
        }
        for (role, label) in found {
            if label.trim().is_empty() {
                failures.push(format!("{state}: a {role:?} has an empty accessible-label"));
            }
        }
    }

    fn logged_in() -> Harness {
        let mut h = Harness::new();
        h.emit(Event::LoggedIn {
            user: mello_core::events::User {
                id: "u1".into(),
                username: "alice".into(),
                display_name: "Alice".into(),
                tag: String::new(),
                created_at: None,
            },
        });
        h.emit(Event::CrewsLoaded { crews: crews(2) });
        h
    }

    #[test]
    fn every_control_on_screen_has_a_label() {
        let mut failures = Vec::new();

        let mut h = Harness::new();
        h.emit(Event::DiscoverCrewsLoaded {
            crews: crews(3),
            cursor: None,
        });
        check("onboarding step 1", &mut h, &mut failures);
        h.app().set_onboarding_step(2);
        check("onboarding step 2", &mut h, &mut failures);
        h.app().set_onboarding_step(3);
        check("onboarding step 3", &mut h, &mut failures);
        h.app().set_onboarding_step(1);
        h.app().set_show_sign_in(true);
        check("sign-in panel", &mut h, &mut failures);

        let mut h = logged_in();
        check("app", &mut h, &mut failures);

        type Open = fn(&MainWindow, bool);
        let modals: [(&str, Open); 10] = [
            ("settings", MainWindow::set_settings_open),
            ("crew settings", MainWindow::set_crew_settings_open),
            ("new crew", MainWindow::set_new_crew_open),
            ("join crew", MainWindow::set_join_crew_modal_open),
            ("invite share", MainWindow::set_invite_share_open),
            ("stats profile", MainWindow::set_stats_profile_open),
            ("stream source picker", MainWindow::set_source_picker_open),
            ("stream source menu", MainWindow::set_source_menu_open),
            ("riot link", MainWindow::set_riot_dialog_open),
            ("discover", MainWindow::set_show_discover),
        ];
        for (name, open) in modals {
            let mut h = logged_in();
            open(h.app(), true);
            check(name, &mut h, &mut failures);
        }

        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(src: &str) -> Vec<usize> {
        check_source("t.slint", src)
            .into_iter()
            .map(|v| v.line)
            .collect()
    }

    #[test]
    fn a_bare_touch_area_is_reported() {
        let src =
            "component A inherits Rectangle {\n    TouchArea {\n        clicked => { }\n    }\n}\n";
        assert_eq!(lines(src), vec![2]);
    }

    #[test]
    fn labels_on_the_touch_area_pass() {
        let src = "component A inherits Rectangle {\n    t := TouchArea {\n        accessible-role: button;\n        accessible-label: \"Join crew\";\n        clicked => { }\n    }\n}\n";
        assert!(lines(src).is_empty());
    }

    #[test]
    fn labels_on_the_direct_parent_pass() {
        let src = "Rectangle {\n    accessible-role: button;\n    accessible-label: root.name;\n    TouchArea { }\n}\n";
        assert!(lines(src).is_empty());
    }

    #[test]
    fn labels_on_a_grandparent_do_not_count() {
        let src = "Rectangle {\n    accessible-role: button;\n    accessible-label: \"x\";\n    HorizontalLayout {\n        TouchArea { }\n    }\n}\n";
        assert_eq!(lines(src), vec![5]);
    }

    #[test]
    fn a_role_without_a_label_is_reported() {
        let src = "TouchArea {\n    accessible-role: button;\n}\n";
        assert_eq!(lines(src), vec![1]);
    }

    #[test]
    fn a_labelled_child_does_not_label_the_touch_area() {
        let src = "TouchArea {\n    Rectangle {\n        accessible-role: button;\n        accessible-label: \"x\";\n    }\n}\n";
        assert_eq!(lines(src), vec![1]);
    }

    #[test]
    fn the_exemption_marker_passes() {
        let src = "bg := TouchArea {\n    // a11y: none (backdrop, closes the modal)\n    clicked => { }\n}\n";
        assert!(lines(src).is_empty());
    }

    #[test]
    fn braces_and_words_in_strings_and_comments_do_not_count() {
        let src = "Rectangle {\n    // TouchArea { accessible-role: button; }\n    Text { text: \"TouchArea { }\"; }\n}\n";
        assert!(lines(src).is_empty());
    }

    #[test]
    fn a_text_input_needs_an_accessible_label() {
        let src = "Rectangle {\n    a := MelloTextInput {\n        text: \"x\";\n    }\n    b := TextInput {\n        accessible-label: \"Email\";\n    }\n}\n";
        assert_eq!(lines(src), vec![2]);
    }

    #[test]
    fn an_input_field_needs_a_label() {
        let src = "Rectangle {\n    MelloInputField { placeholder: \"x\"; }\n    MelloInputField {\n        label: \"RIOT ID\";\n    }\n}\n";
        assert_eq!(lines(src), vec![2]);
    }

    #[test]
    fn a_text_input_definition_is_not_an_instance() {
        let src = "export component MelloTextInput inherits TextInput {\n    selection-foreground-color: red;\n}\n";
        assert!(lines(src).is_empty());
    }

    #[test]
    fn a_component_inheriting_touch_area_is_checked() {
        let src = "component Btn inherits TouchArea {\n    clicked => { }\n}\n";
        assert_eq!(lines(src), vec![1]);
    }
}
