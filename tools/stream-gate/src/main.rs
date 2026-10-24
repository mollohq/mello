//! stream-gate: run the streaming quality gate. Use scripts/stream-gate.sh.
//!
//! ```text
//! stream-gate [--lane cpu|hw] [--only clean,mobile] [--out DIR] [--baseline FILE]
//!             [--report-only] [--write-baseline] [--no-vmaf] [--keep] [--repeat N]
//!             [--clip FILE --clip-size WxH [--clip-fps N]] [--gen-clips]
//!             [--write-fixture FILE]
//! ```
//!
//! `--lane cpu` (default): the 720p60 clip, OpenH264 encode and decode in
//! software (`MELLO_SOFT_ENCODER=1`, `MELLO_SOFT_DECODER=1`). `--lane hw`:
//! the 1080p60 clip, the hardware encoder and decoder of the machine.
//!
//! `--baseline FILE` compares with FILE instead of
//! `benchmarks/baselines/stream/baseline-2.1-<lane>.json`. With
//! `--write-baseline` the run writes it (refused when it exists).
//!
//! `--repeat N` runs every profile N times and prints the spread of each
//! metric: the input of `tools/stream-gate/make_gates.py`.
//!
//! A profile that fails a gate runs twice more, and the gates then read the
//! median of its three runs (`--no-confirm` turns this off). One run of a
//! real-time path can land in its tail; the median of three is the
//! estimate. A profile that passes only on a lucky run does not pass.
//!
//! `--clip FILE` plays a real YUV 4:2:0 (I420) file instead of the generated
//! clip. Its frames carry no burned-in code, so only the bandwidth and
//! transport metrics are valid; the run never compares with a baseline.
//!
//! `--write-fixture FILE` encodes the first 2 s of the 720p clip once with
//! the software encoder (no network) and writes the Annex-B stream: the
//! H.264 fixture of mello-sfu tools/stream-soak.
//!
//! Exit codes: 0 = every enforced gate passed, 1 = an enforced gate failed,
//! 2 = setup or run error.

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::Arc;
use std::time::{Duration, Instant};
use stream_gate::metrics::{self, Present, SourceClock};
use stream_gate::profile::{self, GateFile, Lane, Profile};
use stream_gate::report::{self, Metrics};
use stream_gate::session::{self, clock_us, Ctx, Host, ViewerRecord, ViewerSpec};
use stream_gate::shim::{Plan, Shim};
use stream_gate::{audio, clip, score, sfu};

/// Stage 7: the clip's game audio and how to score it.
struct AudioRun {
    spec: audio::AudioSpec,
    reference: audio::ReferenceAudio,
    /// The raw track the host plays (mello_test_set_reference_audio).
    track: PathBuf,
    /// The PESQ scorer (scripts/voice-gate-score.py).
    script: PathBuf,
}

struct Args {
    lane: String,
    only: Option<Vec<String>>,
    out: Option<PathBuf>,
    baseline: Option<PathBuf>,
    report_only: bool,
    write_baseline: bool,
    no_vmaf: bool,
    keep: bool,
    no_confirm: bool,
    repeat: u32,
    clip: Option<PathBuf>,
    clip_size: Option<(u32, u32)>,
    clip_fps: u32,
    gen_clips: bool,
    write_fixture: Option<PathBuf>,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        lane: "cpu".into(),
        only: None,
        out: None,
        baseline: None,
        report_only: false,
        write_baseline: false,
        no_vmaf: false,
        keep: false,
        no_confirm: false,
        repeat: 1,
        clip: None,
        clip_size: None,
        clip_fps: 60,
        gen_clips: false,
        write_fixture: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut val = |name: &str| it.next().ok_or(format!("{name} needs a value"));
        match arg.as_str() {
            "--lane" => a.lane = val("--lane")?,
            "--only" => a.only = Some(val("--only")?.split(',').map(str::to_owned).collect()),
            "--out" => a.out = Some(PathBuf::from(val("--out")?)),
            "--baseline" => a.baseline = Some(PathBuf::from(val("--baseline")?)),
            "--report-only" => a.report_only = true,
            "--write-baseline" => a.write_baseline = true,
            "--no-vmaf" => a.no_vmaf = true,
            "--keep" => a.keep = true,
            "--no-confirm" => a.no_confirm = true,
            "--repeat" => {
                a.repeat = val("--repeat")?
                    .parse()
                    .map_err(|e| format!("--repeat: {e}"))?
            }
            "--clip" => a.clip = Some(PathBuf::from(val("--clip")?)),
            "--clip-size" => {
                let v = val("--clip-size")?;
                let (w, h) = v.split_once('x').ok_or("--clip-size WxH")?;
                a.clip_size = Some((
                    w.parse().map_err(|e| format!("--clip-size: {e}"))?,
                    h.parse().map_err(|e| format!("--clip-size: {e}"))?,
                ));
            }
            "--clip-fps" => {
                a.clip_fps = val("--clip-fps")?
                    .parse()
                    .map_err(|e| format!("--clip-fps: {e}"))?
            }
            "--gen-clips" => a.gen_clips = true,
            "--write-fixture" => a.write_fixture = Some(PathBuf::from(val("--write-fixture")?)),
            "-h" | "--help" => {
                println!(
                    "stream-gate [--lane cpu|hw] [--only a,b] [--out DIR] [--baseline FILE] [--report-only] \
                     [--write-baseline] [--no-vmaf] [--keep] [--repeat N] [--clip FILE --clip-size WxH] \
                     [--gen-clips] [--write-fixture FILE]"
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument '{other}'")),
        }
    }
    if a.repeat == 0 {
        return Err("--repeat must be at least 1".into());
    }
    Ok(a)
}

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn repo_root() -> Result<PathBuf, String> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let root = root
        .canonicalize()
        .map_err(|e| format!("{}: {e}", root.display()))?;
    Ok(PathBuf::from(
        root.to_string_lossy()
            .trim_start_matches(r"\\?\")
            .to_owned(),
    ))
}

/// The clip file of a lane: rendered on first use, hash checked every run.
fn ensure_clip(root: &Path, entry: &profile::ClipEntry) -> Result<(PathBuf, String), String> {
    let path = root.join("benchmarks/clips").join(entry.spec.file_name());
    let hash = if path.exists() {
        eprint!("stream-gate: hashing {} ... ", path.display());
        let h = clip::hash_file(&path)?;
        eprintln!("{h}");
        h
    } else {
        eprint!("stream-gate: rendering {} ... ", path.display());
        let t = Instant::now();
        let h = clip::write_clip(&entry.spec, &path)?;
        eprintln!("{:.1} s, {h}", t.elapsed().as_secs_f64());
        h
    };
    if !entry.fnv1a.is_empty() && entry.fnv1a != hash {
        return Err(format!(
            "{} has hash {hash}, profiles.json freezes {}. Delete the file to render it again; \
             if it renders to the same wrong hash, the generator changed.",
            path.display(),
            entry.fnv1a
        ));
    }
    Ok((path, hash))
}

fn host_info() -> Value {
    let ps = |q: &str| {
        Command::new("powershell")
            .args(["-NoProfile", "-Command", q])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
            .unwrap_or_default()
    };
    if cfg!(windows) {
        json!({
            "cpu": ps("(Get-CimInstance Win32_Processor).Name"),
            "gpus": ps("Get-CimInstance Win32_VideoController | ForEach-Object { $_.Name + ' (driver ' + $_.DriverVersion + ')' }")
                .lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>(),
            "os": ps("(Get-CimInstance Win32_OperatingSystem).Caption + ' ' + (Get-CimInstance Win32_OperatingSystem).Version"),
            "computer": std::env::var("COMPUTERNAME").unwrap_or_default(),
            "memory_gb": ps("[math]::Round((Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory/1GB)"),
        })
    } else {
        json!({ "os": std::env::consts::OS })
    }
}

fn names(ctx: &Ctx, encoders: bool) -> Vec<String> {
    let mut out = Vec::new();
    // SAFETY: arrays sized to max_count.
    unsafe {
        if encoders {
            let mut v = [0 as mello_sys::MelloEncoderBackend; 8];
            let n = mello_sys::mello_get_encoders(ctx.0, v.as_mut_ptr(), 8);
            for b in v.iter().take(n.max(0) as usize) {
                out.push(
                    match *b {
                        mello_sys::MelloEncoderBackend_MELLO_ENCODER_NVENC => "NVENC",
                        mello_sys::MelloEncoderBackend_MELLO_ENCODER_AMF => "AMF",
                        mello_sys::MelloEncoderBackend_MELLO_ENCODER_QSV => "QSV",
                        _ => "other",
                    }
                    .to_owned(),
                );
            }
        } else {
            let mut v = [0 as mello_sys::MelloDecoderBackend; 8];
            let n = mello_sys::mello_get_decoders(ctx.0, v.as_mut_ptr(), 8);
            for b in v.iter().take(n.max(0) as usize) {
                out.push(
                    match *b {
                        mello_sys::MelloDecoderBackend_MELLO_DECODER_NVDEC => "NVDEC",
                        mello_sys::MelloDecoderBackend_MELLO_DECODER_AMF => "AMF",
                        mello_sys::MelloDecoderBackend_MELLO_DECODER_D3D11VA => "D3D11VA",
                        mello_sys::MelloDecoderBackend_MELLO_DECODER_OPENH264 => "OpenH264",
                        _ => "other",
                    }
                    .to_owned(),
                );
            }
        }
    }
    out
}

fn cstr(b: &[std::os::raw::c_char]) -> String {
    let bytes: Vec<u8> = b
        .iter()
        .take_while(|c| **c != 0)
        .map(|c| *c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The legs of a profile for a lane: a cap is a fraction of the preset.
fn plan_of(p: &Profile, lane: &Lane) -> Plan {
    let mut host = p.host.clone();
    let mut viewer = p.viewer.clone();
    for leg in [&mut host, &mut viewer] {
        if let Some(c) = &mut leg.cap {
            c.rate_kbps = c.rate_fraction * f64::from(lane.bitrate_kbps);
        }
    }
    Plan {
        seed: p.seed,
        duration_s: p.duration_s,
        host,
        viewer,
    }
}

/// Everything one profile run produced.
struct Run {
    metrics: Metrics,
    info: Value,
    vmaf_job: Option<score::VmafJob>,
}

fn presents_of(v: &ViewerRecord) -> Vec<Present> {
    v.presents
        .iter()
        .map(|p| Present {
            t_us: p.t_us,
            code: p.code,
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn run_profile(
    rt: &Arc<tokio::runtime::Runtime>,
    ctxs: &[Arc<Ctx>; 3],
    sfu: &sfu::Sfu,
    shim: &Shim,
    gf: &GateFile,
    lane: &Lane,
    p: &Profile,
    clip_path: &Path,
    clip_spec: &clip::ClipSpec,
    run_name: &str,
    out_dir: &Path,
    no_vmaf: bool,
    keep: bool,
    audio_run: Option<&AudioRun>,
) -> Result<Run, String> {
    let load_start = stream_gate::load::sample(&[sfu.raw_handle()]);
    let pdir = out_dir.join(run_name);
    let _ = std::fs::remove_dir_all(&pdir);
    std::fs::create_dir_all(&pdir).map_err(|e| e.to_string())?;
    let plan = plan_of(p, lane);
    shim.begin(plan.clone());
    let session_id = format!(
        "gate-{}-{}-{}",
        lane.name,
        run_name,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis())
    );
    let path_c = std::ffi::CString::new(clip_path.to_string_lossy().as_bytes())
        .map_err(|e| e.to_string())?;
    // SAFETY: the host context is live; the path is copied.
    let r = unsafe {
        mello_sys::mello_test_set_reference_clip(
            ctxs[0].0,
            path_c.as_ptr(),
            clip_spec.width,
            clip_spec.height,
            clip_spec.fps,
            mello_sys::MelloReferencePixelFormat_MELLO_REFERENCE_I420,
        )
    };
    if r != mello_sys::MelloResult_MELLO_OK {
        return Err(format!("mello_test_set_reference_clip: {r}"));
    }
    // Stage 7: the clip's game audio (or none).
    let track_c = match audio_run {
        Some(a) => Some(
            std::ffi::CString::new(a.track.to_string_lossy().as_bytes())
                .map_err(|e| e.to_string())?,
        ),
        None => None,
    };
    // SAFETY: the host context is live; the path is copied.
    unsafe {
        mello_sys::mello_test_set_reference_audio(
            ctxs[0].0,
            track_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
        )
    };

    let host = Host::start(
        rt,
        &ctxs[0],
        &sfu.ws,
        &sfu.password,
        &session_id,
        lane.bitrate_kbps,
        lane.fps,
        audio_run.is_some(),
        || shim.expect("host"),
    )?;
    let (w, h) = (host.width, host.height);
    if (w, h) != (clip_spec.width, clip_spec.height) {
        return Err(format!(
            "host encodes {w}x{h}, the clip is {}x{}",
            clip_spec.width, clip_spec.height
        ));
    }

    // Profile time 0: the first viewer clicks Watch.
    shim.expect("viewer1");
    shim.arm();
    let arm_us = clock_us();
    let end_us = arm_us + (p.duration_s * 1e6) as u64;
    let spec = |role: &str| ViewerSpec {
        role: role.to_owned(),
        ws: sfu.ws.clone(),
        password: sfu.password.clone(),
        session_id: session_id.clone(),
        width: w,
        height: h,
        receive_kbps: lane.bitrate_kbps,
        end_us,
        out_dir: pdir.clone(),
        capture_audio: audio_run.is_some(),
    };
    let v1 = session::spawn_viewer(Arc::clone(rt), Arc::clone(&ctxs[1]), spec("viewer1"));
    let mut v2 = None;
    if let Some(at) = p.late_viewer_s {
        let due = arm_us + (at * 1e6) as u64;
        while clock_us() < due {
            std::thread::sleep(Duration::from_millis(2));
        }
        shim.expect("viewer2");
        v2 = Some(session::spawn_viewer(
            Arc::clone(rt),
            Arc::clone(&ctxs[2]),
            spec("viewer2"),
        ));
    }
    let r1 = v1
        .join()
        .map_err(|_| "viewer1 thread panicked".to_string())?;
    let r2 = match v2 {
        Some(t) => Some(
            t.join()
                .map_err(|_| "viewer2 thread panicked".to_string())?,
        ),
        None => None,
    };
    let reference = host.reference();
    let host_end = host.stop(rt);
    let flows = shim.report();
    let other_cpu =
        stream_gate::load::other_pct(load_start, stream_gate::load::sample(&[sfu.raw_handle()]));

    for v in std::iter::once(&r1).chain(r2.as_ref()) {
        if let Some(e) = &v.error {
            return Err(format!("{run_name}: {}: {e}", v.role));
        }
    }
    let clock = SourceClock {
        start_us: reference.start_us,
        clip_frames: reference.clip_frames.max(1),
        fps: reference.fps.max(1),
    };
    let measured = r2.as_ref().unwrap_or(&r1);
    let d = metrics::delivery(
        &presents_of(measured),
        &clock,
        measured.join_us,
        measured.end_us,
        gf.analysis.freeze_gap_ms,
    );

    let mut m = Metrics::new();
    let mut put = |k: &str, v: Option<f64>| {
        m.insert(k.to_owned(), v);
    };
    put("other_cpu_pct", other_cpu);
    put("frames_presented", Some(d.presents as f64));
    put("frames_new", Some(d.new_frames as f64));
    put("frames_duplicated", Some(d.duplicated as f64));
    put("frames_dropped", Some(d.dropped as f64));
    put("frames_unreadable", Some(d.unreadable as f64));
    put("fps_delivered", d.fps);
    put("freeze_count", Some(d.freeze_count as f64));
    put("freeze_ms", Some(d.freeze_ms));
    put("freeze_pct", d.freeze_pct);
    put("ttff_ms", d.ttff_ms);
    put("frame_interval_p95_ms", d.interval_p95_ms);
    put("g2g_p50_ms", d.g2g_p50_ms);
    put("g2g_p95_ms", d.g2g_p95_ms);
    put("g2g_max_ms", d.g2g_max_ms);
    if let Some(r2) = &r2 {
        let d1 = metrics::delivery(
            &presents_of(&r1),
            &clock,
            r1.join_us,
            r1.end_us,
            gf.analysis.freeze_gap_ms,
        );
        put("other_viewer_freeze_ms", Some(d1.freeze_ms));
        let _ = r2;
    }
    // The first new frame after an outage ends.
    if let Some((s, dur)) = plan.host.outage.or(plan.viewer.outage) {
        let t_end = arm_us + ((s + dur) * 1e6) as u64;
        let before = measured
            .presents
            .iter()
            .rev()
            .find(|x| x.t_us <= t_end)
            .and_then(|x| x.code);
        let after = measured
            .presents
            .iter()
            .find(|x| x.t_us > t_end && (x.code.is_none() || x.code != before));
        put(
            "outage_recovery_ms",
            after.map(|x| (x.t_us - t_end) as f64 / 1000.0),
        );
    }

    let host_flow = flows.iter().find(|f| f.role == "host");
    let viewer_flow = flows.iter().find(|f| f.role == measured.role);
    if let Some(hf) = host_flow {
        let up = &hf.up;
        put(
            "host_wire_kbps",
            metrics::mean_kbps(&up.offered_bins, 0.0, p.duration_s),
        );
        put(
            "host_media_kbps",
            metrics::mean_kbps(&up.media_bins, 0.0, p.duration_s),
        );
        let vb = up.video_bytes.max(1) as f64;
        put("fec_overhead_pct", Some(100.0 * up.fec_bytes as f64 / vb));
        put("rtx_overhead_pct", Some(100.0 * up.rtx_bytes as f64 / vb));
        put(
            "host_leg_lost",
            Some((hf.up_counters.lost() + hf.down_counters.lost()) as f64),
        );
        if let Some(c) = &plan.host.cap {
            // The cap counts IP and UDP headers: add them per datagram.
            let wire: Vec<u64> = up
                .offered_bins
                .iter()
                .enumerate()
                .map(|(i, b)| {
                    b + up.offered_pkt_bins.get(i).copied().unwrap_or(0) * c.overhead_bytes as u64
                })
                .collect();
            let end = c.start_s + c.dur_s;
            put("cap_kbps", Some(c.rate_kbps));
            let send = metrics::mean_kbps(&wire, c.start_s + 2.0, end);
            put("cap_send_kbps", send);
            put(
                "cap_overshoot_pct",
                send.map(|s| 100.0 * (s / c.rate_kbps - 1.0)),
            );
            put("cap_queue_drops", Some(hf.up_counters.lost_queue as f64));
            put(
                "bitrate_reaction_s",
                metrics::reaction_s(&wire, c.start_s, end, c.rate_kbps),
            );
        }
    }
    if let Some(vf) = viewer_flow {
        let vb = vf.down.video_bytes.max(1) as f64;
        put(
            "viewer_rtx_pct",
            Some(100.0 * vf.down.rtx_bytes as f64 / vb),
        );
        put(
            "viewer_leg_lost",
            Some((vf.up_counters.lost() + vf.down_counters.lost()) as f64),
        );
    }
    if let Some(v) = &measured.video {
        put("rx_nacks", Some(v.rx_nack_sequences_sent as f64));
        put("rx_pli", Some(v.rx_pli_requests as f64));
        put("rx_fec_recovered", Some(v.rx_fec_recovered as f64));
        put(
            "rx_missing",
            Some(v.rx_core_missing_sequences_detected as f64),
        );
        put("rx_repaired", Some(v.rx_core_repaired_packets as f64));
        put("rx_gate_entries", Some(v.rx_core_gate_entries as f64));
        put(
            "rx_gate_dropped_aus",
            Some(v.rx_core_gate_dropped_access_units as f64),
        );
    }
    put("viewer_backlog_drops", Some(measured.backlog_drops as f64));
    put("tx_pli", Some(host_end.video.tx_pli_requests as f64));
    // The stage 4 sender (report rows, no gate).
    let hv = &host_end.video;
    put(
        "tx_max_idr_send_ms",
        Some(hv.tx_max_idr_send_us as f64 / 1000.0),
    );
    put("tx_idr_aus", Some(hv.tx_idr_access_units_sent as f64));
    put(
        "tx_idr_over_50ms",
        Some(hv.tx_idr_access_units_over_50ms as f64),
    );
    put(
        "tx_rtx_pct",
        (hv.tx_rtp_wire_bytes_sent > 0)
            .then(|| 100.0 * hv.tx_rtx_bytes_sent as f64 / hv.tx_rtp_wire_bytes_sent as f64),
    );
    put(
        "tx_probe_clusters",
        Some(hv.tx_probe_clusters_completed as f64),
    );
    put("tx_padding", Some(hv.tx_padding_packets_sent as f64));
    put("tx_send_pauses", Some(hv.tx_send_pauses as f64));
    put("tx_reprobes", Some(hv.tx_reprobes as f64));
    put("tx_rtx_negotiated", Some(f64::from(hv.tx_rtx_negotiated)));
    put("tx_rtx_suppressed", Some(hv.tx_rtx_suppressed as f64));
    put(
        "tx_feedback_rtt_ms",
        Some(hv.tx_feedback_rtt_us as f64 / 1000.0),
    );
    put(
        "host_keyframes",
        Some(f64::from(host_end.stats.keyframes_sent)),
    );
    put(
        "host_frames_late",
        Some(host_end.reference.late_frames as f64),
    );

    // Stage 7: the game audio and the A/V sync of the measured viewer.
    let mut audio_info = Value::Null;
    if let (Some(a), Some(cap)) = (audio_run, measured.audio.as_ref()) {
        let av = audio::score_av(
            &a.spec,
            &a.reference,
            cap,
            &presents_of(measured),
            &clock,
            measured.end_us,
        );
        put("av_within_40ms_pct", av.within_pct);
        put("av_offset_p50_ms", av.offset_p50_ms);
        put("av_offset_abs_p95_ms", av.offset_abs_p95_ms);
        put("av_markers_expected", Some(av.markers_expected as f64));
        put("av_markers_matched", Some(av.markers_matched as f64));
        put("audio_gaps", Some(av.gaps as f64));
        put("audio_gap_ms", Some(av.gap_ms));
        let (mos, pesq) = audio::score_speech(
            &a.spec,
            &a.reference,
            cap,
            &av,
            &audio::SpeechJob {
                start_us: reference.start_us,
                dir: &pdir,
                profile: run_name,
                script: &a.script,
            },
        );
        put("audio_mos", mos);
        if let Some(st) = measured.audio_stats {
            put("audio_delay_ms", Some(f64::from(st.delay_ms)));
            put("av_sync_offset_ms", Some(f64::from(st.av_offset_ms)));
            put(
                "av_video_min_delay_ms",
                Some(f64::from(st.av_video_min_delay_ms)),
            );
            put(
                "av_audio_min_delay_ms",
                Some(f64::from(st.av_audio_min_delay_ms)),
            );
            put("audio_expand_ms", Some(st.expand_ms as f64));
            put("audio_fec_used", Some(st.fec_used as f64));
            put("audio_packets", Some(st.packets as f64));
            put("audio_sync_extensions", Some(st.sync_extensions as f64));
        }
        // Each marker's offset, with when it was heard (ms after the arm).
        let mut csv = String::from("marker,heard_ms,offset_ms\n");
        for (k, _, at, ms) in &av.matches {
            let heard = cap
                .heard_us(*at)
                .map_or(-1.0, |t| (t - arm_us as f64) / 1000.0);
            csv.push_str(&format!("{k},{heard:.0},{ms:.1}\n"));
        }
        let _ = std::fs::write(pdir.join("av.csv"), csv);
        audio_info = json!({
            "markers_expected": av.markers_expected,
            "markers_matched": av.markers_matched,
            "offset_min_ms": av.offset_min_ms,
            "offset_max_ms": av.offset_max_ms,
            "gaps": av.gap_list.iter().map(|(t, ms)| json!({
                "heard_ms": ((t - arm_us as f64) / 1000.0).round(),
                "ms": ms,
            })).collect::<Vec<_>>(),
            "pulls": cap.pull_us.len(),
            "pesq": pesq,
            "viewer": measured.audio_stats.map(|st| json!({
                "delay_ms": st.delay_ms,
                "av_offset_ms": st.av_offset_ms,
                "av_video_min_delay_ms": st.av_video_min_delay_ms,
                "av_audio_min_delay_ms": st.av_audio_min_delay_ms,
                "packets": st.packets,
                "expand_ms": st.expand_ms,
                "fec_used": st.fec_used,
                "sync_extensions": st.sync_extensions,
                "sync_changes": st.sync_changes,
            })),
        });
    }

    // Visual scores on the measured viewer's new frames.
    let pairs: Vec<score::Pair> = d
        .scored
        .iter()
        .filter_map(|(i, abs)| {
            measured.presents[*i].slot.map(|slot| score::Pair {
                dis_slot: slot,
                clip_frame: (*abs % u64::from(clock.clip_frames)) as u32,
            })
        })
        .collect();
    let (wu, hu) = (w as usize, h as usize);
    let mut vmaf_job = None;
    if !pairs.is_empty() {
        let vis = score::psnr_ssim(clip_path, &measured.luma_path, wu, hu, &pairs)?;
        let mean = |v: &[f64]| {
            if v.is_empty() {
                None
            } else {
                Some(v.iter().sum::<f64>() / v.len() as f64)
            }
        };
        put("psnr_y_mean", mean(&vis.psnr));
        put("ssim_y_mean", mean(&vis.ssim));
        if !no_vmaf {
            let (rp, dp) = (pdir.join("vmaf-ref.yuv"), pdir.join("vmaf-dis.yuv"));
            let n = score::write_vmaf_pair(
                clip_path,
                &measured.luma_path,
                wu,
                hu,
                &pairs,
                gf.analysis.vmaf_subsample as usize,
                &rp,
                &dp,
            )?;
            if n > 0 {
                vmaf_job = Some(score::VmafJob {
                    id: run_name.to_owned(),
                    reference: rp,
                    distorted: dp,
                    width: wu,
                    height: hu,
                });
            }
        }
    }
    if !keep {
        for v in std::iter::once(&r1).chain(r2.as_ref()) {
            let _ = std::fs::remove_file(&v.luma_path);
        }
    }

    // Per-run files: presents and the host series.
    let mut csv = String::from("t_ms,index,clock_ms,slot\n");
    for x in &measured.presents {
        let t = (x.t_us as i64 - arm_us as i64) as f64 / 1000.0;
        match x.code {
            Some(c) => csv.push_str(&format!(
                "{t:.1},{},{},{}\n",
                c.index,
                c.clock_ms,
                x.slot.map_or(-1, |s| s as i64)
            )),
            None => csv.push_str(&format!("{t:.1},,,\n")),
        }
    }
    let _ = std::fs::write(pdir.join("presents.csv"), csv);
    let mut hcsv = String::from(
        "t_ms,encoder_kbps,pacing_target_kbps,gcc_target_kbps,keyframes,pacing_rate_kbps,width,height,fps,\
         wire_bytes,rtx_bytes,plis,local_idrs,aus_dropped,queued_aus,enc_target_kbps,\
         idrs_sent,last_idr_send_us\n",
    );
    for s in &host_end.samples {
        let t = (s.t_us as i64 - arm_us as i64) as f64 / 1000.0;
        hcsv.push_str(&format!(
            "{t:.0},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}\n",
            s.encoder_kbps,
            s.pacing_target_kbps,
            s.gcc_target_kbps,
            s.keyframes,
            s.pacing_rate_kbps,
            s.width,
            s.height,
            s.fps,
            s.wire_bytes,
            s.rtx_bytes,
            s.plis,
            s.local_idrs,
            s.aus_dropped,
            s.queued_aus,
            s.enc_target_kbps,
            s.idrs_sent,
            s.last_idr_send_us
        ));
    }
    let _ = std::fs::write(pdir.join("host.csv"), hcsv);
    let mut wcsv = String::from(
        "t_s,host_offered_kbps,host_delivered_kbps,viewer_offered_kbps,viewer_delivered_kbps\n",
    );
    let bins = |f: Option<&stream_gate::shim::FlowReport>, up: bool, offered: bool| -> Vec<u64> {
        f.map(|f| {
            let w = if up { &f.up } else { &f.down };
            if offered {
                w.offered_bins.clone()
            } else {
                w.delivered_bins.clone()
            }
        })
        .unwrap_or_default()
    };
    let cols = [
        bins(host_flow, true, true),
        bins(host_flow, true, false),
        bins(viewer_flow, false, true),
        bins(viewer_flow, false, false),
    ];
    let n = cols.iter().map(Vec::len).max().unwrap_or(0);
    for i in 0..n {
        let v: Vec<String> = cols
            .iter()
            .map(|c| format!("{}", c.get(i).copied().unwrap_or(0) * 8 / 100))
            .collect();
        wcsv.push_str(&format!("{:.1},{}\n", i as f64 / 10.0, v.join(",")));
    }
    let _ = std::fs::write(pdir.join("wire.csv"), wcsv);

    let viewers_json: Vec<Value> = std::iter::once(&r1)
        .chain(r2.as_ref())
        .map(|v| {
            json!({
                "role": v.role,
                "presents": v.presents.len(),
                "feed_failures": v.feed_failures,
                "backlog_drops": v.backlog_drops,
                "aus_fed": v.aus_fed,
                "tick_max_ms": v.tick_max_ms,
                "ticks_over_30ms": v.ticks_over_30ms,
                "present_max_ms": v.present_max_ms,
                "callback_max_ms": v.callback_max_ms,
                "aus_idr": v.aus_idr,
                "timeline": v.timeline.map(|t| json!({
                    "target_delay_ms": t.target_delay_ms,
                    "jitter_ms": t.jitter_ms,
                    "decode_ms_p95": t.decode_ms_p95,
                    "frames_released": t.frames_released,
                    "frames_late": t.frames_late,
                    "frames_dropped_late": t.frames_dropped_late,
                    "frames_decoded": t.frames_decoded,
                    "frames_presented": t.frames_presented,
                    "keyframe_requests": t.keyframe_requests,
                    "loss_events": t.loss_events,
                })),
                "rx": v.video.as_ref().map(|x| json!({
                    "ingress_packets": x.rx_ingress_packets,
                    "ingress_overflows": x.rx_ingress_overflows,
                    "aus_queued": x.rx_access_units_queued_total,
                    "aus_dropped": x.rx_access_units_dropped,
                    "core_complete": x.rx_core_complete_access_units,
                    "core_incomplete": x.rx_core_incomplete_access_units,
                    "core_emitted": x.rx_core_emitted_access_units,
                    "gate_entries": x.rx_core_gate_entries,
                    "gate_dropped": x.rx_core_gate_dropped_access_units,
                    "evictions": x.rx_core_buffer_evictions,
                    "missing": x.rx_core_missing_sequences_detected,
                    "late": x.rx_core_late_packets,
                    "pli": x.rx_pli_requests,
                    "nack_sequences": x.rx_nack_sequences_sent,
                    "awaiting_idr": x.rx_awaiting_output_idr,
                    "core_restarts": x.rx_core_restarts,
                    "wrong_ssrc": x.rx_core_wrong_ssrc_packets,
                    "invalid_h264": x.rx_core_invalid_h264_packets,
                })),
                "scaled_frames": v.scaled_frames,
            })
        })
        .collect();
    let info = json!({
        "session": session_id,
        "measured_viewer": measured.role,
        "host": {
            "encoder": cstr(&host_end.stats.encoder_name),
            "gpu": cstr(&host_end.stats.gpu_name),
            "capture": cstr(&host_end.stats.capture_backend),
            "encode_queue_drops": host_end.stats.encode_queue_drops,
            "reference": {
                "frames_delivered": host_end.reference.frames_delivered,
                "late_frames": host_end.reference.late_frames,
                "max_late_us": host_end.reference.max_late_us,
            },
            "tx": {
                "aus_sent": host_end.video.tx_access_units_sent,
                "aus_dropped": host_end.video.tx_access_units_dropped,
                "rtp_packets": host_end.video.tx_rtp_packets_sent,
                "rtx_sent": host_end.video.tx_rtx_sent,
                "fec_packets": host_end.video.tx_fec_packets_sent,
                "pli": host_end.video.tx_pli_requests,
                "remb": host_end.video.tx_remb_reports,
            },
        },
        "viewers": viewers_json,
        "audio": audio_info,
        "wire": flows.iter().map(|f| json!({
            "role": f.role,
            "up": { "offered_packets": f.up.offered_packets, "offered_bytes": f.up.offered_bytes,
                    "video_packets": f.up.video_packets, "rtx_packets": f.up.rtx_packets, "fec_packets": f.up.fec_packets,
                    "audio_packets": f.up.audio_packets, "rtcp_packets": f.up.rtcp_packets,
                    "lost": f.up_counters.lost(), "lost_queue": f.up_counters.lost_queue },
            "down": { "offered_packets": f.down.offered_packets, "offered_bytes": f.down.offered_bytes,
                      "video_packets": f.down.video_packets, "rtx_packets": f.down.rtx_packets, "fec_packets": f.down.fec_packets,
                      "audio_packets": f.down.audio_packets, "rtcp_packets": f.down.rtcp_packets,
                      "lost": f.down_counters.lost(), "lost_queue": f.down_counters.lost_queue },
        })).collect::<Vec<_>>(),
    });
    Ok(Run {
        metrics: m,
        info,
        vmaf_job,
    })
}

/// Encode the first 2 s of the 720p clip with the software encoder and
/// write the Annex-B access units back to back.
fn write_fixture(root: &Path, gf: &GateFile, out: &Path) -> Result<(), String> {
    let entry = gf
        .clips
        .get("720p60")
        .ok_or("profiles.json has no 720p60 clip")?;
    let (clip_path, _) = ensure_clip(root, entry)?;
    // A 2 s copy of the clip: the source loops it, the fixture is its first
    // pass.
    let two = root.join("target/stream-gate/fixture-2s.yuv");
    if let Some(d) = two.parent() {
        std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
    }
    {
        use std::io::Read;
        let mut f = std::fs::File::open(&clip_path).map_err(|e| e.to_string())?;
        let mut buf = vec![0u8; entry.spec.frame_bytes() * 120];
        f.read_exact(&mut buf).map_err(|e| e.to_string())?;
        std::fs::write(&two, &buf).map_err(|e| e.to_string())?;
    }
    std::env::set_var("MELLO_SOFT_ENCODER", "1");
    let ctx = Ctx::new()?;
    let path_c =
        std::ffi::CString::new(two.to_string_lossy().as_bytes()).map_err(|e| e.to_string())?;
    // SAFETY: ctx is live.
    unsafe {
        mello_sys::mello_test_set_reference_clip(
            ctx.0,
            path_c.as_ptr(),
            entry.spec.width,
            entry.spec.height,
            60,
            mello_sys::MelloReferencePixelFormat_MELLO_REFERENCE_I420,
        );
    }
    struct Collect {
        aus: std::sync::Mutex<Vec<Vec<u8>>>,
    }
    unsafe extern "C" fn on_packet(
        user: *mut std::ffi::c_void,
        data: *const u8,
        size: i32,
        _key: bool,
        _ts: u64,
    ) {
        // SAFETY: user is the Collect below, live until the host stops.
        let c = unsafe { &*(user as *const Collect) };
        let au = unsafe { std::slice::from_raw_parts(data, size as usize) }.to_vec();
        c.aus.lock().unwrap_or_else(|p| p.into_inner()).push(au);
    }
    let collect = Box::new(Collect {
        aus: std::sync::Mutex::new(Vec::new()),
    });
    let source = mello_sys::MelloCaptureSource {
        mode: mello_sys::MelloCaptureMode_MELLO_CAPTURE_REFERENCE,
        monitor_index: 0,
        hwnd: std::ptr::null_mut(),
        pid: 0,
        allow_hook: false,
    };
    let config = mello_sys::MelloStreamConfig {
        width: 0,
        height: 0,
        fps: 60,
        bitrate_kbps: 2500,
    };
    // SAFETY: ctx live; collect outlives the host (stopped below).
    let host = unsafe {
        mello_sys::mello_stream_start_host(
            ctx.0,
            &source,
            &config,
            Some(on_packet),
            &*collect as *const Collect as *mut _,
        )
    };
    if host.is_null() {
        return Err("mello_stream_start_host (reference, software encoder) failed".into());
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while collect.aus.lock().map(|a| a.len()).unwrap_or(0) < 120 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    // SAFETY: host from start_host above.
    unsafe { mello_sys::mello_stream_stop_host(host) };
    let aus = collect.aus.lock().map(|a| a.clone()).unwrap_or_default();
    if aus.len() < 120 {
        return Err(format!("only {} access units in 10 s", aus.len()));
    }
    let bytes: Vec<u8> = aus.iter().take(120).flatten().copied().collect();
    std::fs::write(out, &bytes).map_err(|e| format!("{}: {e}", out.display()))?;
    println!(
        "fixture: {} ({} bytes, 120 access units, 1280x720, 60 fps, 2500 kbps, OpenH264), fnv1a {:016x}",
        out.display(),
        bytes.len(),
        clip::fnv1a(clip::FNV_INIT, &bytes)
    );
    let _ = std::fs::remove_file(&two);
    Ok(())
}

/// One finished profile run, with VMAF merged in.
struct Done {
    name: String,
    profile: String,
    metrics: Metrics,
    info: Value,
}

/// Runs profiles on the live path and scores each batch with VMAF.
struct Runner<'a> {
    rt: &'a Arc<tokio::runtime::Runtime>,
    ctxs: &'a [Arc<Ctx>; 3],
    sfu: &'a mut sfu::Sfu,
    shim: &'a Shim,
    gf: &'a GateFile,
    lane: &'a Lane,
    clip_path: &'a Path,
    clip_spec: &'a clip::ClipSpec,
    out_dir: &'a Path,
    no_vmaf: bool,
    keep: bool,
    script: &'a Path,
    audio: Option<&'a AudioRun>,
    /// The scorer's answer of a batch that ran VMAF.
    vmaf_detail: Option<score::VmafScores>,
    /// Runs during which other processes used more CPU than
    /// `load::MAX_OTHER_PCT`: (run, percent).
    busy: Vec<(String, f64)>,
}

impl Runner<'_> {
    fn run_batch(&mut self, plan: &[(&Profile, String)]) -> Result<Vec<Done>, String> {
        let mut done = Vec::new();
        let mut jobs = Vec::new();
        for (p, name) in plan {
            eprint!("stream-gate: {:<16} {:>4.0} s ... ", name, p.duration_s);
            let t = Instant::now();
            if !self.sfu.is_running() {
                return Err(format!("the SFU stopped; see {}", self.sfu.log.display()));
            }
            let run = run_profile(
                self.rt,
                self.ctxs,
                self.sfu,
                self.shim,
                self.gf,
                self.lane,
                p,
                self.clip_path,
                self.clip_spec,
                name,
                self.out_dir,
                self.no_vmaf,
                self.keep,
                self.audio,
            )?;
            eprintln!(
                "{:.1} s (fps {}, freeze {} ms, g2g p95 {} ms)",
                t.elapsed().as_secs_f64(),
                report::fmt_num(run.metrics.get("fps_delivered").cloned().flatten()),
                report::fmt_num(run.metrics.get("freeze_ms").cloned().flatten()),
                report::fmt_num(run.metrics.get("g2g_p95_ms").cloned().flatten()),
            );
            if let Some(j) = run.vmaf_job {
                jobs.push(j);
            }
            if let Some(pct) = run.metrics.get("other_cpu_pct").cloned().flatten() {
                if pct > stream_gate::load::MAX_OTHER_PCT {
                    eprintln!(
                        "stream-gate: WARNING: other processes used {pct:.0} % of the CPU during {name}"
                    );
                    self.busy.push((name.clone(), pct));
                }
            }
            done.push(Done {
                name: name.clone(),
                profile: p.name.clone(),
                metrics: run.metrics,
                info: run.info,
            });
        }
        if !jobs.is_empty() {
            eprintln!("stream-gate: VMAF on {} runs ...", jobs.len());
            let scores = score::vmaf(self.script, self.out_dir, &jobs);
            for (id, s) in &scores.results {
                if let Some(d) = done.iter_mut().find(|d| &d.name == id) {
                    let (mean, p5, min) = match s {
                        Some((m, p, n)) => (Some(*m), Some(*p), Some(*n)),
                        None => (None, None, None),
                    };
                    d.metrics.insert("vmaf_mean".into(), mean);
                    d.metrics.insert("vmaf_p5".into(), p5);
                    d.metrics.insert("vmaf_min".into(), min);
                }
            }
            if !self.keep {
                for j in &jobs {
                    let _ = std::fs::remove_file(&j.reference);
                    let _ = std::fs::remove_file(&j.distorted);
                }
            }
            if self.vmaf_detail.is_none() || scores.available {
                self.vmaf_detail = Some(scores);
            }
        }
        Ok(done)
    }

    fn vmaf_available(&self) -> bool {
        self.vmaf_detail.as_ref().is_some_and(|v| v.available)
    }

    fn vmaf_scores(&mut self) -> score::VmafScores {
        let no_vmaf = self.no_vmaf;
        self.vmaf_detail
            .take()
            .unwrap_or_else(|| score::VmafScores {
                available: false,
                scorer: "none".into(),
                detail: json!({
                    "available": false,
                    "reason": if no_vmaf { "--no-vmaf" } else { "no frames" },
                }),
                results: Vec::new(),
            })
    }
}

fn real_main() -> Result<bool, String> {
    let args = parse_args()?;
    let started = Instant::now();
    let root = repo_root()?;
    let base_dir = root.join("benchmarks/baselines/stream");
    let gf = profile::load(&base_dir.join("profiles.json"))?;
    let gates = profile::load_gates(&base_dir.join("gates.json"))?;
    // No audio device: the gate measures video, and a context with the test
    // backend opens nothing (TESTING.md, voice quality gate).
    std::env::set_var("MELLO_AUDIO_BACKEND", "test");
    let log_level = std::env::var("STREAM_GATE_LOG")
        .ok()
        .and_then(|v| v.parse::<i32>().ok())
        .unwrap_or(2);
    // SAFETY: global setter.
    unsafe { mello_sys::mello_set_log_level(log_level) };

    if args.gen_clips {
        for entry in gf.clips.values() {
            ensure_clip(&root, entry)?;
        }
        return Ok(true);
    }
    if let Some(out) = &args.write_fixture {
        write_fixture(&root, &gf, out)?;
        return Ok(true);
    }

    let lane = gf
        .lanes
        .get(&args.lane)
        .cloned()
        .ok_or(format!("unknown lane '{}'", args.lane))?;
    if lane.soft {
        std::env::set_var("MELLO_SOFT_ENCODER", "1");
        std::env::set_var("MELLO_SOFT_DECODER", "1");
    } else {
        std::env::remove_var("MELLO_SOFT_ENCODER");
        std::env::remove_var("MELLO_SOFT_DECODER");
    }
    let baseline_path = match &args.baseline {
        Some(p) if p.components().count() == 1 => base_dir.join(p),
        Some(p) => p.clone(),
        None => base_dir.join(format!("baseline-2.1-{}.json", lane.name)),
    };
    let stem = baseline_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let baseline_name = stem.strip_prefix("baseline-").unwrap_or(&stem).to_owned();
    let out_dir = args
        .out
        .clone()
        .unwrap_or_else(|| root.join("target/stream-gate").join(&lane.name));
    std::fs::create_dir_all(&out_dir).map_err(|e| format!("{}: {e}", out_dir.display()))?;
    // Absolute: the SFU build runs in the SFU directory.
    let out_dir =
        std::path::absolute(&out_dir).map_err(|e| format!("{}: {e}", out_dir.display()))?;
    if args.write_baseline && baseline_path.exists() {
        return Err(format!(
            "{} exists. A baseline is never edited. Delete it only to re-take it on the commit it came from.",
            baseline_path.display()
        ));
    }

    // The clip.
    let entry = gf
        .clips
        .get(&lane.clip)
        .cloned()
        .ok_or("lane clip missing")?;
    let (clip_path, clip_hash, clip_spec, custom_clip) = match &args.clip {
        Some(path) => {
            let (w, h) = args.clip_size.ok_or("--clip needs --clip-size WxH")?;
            let spec = clip::ClipSpec {
                width: w,
                height: h,
                fps: args.clip_fps,
                frames: 0,
                seed: 0,
            };
            (path.clone(), clip::hash_file(path)?, spec, true)
        }
        None => {
            let (p, h) = ensure_clip(&root, &entry)?;
            (p, h, entry.spec.clone(), false)
        }
    };
    if args.write_baseline && (custom_clip || entry.fnv1a.is_empty()) {
        return Err(
            "--write-baseline needs the generated clip with its hash frozen in profiles.json"
                .into(),
        );
    }

    let baseline: Option<Value> = if args.write_baseline || custom_clip || !baseline_path.exists() {
        None
    } else {
        let b: Value =
            serde_json::from_slice(&std::fs::read(&baseline_path).map_err(|e| e.to_string())?)
                .map_err(|e| format!("{}: {e}", baseline_path.display()))?;
        let b_inputs = b
            .get("inputs_fingerprint")
            .and_then(Value::as_str)
            .unwrap_or("");
        if b_inputs != gf.inputs_fingerprint {
            return Err(format!(
                "frozen inputs changed since the baseline (profiles {} vs {}). Re-take the baseline on the \
                 commit it came from; never compare across inputs.",
                gf.inputs_fingerprint, b_inputs
            ));
        }
        Some(b)
    };

    let selected: Vec<&Profile> = gf
        .profiles
        .iter()
        .filter(|p| {
            args.only
                .as_ref()
                .is_none_or(|o| o.iter().any(|n| n == &p.name))
        })
        .collect();
    if selected.is_empty() {
        return Err("no profile selected".into());
    }
    if args.write_baseline && (selected.len() != gf.profiles.len() || args.repeat != 1) {
        return Err("--write-baseline runs every profile once; drop --only and --repeat".into());
    }

    // The path: shim, then the SFU behind it.
    let shim_addr: std::net::SocketAddr = format!("{}:{}", sfu::SHIM_IP, sfu::MUX_PORT)
        .parse()
        .map_err(|e| format!("{e}"))?;
    let mux_addr: std::net::SocketAddr = format!("127.0.0.1:{}", sfu::MUX_PORT)
        .parse()
        .map_err(|e| format!("{e}"))?;
    let shim = Shim::start(shim_addr, mux_addr)?;
    let sfu_dir = std::env::var("MELLO_SFU_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| root.join("../mello-sfu"));
    eprintln!(
        "stream-gate: building and starting the SFU from {}",
        sfu_dir.display()
    );
    let mut sfu = sfu::Sfu::start(&sfu_dir, &out_dir)?;
    let rt = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?,
    );
    let ctxs = [
        Arc::new(Ctx::new()?),
        Arc::new(Ctx::new()?),
        Arc::new(Ctx::new()?),
    ];
    let encoders = names(&ctxs[0], true);
    let decoders = names(&ctxs[1], false);

    let script = root.join("scripts/stream-gate-score.py");
    // Stage 7: the clip's game audio, built from the voice gate corpus.
    let audio_run = if custom_clip {
        None
    } else {
        let spec = audio::load(&base_dir.join("audio.json"))?;
        let loop_samples = (u64::from(clip_spec.frames) * audio::SAMPLE_RATE as u64
            / u64::from(clip_spec.fps.max(1))) as usize;
        let reference = audio::build(
            &spec,
            &root.join("libmello/tests/fixtures/voice"),
            loop_samples,
        )?;
        let track = audio::track_path(&out_dir, &spec, loop_samples);
        audio::write_raw(&track, &reference)?;
        Some(AudioRun {
            spec,
            reference,
            track,
            script: root.join("scripts/voice-gate-score.py"),
        })
    };
    let mut runner = Runner {
        rt: &rt,
        ctxs: &ctxs,
        sfu: &mut sfu,
        shim: &shim,
        gf: &gf,
        lane: &lane,
        clip_path: &clip_path,
        clip_spec: &clip_spec,
        out_dir: &out_dir,
        no_vmaf: args.no_vmaf || custom_clip,
        keep: args.keep,
        script: &script,
        audio: audio_run.as_ref(),
        vmaf_detail: None,
        busy: Vec::new(),
    };

    let mut plan: Vec<(&Profile, String)> = Vec::new();
    for p in &selected {
        for rep in 0..args.repeat {
            let name = if rep == 0 {
                p.name.clone()
            } else {
                format!("{}#{}", p.name, rep + 1)
            };
            plan.push((*p, name));
        }
    }
    let first = runner.run_batch(&plan)?;
    let mut results: BTreeMap<String, Metrics> = BTreeMap::new();
    let mut infos: BTreeMap<String, Value> = BTreeMap::new();
    let mut order = Vec::new();
    let mut repeats: BTreeMap<String, Vec<Metrics>> = BTreeMap::new();
    for done in first {
        if done.name == done.profile {
            order.push(done.name.clone());
            results.insert(done.name.clone(), done.metrics.clone());
            infos.insert(done.name.clone(), done.info);
        }
        repeats.entry(done.profile).or_default().push(done.metrics);
    }

    let (mut gate_results, mut notes) = report::evaluate(
        &lane.name,
        &gates,
        &results,
        runner.vmaf_available(),
        baseline.as_ref(),
    );
    if let Some(a) = &audio_run {
        gate_results.extend(audio::evaluate(&a.spec, &results));
    }

    // A failing profile runs twice more and the gates read the median of
    // its three runs: one run of a real-time path can land in its tail. The
    // median of three is the estimate; a profile that only passes on a lucky
    // run does not pass.
    let failing: Vec<&Profile> = selected
        .iter()
        .copied()
        .filter(|p| {
            gate_results
                .iter()
                .any(|g| g.enforced && !g.pass && g.profile == p.name)
        })
        .collect();
    if baseline.is_some()
        && !args.report_only
        && !args.write_baseline
        && !args.no_confirm
        && args.repeat == 1
        && !failing.is_empty()
    {
        eprintln!(
            "stream-gate: {} profile(s) outside a gate; two more runs each for the median of three",
            failing.len()
        );
        let mut again: Vec<(&Profile, String)> = Vec::new();
        for p in &failing {
            again.push((*p, format!("{}#confirm2", p.name)));
            again.push((*p, format!("{}#confirm3", p.name)));
        }
        let more = runner.run_batch(&again)?;
        let mut confirmed = Vec::new();
        for p in &failing {
            let mut runs = vec![results[&p.name].clone()];
            runs.extend(
                more.iter()
                    .filter(|d| d.profile == p.name)
                    .map(|d| d.metrics.clone()),
            );
            if let Some(info) = infos.get_mut(&p.name).and_then(Value::as_object_mut) {
                info.insert(
                    "confirm".into(),
                    json!(runs.iter().map(report::metrics_json).collect::<Vec<_>>()),
                );
            }
            results.insert(p.name.clone(), metrics::median_metrics(&runs));
            confirmed.push(p.name.clone());
        }
        let (g, n) = report::evaluate(
            &lane.name,
            &gates,
            &results,
            runner.vmaf_available(),
            baseline.as_ref(),
        );
        gate_results = g;
        if let Some(a) = &audio_run {
            gate_results.extend(audio::evaluate(&a.spec, &results));
        }
        notes = n;
        notes.push(format!(
            "Median of three runs (the first failed a gate): {}.",
            confirmed.join(", ")
        ));
    }
    let vmaf = runner.vmaf_scores();
    let busy = std::mem::take(&mut runner.busy);
    let (sfu_commit, sfu_toolchain) = (sfu.commit.clone(), sfu.toolchain.clone());
    drop(shim);
    drop(sfu);
    if !vmaf.available {
        eprintln!(
            "stream-gate: WARNING: no VMAF ({}). PSNR and SSIM only.",
            vmaf.detail
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
        );
    }

    let commit = git(&root, &["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let dirty = git(&root, &["status", "--porcelain", "--untracked-files=no"])
        .map(|s| !s.is_empty())
        .unwrap_or(true);
    let host_encoder = infos
        .values()
        .next()
        .and_then(|i| i["host"]["encoder"].as_str())
        .unwrap_or("")
        .to_owned();
    let gpu = infos
        .values()
        .next()
        .and_then(|i| i["host"]["gpu"].as_str())
        .unwrap_or("")
        .to_owned();
    let mut profiles_json = serde_json::Map::new();
    for name in &order {
        let p = gf.profiles.iter().find(|p| &p.name == name);
        profiles_json.insert(
            name.clone(),
            json!({
                "description": p.map(|p| p.description.clone()),
                "duration_s": p.map(|p| p.duration_s),
                "seed": p.map(|p| p.seed),
                "metrics": report::metrics_json(&results[name]),
                "info": infos[name],
            }),
        );
    }
    let mut spread = serde_json::Map::new();
    if args.repeat > 1 {
        for (name, list) in &repeats {
            let mut per = serde_json::Map::new();
            for (k, _, _) in report::METRICS {
                let vals: Vec<Option<f64>> =
                    list.iter().map(|m| m.get(*k).cloned().flatten()).collect();
                if vals.iter().any(Option::is_some) {
                    per.insert((*k).to_owned(), json!(vals));
                }
            }
            spread.insert(name.clone(), Value::Object(per));
        }
    }
    let result = json!({
        "schema": 1,
        "kind": "stream-gate-result",
        "lane": lane.name,
        "git": { "commit": commit, "dirty": dirty },
        "sfu": { "dir": sfu_dir.to_string_lossy(), "commit": sfu_commit, "go": sfu_toolchain },
        "taken_at_utc": voice_gate::report::utc_now(),
        "platform": { "os": std::env::consts::OS, "arch": std::env::consts::ARCH },
        "machine": host_info(),
        "gpu": gpu,
        "encoder": host_encoder,
        "encoders_available": encoders,
        "decoders_available": decoders,
        "decoder": if lane.soft { "OpenH264 (MELLO_SOFT_DECODER=1)".to_owned() } else { decoders.first().cloned().unwrap_or_default() },
        "inputs_fingerprint": gf.inputs_fingerprint,
        "clip": {
            "file": clip_path.file_name().map(|f| f.to_string_lossy().into_owned()),
            "width": clip_spec.width, "height": clip_spec.height, "fps": clip_spec.fps,
            "frames": clip_spec.frames, "seed": clip_spec.seed, "fnv1a": clip_hash,
            "generator_version": clip::CLIP_VERSION, "custom": custom_clip,
        },
        "seeds": gf.profiles.iter().map(|p| (p.name.clone(), p.seed)).collect::<BTreeMap<_, _>>(),
        "scorer": {
            "vmaf": vmaf.detail,
            "psnr_ssim": "stream-gate score.rs: luma, PSNR 10 log10(255^2/MSE) capped at 100, SSIM 8x8 windows on a 4 px grid",
        },
        "method": {
            "path": "reference clip (MELLO_CAPTURE_REFERENCE) -> D3D11 video processor -> preprocessor -> encoder -> mello-core manager and SfuSink -> RTP sender -> shim (UDP proxy, host leg) -> mello-sfu in test mode -> shim (viewer leg) -> RTP receiver session -> receive timeline (stage 3) -> viewer decode thread -> present thread at each render time -> CPU frame callback",
            "lane": format!("{}: clip {}, {} kbps at {} fps, preset {}, {}", lane.name, lane.clip, lane.bitrate_kbps, lane.fps, lane.preset,
                if lane.soft { "OpenH264 encode and decode (MELLO_SOFT_ENCODER=1, MELLO_SOFT_DECODER=1)" } else { "hardware encode and decode" }),
            "shim": format!("netmodel shim version {}; base delay {} ms each way per leg", stream_gate::netmodel::SHIM_VERSION, gf.profiles.first().map_or(0.0, |p| p.host.base_delay_ms)),
            "freeze": format!("present gap over {} ms between new frames", gf.analysis.freeze_gap_ms),
            "vmaf": format!("vmaf_v0.6.1 on luma, every {} new frame(s)", gf.analysis.vmaf_subsample),
        },
        "profiles": Value::Object(profiles_json),
        "audio": audio_run.as_ref().map(|a| json!({
            "audio_fingerprint": a.spec.fingerprint,
            "audio_version": a.spec.version,
            "speech": a.spec.speech,
            "voice_mobile_mos": a.spec.voice_mobile_mos,
            "voice_mobile_mos_source": a.spec.voice_mobile_mos_source,
            "method": "the viewer context's mixed output pulled 10 ms at a time on a fixed schedule (mello_voice_test_pull_output); markers: tone-burst onsets against the present time of their video frame; gaps: the pilot tone; MOS: PESQ-WB on the speech, 4 s segments aligned by the markers",
        })),
        "spread": Value::Object(spread),
        "gates": report::gates_json(&gate_results),
        "runtime_s": (started.elapsed().as_secs_f64() * 10.0).round() / 10.0,
    });
    let results_path = out_dir.join("results.json");
    std::fs::write(
        &results_path,
        serde_json::to_string_pretty(&result).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("{}: {e}", results_path.display()))?;

    println!();
    match &baseline {
        Some(b) => println!(
            "Streaming quality gate, lane {}. Delta against baseline {baseline_name} ({}).",
            lane.name,
            b.get("git")
                .and_then(|g| g.get("commit"))
                .and_then(Value::as_str)
                .unwrap_or("?")
        ),
        None => println!(
            "Streaming quality gate, lane {}. No baseline to compare.",
            lane.name
        ),
    }
    println!(
        "Encoder: {host_encoder}. GPU: {gpu}. VMAF: {}.",
        vmaf.scorer
    );
    println!();
    print!(
        "{}",
        report::summary_table(&order, &results, baseline.as_ref())
    );
    print!(
        "{}",
        report::detail_table(&order, &results, baseline.as_ref())
    );
    if args.repeat > 1 {
        println!("\nSpread over {} runs (min / max):", args.repeat);
        for (name, list) in &repeats {
            let mm = |k: &str| {
                let v: Vec<f64> = list
                    .iter()
                    .filter_map(|m| m.get(k).cloned().flatten())
                    .collect();
                if v.is_empty() {
                    "-".to_owned()
                } else {
                    format!(
                        "{} / {}",
                        report::fmt_num(v.iter().cloned().reduce(f64::min)),
                        report::fmt_num(v.iter().cloned().reduce(f64::max))
                    )
                }
            };
            println!(
                "  {name:<10} vmaf {}  freeze_ms {}  fps {}  ttff {}  g2g_p95 {}",
                mm("vmaf_mean"),
                mm("freeze_ms"),
                mm("fps_delivered"),
                mm("ttff_ms"),
                mm("g2g_p95_ms")
            );
        }
    }
    println!();
    print!("{}", report::gate_table(&gate_results));
    for n in &notes {
        println!("NOTE: {n}");
    }
    println!();
    println!("Results: {}", results_path.display());
    println!("Runtime: {:.1} s", started.elapsed().as_secs_f64());

    if !busy.is_empty() {
        let list: Vec<String> = busy
            .iter()
            .map(|(n, p)| format!("{n} ({p:.0} %)"))
            .collect();
        return Err(format!(
            "INVALID: other processes used more than {} % of the CPU during {}. A busy machine moves every number; run the gate again on a quiet machine. Nothing was compared{}.",
            stream_gate::load::MAX_OTHER_PCT,
            list.join(", "),
            if args.write_baseline { " and no baseline was written" } else { "" }
        ));
    }
    if args.write_baseline {
        let mut b = result.clone();
        if let Some(m) = b.as_object_mut() {
            m.insert("kind".into(), json!("stream-gate-baseline"));
            m.insert("baseline".into(), json!(baseline_name));
        }
        std::fs::write(
            &baseline_path,
            serde_json::to_string_pretty(&b).map_err(|e| e.to_string())? + "\n",
        )
        .map_err(|e| format!("{}: {e}", baseline_path.display()))?;
        println!("Baseline written: {}", baseline_path.display());
        return Ok(true);
    }
    let failed = gate_results
        .iter()
        .filter(|g| g.enforced && !g.pass)
        .count();
    if args.report_only || custom_clip {
        println!("Stream gate: REPORT ONLY, {failed} enforced gate(s) outside their limits, not enforced.");
        return Ok(true);
    }
    if failed == 0 {
        println!("Stream gate: PASS");
        Ok(true)
    } else {
        println!("Stream gate: FAIL ({failed} enforced gate(s))");
        Ok(false)
    }
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    match real_main() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("stream-gate: error: {e}");
            ExitCode::from(2)
        }
    }
}
