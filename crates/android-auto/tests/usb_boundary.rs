use std::{
    collections::VecDeque,
    io::{Read, Write},
    sync::{Arc, Mutex},
    time::Duration,
};

use prelude_android_auto::{
    AccessoryIdentity, AoapCandidate, AoapMode, AoapTimeouts, BulkEndpoints, CancellationToken,
    ConfigurationDescriptor, EndpointDescriptor, Error, GET_PROTOCOL_REQUEST, GOOGLE_VENDOR_ID,
    InterfaceDescriptor, Result, SEND_STRING_REQUEST, START_ACCESSORY_REQUEST, TransferKind,
    UsbBackend, UsbDeviceSnapshot, UsbHandle, UsbTopology, discover, discover_with_vendor_ids,
    is_accessory_id, open_accessory, run_handshake, select_bulk_endpoints,
};

#[derive(Debug, Clone, PartialEq, Eq)]
enum Call {
    ControlRead(u8, u16, u16, usize),
    ControlWrite(u8, u16, u16, Vec<u8>),
    SetConfiguration(u8),
    Claim(u8),
    SetAlternate(u8, u8),
    Release(u8),
    Reset,
    ClearHalt(u8),
    Read(u8, Duration),
    Write(u8, Vec<u8>, Duration),
}

#[derive(Debug)]
struct HandleState {
    calls: Vec<Call>,
    protocol_reads: VecDeque<std::result::Result<Vec<u8>, rusb::Error>>,
    claim_results: VecDeque<std::result::Result<(), rusb::Error>>,
    read_results: VecDeque<std::result::Result<usize, rusb::Error>>,
    write_results: VecDeque<std::result::Result<usize, rusb::Error>>,
    stalled_writes: bool,
}

impl Default for HandleState {
    fn default() -> Self {
        Self {
            calls: Vec::new(),
            protocol_reads: VecDeque::from([Ok(vec![2, 0])]),
            claim_results: VecDeque::new(),
            read_results: VecDeque::new(),
            write_results: VecDeque::new(),
            stalled_writes: false,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct MockHandle(Arc<Mutex<HandleState>>);

impl MockHandle {
    fn with_protocol(response: std::result::Result<Vec<u8>, rusb::Error>) -> Self {
        let state = HandleState {
            protocol_reads: VecDeque::from([response]),
            ..HandleState::default()
        };
        Self(Arc::new(Mutex::new(state)))
    }

    fn calls(&self) -> Vec<Call> {
        self.0.lock().expect("mock handle lock").calls.clone()
    }
}

impl UsbHandle for MockHandle {
    fn control_read(
        &mut self,
        request: u8,
        value: u16,
        index: u16,
        bytes: &mut [u8],
        _timeout: Duration,
    ) -> std::result::Result<usize, rusb::Error> {
        let mut state = self.0.lock().expect("mock handle lock");
        state
            .calls
            .push(Call::ControlRead(request, value, index, bytes.len()));
        let response = state
            .protocol_reads
            .pop_front()
            .unwrap_or(Err(rusb::Error::Pipe))?;
        let length = response.len().min(bytes.len());
        bytes[..length].copy_from_slice(&response[..length]);
        Ok(length)
    }

    fn control_write(
        &mut self,
        request: u8,
        value: u16,
        index: u16,
        bytes: &[u8],
        _timeout: Duration,
    ) -> std::result::Result<usize, rusb::Error> {
        self.0
            .lock()
            .expect("mock handle lock")
            .calls
            .push(Call::ControlWrite(request, value, index, bytes.to_vec()));
        Ok(bytes.len())
    }

    fn set_active_configuration(
        &mut self,
        configuration: u8,
    ) -> std::result::Result<(), rusb::Error> {
        self.0
            .lock()
            .expect("mock handle lock")
            .calls
            .push(Call::SetConfiguration(configuration));
        Ok(())
    }

    fn claim_interface(&mut self, interface: u8) -> std::result::Result<(), rusb::Error> {
        let mut state = self.0.lock().expect("mock handle lock");
        state.calls.push(Call::Claim(interface));
        state.claim_results.pop_front().unwrap_or(Ok(()))
    }

    fn set_alternate_setting(
        &mut self,
        interface: u8,
        alternate_setting: u8,
    ) -> std::result::Result<(), rusb::Error> {
        self.0
            .lock()
            .expect("mock handle lock")
            .calls
            .push(Call::SetAlternate(interface, alternate_setting));
        Ok(())
    }

    fn release_interface(&mut self, interface: u8) -> std::result::Result<(), rusb::Error> {
        self.0
            .lock()
            .expect("mock handle lock")
            .calls
            .push(Call::Release(interface));
        Ok(())
    }

    fn reset(&mut self) -> std::result::Result<(), rusb::Error> {
        self.0
            .lock()
            .expect("mock handle lock")
            .calls
            .push(Call::Reset);
        Ok(())
    }

    fn clear_halt(&mut self, endpoint: u8) -> std::result::Result<(), rusb::Error> {
        self.0
            .lock()
            .expect("mock handle lock")
            .calls
            .push(Call::ClearHalt(endpoint));
        Ok(())
    }

    fn read_bulk(
        &mut self,
        endpoint: u8,
        bytes: &mut [u8],
        timeout: Duration,
    ) -> std::result::Result<usize, rusb::Error> {
        let mut state = self.0.lock().expect("mock handle lock");
        state.calls.push(Call::Read(endpoint, timeout));
        let result = state.read_results.pop_front().unwrap_or(Ok(0));
        if let Ok(length) = result {
            bytes[..length].fill(0xa5);
        }
        result
    }

    fn write_bulk(
        &mut self,
        endpoint: u8,
        bytes: &[u8],
        timeout: Duration,
    ) -> std::result::Result<usize, rusb::Error> {
        let mut state = self.0.lock().expect("mock handle lock");
        state
            .calls
            .push(Call::Write(endpoint, bytes.to_vec(), timeout));
        let stalled = state.stalled_writes;
        state.write_results.pop_front().unwrap_or(if stalled {
            Err(rusb::Error::Timeout)
        } else {
            Ok(bytes.len())
        })
    }
}

#[derive(Debug, Default)]
struct BackendState {
    opens: Vec<UsbTopology>,
    sleeps: Vec<Duration>,
}

#[derive(Debug, Default)]
struct MockBackend {
    scans: VecDeque<Vec<UsbDeviceSnapshot>>,
    handles: VecDeque<std::result::Result<MockHandle, rusb::Error>>,
    state: Arc<Mutex<BackendState>>,
}

impl UsbBackend for MockBackend {
    type Handle = MockHandle;

    fn devices(&mut self) -> Result<Vec<UsbDeviceSnapshot>> {
        if self.scans.len() > 1 {
            Ok(self.scans.pop_front().expect("scan present"))
        } else {
            Ok(self.scans.front().cloned().unwrap_or_default())
        }
    }

    fn open(
        &mut self,
        device: &UsbDeviceSnapshot,
    ) -> std::result::Result<Self::Handle, rusb::Error> {
        self.state
            .lock()
            .expect("backend state lock")
            .opens
            .push(device.topology.clone());
        self.handles
            .pop_front()
            .unwrap_or(Err(rusb::Error::NoDevice))
    }

    fn sleep(&mut self, duration: Duration) {
        self.state
            .lock()
            .expect("backend state lock")
            .sleeps
            .push(duration);
        std::thread::sleep(duration);
    }
}

fn interface(
    class: u8,
    alternate_setting: u8,
    endpoints: &[(u8, TransferKind)],
) -> InterfaceDescriptor {
    InterfaceDescriptor {
        number: 3,
        alternate_setting,
        class,
        subclass: 0xff,
        protocol: 0,
        endpoints: endpoints
            .iter()
            .map(|(address, transfer_kind)| EndpointDescriptor {
                address: *address,
                transfer_kind: *transfer_kind,
            })
            .collect(),
    }
}

fn device(bus: u8, port_path: &[u8], vendor_id: u16, product_id: u16) -> UsbDeviceSnapshot {
    UsbDeviceSnapshot {
        topology: UsbTopology {
            bus,
            port_path: port_path.to_vec(),
        },
        address: 7,
        vendor_id,
        product_id,
        device_class: 0,
        active_configuration: Some(1),
        configurations: vec![ConfigurationDescriptor {
            value: 1,
            interfaces: vec![interface(
                0xff,
                0,
                &[(0x81, TransferKind::Bulk), (0x02, TransferKind::Bulk)],
            )],
        }],
    }
}

fn accessory(bus: u8, port_path: &[u8]) -> UsbDeviceSnapshot {
    device(bus, port_path, GOOGLE_VENDOR_ID, 0x2d00)
}

fn timeouts() -> AoapTimeouts {
    AoapTimeouts {
        control: Duration::from_millis(10),
        reenumeration: Duration::from_millis(50),
        poll_interval: Duration::from_millis(1),
        transfer_poll_interval: Duration::from_millis(1),
        retry_interval: Duration::ZERO,
        open_attempts: 1,
        claim_attempts: 1,
    }
}

#[test]
fn recognizes_only_the_documented_google_accessory_ids() {
    for product_id in 0x2d00..=0x2d05 {
        assert!(is_accessory_id(GOOGLE_VENDOR_ID, product_id));
    }
    assert!(!is_accessory_id(GOOGLE_VENDOR_ID, 0x2cff));
    assert!(!is_accessory_id(0x04e8, 0x2d00));
}

#[test]
fn discovery_probes_phone_like_candidates_and_uses_positive_aoap() {
    let samsung = device(1, &[2], 0x04e8, 0x6860);
    let apple = device(1, &[3], 0x05ac, 0x12a8);
    let unknown_oem = device(1, &[4], 0x1234, 0xabcd);
    let mut hid_phone_vendor = device(1, &[5], 0x04e8, 0x0001);
    hid_phone_vendor.configurations[0].interfaces[0].class = 0x03;
    let accessory = accessory(1, &[6]);
    let protocol_handle = MockHandle::with_protocol(Ok(vec![2, 0]));
    let second_protocol_handle = MockHandle::with_protocol(Ok(vec![2, 0]));
    let state = Arc::new(Mutex::new(BackendState::default()));
    let mut backend = MockBackend {
        scans: VecDeque::from([vec![
            unknown_oem.clone(),
            apple,
            hid_phone_vendor,
            samsung.clone(),
            accessory.clone(),
        ]]),
        handles: VecDeque::from([Ok(protocol_handle), Ok(second_protocol_handle)]),
        state: Arc::clone(&state),
    };

    let found = discover_with_vendor_ids(
        &mut backend,
        Duration::from_millis(10),
        &CancellationToken::default(),
        &[0x1234],
    )
    .expect("discover AOAP devices");

    assert_eq!(found.len(), 3);
    assert!(found.iter().any(|candidate| candidate.device == samsung));
    assert!(
        found
            .iter()
            .any(|candidate| candidate.device == unknown_oem)
    );
    assert!(found.iter().any(|candidate| candidate.device == accessory));
    assert_eq!(state.lock().expect("backend lock").opens.len(), 2);
}

#[test]
fn discovery_rejects_malformed_and_failed_protocol_responses() {
    let first = device(1, &[2], 0x04e8, 0x6860);
    let second = device(1, &[3], 0x18d1, 0x4ee1);
    let mut backend = MockBackend {
        scans: VecDeque::from([vec![first, second]]),
        handles: VecDeque::from([
            Ok(MockHandle::with_protocol(Ok(vec![2]))),
            Ok(MockHandle::with_protocol(Err(rusb::Error::Pipe))),
        ]),
        ..MockBackend::default()
    };

    let found = discover(
        &mut backend,
        Duration::from_millis(10),
        &CancellationToken::default(),
    )
    .expect("discovery remains resilient to per-device failures");
    assert!(found.is_empty());
}

#[test]
fn handshake_sends_the_neutral_identity_in_aoap_order_without_claiming() {
    let identity = AccessoryIdentity::default();
    assert_eq!(identity.manufacturer, "Android");
    assert_eq!(identity.model, "Android Auto");
    assert!(identity.description.contains("Prelude"));
    let mut handle = MockHandle::default();
    let protocol = run_handshake(
        &mut handle,
        &identity,
        Duration::from_millis(10),
        &CancellationToken::default(),
    )
    .expect("AOAP handshake");

    assert_eq!(protocol, 2);
    let expected_strings = [
        identity.manufacturer,
        identity.model,
        identity.description,
        identity.version,
        identity.uri,
        identity.serial,
    ];
    let mut expected = vec![Call::ControlRead(GET_PROTOCOL_REQUEST, 0, 0, 2)];
    expected.extend(
        expected_strings
            .into_iter()
            .enumerate()
            .map(|(index, value)| {
                let mut bytes = value.into_bytes();
                bytes.push(0);
                Call::ControlWrite(SEND_STRING_REQUEST, 0, index as u16, bytes)
            }),
    );
    expected.push(Call::ControlWrite(
        START_ACCESSORY_REQUEST,
        0,
        0,
        Vec::new(),
    ));
    assert_eq!(handle.calls(), expected);
}

#[test]
fn handshake_rejects_protocol_zero_before_sending_strings() {
    let mut handle = MockHandle::with_protocol(Ok(vec![0, 0]));
    let error = run_handshake(
        &mut handle,
        &AccessoryIdentity::default(),
        Duration::from_millis(10),
        &CancellationToken::default(),
    )
    .expect_err("protocol zero must be rejected");
    assert!(matches!(error, Error::UnsupportedProtocol(0)));
    assert_eq!(handle.calls().len(), 1);
}

#[test]
fn post_start_wait_matches_bus_and_full_port_path_not_address_or_vid_alone() {
    let normal = device(2, &[4, 7], 0x04e8, 0x6860);
    let wrong_port = accessory(2, &[4, 8]);
    let mut matching = accessory(2, &[4, 7]);
    matching.address = 19;
    let handshake = MockHandle::default();
    let transport_handle = MockHandle::default();
    let mut backend = MockBackend {
        scans: VecDeque::from([vec![wrong_port], vec![matching.clone()]]),
        handles: VecDeque::from([Ok(handshake), Ok(transport_handle.clone())]),
        ..MockBackend::default()
    };
    let candidate = AoapCandidate {
        device: normal,
        mode: AoapMode::Normal {
            protocol_version: 2,
        },
    };

    let transport = open_accessory(
        &mut backend,
        &candidate,
        &AccessoryIdentity::default(),
        timeouts(),
        &CancellationToken::default(),
    )
    .expect("open re-enumerated accessory");
    assert_eq!(transport.device(), &matching);
    assert_eq!(transport_handle.calls(), vec![Call::Claim(3)]);
    drop(transport);
}

#[test]
fn post_start_wait_has_a_hard_reenumeration_deadline() {
    let normal = device(2, &[4, 7], 0x04e8, 0x6860);
    let topology = normal.topology.clone();
    let mut backend = MockBackend {
        scans: VecDeque::from([Vec::new()]),
        handles: VecDeque::from([Ok(MockHandle::default())]),
        ..MockBackend::default()
    };
    let candidate = AoapCandidate {
        device: normal,
        mode: AoapMode::Normal {
            protocol_version: 2,
        },
    };
    let mut bounded = timeouts();
    bounded.reenumeration = Duration::ZERO;

    let error = open_accessory(
        &mut backend,
        &candidate,
        &AccessoryIdentity::default(),
        bounded,
        &CancellationToken::default(),
    )
    .expect_err("accessory re-enumeration must be bounded");
    assert!(matches!(
        error,
        Error::ReenumerationTimeout {
            topology: observed,
            timeout: Duration::ZERO,
        } if observed == topology
    ));
}

#[test]
fn endpoint_selection_requires_configuration_one_and_the_accessory_interface() {
    let mut accessory = accessory(1, &[2]);
    accessory.active_configuration = Some(2);
    accessory.configurations = vec![
        ConfigurationDescriptor {
            value: 2,
            interfaces: vec![interface(
                0xff,
                0,
                &[(0x83, TransferKind::Bulk), (0x04, TransferKind::Bulk)],
            )],
        },
        ConfigurationDescriptor {
            value: 1,
            interfaces: vec![
                {
                    let mut adb = interface(
                        0xff,
                        0,
                        &[(0x83, TransferKind::Bulk), (0x04, TransferKind::Bulk)],
                    );
                    adb.subclass = 0x42;
                    adb.protocol = 1;
                    adb
                },
                interface(
                    0xff,
                    1,
                    &[(0x85, TransferKind::Bulk), (0x06, TransferKind::Bulk)],
                ),
            ],
        },
    ];

    assert_eq!(
        select_bulk_endpoints(&accessory).expect("bulk endpoints"),
        BulkEndpoints {
            configuration: 1,
            interface: 3,
            alternate_setting: 1,
            bulk_in: 0x85,
            bulk_out: 0x06,
        }
    );
}

fn open_mock_transport(handle: &MockHandle) -> prelude_android_auto::UsbAoapTransport<MockHandle> {
    let accessory = accessory(1, &[2]);
    let candidate = AoapCandidate {
        device: accessory,
        mode: AoapMode::Accessory,
    };
    let mut backend = MockBackend {
        handles: VecDeque::from([Ok(handle.clone())]),
        ..MockBackend::default()
    };
    open_accessory(
        &mut backend,
        &candidate,
        &AccessoryIdentity::default(),
        timeouts(),
        &CancellationToken::default(),
    )
    .expect("open mock transport")
}

#[test]
fn transport_observes_deadline_cancellation_disconnect_and_partial_writes() {
    let handle = MockHandle::default();
    let mut transport = open_mock_transport(&handle);
    let mut byte = [0_u8; 1];
    let error = transport
        .read(&mut byte, Duration::ZERO, &CancellationToken::default())
        .expect_err("zero deadline");
    assert!(matches!(error, Error::Timeout { .. }));

    let cancellation = CancellationToken::default();
    cancellation.cancel();
    let error = transport
        .read(&mut byte, Duration::from_secs(1), &cancellation)
        .expect_err("cancelled read");
    assert!(matches!(error, Error::Cancelled { .. }));

    handle
        .0
        .lock()
        .expect("mock handle lock")
        .read_results
        .extend([Err(rusb::Error::Pipe), Ok(1)]);
    assert_eq!(
        transport
            .read(
                &mut byte,
                Duration::from_secs(1),
                &CancellationToken::default(),
            )
            .expect("stalled endpoint recovers"),
        1
    );
    assert_eq!(byte, [0xa5]);

    handle
        .0
        .lock()
        .expect("mock handle lock")
        .read_results
        .push_back(Err(rusb::Error::NoDevice));
    let error = transport
        .read(
            &mut byte,
            Duration::from_secs(1),
            &CancellationToken::default(),
        )
        .expect_err("disconnected read");
    assert!(matches!(error, Error::Disconnected { .. }));

    handle
        .0
        .lock()
        .expect("mock handle lock")
        .write_results
        .extend([Ok(2), Ok(2)]);
    transport
        .write_all(
            &[1, 2, 3, 4],
            Duration::from_secs(1),
            &CancellationToken::default(),
        )
        .expect("partial writes complete");
    transport.close().expect("deterministic cleanup");

    let calls = handle.calls();
    assert!(calls.contains(&Call::ClearHalt(0x81)));
    assert!(calls.contains(&Call::Release(3)));
    assert!(calls.contains(&Call::Reset));
    let writes = calls
        .iter()
        .filter(|call| matches!(call, Call::Write(..)))
        .count();
    assert_eq!(writes, 2);
}

#[test]
fn transfers_never_get_a_timeout_that_rounds_down_to_libusbs_wait_forever() {
    let handle = MockHandle::default();
    handle.0.lock().expect("mock handle lock").stalled_writes = true;
    let mut transport = open_mock_transport(&handle);
    let deadline = Duration::from_millis(5);
    let error = transport
        .read(&mut [0_u8; 1], deadline, &CancellationToken::default())
        .expect_err("idle read times out");
    assert!(matches!(error, Error::Timeout { .. }));
    let error = transport
        .write_all(&[1], deadline, &CancellationToken::default())
        .expect_err("stalled write times out");
    assert!(matches!(error, Error::Timeout { .. }));

    let calls = handle.calls();
    let timeouts: Vec<Duration> = calls
        .iter()
        .filter_map(|call| match call {
            Call::Read(_, timeout) | Call::Write(_, _, timeout) => Some(*timeout),
            _ => None,
        })
        .collect();
    assert!(!timeouts.is_empty(), "transfers were attempted");
    for timeout in timeouts {
        assert!(
            timeout.as_millis() >= 1,
            "{timeout:?} reaches libusb as 0 ms, which waits forever"
        );
    }
}

#[test]
fn io_adapter_is_blocking_read_write_send_and_preserves_typed_errors() {
    fn assert_read_write_send<T: Read + Write + Send>() {}
    assert_read_write_send::<prelude_android_auto::AoapIo<MockHandle>>();

    let handle = MockHandle::default();
    let cancellation = CancellationToken::default();
    cancellation.cancel();
    let mut io = open_mock_transport(&handle).into_io(
        cancellation,
        Duration::from_secs(1),
        Duration::from_secs(1),
    );
    let error = io.read(&mut [0_u8; 1]).expect_err("cancelled adapter read");
    assert_eq!(error.kind(), std::io::ErrorKind::Interrupted);
    assert!(
        error
            .get_ref()
            .and_then(|source| source.downcast_ref::<Error>())
            .is_some_and(|source| matches!(source, Error::Cancelled { .. }))
    );
}

#[test]
fn failed_claim_is_bounded_and_resets_the_device_for_recovery() {
    let handle = MockHandle::default();
    handle
        .0
        .lock()
        .expect("mock handle lock")
        .claim_results
        .extend([Err(rusb::Error::Busy), Err(rusb::Error::Busy)]);
    let candidate = AoapCandidate {
        device: accessory(1, &[2]),
        mode: AoapMode::Accessory,
    };
    let mut backend = MockBackend {
        handles: VecDeque::from([Ok(handle.clone())]),
        ..MockBackend::default()
    };
    let mut bounded = timeouts();
    bounded.claim_attempts = 2;

    let error = open_accessory(
        &mut backend,
        &candidate,
        &AccessoryIdentity::default(),
        bounded,
        &CancellationToken::default(),
    )
    .expect_err("busy interface must fail after configured attempts");
    assert!(matches!(
        error,
        Error::Usb {
            source: rusb::Error::Busy,
            ..
        }
    ));
    assert_eq!(
        handle.calls(),
        vec![Call::Claim(3), Call::Claim(3), Call::Reset]
    );
}

#[test]
fn cancelled_open_does_not_touch_usb() {
    let candidate = AoapCandidate {
        device: accessory(1, &[2]),
        mode: AoapMode::Accessory,
    };
    let state = Arc::new(Mutex::new(BackendState::default()));
    let mut backend = MockBackend {
        state: Arc::clone(&state),
        ..MockBackend::default()
    };
    let cancellation = CancellationToken::default();
    cancellation.cancel();

    let error = open_accessory(
        &mut backend,
        &candidate,
        &AccessoryIdentity::default(),
        timeouts(),
        &cancellation,
    )
    .expect_err("cancelled setup");
    assert!(matches!(error, Error::Cancelled { .. }));
    assert!(state.lock().expect("backend state lock").opens.is_empty());
}
