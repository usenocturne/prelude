#![cfg_attr(
    not(all(target_os = "linux", feature = "linux-media")),
    allow(dead_code)
)]

use std::time::{Duration, Instant};

pub(crate) const GAP_BUCKET_BOUNDS_MS: [u64; 6] = [20, 40, 70, 120, 250, 1_000];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct GapHistogram([u64; GAP_BUCKET_BOUNDS_MS.len() + 1]);

impl GapHistogram {
    fn record(&mut self, gap_micros: u64) {
        let bucket = GAP_BUCKET_BOUNDS_MS
            .iter()
            .position(|bound_ms| gap_micros < bound_ms * 1_000)
            .unwrap_or(GAP_BUCKET_BOUNDS_MS.len());
        self.0[bucket] = self.0[bucket].saturating_add(1);
    }

    pub(crate) fn render(&self) -> String {
        let mut rendered = String::new();
        for (index, count) in self.0.iter().enumerate() {
            if index != 0 {
                rendered.push(' ');
            }
            match GAP_BUCKET_BOUNDS_MS.get(index) {
                Some(bound) => rendered.push_str(&format!("<{bound}:{count}")),
                None => rendered.push_str(&format!(
                    ">={}:{count}",
                    GAP_BUCKET_BOUNDS_MS[GAP_BUCKET_BOUNDS_MS.len() - 1]
                )),
            }
        }
        rendered
    }
}

const PAUSE_THRESHOLD: Duration = Duration::from_millis(80);
const MAXIMUM_PAUSE_INTERVALS: usize = 64;

#[derive(Debug, Clone, Copy)]
pub(crate) struct PauseIntervals {
    millis: [u16; MAXIMUM_PAUSE_INTERVALS],
    recorded: usize,
    pub(crate) pauses: u64,
    last_resumption: Option<Instant>,
}

impl Default for PauseIntervals {
    fn default() -> Self {
        Self {
            millis: [0; MAXIMUM_PAUSE_INTERVALS],
            recorded: 0,
            pauses: 0,
            last_resumption: None,
        }
    }
}

impl PauseIntervals {
    fn record(&mut self, arrived_at: Instant, gap: Duration) {
        if gap < PAUSE_THRESHOLD {
            return;
        }
        self.pauses = self.pauses.saturating_add(1);
        if let Some(last) = self.last_resumption
            && self.recorded < MAXIMUM_PAUSE_INTERVALS
        {
            let interval = arrived_at.saturating_duration_since(last).as_millis();
            self.millis[self.recorded] = u16::try_from(interval).unwrap_or(u16::MAX);
            self.recorded += 1;
        }
        self.last_resumption = Some(arrived_at);
    }

    pub(crate) fn render(&self) -> String {
        self.millis[..self.recorded]
            .iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct VideoArrivalWindow {
    pub(crate) frames: u64,
    pub(crate) timestamped_frames: u64,
    pub(crate) bytes: u64,
    pub(crate) largest_frame_bytes: u64,
    first_arrival: Option<Instant>,
    last_arrival: Option<Instant>,
    pub(crate) maximum_arrival_gap: Duration,
    pub(crate) arrival_gaps: GapHistogram,
    pub(crate) pause_intervals: PauseIntervals,
    first_phone_micros: Option<u64>,
    last_phone_micros: Option<u64>,
    pub(crate) maximum_phone_gap_micros: u64,
    pub(crate) phone_gaps: GapHistogram,
    anchor: Option<(Instant, u64)>,
    minimum_skew_micros: Option<i64>,
    maximum_skew_micros: Option<i64>,
    session_minimum_skew_micros: Option<i64>,
}

impl VideoArrivalWindow {
    pub(crate) fn record(&mut self, arrived_at: Instant, phone_micros: Option<u64>, bytes: usize) {
        self.frames = self.frames.saturating_add(1);
        self.bytes = self.bytes.saturating_add(bytes as u64);
        self.largest_frame_bytes = self.largest_frame_bytes.max(bytes as u64);
        if let Some(last) = self.last_arrival {
            let gap = arrived_at.saturating_duration_since(last);
            self.maximum_arrival_gap = self.maximum_arrival_gap.max(gap);
            self.arrival_gaps
                .record(u64::try_from(gap.as_micros()).unwrap_or(u64::MAX));
            self.pause_intervals.record(arrived_at, gap);
        }
        self.first_arrival.get_or_insert(arrived_at);
        self.last_arrival = Some(arrived_at);
        let Some(phone_micros) = phone_micros else {
            return;
        };
        self.timestamped_frames = self.timestamped_frames.saturating_add(1);
        if let Some(last) = self.last_phone_micros {
            let gap = phone_micros.saturating_sub(last);
            self.maximum_phone_gap_micros = self.maximum_phone_gap_micros.max(gap);
            self.phone_gaps.record(gap);
        }
        self.first_phone_micros.get_or_insert(phone_micros);
        self.last_phone_micros = Some(phone_micros);
        let (anchor_arrival, anchor_phone) = *self.anchor.get_or_insert((arrived_at, phone_micros));
        let local = i64::try_from(
            arrived_at
                .saturating_duration_since(anchor_arrival)
                .as_micros(),
        )
        .unwrap_or(i64::MAX);
        let phone = i64::try_from(phone_micros.saturating_sub(anchor_phone)).unwrap_or(i64::MAX);
        let skew = local.saturating_sub(phone);
        self.minimum_skew_micros = Some(self.minimum_skew_micros.map_or(skew, |v| v.min(skew)));
        self.maximum_skew_micros = Some(self.maximum_skew_micros.map_or(skew, |v| v.max(skew)));
        self.session_minimum_skew_micros = Some(
            self.session_minimum_skew_micros
                .map_or(skew, |v| v.min(skew)),
        );
    }

    pub(crate) fn arrival_span(&self) -> Option<Duration> {
        Some(
            self.last_arrival?
                .saturating_duration_since(self.first_arrival?),
        )
    }

    pub(crate) fn phone_span_micros(&self) -> Option<u64> {
        Some(
            self.last_phone_micros?
                .saturating_sub(self.first_phone_micros?),
        )
    }

    pub(crate) fn minimum_delay_over_floor_micros(&self) -> Option<i64> {
        Some(
            self.minimum_skew_micros?
                .saturating_sub(self.session_minimum_skew_micros?),
        )
    }

    pub(crate) fn maximum_delay_over_floor_micros(&self) -> Option<i64> {
        Some(
            self.maximum_skew_micros?
                .saturating_sub(self.session_minimum_skew_micros?),
        )
    }

    pub(crate) fn restart(&mut self) {
        *self = Self {
            last_arrival: self.last_arrival,
            pause_intervals: PauseIntervals {
                last_resumption: self.pause_intervals.last_resumption,
                ..PauseIntervals::default()
            },
            last_phone_micros: self.last_phone_micros,
            anchor: self.anchor,
            session_minimum_skew_micros: self.session_minimum_skew_micros,
            ..Self::default()
        };
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct LoopWindow {
    pub(crate) iterations: u64,
    pub(crate) transport_wait: Duration,
    pub(crate) maximum_transport_wait: Duration,
    pub(crate) input_wait: Duration,
    pub(crate) maximum_input_wait: Duration,
}

impl LoopWindow {
    pub(crate) fn record_transport_wait(&mut self, waited: Duration) {
        self.iterations = self.iterations.saturating_add(1);
        self.transport_wait += waited;
        self.maximum_transport_wait = self.maximum_transport_wait.max(waited);
    }

    pub(crate) fn record_input_wait(&mut self, waited: Duration) {
        self.input_wait += waited;
        self.maximum_input_wait = self.maximum_input_wait.max(waited);
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct TcpCounters {
    pub(crate) segments_received: u64,
    pub(crate) segments_sent: u64,
    pub(crate) segments_retransmitted: u64,
}

struct CounterTable<'a> {
    names: Vec<&'a str>,
    values: Vec<&'a str>,
}

impl<'a> CounterTable<'a> {
    fn find(table: &'a str, prefix: &str) -> Option<Self> {
        let mut lines = table.lines().filter_map(|line| line.strip_prefix(prefix));
        Some(Self {
            names: lines.next()?.split_whitespace().collect(),
            values: lines.next()?.split_whitespace().collect(),
        })
    }

    fn column(&self, name: &str) -> Option<u64> {
        let index = self.names.iter().position(|candidate| *candidate == name)?;
        self.values.get(index)?.parse().ok()
    }
}

impl TcpCounters {
    pub(crate) fn read() -> Option<Self> {
        Self::parse(&std::fs::read_to_string("/proc/net/snmp").ok()?)
    }

    fn parse(table: &str) -> Option<Self> {
        let table = CounterTable::find(table, "Tcp:")?;
        Some(Self {
            segments_received: table.column("InSegs")?,
            segments_sent: table.column("OutSegs")?,
            segments_retransmitted: table.column("RetransSegs")?,
        })
    }

    pub(crate) fn since(self, earlier: Self) -> Self {
        Self {
            segments_received: self
                .segments_received
                .saturating_sub(earlier.segments_received),
            segments_sent: self.segments_sent.saturating_sub(earlier.segments_sent),
            segments_retransmitted: self
                .segments_retransmitted
                .saturating_sub(earlier.segments_retransmitted),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct TcpRecoveryCounters {
    pub(crate) timeouts: u64,
    pub(crate) loss_probes: u64,
    pub(crate) fast_retransmits: u64,
    pub(crate) needless_retransmits_reported: u64,
    pub(crate) duplicates_received: u64,
    pub(crate) out_of_order_received: u64,
    pub(crate) delayed_acknowledgements: u64,
}

impl TcpRecoveryCounters {
    pub(crate) fn read() -> Option<Self> {
        Self::parse(&std::fs::read_to_string("/proc/net/netstat").ok()?)
    }

    fn parse(table: &str) -> Option<Self> {
        let table = CounterTable::find(table, "TcpExt:")?;
        Some(Self {
            timeouts: table.column("TCPTimeouts")?,
            loss_probes: table.column("TCPLossProbes")?,
            fast_retransmits: table.column("TCPFastRetrans")?,
            needless_retransmits_reported: table.column("TCPDSACKRecv")?,
            duplicates_received: table
                .column("TCPDSACKOldSent")?
                .saturating_add(table.column("TCPDSACKOfoSent")?),
            out_of_order_received: table.column("TCPOFOQueue")?,
            delayed_acknowledgements: table.column("DelayedACKs")?,
        })
    }

    pub(crate) fn since(self, earlier: Self) -> Self {
        Self {
            timeouts: self.timeouts.saturating_sub(earlier.timeouts),
            loss_probes: self.loss_probes.saturating_sub(earlier.loss_probes),
            fast_retransmits: self
                .fast_retransmits
                .saturating_sub(earlier.fast_retransmits),
            needless_retransmits_reported: self
                .needless_retransmits_reported
                .saturating_sub(earlier.needless_retransmits_reported),
            duplicates_received: self
                .duplicates_received
                .saturating_sub(earlier.duplicates_received),
            out_of_order_received: self
                .out_of_order_received
                .saturating_sub(earlier.out_of_order_received),
            delayed_acknowledgements: self
                .delayed_acknowledgements
                .saturating_sub(earlier.delayed_acknowledgements),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SecondCounts {
    previous: Option<(Instant, TcpCounters)>,
    received: Vec<u64>,
    retransmitted: Vec<u64>,
    retransmission_timeouts: Vec<u64>,
    unacknowledged: Vec<u64>,
    backoffs: Vec<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ConnectionState {
    pub(crate) retransmission_timeout_ms: u64,
    pub(crate) unacknowledged_bytes: u64,
    pub(crate) backoffs: u64,
}

impl ConnectionState {
    const TICK_MS: u64 = 10;

    pub(crate) fn read(local_port: u16, remote_port: u16) -> Option<Self> {
        ["/proc/net/tcp", "/proc/net/tcp6"]
            .iter()
            .filter_map(|table| std::fs::read_to_string(table).ok())
            .find_map(|table| Self::parse(&table, local_port, remote_port))
    }

    fn parse(table: &str, local_port: u16, remote_port: u16) -> Option<Self> {
        let port = |address: &str| {
            let (_, port) = address.rsplit_once(':')?;
            u16::from_str_radix(port, 16).ok()
        };
        table.lines().find_map(|line| {
            let fields: Vec<&str> = line.split_ascii_whitespace().collect();
            if port(fields.get(1)?) != Some(local_port) || port(fields.get(2)?) != Some(remote_port)
            {
                return None;
            }
            let (unacknowledged, _) = fields.get(4)?.split_once(':')?;
            Some(Self {
                retransmission_timeout_ms: fields
                    .get(12)?
                    .parse::<u64>()
                    .ok()?
                    .saturating_mul(Self::TICK_MS),
                unacknowledged_bytes: u64::from_str_radix(unacknowledged, 16).ok()?,
                backoffs: u64::from_str_radix(fields.get(6)?, 16).ok()?,
            })
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SecondSeries {
    pub(crate) received: String,
    pub(crate) retransmitted: String,
    pub(crate) retransmission_timeouts_ms: String,
    pub(crate) unacknowledged_bytes: String,
    pub(crate) backoffs: String,
}

impl SecondCounts {
    const INTERVAL: Duration = Duration::from_secs(1);
    const LIMIT: usize = 30;

    pub(crate) fn due(&self, now: Instant) -> bool {
        self.previous
            .is_none_or(|(at, _)| now.saturating_duration_since(at) >= Self::INTERVAL)
    }

    pub(crate) fn record(
        &mut self,
        now: Instant,
        counters: TcpCounters,
        connection: Option<ConnectionState>,
    ) {
        if let Some((_, previous)) = self.previous
            && self.received.len() < Self::LIMIT
        {
            let moved = counters.since(previous);
            self.received.push(moved.segments_received);
            self.retransmitted.push(moved.segments_retransmitted);
            if let Some(connection) = connection {
                self.retransmission_timeouts
                    .push(connection.retransmission_timeout_ms);
                self.unacknowledged.push(connection.unacknowledged_bytes);
                self.backoffs.push(connection.backoffs);
            }
        }
        self.previous = Some((now, counters));
    }

    pub(crate) fn take(&mut self) -> SecondSeries {
        let series = SecondSeries {
            received: render_list(&self.received),
            retransmitted: render_list(&self.retransmitted),
            retransmission_timeouts_ms: render_list(&self.retransmission_timeouts),
            unacknowledged_bytes: render_list(&self.unacknowledged),
            backoffs: render_list(&self.backoffs),
        };
        self.received.clear();
        self.retransmitted.clear();
        self.retransmission_timeouts.clear();
        self.unacknowledged.clear();
        self.backoffs.clear();
        series
    }
}

pub(crate) fn render_list<T: ToString>(values: &[T]) -> String {
    values
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn hexadecimal(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(start: Instant, millis: u64) -> Instant {
        start + Duration::from_millis(millis)
    }

    #[test]
    fn tcp_counters_follow_the_named_columns_and_subtract() {
        let table = "Ip: Forwarding DefaultTTL\nIp: 1 64\n\
            Tcp: RtoAlgorithm RtoMin InSegs OutSegs RetransSegs InErrs\n\
            Tcp: 1 200 900 700 12 0\nUdp: InDatagrams\nUdp: 4\n";
        let now = TcpCounters::parse(table).expect("counters");
        assert_eq!(
            now,
            TcpCounters {
                segments_received: 900,
                segments_sent: 700,
                segments_retransmitted: 12,
            }
        );
        let earlier = TcpCounters {
            segments_received: 400,
            segments_sent: 300,
            segments_retransmitted: 10,
        };
        assert_eq!(
            now.since(earlier),
            TcpCounters {
                segments_received: 500,
                segments_sent: 400,
                segments_retransmitted: 2,
            }
        );
        assert_eq!(TcpCounters::parse("Ip: Forwarding\nIp: 1\n"), None);
    }

    #[test]
    fn tcp_recovery_counters_follow_the_named_columns_and_subtract() {
        let table = "TcpExt: DelayedACKs TCPFastRetrans TCPTimeouts TCPLossProbes \
            TCPDSACKOldSent TCPDSACKOfoSent TCPDSACKRecv TCPOFOQueue\n\
            TcpExt: 40 3 9 12 5 2 6 11\nIpExt: InOctets\nIpExt: 7\n";
        let now = TcpRecoveryCounters::parse(table).expect("counters");
        assert_eq!(
            now,
            TcpRecoveryCounters {
                timeouts: 9,
                loss_probes: 12,
                fast_retransmits: 3,
                needless_retransmits_reported: 6,
                duplicates_received: 7,
                out_of_order_received: 11,
                delayed_acknowledgements: 40,
            }
        );
        let earlier = TcpRecoveryCounters {
            timeouts: 4,
            delayed_acknowledgements: 10,
            ..TcpRecoveryCounters::default()
        };
        let moved = now.since(earlier);
        assert_eq!((moved.timeouts, moved.delayed_acknowledgements), (5, 30));
        assert_eq!(moved.duplicates_received, 7);
        assert_eq!(
            TcpRecoveryCounters::parse("TcpExt: DelayedACKs\nTcpExt: 1\n"),
            None
        );
    }

    #[test]
    fn second_counts_report_what_moved_in_each_second() {
        let start = Instant::now();
        let counters = |received, retransmitted| TcpCounters {
            segments_received: received,
            segments_sent: 0,
            segments_retransmitted: retransmitted,
        };
        let mut seconds = SecondCounts::default();
        assert!(seconds.due(start));
        let connection = |retransmission_timeout_ms, unacknowledged_bytes, backoffs| {
            Some(ConnectionState {
                retransmission_timeout_ms,
                unacknowledged_bytes,
                backoffs,
            })
        };
        seconds.record(start, counters(100, 0), connection(200, 0, 0));
        assert!(!seconds.due(at(start, 999)));
        assert!(seconds.due(at(start, 1_000)));
        seconds.record(at(start, 1_000), counters(160, 0), connection(210, 14, 0));
        seconds.record(at(start, 2_000), counters(200, 7), connection(880, 620, 2));
        assert_eq!(
            seconds.take(),
            SecondSeries {
                received: "60 40".to_owned(),
                retransmitted: "0 7".to_owned(),
                retransmission_timeouts_ms: "210 880".to_owned(),
                unacknowledged_bytes: "14 620".to_owned(),
                backoffs: "0 2".to_owned(),
            }
        );
        seconds.record(at(start, 3_000), counters(230, 9), None);
        assert_eq!(
            seconds.take(),
            SecondSeries {
                received: "30".to_owned(),
                retransmitted: "2".to_owned(),
                ..SecondSeries::default()
            }
        );
        assert_eq!(seconds.take(), SecondSeries::default());
    }

    #[test]
    fn the_connection_is_found_by_its_ports_in_either_table() {
        let table = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
               0: 00000000:1515 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 9153 1 0000000000000000 100 0 0 10 0\n\
               1: 01002F0A:1515 1E002F0A:C1C4 01 0000026C:00000000 01:00000014 00000002     0        0 9160 3 0000000000000000 88 4 30 10 7\n";
        assert_eq!(
            ConnectionState::parse(table, 0x1515, 0xC1C4),
            Some(ConnectionState {
                retransmission_timeout_ms: 880,
                unacknowledged_bytes: 0x26C,
                backoffs: 2,
            })
        );
        assert_eq!(ConnectionState::parse(table, 0x1515, 0xC1C5), None);

        let table6 = "   0: 0000000000000000FFFF000001002F0A:1515 0000000000000000FFFF00001E002F0A:C1C4 01 00000000:00000000 02:000004E2 00000000     0        0 9160 3 0000000000000000 21 4 30 10 -1\n";
        assert_eq!(
            ConnectionState::parse(table6, 0x1515, 0xC1C4),
            Some(ConnectionState {
                retransmission_timeout_ms: 210,
                unacknowledged_bytes: 0,
                backoffs: 0,
            })
        );
    }

    #[test]
    fn steady_arrivals_match_the_phone_clock() {
        let start = Instant::now();
        let mut window = VideoArrivalWindow::default();
        for frame in 0..10_u64 {
            window.record(
                at(start, frame * 33),
                Some(5_000_000 + frame * 33_000),
                4_000,
            );
        }
        assert_eq!(window.frames, 10);
        assert_eq!(window.timestamped_frames, 10);
        assert_eq!(window.arrival_span(), Some(Duration::from_millis(297)));
        assert_eq!(window.phone_span_micros(), Some(297_000));
        assert_eq!(window.maximum_arrival_gap, Duration::from_millis(33));
        assert_eq!(window.maximum_phone_gap_micros, 33_000);
        assert_eq!(window.minimum_delay_over_floor_micros(), Some(0));
        assert_eq!(window.maximum_delay_over_floor_micros(), Some(0));
        assert_eq!(
            window.arrival_gaps.render(),
            "<20:0 <40:9 <70:0 <120:0 <250:0 <1000:0 >=1000:0"
        );
    }

    #[test]
    fn a_frame_delayed_in_transit_shows_as_delay_over_the_floor() {
        let start = Instant::now();
        let mut window = VideoArrivalWindow::default();
        window.record(at(start, 0), Some(0), 1);
        window.record(at(start, 33), Some(33_000), 1);
        window.record(at(start, 166), Some(66_000), 1);
        assert_eq!(window.maximum_delay_over_floor_micros(), Some(100_000));
        assert_eq!(window.minimum_delay_over_floor_micros(), Some(0));
        assert_eq!(window.maximum_arrival_gap, Duration::from_millis(133));
        assert_eq!(window.maximum_phone_gap_micros, 33_000);
    }

    #[test]
    fn a_slow_phone_shows_as_phone_gaps_without_transit_delay() {
        let start = Instant::now();
        let mut window = VideoArrivalWindow::default();
        for frame in 0..5_u64 {
            window.record(at(start, frame * 100), Some(frame * 100_000), 1);
        }
        assert_eq!(window.maximum_delay_over_floor_micros(), Some(0));
        assert_eq!(
            window.phone_gaps.render(),
            "<20:0 <40:0 <70:0 <120:4 <250:0 <1000:0 >=1000:0"
        );
    }

    #[test]
    fn pauses_report_the_time_between_resumptions() {
        let start = Instant::now();
        let mut window = VideoArrivalWindow::default();
        for burst in 0..4_u64 {
            for frame in 0..3_u64 {
                window.record(at(start, burst * 250 + frame * 33), None, 1);
            }
        }
        assert_eq!(window.pause_intervals.pauses, 3);
        assert_eq!(window.pause_intervals.render(), "250 250");
        window.restart();
        assert_eq!(window.pause_intervals.pauses, 0);
        assert_eq!(window.pause_intervals.render(), "");
        window.record(at(start, 1_000), None, 1);
        assert_eq!(window.pause_intervals.render(), "250");
    }

    #[test]
    fn restart_keeps_the_session_anchor_and_the_previous_frame() {
        let start = Instant::now();
        let mut window = VideoArrivalWindow::default();
        window.record(at(start, 0), Some(0), 1);
        window.record(at(start, 100), Some(100_000), 1);
        window.restart();
        assert_eq!(window.frames, 0);
        window.record(at(start, 350), Some(200_000), 1);
        assert_eq!(window.maximum_arrival_gap, Duration::from_millis(250));
        assert_eq!(window.maximum_phone_gap_micros, 100_000);
        assert_eq!(window.minimum_delay_over_floor_micros(), Some(150_000));
        assert_eq!(window.arrival_span(), Some(Duration::ZERO));
    }

    #[test]
    fn frames_without_a_phone_timestamp_only_count_arrivals() {
        let start = Instant::now();
        let mut window = VideoArrivalWindow::default();
        window.record(at(start, 0), None, 10);
        window.record(at(start, 2_000), None, 30);
        assert_eq!(window.frames, 2);
        assert_eq!(window.timestamped_frames, 0);
        assert_eq!(window.largest_frame_bytes, 30);
        assert_eq!(window.phone_span_micros(), None);
        assert_eq!(window.maximum_delay_over_floor_micros(), None);
        assert_eq!(
            window.arrival_gaps.render(),
            "<20:0 <40:0 <70:0 <120:0 <250:0 <1000:0 >=1000:1"
        );
    }

    #[test]
    fn loop_window_tracks_both_waits() {
        let mut window = LoopWindow::default();
        window.record_transport_wait(Duration::from_millis(250));
        window.record_input_wait(Duration::from_millis(20));
        window.record_transport_wait(Duration::from_millis(5));
        assert_eq!(window.iterations, 2);
        assert_eq!(window.transport_wait, Duration::from_millis(255));
        assert_eq!(window.maximum_transport_wait, Duration::from_millis(250));
        assert_eq!(window.maximum_input_wait, Duration::from_millis(20));
    }

    #[test]
    fn hexadecimal_renders_every_byte() {
        assert_eq!(hexadecimal(&[0x67, 0x00, 0xff]), "6700ff");
    }
}
