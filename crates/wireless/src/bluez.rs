use std::{
    collections::HashMap,
    fmt,
    io::{self, Read, Write},
    os::{fd::OwnedFd, unix::net::UnixStream},
    pin::Pin,
    sync::{Arc, Mutex, mpsc},
    thread,
    time::Duration,
};

use zbus::{
    Connection, MatchRule, MessageStream,
    export::futures_core::Stream,
    message,
    zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value},
};

use crate::{
    Error, Result,
    hfp::{self, AUDIO_GATEWAY_UUID, HANDS_FREE_UUID},
    wpp::ANDROID_AUTO_RFCOMM_UUID,
};

pub const IAP2_SERVER_UUID: &str = "00000000-deca-fade-deca-deafdecacaff";
const IAP2_DEVICE_UUID: &str = "00000000-deca-fade-deca-deafdecacafe";
pub const CARPLAY_SERVICE_UUID: &str = "ec884348-cd41-40a2-9727-575d50bf1fd3";

const IAP2_CHANNEL: u16 = 3;
const CARPLAY_CHANNEL: u16 = 4;
const ANDROID_AUTO_CHANNEL: u16 = 8;
const ADAPTER_WAIT: Duration = Duration::from_secs(30);
const ADAPTER_POLL: Duration = Duration::from_millis(250);
const NAME_TIMEOUT: Duration = Duration::from_secs(2);
const HCITOOL: &str = "/usr/bin/hcitool";
const HCI_COMMAND_TIMEOUT: Duration = Duration::from_secs(3);
const BCM_ROUTE_SCO_OVER_TRANSPORT: [&str; 7] =
    ["0x3f", "0x1c", "0x01", "0x02", "0x00", "0x01", "0x01"];

const AGENT_PATH: &str = "/com/usenocturne/prelude/agent";
const ANDROID_AUTO_PATH: &str = "/com/usenocturne/prelude/android_auto";
const HANDS_FREE_PATH: &str = "/com/usenocturne/prelude/hands_free";
const IAP2_PATH: &str = "/com/usenocturne/prelude/iap2";
const IAP2_CLIENT_PATH: &str = "/com/usenocturne/prelude/iap2_client";
const CARPLAY_PATH: &str = "/com/usenocturne/prelude/carplay";

const ANDROID_AUTO_RECORD: &str = r#"<?xml version="1.0" encoding="UTF-8" ?>
<record>
  <attribute id="0x0001"><sequence><uuid value="4de17a00-52cb-11e6-bdf4-0800200c9a66" /></sequence></attribute>
  <attribute id="0x0004"><sequence>
    <sequence><uuid value="0x0100" /></sequence>
    <sequence><uuid value="0x0003" /><uint8 value="0x08" /></sequence>
  </sequence></attribute>
  <attribute id="0x0005"><sequence><uuid value="0x1002" /></sequence></attribute>
  <attribute id="0x0100"><text value="Android Auto Wireless" /></attribute>
</record>
"#;

const IAP2_RECORD: &str = r#"<?xml version="1.0" encoding="UTF-8" ?>
<record>
  <attribute id="0x0001"><sequence><uuid value="00000000-deca-fade-deca-deafdecacaff" /></sequence></attribute>
  <attribute id="0x0002"><uint32 value="0x00000000" /></attribute>
  <attribute id="0x0004"><sequence>
    <sequence><uuid value="0x0100" /></sequence>
    <sequence><uuid value="0x0003" /><uint8 value="0x03" /></sequence>
  </sequence></attribute>
  <attribute id="0x0005"><sequence><uuid value="0x1002" /></sequence></attribute>
  <attribute id="0x0008"><uint8 value="0xff" /></attribute>
  <attribute id="0x0009"><sequence><sequence><uuid value="0x1101" /><uint16 value="0x0100" /></sequence></sequence></attribute>
  <attribute id="0x0100"><text value="Wireless iAP" /></attribute>
</record>
"#;

const CARPLAY_RECORD: &str = r#"<?xml version="1.0" encoding="UTF-8" ?>
<record>
  <attribute id="0x0001"><sequence><uuid value="ec884348-cd41-40a2-9727-575d50bf1fd3" /></sequence></attribute>
  <attribute id="0x0002"><uint32 value="0x00000000" /></attribute>
  <attribute id="0x0004"><sequence>
    <sequence><uuid value="0x0100" /></sequence>
    <sequence><uuid value="0x0003" /><uint8 value="0x04" /></sequence>
  </sequence></attribute>
  <attribute id="0x0005"><sequence><uuid value="0x1002" /></sequence></attribute>
  <attribute id="0x0008"><uint8 value="0xff" /></attribute>
  <attribute id="0x0009"><sequence><sequence><uuid value="0x1101" /><uint16 value="0x0100" /></sequence></sequence></attribute>
  <attribute id="0x0100"><text value="CarPlay" /></attribute>
</record>
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RfcommProfile {
    AndroidAuto,
    HandsFree,
    Iap2,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BluetoothConfig {
    pub adapter: String,
    pub alias: String,
    pub discoverable: bool,
    pub pairable: bool,
    pub projection: bool,
    pub android_auto: bool,
    pub carplay: bool,
    pub wideband_calls: bool,
}

pub struct RfcommConnection {
    pub profile: RfcommProfile,
    pub peer: String,
    stream: UnixStream,
}

impl fmt::Debug for RfcommConnection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RfcommConnection")
            .field("profile", &self.profile)
            .field("peer", &self.peer)
            .finish_non_exhaustive()
    }
}

impl RfcommConnection {
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.stream.set_read_timeout(timeout)
    }

    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.stream.set_write_timeout(timeout)
    }

    pub fn shutdown(&self) -> io::Result<()> {
        self.stream.shutdown(std::net::Shutdown::Both)
    }
}

impl Read for RfcommConnection {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.stream.read(buffer)
    }
}

impl Write for RfcommConnection {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.stream.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

#[derive(Debug, Clone, Default)]
pub struct PairingPasskeys {
    latest: Arc<Mutex<Option<(String, u32)>>>,
    unshown: Arc<Mutex<Option<(String, u32)>>>,
}

impl PairingPasskeys {
    fn record(&self, peer: String, passkey: u32) {
        if let Ok(mut unshown) = self.unshown.lock() {
            *unshown = Some((peer.clone(), passkey));
        }
        if let Ok(mut latest) = self.latest.lock() {
            *latest = Some((peer, passkey));
        }
    }

    pub fn take_unshown(&self) -> Option<(String, u32)> {
        self.unshown.lock().ok()?.take()
    }

    pub fn take(&self, peer: &str) -> Option<u32> {
        let mut latest = self.latest.lock().ok()?;
        if latest
            .as_ref()
            .is_some_and(|(recorded, _)| recorded.eq_ignore_ascii_case(peer))
        {
            return latest.take().map(|(_, passkey)| passkey);
        }
        None
    }
}

#[derive(Debug)]
pub struct BluetoothService {
    address: [u8; 6],
    control: BluetoothControl,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhoneKind {
    CarPlay,
    AndroidAuto,
}

impl PhoneKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CarPlay => "carplay",
            Self::AndroidAuto => "android-auto",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        [Self::CarPlay, Self::AndroidAuto]
            .into_iter()
            .find(|kind| kind.as_str() == text)
    }

    const fn page_uuid(self) -> &'static str {
        match self {
            Self::CarPlay => IAP2_DEVICE_UUID,
            Self::AndroidAuto => AUDIO_GATEWAY_UUID,
        }
    }
}

#[derive(Debug)]
enum DeviceCommand {
    Connect(String, PhoneKind),
    Disconnect(String),
    ReleaseHandsFree(String),
    Remove(String),
    Name(String, mpsc::Sender<Option<String>>),
    Paired(String, mpsc::Sender<bool>),
}

#[derive(Debug, Clone)]
pub struct BluetoothControl(tokio::sync::mpsc::UnboundedSender<DeviceCommand>);

impl BluetoothControl {
    pub fn connect(&self, peer: &str, kind: PhoneKind) {
        let _ = self.0.send(DeviceCommand::Connect(peer.to_owned(), kind));
    }

    pub fn disconnect(&self, peer: &str) {
        let _ = self.0.send(DeviceCommand::Disconnect(peer.to_owned()));
    }

    pub fn release_hands_free(&self, peer: &str) {
        let _ = self
            .0
            .send(DeviceCommand::ReleaseHandsFree(peer.to_owned()));
    }

    pub fn remove(&self, peer: &str) {
        let _ = self.0.send(DeviceCommand::Remove(peer.to_owned()));
    }

    pub fn name(&self, peer: &str) -> Option<String> {
        let (reply, answer) = mpsc::channel();
        self.0
            .send(DeviceCommand::Name(peer.to_owned(), reply))
            .ok()?;
        answer.recv_timeout(NAME_TIMEOUT).ok().flatten()
    }

    pub fn is_paired(&self, peer: &str) -> Option<bool> {
        let (reply, answer) = mpsc::channel();
        self.0
            .send(DeviceCommand::Paired(peer.to_owned(), reply))
            .ok()?;
        answer.recv_timeout(NAME_TIMEOUT).ok()
    }
}

impl BluetoothService {
    pub fn start(
        config: BluetoothConfig,
        passkeys: PairingPasskeys,
    ) -> Result<(Self, mpsc::Receiver<RfcommConnection>)> {
        let (connections, incoming) = mpsc::channel();
        let (ready_sender, ready) = mpsc::channel();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let (control, commands) = tokio::sync::mpsc::unbounded_channel();
        let thread = thread::Builder::new()
            .name("prelude-bluez".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = ready_sender
                            .send(Err(Error::io_context("building the BlueZ runtime", error)));
                        return;
                    }
                };
                runtime.block_on(async move {
                    match register(&config, connections, passkeys).await {
                        Ok((connection, mut watch, address)) => {
                            let _ = ready_sender.send(Ok(address));
                            let adapter_path = format!("/org/bluez/{}", config.adapter);
                            serve_commands(
                                &connection,
                                &adapter_path,
                                &mut watch,
                                commands,
                                stopped,
                            )
                            .await;
                            let _ = connection.close().await;
                        }
                        Err(error) => {
                            let _ = ready_sender.send(Err(error));
                        }
                    }
                });
            })
            .map_err(|error| Error::io_context("spawning the BlueZ thread", error))?;
        let address = ready
            .recv()
            .map_err(|_| Error::Protocol("BlueZ thread exited during registration"))??;
        Ok((
            Self {
                address,
                control: BluetoothControl(control),
                stop: Some(stop),
                thread: Some(thread),
            },
            incoming,
        ))
    }
}

pub fn power_off(adapter: &str) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| Error::io_context("building the BlueZ runtime", error))?;
    runtime.block_on(async {
        let connection = Connection::system().await?;
        let powered_off = set_property(
            &connection,
            &format!("/org/bluez/{adapter}"),
            "org.bluez.Adapter1",
            "Powered",
            Value::from(false),
        )
        .await;
        let _ = connection.close().await;
        powered_off
    })
}

impl BluetoothService {
    pub const fn address(&self) -> [u8; 6] {
        self.address
    }

    pub fn control(&self) -> BluetoothControl {
        self.control.clone()
    }

    pub fn is_running(&self) -> bool {
        self.thread
            .as_ref()
            .is_some_and(|thread| !thread.is_finished())
    }
}

struct BluezWatch {
    owner_changes: MessageStream,
    removals: MessageStream,
}

impl BluezWatch {
    async fn subscribe(connection: &Connection) -> Result<Self> {
        let owner_changes = MatchRule::builder()
            .msg_type(message::Type::Signal)
            .interface("org.freedesktop.DBus")?
            .member("NameOwnerChanged")?
            .arg(0, "org.bluez")?
            .build();
        let removals = MatchRule::builder()
            .msg_type(message::Type::Signal)
            .interface("org.freedesktop.DBus.ObjectManager")?
            .member("InterfacesRemoved")?
            .build();
        Ok(Self {
            owner_changes: MessageStream::for_match_rule(owner_changes, connection, None).await?,
            removals: MessageStream::for_match_rule(removals, connection, None).await?,
        })
    }

    async fn lost(&mut self, adapter_path: &str) -> &'static str {
        loop {
            tokio::select! {
                message = next_message(&mut self.owner_changes) => {
                    let Some(message) = message else {
                        return "the D-Bus connection closed";
                    };
                    if message
                        .body()
                        .deserialize::<(String, String, String)>()
                        .is_ok_and(|(name, old_owner, _)| bluez_owner_lost(&name, &old_owner))
                    {
                        return "bluetoothd restarted";
                    }
                }
                message = next_message(&mut self.removals) => {
                    let Some(message) = message else {
                        return "the D-Bus connection closed";
                    };
                    if message
                        .body()
                        .deserialize::<(OwnedObjectPath, Vec<String>)>()
                        .is_ok_and(|(path, interfaces)| {
                            path.as_str() == adapter_path
                                && interfaces.iter().any(|name| name == "org.bluez.Adapter1")
                        })
                    {
                        return "the Bluetooth adapter was removed";
                    }
                }
            }
        }
    }
}

async fn next_message(stream: &mut MessageStream) -> Option<zbus::Message> {
    loop {
        match std::future::poll_fn(|context| Pin::new(&mut *stream).poll_next(context)).await? {
            Ok(message) => return Some(message),
            Err(error) => tracing::debug!(%error, "skipping an unreadable BlueZ signal"),
        }
    }
}

async fn serve_commands(
    connection: &Connection,
    adapter_path: &str,
    watch: &mut BluezWatch,
    mut commands: tokio::sync::mpsc::UnboundedReceiver<DeviceCommand>,
    mut stopped: tokio::sync::oneshot::Receiver<()>,
) {
    loop {
        tokio::select! {
            _ = &mut stopped => return,
            reason = watch.lost(adapter_path) => {
                tracing::warn!(reason, "BlueZ dropped the projection registration; registering again");
                return;
            }
            Some(command) = commands.recv() => {
                let connection = connection.clone();
                let adapter_path = adapter_path.to_owned();
                tokio::spawn(async move {
                    run_device_command(&connection, &adapter_path, command).await;
                });
            }
        }
    }
}

async fn run_device_command(connection: &Connection, adapter_path: &str, command: DeviceCommand) {
    match command {
        DeviceCommand::Connect(peer, kind) => {
            let path = device_path(adapter_path, &peer);
            if !device_flag(connection, &path, "Paired").await {
                tracing::debug!(peer, "not paging a phone that is no longer bonded");
                return;
            }
            match call(
                connection,
                &path,
                "org.bluez.Device1",
                "ConnectProfile",
                &(kind.page_uuid(),),
            )
            .await
            {
                Ok(()) => tracing::info!(peer, kind = kind.as_str(), "reconnected the last phone"),
                Err(error) => {
                    tracing::debug!(peer, kind = kind.as_str(), %error, "the last phone did not answer");
                }
            }
        }
        DeviceCommand::Disconnect(peer) => {
            let path = device_path(adapter_path, &peer);
            match call(connection, &path, "org.bluez.Device1", "Disconnect", &()).await {
                Ok(()) => tracing::info!(peer, "disconnected Bluetooth from the phone"),
                Err(error) => tracing::warn!(peer, %error, "disconnecting the phone failed"),
            }
        }
        DeviceCommand::ReleaseHandsFree(peer) => {
            let path = device_path(adapter_path, &peer);
            match call(
                connection,
                &path,
                "org.bluez.Device1",
                "DisconnectProfile",
                &(AUDIO_GATEWAY_UUID,),
            )
            .await
            {
                Ok(()) => tracing::info!(peer, "released hands-free so calls stay on the phone"),
                Err(error) => tracing::debug!(peer, %error, "releasing hands-free failed"),
            }
        }
        DeviceCommand::Remove(peer) => {
            let path = device_path(adapter_path, &peer);
            let Ok(device) = ObjectPath::try_from(path.as_str()) else {
                return;
            };
            match call(
                connection,
                adapter_path,
                "org.bluez.Adapter1",
                "RemoveDevice",
                &(device,),
            )
            .await
            {
                Ok(()) => tracing::info!(peer, "removed the phone's Bluetooth bond"),
                Err(error) => tracing::warn!(peer, %error, "removing the phone's bond failed"),
            }
        }
        DeviceCommand::Name(peer, reply) => {
            let path = device_path(adapter_path, &peer);
            let _ = reply.send(device_string(connection, &path, "Alias").await);
        }
        DeviceCommand::Paired(peer, reply) => {
            let path = device_path(adapter_path, &peer);
            let _ = reply.send(device_flag(connection, &path, "Paired").await);
        }
    }
}

fn device_path(adapter_path: &str, peer: &str) -> String {
    format!("{adapter_path}/dev_{}", peer.replace(':', "_"))
}

async fn device_flag(connection: &Connection, path: &str, name: &str) -> bool {
    let Ok(reply) = connection
        .call_method(
            Some("org.bluez"),
            path,
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &("org.bluez.Device1", name),
        )
        .await
    else {
        return false;
    };
    reply
        .body()
        .deserialize::<OwnedValue>()
        .ok()
        .and_then(|value| bool::try_from(value).ok())
        .unwrap_or(false)
}

async fn device_string(connection: &Connection, path: &str, name: &str) -> Option<String> {
    let reply = connection
        .call_method(
            Some("org.bluez"),
            path,
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &("org.bluez.Device1", name),
        )
        .await
        .ok()?;
    let value = reply.body().deserialize::<OwnedValue>().ok()?;
    String::try_from(value).ok()
}

impl Drop for BluetoothService {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

async fn register(
    config: &BluetoothConfig,
    connections: mpsc::Sender<RfcommConnection>,
    passkeys: PairingPasskeys,
) -> Result<(Connection, BluezWatch, [u8; 6])> {
    let connection = Connection::system().await?;
    let registered = async {
        let watch = BluezWatch::subscribe(&connection).await?;
        let address = register_on(&connection, config, connections, passkeys).await?;
        Ok((watch, address))
    };
    match registered.await {
        Ok((watch, address)) => Ok((connection, watch, address)),
        Err(error) => {
            let _ = connection.close().await;
            Err(error)
        }
    }
}

async fn register_on(
    connection: &Connection,
    config: &BluetoothConfig,
    connections: mpsc::Sender<RfcommConnection>,
    passkeys: PairingPasskeys,
) -> Result<[u8; 6]> {
    let adapter_path = format!("/org/bluez/{}", config.adapter);
    let address = wait_for_adapter(connection, &adapter_path).await?;

    let profile = |kind| Profile {
        kind,
        adapter_path: adapter_path.clone(),
        connections: connections.clone(),
    };
    let server = connection.object_server();
    server
        .at(
            AGENT_PATH,
            Agent {
                connection: connection.clone(),
                passkeys,
            },
        )
        .await?;
    call(
        connection,
        "/org/bluez",
        "org.bluez.AgentManager1",
        "RegisterAgent",
        &(ObjectPath::try_from(AGENT_PATH)?, "DisplayYesNo"),
    )
    .await?;
    call(
        connection,
        "/org/bluez",
        "org.bluez.AgentManager1",
        "RequestDefaultAgent",
        &(ObjectPath::try_from(AGENT_PATH)?,),
    )
    .await?;

    if config.android_auto {
        if config.projection {
            server
                .at(ANDROID_AUTO_PATH, profile(RfcommProfile::AndroidAuto))
                .await?;
            register_profile(
                connection,
                ANDROID_AUTO_PATH,
                ANDROID_AUTO_RFCOMM_UUID,
                server_options(ANDROID_AUTO_CHANNEL, ANDROID_AUTO_RECORD),
            )
            .await?;
        }
        server
            .at(HANDS_FREE_PATH, profile(RfcommProfile::HandsFree))
            .await?;
        let mut options = HashMap::new();
        options.insert("Name", Value::from("Hands-Free"));
        options.insert(
            "Features",
            Value::from(hfp::sdp_features(config.wideband_calls)),
        );
        options.insert("AutoConnect", Value::from(true));
        options.insert("RequireAuthentication", Value::from(true));
        register_profile(connection, HANDS_FREE_PATH, HANDS_FREE_UUID, options).await?;
    }
    if config.carplay && config.projection {
        server.at(IAP2_PATH, profile(RfcommProfile::Iap2)).await?;
        register_profile(
            connection,
            IAP2_PATH,
            IAP2_SERVER_UUID,
            server_options(IAP2_CHANNEL, IAP2_RECORD),
        )
        .await?;
        server
            .at(IAP2_CLIENT_PATH, profile(RfcommProfile::Iap2))
            .await?;
        let mut options = HashMap::new();
        options.insert("Role", Value::from("client"));
        options.insert("RequireAuthentication", Value::from(true));
        register_profile(connection, IAP2_CLIENT_PATH, IAP2_DEVICE_UUID, options).await?;
        server
            .at(CARPLAY_PATH, profile(RfcommProfile::Iap2))
            .await?;
        register_profile(
            connection,
            CARPLAY_PATH,
            CARPLAY_SERVICE_UUID,
            server_options(CARPLAY_CHANNEL, CARPLAY_RECORD),
        )
        .await?;
    }

    set_property(
        connection,
        &adapter_path,
        "org.bluez.Adapter1",
        "Alias",
        Value::from(config.alias.as_str()),
    )
    .await?;
    set_property(
        connection,
        &adapter_path,
        "org.bluez.Adapter1",
        "Powered",
        Value::from(true),
    )
    .await?;
    if let Err(error) = set_property(
        connection,
        &adapter_path,
        "org.bluez.Adapter1",
        "Connectable",
        Value::from(true),
    )
    .await
    {
        tracing::debug!(%error, "BlueZ did not accept the Connectable property");
    }
    set_property(
        connection,
        &adapter_path,
        "org.bluez.Adapter1",
        "DiscoverableTimeout",
        Value::from(0_u32),
    )
    .await?;
    set_property(
        connection,
        &adapter_path,
        "org.bluez.Adapter1",
        "Pairable",
        Value::from(config.pairable),
    )
    .await?;
    set_property(
        connection,
        &adapter_path,
        "org.bluez.Adapter1",
        "Discoverable",
        Value::from(config.discoverable),
    )
    .await?;
    if config.android_auto {
        route_sco_to_host(&config.adapter).await;
    }
    tracing::info!(
        adapter = config.adapter,
        alias = config.alias,
        projection = config.projection,
        android_auto = config.android_auto,
        carplay = config.carplay,
        "Bluetooth profiles registered"
    );
    Ok(address)
}

async fn route_sco_to_host(adapter: &str) {
    let status = tokio::process::Command::new(HCITOOL)
        .args(["-i", adapter, "cmd"])
        .args(BCM_ROUTE_SCO_OVER_TRANSPORT)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .status();
    match tokio::time::timeout(HCI_COMMAND_TIMEOUT, status).await {
        Ok(Ok(status)) if status.success() => {
            tracing::info!(adapter, "routing Bluetooth call audio over HCI");
        }
        Ok(Ok(status)) => {
            tracing::warn!(adapter, %status, "the controller did not route call audio over HCI");
        }
        Ok(Err(error)) => {
            tracing::warn!(adapter, %error, "running hcitool to route call audio failed");
        }
        Err(_) => tracing::warn!(adapter, "hcitool timed out routing call audio"),
    }
}

fn server_options(channel: u16, record: &'static str) -> HashMap<&'static str, Value<'static>> {
    let mut options = HashMap::new();
    options.insert("Role", Value::from("server"));
    options.insert("Channel", Value::from(channel));
    options.insert("ServiceRecord", Value::from(record));
    options.insert("RequireAuthentication", Value::from(false));
    options.insert("RequireAuthorization", Value::from(false));
    options
}

async fn register_profile(
    connection: &Connection,
    path: &str,
    uuid: &str,
    options: HashMap<&str, Value<'_>>,
) -> Result<()> {
    call(
        connection,
        "/org/bluez",
        "org.bluez.ProfileManager1",
        "RegisterProfile",
        &(ObjectPath::try_from(path)?, uuid, options),
    )
    .await
}

async fn call<B>(
    connection: &Connection,
    path: &str,
    interface: &str,
    method: &str,
    body: &B,
) -> Result<()>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    connection
        .call_method(Some("org.bluez"), path, Some(interface), method, body)
        .await?;
    Ok(())
}

async fn set_property(
    connection: &Connection,
    path: &str,
    interface: &str,
    name: &str,
    value: Value<'_>,
) -> Result<()> {
    call(
        connection,
        path,
        "org.freedesktop.DBus.Properties",
        "Set",
        &(interface, name, value),
    )
    .await
}

async fn wait_for_adapter(connection: &Connection, path: &str) -> Result<[u8; 6]> {
    let deadline = tokio::time::Instant::now() + ADAPTER_WAIT;
    loop {
        let probe = connection
            .call_method(
                Some("org.bluez"),
                path,
                Some("org.freedesktop.DBus.Properties"),
                "Get",
                &("org.bluez.Adapter1", "Address"),
            )
            .await;
        match probe {
            Ok(reply) => {
                let value: OwnedValue = reply.body().deserialize()?;
                let address = String::try_from(value)?;
                return parse_address(&address).ok_or(Error::Protocol(
                    "BlueZ reported a malformed adapter address",
                ));
            }
            Err(error) if tokio::time::Instant::now() >= deadline => return Err(error.into()),
            Err(_) => tokio::time::sleep(ADAPTER_POLL).await,
        }
    }
}

fn bluez_owner_lost(name: &str, old_owner: &str) -> bool {
    name == "org.bluez" && !old_owner.is_empty()
}

fn parse_address(text: &str) -> Option<[u8; 6]> {
    let mut address = [0_u8; 6];
    let mut octets = text.split(':');
    for byte in &mut address {
        *byte = u8::from_str_radix(octets.next()?, 16).ok()?;
    }
    octets.next().is_none().then_some(address)
}

fn peer_address(device: &str) -> Option<String> {
    let address = device
        .rsplit('/')
        .next()?
        .strip_prefix("dev_")?
        .replace('_', ":");
    (address.len() == 17).then(|| address.to_ascii_uppercase())
}

struct Profile {
    kind: RfcommProfile,
    adapter_path: String,
    connections: mpsc::Sender<RfcommConnection>,
}

#[zbus::interface(name = "org.bluez.Profile1")]
impl Profile {
    async fn new_connection(
        &self,
        device: OwnedObjectPath,
        fd: zbus::zvariant::OwnedFd,
        _properties: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<()> {
        if !device
            .as_str()
            .starts_with(&format!("{}/", self.adapter_path))
        {
            return Err(zbus::fdo::Error::AccessDenied(
                "connection arrived on a foreign adapter".into(),
            ));
        }
        let peer = peer_address(device.as_str())
            .ok_or_else(|| zbus::fdo::Error::InvalidArgs("malformed device path".into()))?;
        tracing::info!(profile = ?self.kind, peer, "accepted RFCOMM connection");
        let stream = UnixStream::from(OwnedFd::from(fd));
        stream
            .set_nonblocking(false)
            .map_err(|error| zbus::fdo::Error::IOError(error.to_string()))?;
        self.connections
            .send(RfcommConnection {
                profile: self.kind,
                peer,
                stream,
            })
            .map_err(|_| {
                zbus::fdo::Error::Failed("wireless runtime is not accepting connections".into())
            })
    }

    async fn request_disconnection(&self, device: OwnedObjectPath) {
        tracing::debug!(profile = ?self.kind, device = device.as_str(), "BlueZ requested disconnection");
    }

    async fn release(&self) {}
}

struct Agent {
    connection: Connection,
    passkeys: PairingPasskeys,
}

impl Agent {
    async fn trust(&self, device: &ObjectPath<'_>) {
        if let Err(error) = set_property(
            &self.connection,
            device.as_str(),
            "org.bluez.Device1",
            "Trusted",
            Value::from(true),
        )
        .await
        {
            tracing::warn!(device = device.as_str(), %error, "could not mark the phone trusted");
        }
    }
}

#[zbus::interface(name = "org.bluez.Agent1")]
impl Agent {
    async fn release(&self) {}

    async fn request_pin_code(&self, _device: ObjectPath<'_>) -> zbus::fdo::Result<String> {
        Err(zbus::fdo::Error::Failed(
            "PIN pairing is not supported".into(),
        ))
    }

    async fn display_pin_code(&self, _device: ObjectPath<'_>, _pincode: String) {}

    async fn request_passkey(&self, _device: ObjectPath<'_>) -> zbus::fdo::Result<u32> {
        Err(zbus::fdo::Error::Failed(
            "passkey entry is not supported".into(),
        ))
    }

    async fn display_passkey(&self, device: ObjectPath<'_>, passkey: u32, _entered: u16) {
        if let Some(peer) = peer_address(device.as_str()) {
            self.passkeys.record(peer, passkey);
        }
    }

    async fn request_confirmation(&self, device: ObjectPath<'_>, passkey: u32) {
        tracing::info!(
            device = device.as_str(),
            passkey,
            "accepting Bluetooth pairing"
        );
        if let Some(peer) = peer_address(device.as_str()) {
            self.passkeys.record(peer, passkey);
        }
        self.trust(&device).await;
    }

    async fn request_authorization(&self, device: ObjectPath<'_>) {
        self.trust(&device).await;
    }

    async fn authorize_service(&self, device: ObjectPath<'_>, _uuid: String) {
        self.trust(&device).await;
    }

    async fn cancel(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passkeys_are_handed_out_once_to_the_phone_that_paired() {
        let passkeys = PairingPasskeys::default();
        passkeys.record("F0:5C:77:12:34:56".to_owned(), 42);
        assert_eq!(passkeys.take("AA:BB:CC:DD:EE:FF"), None);
        assert_eq!(passkeys.take("f0:5c:77:12:34:56"), Some(42));
        assert_eq!(passkeys.take("F0:5C:77:12:34:56"), None);
        assert_eq!(
            passkeys.take_unshown(),
            Some(("F0:5C:77:12:34:56".to_owned(), 42))
        );
        assert_eq!(passkeys.take_unshown(), None);
    }

    #[test]
    fn peer_address_comes_from_the_device_path() {
        assert_eq!(
            peer_address("/org/bluez/hci0/dev_aa_BB_cc_DD_ee_FF").as_deref(),
            Some("AA:BB:CC:DD:EE:FF")
        );
        assert_eq!(peer_address("/org/bluez/hci0"), None);
        assert_eq!(peer_address("/org/bluez/hci0/dev_AA_BB"), None);
    }

    #[test]
    fn bluetoothd_appearing_is_not_a_restart() {
        assert!(!bluez_owner_lost("org.bluez", ""));
        assert!(bluez_owner_lost("org.bluez", ":1.7"));
        assert!(!bluez_owner_lost("org.freedesktop.NetworkManager", ":1.7"));
    }

    #[test]
    fn adapter_addresses_parse_most_significant_first() {
        assert_eq!(
            parse_address("30:E3:D6:03:96:1E"),
            Some([0x30, 0xe3, 0xd6, 0x03, 0x96, 0x1e])
        );
        assert_eq!(parse_address("30:E3:D6:03:96"), None);
        assert_eq!(parse_address("30:E3:D6:03:96:1E:00"), None);
        assert_eq!(parse_address("zz:E3:D6:03:96:1E"), None);
    }

    #[test]
    fn service_records_advertise_their_channels() {
        assert!(ANDROID_AUTO_RECORD.contains(ANDROID_AUTO_RFCOMM_UUID));
        assert!(ANDROID_AUTO_RECORD.contains(&format!("0x{ANDROID_AUTO_CHANNEL:02x}")));
        assert!(IAP2_RECORD.contains(IAP2_SERVER_UUID));
        assert!(IAP2_RECORD.contains(&format!("0x{IAP2_CHANNEL:02x}")));
        assert!(CARPLAY_RECORD.contains(CARPLAY_SERVICE_UUID));
        assert!(CARPLAY_RECORD.contains(&format!("0x{CARPLAY_CHANNEL:02x}")));
    }
}
