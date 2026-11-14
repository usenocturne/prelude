//! Watches the USB buses through the kernel's usbmon text interface, to tell
//! whether packets for the phone wait on this side of the Wi-Fi adapter, wait
//! inside it, or are lost on the cable.
//!
//! Only the head of each event is used. The bytes carried by a transfer are
//! never kept or logged.

#![cfg_attr(
    not(all(target_os = "linux", feature = "linux-media")),
    allow(dead_code)
)]

use std::{
    collections::{BTreeMap, HashMap},
    fs::File,
    io::{BufRead, BufReader},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    thread,
};

/// Every bus at once. Root hubs are told apart by their device number.
const MONITOR_PATH: &str = "/sys/kernel/debug/usb/usbmon/0u";
const ROOT_HUB: u16 = 1;
/// usbmon stamps events in microseconds that start over every 4096 seconds.
const TIMESTAMP_SPAN: u64 = 4_096 * 1_000_000;
/// A silence this long, in microseconds, counts as a pause.
const PAUSE: u64 = 100_000;
/// How much must arrive from the adapter during a silence towards it before
/// the silence counts as packets being held rather than nothing to send.
const HOLD_INBOUND_BYTES: u64 = 4_000;
/// Control transfers closer together than this belong to one burst.
const BURST_GAP: u64 = 20_000;
const SAMPLES: usize = 12;
const FAILURES: usize = 8;
const PENDING_LIMIT: usize = 1_024;
const PENDING_AGE: u64 = 5_000_000;
const EXAMPLE_LENGTH: usize = 120;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Submitted,
    Completed,
    Refused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transfer {
    Control,
    Bulk,
    Interrupt,
    Isochronous,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UsbEvent {
    urb: u64,
    micros: u64,
    stage: Stage,
    transfer: Transfer,
    inbound: bool,
    device: u16,
    endpoint: u8,
    /// The transfer's status. Only a completion or a refusal carries a
    /// meaningful one.
    status: Option<i32>,
    length: u64,
}

/// Reads the head of one usbmon `u` format line, such as
/// `ffff 3575914555 S Bo:1:005:2 -115 31 = 55534243`.
fn parse_event(line: &str) -> Option<UsbEvent> {
    let mut fields = line.split_ascii_whitespace();
    let urb = u64::from_str_radix(fields.next()?, 16).ok()?;
    let micros = fields.next()?.parse().ok()?;
    let stage = match fields.next()? {
        "S" => Stage::Submitted,
        "C" => Stage::Completed,
        "E" => Stage::Refused,
        _ => return None,
    };
    let mut address = fields.next()?.split(':');
    let mut kind = address.next()?.chars();
    let transfer = match kind.next()? {
        'C' => Transfer::Control,
        'B' => Transfer::Bulk,
        'I' => Transfer::Interrupt,
        'Z' => Transfer::Isochronous,
        _ => return None,
    };
    let inbound = match kind.next()? {
        'i' => true,
        'o' => false,
        _ => return None,
    };
    let _bus: u16 = address.next()?.parse().ok()?;
    let device = address.next()?.parse().ok()?;
    let endpoint = address.next()?.parse().ok()?;
    let word = fields.next()?;
    let (status, length) = match word.split(':').next()?.parse::<i32>() {
        Ok(status) => (Some(status), fields.next()?),
        // A setup packet stands where the status would: a tag and five
        // fields, then the length.
        Err(_) => (None, fields.nth(5)?),
    };
    Some(UsbEvent {
        urb,
        micros,
        stage,
        transfer,
        inbound,
        device,
        endpoint,
        status,
        length: length.parse().ok()?,
    })
}

/// Microseconds from one usbmon stamp to a later one.
fn elapsed(from: u64, to: u64) -> u64 {
    (to % TIMESTAMP_SPAN + TIMESTAMP_SPAN - from % TIMESTAMP_SPAN) % TIMESTAMP_SPAN
}

/// What crossed the USB buses, other than their root hubs, in one window.
/// Times are in microseconds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct UsbWindow {
    pub(crate) events: u64,
    pub(crate) unparsed: u64,
    /// Transfers the host controller refused to start.
    pub(crate) refused: u64,
    pub(crate) out_submitted: u64,
    pub(crate) out_completed: u64,
    pub(crate) out_failed: u64,
    pub(crate) out_bytes: u64,
    pub(crate) out_latency_total: u64,
    pub(crate) out_latency_maximum: u64,
    pub(crate) out_in_flight_maximum: u64,
    pub(crate) out_gap_maximum: u64,
    /// Pauses in what was handed to the adapter while it kept delivering
    /// data from the air.
    pub(crate) out_holds: u64,
    pub(crate) out_hold_samples: Vec<u64>,
    /// For those pauses, how long after the driver's latest burst of control
    /// transfers began the first packet was handed over again.
    pub(crate) out_resumed_after_control: Vec<u64>,
    pub(crate) in_completed: u64,
    pub(crate) in_failed: u64,
    pub(crate) in_bytes: u64,
    pub(crate) in_gap_maximum: u64,
    pub(crate) in_pauses: u64,
    pub(crate) in_resumed_after_control: Vec<u64>,
    pub(crate) control_transfers: u64,
    pub(crate) control_failed: u64,
    pub(crate) control_latency_maximum: u64,
    pub(crate) control_bursts: u64,
    pub(crate) control_burst_intervals: Vec<u64>,
    pub(crate) control_burst_span_maximum: u64,
    /// The first transfers that failed, as direction, endpoint, device and
    /// status.
    pub(crate) failures: Vec<String>,
    /// Bulk transfers completed with data, by direction and endpoint.
    pub(crate) endpoints: BTreeMap<(bool, u8), u64>,
}

impl UsbWindow {
    pub(crate) fn out_latency_average(&self) -> Option<u64> {
        self.out_latency_total.checked_div(self.out_completed)
    }

    /// Renders the endpoints as `out5:120 in4:300`.
    pub(crate) fn render_endpoints(&self) -> String {
        self.endpoints
            .iter()
            .map(|((inbound, endpoint), transfers)| {
                let direction = if *inbound { "in" } else { "out" };
                format!("{direction}{endpoint}:{transfers}")
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Renders microseconds as milliseconds separated by spaces.
pub(crate) fn render_millis(micros: &[u64]) -> String {
    micros
        .iter()
        .map(|micros| format!("{:.1}", *micros as f64 / 1_000.0))
        .collect::<Vec<_>>()
        .join(" ")
}

fn sample(samples: &mut Vec<u64>, value: u64) {
    if samples.len() < SAMPLES {
        samples.push(value);
    }
}

#[derive(Debug, Default)]
struct Tracker {
    window: UsbWindow,
    /// When each transfer still in flight was submitted, and whether it is a
    /// bulk transfer towards the device.
    pending: HashMap<u64, (u64, bool)>,
    out_in_flight: u64,
    last_out_submit: Option<u64>,
    inbound_bytes_since_out: u64,
    last_inbound: Option<u64>,
    burst_started: Option<u64>,
    last_control_submit: Option<u64>,
    /// The head of the first line seen of each kind, to check the reading
    /// of the format against the real thing.
    examples: BTreeMap<&'static str, String>,
    examples_reported: bool,
}

impl Tracker {
    fn record(&mut self, line: &str) {
        let Some(event) = parse_event(line) else {
            self.window.unparsed = self.window.unparsed.saturating_add(1);
            self.keep_example("unparsed", line);
            return;
        };
        if event.device == ROOT_HUB {
            return;
        }
        self.window.events = self.window.events.saturating_add(1);
        match (event.transfer, event.inbound, event.stage) {
            (_, _, Stage::Refused) => {
                self.window.refused = self.window.refused.saturating_add(1);
                self.keep_failure(&event);
            }
            (Transfer::Bulk, false, Stage::Submitted) => {
                self.keep_example("out", line);
                self.out_submitted(&event);
            }
            (Transfer::Bulk, false, Stage::Completed) => self.out_completed(&event),
            (Transfer::Bulk, true, Stage::Completed) => {
                self.keep_example("in", line);
                self.in_completed(&event);
            }
            (Transfer::Control, _, Stage::Submitted) => {
                self.keep_example("control", line);
                self.control_submitted(&event);
            }
            (Transfer::Control, _, Stage::Completed) => self.control_completed(&event),
            _ => {}
        }
    }

    fn out_submitted(&mut self, event: &UsbEvent) {
        let window = &mut self.window;
        window.out_submitted = window.out_submitted.saturating_add(1);
        if let Some(previous) = self.last_out_submit {
            let gap = elapsed(previous, event.micros);
            window.out_gap_maximum = window.out_gap_maximum.max(gap);
            if gap >= PAUSE && self.inbound_bytes_since_out >= HOLD_INBOUND_BYTES {
                window.out_holds = window.out_holds.saturating_add(1);
                sample(&mut window.out_hold_samples, gap);
                if let Some(burst) = self.burst_started {
                    sample(
                        &mut window.out_resumed_after_control,
                        elapsed(burst, event.micros),
                    );
                }
            }
        }
        self.last_out_submit = Some(event.micros);
        self.inbound_bytes_since_out = 0;
        self.remember(event, true);
        self.out_in_flight = self.out_in_flight.saturating_add(1);
        self.window.out_in_flight_maximum =
            self.window.out_in_flight_maximum.max(self.out_in_flight);
    }

    fn out_completed(&mut self, event: &UsbEvent) {
        let submitted = self.pending.remove(&event.urb);
        self.out_in_flight = self.out_in_flight.saturating_sub(1);
        let window = &mut self.window;
        window.out_completed = window.out_completed.saturating_add(1);
        window.out_bytes = window.out_bytes.saturating_add(event.length);
        if let Some((submitted, _)) = submitted {
            let latency = elapsed(submitted, event.micros);
            window.out_latency_total = window.out_latency_total.saturating_add(latency);
            window.out_latency_maximum = window.out_latency_maximum.max(latency);
        }
        if event.status == Some(0) {
            let transfers = window.endpoints.entry((false, event.endpoint)).or_default();
            *transfers = transfers.saturating_add(1);
        } else {
            window.out_failed = window.out_failed.saturating_add(1);
            self.keep_failure(event);
        }
    }

    fn in_completed(&mut self, event: &UsbEvent) {
        if event.status != Some(0) {
            self.window.in_failed = self.window.in_failed.saturating_add(1);
            self.keep_failure(event);
            return;
        }
        if event.length == 0 {
            return;
        }
        let window = &mut self.window;
        window.in_completed = window.in_completed.saturating_add(1);
        window.in_bytes = window.in_bytes.saturating_add(event.length);
        let transfers = window.endpoints.entry((true, event.endpoint)).or_default();
        *transfers = transfers.saturating_add(1);
        if let Some(previous) = self.last_inbound {
            let gap = elapsed(previous, event.micros);
            window.in_gap_maximum = window.in_gap_maximum.max(gap);
            if gap >= PAUSE {
                window.in_pauses = window.in_pauses.saturating_add(1);
                if let Some(burst) = self.burst_started {
                    sample(
                        &mut window.in_resumed_after_control,
                        elapsed(burst, event.micros),
                    );
                }
            }
        }
        self.last_inbound = Some(event.micros);
        self.inbound_bytes_since_out = self.inbound_bytes_since_out.saturating_add(event.length);
    }

    fn control_submitted(&mut self, event: &UsbEvent) {
        let continues = self
            .last_control_submit
            .is_some_and(|previous| elapsed(previous, event.micros) < BURST_GAP);
        let window = &mut self.window;
        match self.burst_started {
            Some(started) if continues => {
                window.control_burst_span_maximum = window
                    .control_burst_span_maximum
                    .max(elapsed(started, event.micros));
            }
            started => {
                window.control_bursts = window.control_bursts.saturating_add(1);
                if let Some(started) = started {
                    sample(
                        &mut window.control_burst_intervals,
                        elapsed(started, event.micros),
                    );
                }
                self.burst_started = Some(event.micros);
            }
        }
        self.last_control_submit = Some(event.micros);
        self.remember(event, false);
    }

    fn control_completed(&mut self, event: &UsbEvent) {
        let submitted = self.pending.remove(&event.urb);
        let window = &mut self.window;
        window.control_transfers = window.control_transfers.saturating_add(1);
        if let Some((submitted, _)) = submitted {
            window.control_latency_maximum = window
                .control_latency_maximum
                .max(elapsed(submitted, event.micros));
        }
        if event.status != Some(0) {
            window.control_failed = window.control_failed.saturating_add(1);
            self.keep_failure(event);
        }
    }

    /// Notes when a transfer was submitted. Transfers whose completion was
    /// never seen are forgotten once enough of them pile up.
    fn remember(&mut self, event: &UsbEvent, outbound_bulk: bool) {
        if self.pending.len() >= PENDING_LIMIT {
            let now = event.micros;
            self.pending
                .retain(|_, (submitted, _)| elapsed(*submitted, now) < PENDING_AGE);
            self.out_in_flight = self
                .pending
                .values()
                .filter(|(_, outbound_bulk)| *outbound_bulk)
                .count() as u64;
        }
        self.pending
            .insert(event.urb, (event.micros, outbound_bulk));
    }

    fn keep_failure(&mut self, event: &UsbEvent) {
        if self.window.failures.len() >= FAILURES {
            return;
        }
        let transfer = match event.transfer {
            Transfer::Control => "control",
            Transfer::Bulk => "bulk",
            Transfer::Interrupt => "interrupt",
            Transfer::Isochronous => "isochronous",
        };
        let direction = if event.inbound { "in" } else { "out" };
        let status = event
            .status
            .map_or_else(|| "unknown".to_owned(), |status| status.to_string());
        self.window.failures.push(format!(
            "{transfer}-{direction}{} device{} status{status}",
            event.endpoint, event.device
        ));
    }

    /// Keeps the head of a line, without the bytes of the transfer.
    fn keep_example(&mut self, kind: &'static str, line: &str) {
        if self.examples.contains_key(kind) {
            return;
        }
        let head = line.split(" = ").next().unwrap_or(line);
        let head: String = head.chars().take(EXAMPLE_LENGTH).collect();
        self.examples.insert(kind, head);
    }

    fn take_examples(&mut self) -> Option<String> {
        if self.examples_reported || !self.examples.contains_key("in") {
            return None;
        }
        self.examples_reported = true;
        Some(
            self.examples
                .iter()
                .map(|(kind, head)| format!("{kind}: {head}"))
                .collect::<Vec<_>>()
                .join(" | "),
        )
    }
}

static MONITOR: Mutex<Option<Arc<Mutex<Tracker>>>> = Mutex::new(None);

/// Starts watching the USB buses unless that is already under way. Loads
/// the kernel's usbmon module when its files are missing.
pub(crate) fn start() {
    let Ok(mut monitor) = MONITOR.lock() else {
        return;
    };
    if monitor.is_some() {
        return;
    }
    let file = match open() {
        Ok(file) => file,
        Err(error) => {
            tracing::warn!(
                %error,
                path = MONITOR_PATH,
                "cannot watch the USB buses; USB measurements will be missing"
            );
            return;
        }
    };
    let tracker = Arc::new(Mutex::new(Tracker::default()));
    let shared = Arc::clone(&tracker);
    match thread::Builder::new()
        .name("usb-monitor".to_owned())
        .spawn(move || read_events(file, &shared))
    {
        Ok(_) => {
            tracing::info!(path = MONITOR_PATH, "watching the USB buses");
            *monitor = Some(tracker);
        }
        Err(error) => tracing::warn!(%error, "cannot start the USB bus monitor"),
    }
}

fn open() -> std::io::Result<File> {
    File::open(MONITOR_PATH).or_else(|_| {
        for program in ["modprobe", "/usr/sbin/modprobe", "/sbin/modprobe"] {
            let loaded = Command::new(program)
                .arg("usbmon")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success());
            if loaded {
                break;
            }
        }
        File::open(MONITOR_PATH)
    })
}

fn read_events(file: File, tracker: &Mutex<Tracker>) {
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else {
            break;
        };
        let Ok(mut tracker) = tracker.lock() else {
            break;
        };
        tracker.record(&line);
    }
    tracing::warn!("the USB bus monitor stopped");
}

fn with_tracker<T>(action: impl FnOnce(&mut Tracker) -> T) -> Option<T> {
    let tracker = MONITOR.lock().ok()?.clone()?;
    let mut tracker = tracker.lock().ok()?;
    Some(action(&mut tracker))
}

/// Forgets what was counted so far, so the next window starts now.
pub(crate) fn discard_window() {
    let _ = with_tracker(|tracker| tracker.window = UsbWindow::default());
}

/// Takes what was counted since the last call. The second value is, once,
/// the head of the first line of each kind.
pub(crate) fn take_window() -> Option<(UsbWindow, Option<String>)> {
    with_tracker(|tracker| {
        (
            std::mem::take(&mut tracker.window),
            tracker.take_examples(),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(tracker: &mut Tracker, lines: &[String]) {
        for line in lines {
            tracker.record(line);
        }
    }

    fn out(urb: u64, micros: u64, stage: char, status: i32) -> String {
        format!("{urb:016x} {micros} {stage} Bo:1:002:5 {status} 90 >")
    }

    fn inbound(urb: u64, micros: u64, length: u64) -> String {
        format!("{urb:016x} {micros} C Bi:1:002:4 0 {length} = 01020304")
    }

    fn control(urb: u64, micros: u64) -> String {
        format!("{urb:016x} {micros} S Ci:1:002:0 s c0 63 0000 d02c 0004 4 <")
    }

    #[test]
    fn the_heads_of_usbmon_lines_are_read() {
        let submitted =
            parse_event("ffff0000d5ea89a0 3575914555 S Ci:1:001:0 s a3 00 0000 0003 0004 4 <")
                .expect("control submission");
        assert_eq!(submitted.transfer, Transfer::Control);
        assert_eq!(submitted.stage, Stage::Submitted);
        assert!(submitted.inbound);
        assert_eq!((submitted.device, submitted.endpoint), (1, 0));
        assert_eq!((submitted.status, submitted.length), (None, 4));

        let completed = parse_event("d5ea89a0 3575914560 C Ci:1:001:0 0 4 = 01050000")
            .expect("control completion");
        assert_eq!(completed.urb, 0xd5ea_89a0);
        assert_eq!(completed.micros, 3_575_914_560);
        assert_eq!((completed.status, completed.length), (Some(0), 4));

        let bulk = parse_event("dd65f0e8 4128379752 S Bo:1:005:2 -115 31 = 55534243 5e000000")
            .expect("bulk submission");
        assert_eq!(bulk.transfer, Transfer::Bulk);
        assert!(!bulk.inbound);
        assert_eq!((bulk.device, bulk.endpoint), (5, 2));
        assert_eq!((bulk.status, bulk.length), (Some(-115), 31));

        let interrupt = parse_event("c0de 100 C Ii:1:001:1 0:2048 2 = 0400")
            .expect("interrupt completion");
        assert_eq!(interrupt.transfer, Transfer::Interrupt);
        assert_eq!((interrupt.status, interrupt.length), (Some(0), 2));

        let failed = parse_event("c0de 100 C Bi:1:003:4 -71 0").expect("failed transfer");
        assert_eq!(failed.status, Some(-71));

        let uncaptured = parse_event("c0de 100 S Co:1:002:0 Z __ __ ____ ____ ____ 8 >")
            .expect("setup packet not captured");
        assert_eq!((uncaptured.status, uncaptured.length), (None, 8));

        assert_eq!(parse_event("not an event"), None);
        assert_eq!(parse_event(""), None);
    }

    #[test]
    fn stamps_are_compared_across_their_wrap() {
        assert_eq!(elapsed(1_000, 4_000), 3_000);
        assert_eq!(elapsed(TIMESTAMP_SPAN - 500, 250), 750);
    }

    #[test]
    fn a_silence_towards_the_adapter_while_it_delivers_is_a_hold() {
        let mut tracker = Tracker::default();
        let mut lines = vec![out(1, 1_000_000, 'S', -115), out(1, 1_000_300, 'C', 0)];
        // The driver's control burst, then data arriving while nothing is
        // handed to the adapter for 200 ms.
        lines.push(control(9, 1_010_000));
        lines.push(control(9, 1_011_000));
        for arrival in 0..4_u64 {
            lines.push(inbound(2, 1_020_000 + arrival * 40_000, 1_500));
        }
        lines.push(control(9, 1_190_000));
        lines.push(out(3, 1_200_000, 'S', -115));
        // A silence with nothing arriving is not a hold.
        lines.push(out(4, 1_500_000, 'S', -115));
        record(&mut tracker, &lines);

        let window = &tracker.window;
        assert_eq!(window.out_submitted, 3);
        assert_eq!(window.out_completed, 1);
        assert_eq!(window.out_latency_maximum, 300);
        assert_eq!(window.out_in_flight_maximum, 2);
        assert_eq!(window.out_gap_maximum, 300_000);
        assert_eq!(window.out_holds, 1);
        assert_eq!(window.out_hold_samples, vec![200_000]);
        assert_eq!(window.out_resumed_after_control, vec![10_000]);
        assert_eq!(window.in_completed, 4);
        assert_eq!(window.in_bytes, 6_000);
        assert_eq!(window.in_pauses, 0);
        assert_eq!(window.control_bursts, 2);
        assert_eq!(window.control_burst_intervals, vec![180_000]);
        assert_eq!(window.control_burst_span_maximum, 1_000);
        assert_eq!(window.render_endpoints(), "out5:1 in4:4");
        assert_eq!(render_millis(&window.out_hold_samples), "200.0");
    }

    #[test]
    fn failed_transfers_are_counted_and_named() {
        let mut tracker = Tracker::default();
        record(
            &mut tracker,
            &[
                out(1, 1_000, 'S', -115),
                out(1, 2_000, 'C', -71),
                "00000000000000aa 3000 C Bi:1:002:4 -71 0".to_owned(),
                "00000000000000ab 4000 E Bo:1:002:5 -19 0".to_owned(),
                "00000000000000ac 5000 C Ci:1:002:0 -110 0".to_owned(),
                // The root hub is not the adapter.
                "00000000000000ad 6000 C Ii:1:001:1 -71:2048 0".to_owned(),
                "garbage".to_owned(),
            ],
        );
        let window = &tracker.window;
        assert_eq!(window.out_failed, 1);
        assert_eq!(window.in_failed, 1);
        assert_eq!(window.control_failed, 1);
        assert_eq!(window.refused, 1);
        assert_eq!(window.unparsed, 1);
        assert_eq!(window.events, 5);
        assert_eq!(
            window.failures,
            vec![
                "bulk-out5 device2 status-71",
                "bulk-in4 device2 status-71",
                "bulk-out5 device2 status-19",
                "control-in0 device2 status-110",
            ]
        );
    }

    #[test]
    fn examples_leave_out_the_bytes_and_are_given_once() {
        let mut tracker = Tracker::default();
        assert_eq!(tracker.take_examples(), None);
        record(
            &mut tracker,
            &[out(1, 1_000, 'S', -115), inbound(2, 2_000, 64), inbound(3, 3_000, 64)],
        );
        let examples = tracker.take_examples().expect("examples");
        assert_eq!(
            examples,
            "in: 0000000000000002 2000 C Bi:1:002:4 0 64 | out: 0000000000000001 1000 S Bo:1:002:5 -115 90 >"
        );
        assert_eq!(tracker.take_examples(), None);
    }
}
