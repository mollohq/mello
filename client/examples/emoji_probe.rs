//! Renders a few emoji with the bundled font through the real renderer and
//! writes a PNG. A probe, not a test: the headless viewer draws no colour
//! fonts, so this is the only way to see the glyphs without the full app.
slint::slint! {
    export component Probe inherits Window {
        width: 420px; height: 160px; background: #17171A;
        VerticalLayout {
            padding: 12px; spacing: 8px;
            Text { text: "🔥🔥 😂 💀 👀 🎮 🇸🇪 👍🏽 1️⃣"; font-size: 32px; color: #F3F3F4; }
            Text { text: "picker 24px: 😀 😃 😄 😁 😆 😅 😂 🤣"; font-size: 24px; color: #F3F3F4; }
            Text { text: "body 13px: gg ez, one more? 🎮 that round was insane 😂"; font-size: 13px; color: #A2A2A8; }
        }
    }
}
use slint::ComponentHandle;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let fonts = std::env::var("PROBE_FONT_DIR").unwrap_or_else(|_| "client/ui/fonts".into());
    std::env::set_var("SLINT_FONT_PATH", &fonts);
    let out = std::env::var("PROBE_OUT").unwrap_or_else(|_| "/tmp/emoji_probe.png".into());
    let probe = Probe::new()?;
    probe.show()?;
    let weak = probe.as_weak();
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::SingleShot,
        std::time::Duration::from_millis(600),
        move || {
            let Some(p) = weak.upgrade() else { return };
            match p.window().take_snapshot() {
                Ok(buf) => {
                    let img = image::RgbaImage::from_raw(
                        buf.width(),
                        buf.height(),
                        buf.as_bytes().to_vec(),
                    )
                    .expect("buffer");
                    img.save(&out).expect("png");
                    eprintln!("wrote {out}");
                }
                Err(e) => eprintln!("snapshot failed: {e}"),
            }
            slint::quit_event_loop().ok();
        },
    );
    probe.run()?;
    Ok(())
}
