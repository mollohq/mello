//! The impairment shim: a UDP proxy between the clients and the SFU.
//!
//! Placement. The SFU runs with `SFU_PUBLIC_IP` set to the shim address
//! (127.0.0.2), so its ICE-lite answer carries the one host candidate
//! `127.0.0.2:10000`. The SFU itself listens on `0.0.0.0:10000`; the shim binds
//! the more specific `127.0.0.2:10000` (Windows gives a datagram to the most
//! specific bind). An ICE-lite SFU never sends a connectivity check, so every
//! datagram of a client, STUN, DTLS, SRTP and SRTCP, goes to the shim. The
//! shim opens one upstream socket per client (127.0.0.1:ephemeral to
//! 127.0.0.1:10000), so the SFU sees one peer-reflexive address per client
//! and answers to it. libmello and the SFU are unchanged.
//!
//! Each client flow is one leg: the harness names the next flow before the
//! client connects (`expect`). The forward direction of the host leg is
//! host to SFU; the forward direction of a viewer leg is SFU to viewer.
//! `netmodel.rs` decides the fate of every datagram.
//!
//! SRTP keeps the RTP header in the clear (RFC 3711), so the shim reads the
//! payload type, the sequence number and the SSRC of every RTP datagram:
//! it counts video, FEC (PT 127), audio, retransmissions (an SSRC and
//! sequence number seen before in the same direction) and RTCP, and bins the
//! bytes per 100 ms. That is the wire, as a router would see it.

use crate::netmodel::{Direction, Fate, Leg};
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub const H264_PT: u8 = 96;
pub const FEC_PT: u8 = 127;
pub const OPUS_PT: u8 = 111;
/// Width of one wire bin.
pub const BIN_US: u64 = 100_000;

/// What a datagram is (RFC 7983 demultiplexing).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Stun,
    Dtls,
    Rtp { pt: u8, seq: u16, ssrc: u32 },
    Rtcp { pt: u8 },
    Other,
}

pub fn classify(p: &[u8]) -> Kind {
    let Some(&b0) = p.first() else {
        return Kind::Other;
    };
    match b0 {
        0..=3 => Kind::Stun,
        20..=63 => Kind::Dtls,
        128..=191 if p.len() >= 12 => {
            let b1 = p[1];
            if (192..=223).contains(&b1) {
                Kind::Rtcp { pt: b1 }
            } else {
                Kind::Rtp {
                    pt: b1 & 0x7f,
                    seq: u16::from_be_bytes([p[2], p[3]]),
                    ssrc: u32::from_be_bytes([p[8], p[9], p[10], p[11]]),
                }
            }
        }
        128..=191 if p.len() >= 2 && (192..=223).contains(&p[1]) => Kind::Rtcp { pt: p[1] },
        _ => Kind::Other,
    }
}

/// Wire counts of one direction of one flow.
#[derive(Clone, Debug, Default)]
pub struct Wire {
    /// Every datagram the sender handed to the network.
    pub offered_packets: u64,
    pub offered_bytes: u64,
    pub video_packets: u64,
    pub video_bytes: u64,
    /// RTP video datagrams whose (SSRC, sequence) this direction saw before.
    pub rtx_packets: u64,
    pub rtx_bytes: u64,
    pub fec_packets: u64,
    pub fec_bytes: u64,
    pub audio_packets: u64,
    pub audio_bytes: u64,
    pub rtcp_packets: u64,
    pub rtcp_bytes: u64,
    pub delivered_packets: u64,
    pub delivered_bytes: u64,
    /// Bytes offered and delivered per 100 ms bin since the arm time
    /// (UDP payload; the cap adds 28 bytes of headers per datagram).
    pub offered_bins: Vec<u64>,
    pub delivered_bins: Vec<u64>,
    /// Video, FEC and retransmission bytes offered per bin.
    pub media_bins: Vec<u64>,
    /// Datagrams offered per bin (for the per-datagram header overhead).
    pub offered_pkt_bins: Vec<u64>,
    seen: HashSet<(u32, u16)>,
}

fn bin_add(bins: &mut Vec<u64>, t_us: u64, v: u64) {
    let i = (t_us / BIN_US) as usize;
    if bins.len() <= i {
        bins.resize(i + 1, 0);
    }
    bins[i] += v;
}

impl Wire {
    fn offer(&mut self, p: &[u8], since_arm_us: Option<u64>) {
        let n = p.len() as u64;
        self.offered_packets += 1;
        self.offered_bytes += n;
        let mut media = false;
        match classify(p) {
            Kind::Rtp { pt, seq, ssrc } if pt == H264_PT => {
                media = true;
                if self.seen.insert((ssrc, seq)) {
                    self.video_packets += 1;
                    self.video_bytes += n;
                } else {
                    self.rtx_packets += 1;
                    self.rtx_bytes += n;
                }
            }
            Kind::Rtp { pt, .. } if pt == FEC_PT => {
                media = true;
                self.fec_packets += 1;
                self.fec_bytes += n;
            }
            Kind::Rtp { pt, .. } if pt == OPUS_PT => {
                self.audio_packets += 1;
                self.audio_bytes += n;
            }
            Kind::Rtcp { .. } => {
                self.rtcp_packets += 1;
                self.rtcp_bytes += n;
            }
            _ => {}
        }
        if let Some(t) = since_arm_us {
            bin_add(&mut self.offered_bins, t, n);
            bin_add(&mut self.offered_pkt_bins, t, 1);
            if media {
                bin_add(&mut self.media_bins, t, n);
            }
        }
    }

    fn deliver(&mut self, n: usize, since_arm_us: Option<u64>) {
        self.delivered_packets += 1;
        self.delivered_bytes += n as u64;
        if let Some(t) = since_arm_us {
            bin_add(&mut self.delivered_bins, t, n as u64);
        }
    }
}

/// One client flow: a leg with its two directions.
pub struct FlowReport {
    pub role: String,
    /// Client to SFU.
    pub up: Wire,
    pub up_counters: crate::netmodel::Counters,
    /// SFU to client.
    pub down: Wire,
    pub down_counters: crate::netmodel::Counters,
}

struct Flow {
    role: String,
    client: SocketAddr,
    upstream: Arc<UdpSocket>,
    /// Client to SFU and SFU to client. Which one is the forward direction
    /// of the leg depends on the role.
    up: Direction,
    down: Direction,
    up_wire: Wire,
    down_wire: Wire,
}

/// The legs of one profile run.
#[derive(Clone, Debug)]
pub struct Plan {
    pub seed: u64,
    pub duration_s: f64,
    pub host: Leg,
    pub viewer: Leg,
}

enum Target {
    Upstream(Arc<UdpSocket>),
    Client(SocketAddr),
}

struct Scheduled {
    at_us: u64,
    order: u64,
    bytes: Vec<u8>,
    target: Target,
    flow: SocketAddr,
    up: bool,
}

impl PartialEq for Scheduled {
    fn eq(&self, o: &Self) -> bool {
        (self.at_us, self.order) == (o.at_us, o.order)
    }
}
impl Eq for Scheduled {}
impl PartialOrd for Scheduled {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Scheduled {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        (self.at_us, self.order).cmp(&(o.at_us, o.order))
    }
}

struct State {
    plan: Option<Plan>,
    armed_us: Option<u64>,
    expect: VecDeque<String>,
    flows: HashMap<SocketAddr, Flow>,
    order: u64,
}

struct Inner {
    listen: Arc<UdpSocket>,
    upstream: SocketAddr,
    epoch: Instant,
    state: Mutex<State>,
    queue: Mutex<BinaryHeap<Reverse<Scheduled>>>,
    queue_cv: Condvar,
    stop: AtomicBool,
}

impl Inner {
    fn now_us(&self) -> u64 {
        self.epoch.elapsed().as_micros() as u64
    }
}

/// The running proxy. Dropping it stops its threads.
pub struct Shim {
    inner: Arc<Inner>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Shim {
    /// Bind `listen` (the address the SFU advertises) and forward to
    /// `upstream` (the SFU's UDP mux).
    pub fn start(listen: SocketAddr, upstream: SocketAddr) -> Result<Self, String> {
        let sock = UdpSocket::bind(listen).map_err(|e| format!("shim bind {listen}: {e}"))?;
        sock.set_read_timeout(Some(Duration::from_millis(100)))
            .map_err(|e| e.to_string())?;
        let inner = Arc::new(Inner {
            listen: Arc::new(sock),
            upstream,
            epoch: Instant::now(),
            state: Mutex::new(State {
                plan: None,
                armed_us: None,
                expect: VecDeque::new(),
                flows: HashMap::new(),
                order: 0,
            }),
            queue: Mutex::new(BinaryHeap::new()),
            queue_cv: Condvar::new(),
            stop: AtomicBool::new(false),
        });
        let mut threads = Vec::new();
        let i = Arc::clone(&inner);
        threads.push(
            std::thread::Builder::new()
                .name("shim-listen".into())
                .spawn(move || listen_loop(i))
                .map_err(|e| e.to_string())?,
        );
        let i = Arc::clone(&inner);
        threads.push(
            std::thread::Builder::new()
                .name("shim-send".into())
                .spawn(move || send_loop(i))
                .map_err(|e| e.to_string())?,
        );
        Ok(Self { inner, threads })
    }

    /// Start a profile run: forget the flows of the last one, take the legs,
    /// disarm.
    pub fn begin(&self, plan: Plan) {
        let mut s = lock(&self.inner.state);
        s.flows.clear();
        s.expect.clear();
        s.plan = Some(plan);
        s.armed_us = None;
    }

    /// The next new client flow belongs to `role` ("host", "viewer1",
    /// "viewer2"). A role that starts with "host" takes the host leg; any
    /// other role takes the viewer leg.
    pub fn expect(&self, role: &str) {
        lock(&self.inner.state).expect.push_back(role.to_owned());
    }

    /// Profile time 0: impairments start now.
    pub fn arm(&self) {
        let now = self.inner.now_us();
        lock(&self.inner.state).armed_us = Some(now);
    }

    /// Microseconds since the arm time, or None before.
    pub fn since_arm_us(&self) -> Option<u64> {
        let s = lock(&self.inner.state);
        s.armed_us.map(|a| self.inner.now_us().saturating_sub(a))
    }

    /// The wire counts of every flow of this run, by role.
    pub fn report(&self) -> Vec<FlowReport> {
        let s = lock(&self.inner.state);
        let mut out: Vec<FlowReport> = s
            .flows
            .values()
            .map(|f| FlowReport {
                role: f.role.clone(),
                up: f.up_wire.clone(),
                up_counters: f.up.counters.clone(),
                down: f.down_wire.clone(),
                down_counters: f.down.counters.clone(),
            })
            .collect();
        out.sort_by(|a, b| a.role.cmp(&b.role));
        out
    }
}

impl Drop for Shim {
    fn drop(&mut self) {
        self.inner.stop.store(true, Ordering::Relaxed);
        self.inner.queue_cv.notify_all();
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Take one datagram into the model and the queue. `up`: client to SFU.
fn admit(inner: &Arc<Inner>, client: SocketAddr, up: bool, bytes: Vec<u8>) {
    let now = inner.now_us();
    let mut s = lock(&inner.state);
    let armed = s.armed_us;
    let since = armed.map(|a| now.saturating_sub(a));
    let order = {
        s.order += 1;
        s.order
    };
    let Some(flow) = s.flows.get_mut(&client) else {
        return;
    };
    let (wire, dir) = if up {
        (&mut flow.up_wire, &mut flow.up)
    } else {
        (&mut flow.down_wire, &mut flow.down)
    };
    wire.offer(&bytes, since);
    let fate = dir.send(now, armed, bytes.len());
    let Fate::Deliver(at_us) = fate else {
        return;
    };
    let target = if up {
        Target::Upstream(Arc::clone(&flow.upstream))
    } else {
        Target::Client(flow.client)
    };
    drop(s);
    lock(&inner.queue).push(Reverse(Scheduled {
        at_us,
        order,
        bytes,
        target,
        flow: client,
        up,
    }));
    inner.queue_cv.notify_one();
}

fn new_flow(inner: &Arc<Inner>, client: SocketAddr) -> Option<Arc<UdpSocket>> {
    let mut s = lock(&inner.state);
    if s.flows.contains_key(&client) {
        return None;
    }
    let plan = s.plan.clone()?;
    let role = s
        .expect
        .pop_front()
        .unwrap_or_else(|| format!("extra{}", s.flows.len()));
    let is_host = role.starts_with("host");
    let leg = if is_host { &plan.host } else { &plan.viewer };
    let upstream = match UdpSocket::bind("127.0.0.1:0").and_then(|u| {
        u.connect(inner.upstream)?;
        u.set_read_timeout(Some(Duration::from_millis(100)))?;
        Ok(u)
    }) {
        Ok(u) => Arc::new(u),
        Err(e) => {
            log::error!("shim: upstream socket for {client}: {e}");
            return None;
        }
    };
    // Host leg: forward is up (host to SFU). Viewer leg: forward is down.
    let up = Direction::new(
        leg,
        is_host,
        plan.seed,
        &format!("{role}/up"),
        plan.duration_s,
    );
    let down = Direction::new(
        leg,
        !is_host,
        plan.seed,
        &format!("{role}/down"),
        plan.duration_s,
    );
    log::info!("shim: flow {client} is {role}");
    s.flows.insert(
        client,
        Flow {
            role,
            client,
            upstream: Arc::clone(&upstream),
            up,
            down,
            up_wire: Wire::default(),
            down_wire: Wire::default(),
        },
    );
    Some(upstream)
}

fn listen_loop(inner: Arc<Inner>) {
    let mut buf = vec![0u8; 65536];
    while !inner.stop.load(Ordering::Relaxed) {
        let (n, from) = match inner.listen.recv_from(&mut buf) {
            Ok(v) => v,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::ConnectionReset
                ) =>
            {
                continue;
            }
            Err(e) => {
                log::warn!("shim: listen recv: {e}");
                continue;
            }
        };
        if let Some(upstream) = new_flow(&inner, from) {
            let i = Arc::clone(&inner);
            let _ = std::thread::Builder::new()
                .name("shim-upstream".into())
                .spawn(move || upstream_loop(i, from, upstream));
        }
        admit(&inner, from, true, buf[..n].to_vec());
    }
}

fn upstream_loop(inner: Arc<Inner>, client: SocketAddr, sock: Arc<UdpSocket>) {
    let mut buf = vec![0u8; 65536];
    while !inner.stop.load(Ordering::Relaxed) {
        // The flow ends with the run (begin() clears it).
        if !lock(&inner.state).flows.contains_key(&client) {
            return;
        }
        match sock.recv(&mut buf) {
            Ok(n) => admit(&inner, client, false, buf[..n].to_vec()),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::ConnectionReset
                ) => {}
            Err(e) => {
                log::warn!("shim: upstream recv for {client}: {e}");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

fn send_loop(inner: Arc<Inner>) {
    loop {
        let item = {
            let mut q = lock(&inner.queue);
            loop {
                if inner.stop.load(Ordering::Relaxed) {
                    return;
                }
                let now = inner.now_us();
                match q.peek() {
                    Some(Reverse(top)) if top.at_us <= now => {
                        break q.pop().map(|Reverse(s)| s);
                    }
                    Some(Reverse(top)) => {
                        let wait = top.at_us - now;
                        if wait < 1500 {
                            // Under the timer resolution: yield instead.
                            drop(q);
                            std::thread::yield_now();
                            q = lock(&inner.queue);
                        } else {
                            q = inner
                                .queue_cv
                                .wait_timeout(q, Duration::from_micros(wait - 1000))
                                .map(|(g, _)| g)
                                .unwrap_or_else(|p| p.into_inner().0);
                        }
                    }
                    None => {
                        q = inner
                            .queue_cv
                            .wait_timeout(q, Duration::from_millis(50))
                            .map(|(g, _)| g)
                            .unwrap_or_else(|p| p.into_inner().0);
                    }
                }
            }
        };
        let Some(item) = item else { continue };
        let sent = match &item.target {
            Target::Upstream(sock) => sock.send(&item.bytes),
            Target::Client(addr) => inner.listen.send_to(&item.bytes, addr),
        };
        if sent.is_ok() {
            let now = inner.now_us();
            let mut s = lock(&inner.state);
            let since = s.armed_us.map(|a| now.saturating_sub(a));
            if let Some(flow) = s.flows.get_mut(&item.flow) {
                let wire = if item.up {
                    &mut flow.up_wire
                } else {
                    &mut flow.down_wire
                };
                wire.deliver(item.bytes.len(), since);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_follows_rfc_7983() {
        assert_eq!(classify(&[0x00, 0x01, 0, 0]), Kind::Stun);
        assert_eq!(classify(&[22, 0xfe, 0xfd]), Kind::Dtls);
        let mut rtp = vec![
            0x80,
            0x80 | 96,
            0x12,
            0x34,
            0,
            0,
            0,
            1,
            0xAA,
            0xBB,
            0xCC,
            0xDD,
        ];
        rtp.extend_from_slice(&[0; 20]);
        assert_eq!(
            classify(&rtp),
            Kind::Rtp {
                pt: 96,
                seq: 0x1234,
                ssrc: 0xAABB_CCDD
            }
        );
        let rtcp = [0x81, 201, 0, 7, 0, 0, 0, 1, 0, 0, 0, 2];
        assert_eq!(classify(&rtcp), Kind::Rtcp { pt: 201 });
    }

    #[test]
    fn a_repeated_sequence_counts_as_retransmission() {
        let mut w = Wire::default();
        let pkt = |seq: u16| {
            let mut p = vec![
                0x80,
                96,
                (seq >> 8) as u8,
                seq as u8,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                9,
            ];
            p.extend_from_slice(&[0; 100]);
            p
        };
        w.offer(&pkt(1), Some(0));
        w.offer(&pkt(2), Some(50_000));
        w.offer(&pkt(1), Some(150_000));
        assert_eq!((w.video_packets, w.rtx_packets), (2, 1));
        assert_eq!(w.offered_bins, vec![224, 112]);
        assert_eq!(w.media_bins, vec![224, 112]);
    }

    #[test]
    fn the_proxy_forwards_both_ways_with_the_base_delay() {
        // A fake SFU on loopback and a client; the shim between them.
        let sfu = UdpSocket::bind("127.0.0.1:0").expect("sfu");
        sfu.set_read_timeout(Some(Duration::from_secs(2)))
            .expect("timeout");
        let listen: SocketAddr = "127.0.0.1:0".parse().expect("addr");
        let shim = Shim::start(listen, sfu.local_addr().expect("addr")).expect("shim");
        let shim_addr = shim.inner.listen.local_addr().expect("addr");
        shim.begin(Plan {
            seed: 1,
            duration_s: 5.0,
            host: Leg {
                base_delay_ms: 20.0,
                ..Default::default()
            },
            viewer: Leg::default(),
        });
        shim.expect("host");
        let client = UdpSocket::bind("127.0.0.1:0").expect("client");
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("timeout");
        let t0 = Instant::now();
        client.send_to(b"\x00\x01ping", shim_addr).expect("send");
        let mut buf = [0u8; 64];
        let (n, from) = sfu.recv_from(&mut buf).expect("sfu receives");
        assert_eq!(&buf[..n], b"\x00\x01ping");
        assert!(
            t0.elapsed() >= Duration::from_millis(19),
            "{:?}",
            t0.elapsed()
        );
        sfu.send_to(b"\x00\x01pong", from).expect("reply");
        let (n, _) = client.recv_from(&mut buf).expect("client receives");
        assert_eq!(&buf[..n], b"\x00\x01pong");
        let r = shim.report();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].role, "host");
        assert_eq!((r[0].up.offered_packets, r[0].down.offered_packets), (1, 1));
    }
}
