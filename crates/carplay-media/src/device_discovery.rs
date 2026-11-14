use std::{
    fmt, fs,
    io::Read,
    path::{Path, PathBuf},
};

use crate::{Error, Result};

const MAXIMUM_SYSFS_ATTRIBUTE_BYTES: u64 = 256;
const MAXIMUM_USB_MATCHES: usize = 2;
pub const BUILT_IN_MICROPHONE_CARD_ID: &str = "Superbird";
pub const BUILT_IN_MICROPHONE_DEVICE: u8 = 0;
pub const TARGET_VIDEO_DRIVER_ATTRIBUTE: &str = "/sys/class/video4linux/video1/name";

#[derive(Clone, PartialEq, Eq)]
pub struct UsbDacMatcher {
    vendor_id: Option<String>,
    product_id: Option<String>,
    serial: Option<String>,
    product: Option<String>,
}

impl fmt::Debug for UsbDacMatcher {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UsbDacMatcher")
            .field("vendor_id", &self.vendor_id)
            .field("product_id", &self.product_id)
            .field("serial", &self.serial.as_ref().map(|_| "<redacted>"))
            .field("product", &self.product.as_ref().map(|_| "<configured>"))
            .finish()
    }
}

impl UsbDacMatcher {
    pub const fn any() -> Self {
        Self {
            vendor_id: None,
            product_id: None,
            serial: None,
            product: None,
        }
    }

    pub fn new(
        vendor_id: impl Into<String>,
        product_id: impl Into<String>,
        serial: Option<String>,
        product: Option<String>,
    ) -> Result<Self> {
        let vendor_id = normalized_usb_id(vendor_id.into(), "vendor")?;
        let product_id = normalized_usb_id(product_id.into(), "product")?;
        validate_optional_attribute(serial.as_deref(), "serial")?;
        validate_optional_attribute(product.as_deref(), "product")?;
        Ok(Self {
            vendor_id: Some(vendor_id),
            product_id: Some(product_id),
            serial,
            product,
        })
    }

    pub fn vendor_id(&self) -> Option<&str> {
        self.vendor_id.as_deref()
    }

    pub fn product_id(&self) -> Option<&str> {
        self.product_id.as_deref()
    }

    fn matches_any(&self) -> bool {
        self.vendor_id.is_none()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct AlsaEndpoint {
    card_id: String,
    device: u8,
    sysfs_card_path: PathBuf,
    usb_device_path: Option<PathBuf>,
    usb_identity: Option<UsbDeviceIdentity>,
}

#[derive(Clone, PartialEq, Eq)]
struct UsbDeviceIdentity {
    vendor_id: String,
    product_id: String,
}

impl fmt::Debug for AlsaEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AlsaEndpoint")
            .field("card_id", &self.card_id)
            .field("device", &self.device)
            .field("sysfs_card_path", &"<sysfs-card>")
            .field(
                "usb_device_path",
                &self.usb_device_path.as_ref().map(|_| "<usb-device>"),
            )
            .field(
                "usb_identity",
                &self.usb_identity.as_ref().map(|_| "<usb-identity>"),
            )
            .finish()
    }
}

impl AlsaEndpoint {
    pub fn card_id(&self) -> &str {
        &self.card_id
    }

    pub fn device(&self) -> u8 {
        self.device
    }

    pub fn alsa_device(&self) -> String {
        format!("hw:CARD={},DEV={}", self.card_id, self.device)
    }
}

pub fn resolve_usb_dac(sysfs_root: &Path, matcher: &UsbDacMatcher) -> Result<AlsaEndpoint> {
    let usb_root = sysfs_root.join("bus/usb/devices");
    let mut usb_devices = Vec::new();
    let entries = fs::read_dir(&usb_root).map_err(|error| {
        Error::Backend(format!("cannot enumerate USB devices from sysfs: {error}"))
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            Error::Backend(format!("cannot read a USB sysfs directory entry: {error}"))
        })?;
        let path = entry.path();
        let Some(vendor) = read_optional_attribute(&path.join("idVendor"))? else {
            continue;
        };
        let Some(product_id) = read_optional_attribute(&path.join("idProduct"))? else {
            continue;
        };
        let identity_matches = matcher
            .vendor_id
            .as_deref()
            .is_none_or(|expected| vendor.eq_ignore_ascii_case(expected))
            && matcher
                .product_id
                .as_deref()
                .is_none_or(|expected| product_id.eq_ignore_ascii_case(expected));
        let attributes_match = identity_matches
            && match matcher.serial.as_deref() {
                Some(expected) => {
                    read_optional_attribute(&path.join("serial"))?.as_deref() == Some(expected)
                }
                None => true,
            }
            && match matcher.product.as_deref() {
                Some(expected) => {
                    read_optional_attribute(&path.join("product"))?.as_deref() == Some(expected)
                }
                None => true,
            };
        usb_devices.push((
            canonical_sysfs_path(&path)?,
            UsbDeviceIdentity {
                vendor_id: vendor,
                product_id,
            },
            attributes_match,
        ));
    }

    let all_usb_paths: Vec<_> = usb_devices
        .iter()
        .map(|(path, _, _)| path.clone())
        .collect();
    let mut usb_match_count = 0;
    let mut endpoints = Vec::new();
    for (usb_device, usb_identity, attributes_match) in usb_devices {
        if !attributes_match {
            continue;
        }
        usb_match_count += 1;
        if !matcher.matches_any() && usb_match_count >= MAXIMUM_USB_MATCHES {
            return Err(Error::Backend(
                "multiple matching USB DAC devices were found".to_owned(),
            ));
        }
        let device_endpoints = resolve_playback_endpoints_for_device(
            sysfs_root,
            &usb_device,
            &usb_identity,
            &all_usb_paths,
        )?;
        if let Some(endpoint) = select_playback_endpoint(device_endpoints)? {
            endpoints.push(endpoint);
        }
        if endpoints.len() >= MAXIMUM_USB_MATCHES {
            return Err(Error::Backend(
                "multiple USB DAC devices were found".to_owned(),
            ));
        }
    }
    match endpoints.len() {
        1 => Ok(endpoints.remove(0)),
        0 if usb_match_count == 0 || matcher.matches_any() => Err(Error::NotReady {
            component: "USB DAC",
            state: "not connected",
        }),
        0 => Err(Error::Backend(
            "matched USB DAC has no ALSA playback PCM".to_owned(),
        )),
        _ => Err(Error::Backend(
            "multiple USB DAC devices were found".to_owned(),
        )),
    }
}

pub fn revalidate_usb_dac(
    sysfs_root: &Path,
    matcher: &UsbDacMatcher,
    expected: &AlsaEndpoint,
) -> Result<()> {
    let current = resolve_usb_dac(sysfs_root, matcher)?;
    if current != *expected {
        return Err(Error::Backend(
            "USB DAC identity or ALSA endpoint changed after discovery".to_owned(),
        ));
    }
    Ok(())
}

pub fn resolve_builtin_microphone(sysfs_root: &Path) -> Result<AlsaEndpoint> {
    let sound_root = sysfs_root.join("class/sound");
    let mut matches = Vec::new();
    for entry in fs::read_dir(&sound_root).map_err(|error| {
        Error::Backend(format!("cannot enumerate ALSA cards from sysfs: {error}"))
    })? {
        let entry = entry.map_err(|error| {
            Error::Backend(format!(
                "cannot read an ALSA sysfs directory entry: {error}"
            ))
        })?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !is_card_directory(&name) {
            continue;
        }
        let path = entry.path();
        if read_optional_attribute(&path.join("id"))?.as_deref()
            != Some(BUILT_IN_MICROPHONE_CARD_ID)
        {
            continue;
        }
        let card_number = &name[4..];
        let capture = sound_root.join(format!("pcmC{card_number}D{BUILT_IN_MICROPHONE_DEVICE}c"));
        if !capture.exists() {
            return Err(Error::Backend(
                "Superbird ALSA card has no capture PCM device 0".to_owned(),
            ));
        }
        matches.push(AlsaEndpoint {
            card_id: BUILT_IN_MICROPHONE_CARD_ID.to_owned(),
            device: BUILT_IN_MICROPHONE_DEVICE,
            sysfs_card_path: canonical_sysfs_path(&path)?,
            usb_device_path: None,
            usb_identity: None,
        });
        if matches.len() >= MAXIMUM_USB_MATCHES {
            break;
        }
    }
    exactly_one(matches, "built-in Superbird microphone")
}

pub fn validate_video_decoder_target(device_path: &Path, driver_name_path: &Path) -> Result<()> {
    if device_path != Path::new(crate::TARGET_VIDEO_DEVICE) {
        return Err(Error::Backend(
            "hardware decoder path is not the required /dev/video1 target".to_owned(),
        ));
    }
    if !device_path.exists() {
        return Err(Error::Backend(
            "required /dev/video1 hardware decoder is absent".to_owned(),
        ));
    }
    let driver_name = read_required_attribute(driver_name_path, "video decoder name")?;
    if driver_name != crate::TARGET_VIDEO_DRIVER_NAME {
        return Err(Error::Backend(
            "video1 is not the required meson-video-decoder target".to_owned(),
        ));
    }
    Ok(())
}

fn resolve_playback_endpoints_for_device(
    sysfs_root: &Path,
    usb_device: &Path,
    usb_identity: &UsbDeviceIdentity,
    all_usb_devices: &[PathBuf],
) -> Result<Vec<AlsaEndpoint>> {
    let sound_root = sysfs_root.join("class/sound");
    let mut matches = Vec::new();
    for entry in fs::read_dir(&sound_root).map_err(|error| {
        Error::Backend(format!("cannot enumerate ALSA cards from sysfs: {error}"))
    })? {
        let entry = entry.map_err(|error| {
            Error::Backend(format!(
                "cannot read an ALSA sysfs directory entry: {error}"
            ))
        })?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !is_card_directory(&name) {
            continue;
        }
        let path = entry.path();
        let device = canonical_sysfs_path(&path.join("device"))?;
        let owner = all_usb_devices
            .iter()
            .filter(|candidate| device.starts_with(candidate))
            .max_by_key(|candidate| candidate.components().count());
        if owner.map(PathBuf::as_path) != Some(usb_device) {
            continue;
        }
        let card_number = &name[4..];
        let card_id = read_required_attribute(&path.join("id"), "ALSA card ID")?;
        validate_alsa_card_id(&card_id)?;
        let sysfs_card_path = canonical_sysfs_path(&path)?;
        for playback_entry in fs::read_dir(&sound_root).map_err(|error| {
            Error::Backend(format!("cannot enumerate ALSA PCMs from sysfs: {error}"))
        })? {
            let playback_entry = playback_entry.map_err(|error| {
                Error::Backend(format!("cannot read an ALSA PCM sysfs entry: {error}"))
            })?;
            let playback_name = playback_entry.file_name();
            let playback_name = playback_name.to_string_lossy();
            let Some(pcm_device) = playback_device_number(&playback_name, card_number) else {
                continue;
            };
            matches.push(AlsaEndpoint {
                card_id: card_id.clone(),
                device: pcm_device,
                sysfs_card_path: sysfs_card_path.clone(),
                usb_device_path: Some(usb_device.to_owned()),
                usb_identity: Some(usb_identity.clone()),
            });
        }
    }
    Ok(matches)
}

fn select_playback_endpoint(mut endpoints: Vec<AlsaEndpoint>) -> Result<Option<AlsaEndpoint>> {
    let mut primary = endpoints
        .iter()
        .enumerate()
        .filter(|(_, endpoint)| endpoint.device == 0)
        .map(|(index, _)| index);
    match (primary.next(), primary.next(), endpoints.len()) {
        (Some(index), None, _) => Ok(Some(endpoints.swap_remove(index))),
        (None, None, 0) => Ok(None),
        (None, None, 1) => Ok(endpoints.pop()),
        _ => Err(Error::Backend(
            "USB DAC has multiple ALSA playback PCMs without a unique device zero".to_owned(),
        )),
    }
}

fn playback_device_number(name: &str, card_number: &str) -> Option<u8> {
    name.strip_prefix(&format!("pcmC{card_number}D"))?
        .strip_suffix('p')?
        .parse()
        .ok()
}

fn normalized_usb_id(value: String, field: &str) -> Result<String> {
    if value.len() != 4 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(Error::Backend(format!(
            "USB {field} ID must contain exactly four hexadecimal digits"
        )));
    }
    Ok(value.to_ascii_lowercase())
}

fn validate_optional_attribute(value: Option<&str>, field: &str) -> Result<()> {
    if let Some(value) = value {
        if value.is_empty()
            || value.len() > MAXIMUM_SYSFS_ATTRIBUTE_BYTES as usize
            || value.bytes().any(|byte| byte.is_ascii_control())
        {
            return Err(Error::Backend(format!(
                "USB {field} match is empty, oversized, or contains control bytes"
            )));
        }
    }
    Ok(())
}

fn validate_alsa_card_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 31
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(Error::Backend(
            "ALSA card ID cannot be represented safely as a stable device".to_owned(),
        ));
    }
    Ok(())
}

fn is_card_directory(name: &str) -> bool {
    name.strip_prefix("card").is_some_and(|suffix| {
        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
    })
}

fn read_optional_attribute(path: &Path) -> Result<Option<String>> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(Error::Backend(format!(
                "cannot inspect a sysfs attribute: {error}"
            )));
        }
    };
    let mut bytes = Vec::with_capacity(MAXIMUM_SYSFS_ATTRIBUTE_BYTES as usize + 1);
    file.take(MAXIMUM_SYSFS_ATTRIBUTE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| Error::Backend(format!("cannot read a sysfs attribute: {error}")))?;
    if bytes.len() > MAXIMUM_SYSFS_ATTRIBUTE_BYTES as usize {
        return Err(Error::Backend(
            "sysfs attribute exceeds the configured bound".to_owned(),
        ));
    }
    let value = std::str::from_utf8(&bytes)
        .map_err(|_| Error::Backend("sysfs attribute is not UTF-8".to_owned()))?
        .trim();
    if value.is_empty() || value.bytes().any(|byte| byte.is_ascii_control()) {
        return Err(Error::Backend(
            "sysfs attribute is empty or contains control bytes".to_owned(),
        ));
    }
    Ok(Some(value.to_owned()))
}

fn read_required_attribute(path: &Path, description: &str) -> Result<String> {
    read_optional_attribute(path)?
        .ok_or_else(|| Error::Backend(format!("matched device has no readable {description}")))
}

fn canonical_sysfs_path(path: &Path) -> Result<PathBuf> {
    fs::canonicalize(path)
        .map_err(|error| Error::Backend(format!("cannot resolve a sysfs device path: {error}")))
}

fn exactly_one<T>(mut matches: Vec<T>, description: &str) -> Result<T> {
    match matches.len() {
        1 => Ok(matches.remove(0)),
        0 => Err(Error::Backend(format!(
            "no matching {description} was found"
        ))),
        _ => Err(Error::Backend(format!(
            "multiple matching {description} devices were found"
        ))),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::symlink;

    use tempfile::TempDir;

    use super::*;

    fn fixture() -> TempDir {
        let root = TempDir::new().expect("tempdir");
        let usb_bus = root.path().join("bus/usb/devices");
        let root_hub = root.path().join("devices/platform/usb1");
        let external_hub = root_hub.join("1-1");
        let dac = external_hub.join("1-1.2");
        fs::create_dir_all(dac.join("sound/card7")).expect("usb sound");
        fs::create_dir_all(&usb_bus).expect("USB bus");
        fs::create_dir_all(root.path().join("class/sound")).expect("sound class");
        symlink(&root_hub, usb_bus.join("usb1")).expect("root hub link");
        symlink(&external_hub, usb_bus.join("1-1")).expect("external hub link");
        symlink(&dac, usb_bus.join("1-1.2")).expect("USB DAC link");
        fs::write(root_hub.join("idVendor"), "1d6b\n").expect("root hub vendor");
        fs::write(root_hub.join("idProduct"), "0002\n").expect("root hub product");
        fs::write(external_hub.join("idVendor"), "2109\n").expect("external hub vendor");
        fs::write(external_hub.join("idProduct"), "2817\n").expect("external hub product");
        fs::write(root.path().join("bus/usb/devices/1-1.2/idVendor"), "1234\n").expect("vendor");
        fs::write(
            root.path().join("bus/usb/devices/1-1.2/idProduct"),
            "abcd\n",
        )
        .expect("product id");
        fs::write(
            root.path().join("bus/usb/devices/1-1.2/serial"),
            "serial-1\n",
        )
        .expect("serial");
        fs::write(
            root.path().join("bus/usb/devices/1-1.2/product"),
            "Known DAC\n",
        )
        .expect("product");
        fs::write(
            root.path().join("bus/usb/devices/1-1.2/sound/card7/id"),
            "KnownDAC\n",
        )
        .expect("card id");
        symlink(
            dac.join("sound/card7"),
            root.path().join("class/sound/card7"),
        )
        .expect("card link");
        fs::create_dir_all(root.path().join("class/sound/pcmC7D0p")).expect("playback pcm");
        symlink(&dac, dac.join("sound/card7/device")).expect("device link");
        root
    }

    #[test]
    fn exact_usb_identity_resolves_current_card_to_stable_id() {
        let root = fixture();
        let matcher = UsbDacMatcher::new(
            "1234",
            "ABCD",
            Some("serial-1".to_owned()),
            Some("Known DAC".to_owned()),
        )
        .expect("matcher");
        let endpoint = resolve_usb_dac(root.path(), &matcher).expect("endpoint");
        assert_eq!(endpoint.card_id(), "KnownDAC");
        assert_eq!(endpoint.alsa_device(), "hw:CARD=KnownDAC,DEV=0");
        assert!(!endpoint.alsa_device().contains("card7"));
        revalidate_usb_dac(root.path(), &matcher, &endpoint).expect("stable");
    }

    #[test]
    fn automatic_match_supports_apple_and_non_apple_usb_dacs() {
        let root = fixture();
        let matcher = UsbDacMatcher::any();
        for (vendor, product) in [("001f", "0b23"), ("05ac", "110a"), ("beef", "cafe")] {
            fs::write(root.path().join("bus/usb/devices/1-1.2/idVendor"), vendor).expect("vendor");
            fs::write(root.path().join("bus/usb/devices/1-1.2/idProduct"), product)
                .expect("product");
            let endpoint = resolve_usb_dac(root.path(), &matcher).expect("automatic USB DAC");
            assert_eq!(endpoint.alsa_device(), "hw:CARD=KnownDAC,DEV=0");
        }
    }

    #[test]
    fn automatic_match_ignores_usb_devices_without_playback() {
        let root = fixture();
        let phone = root.path().join("bus/usb/devices/1-2");
        fs::create_dir_all(&phone).expect("phone");
        fs::write(phone.join("idVendor"), "05ac\n").expect("phone vendor");
        fs::write(phone.join("idProduct"), "12a8\n").expect("phone product");

        let endpoint = resolve_usb_dac(root.path(), &UsbDacMatcher::any()).expect("USB DAC");
        assert_eq!(endpoint.card_id(), "KnownDAC");
    }

    #[test]
    fn automatic_match_resolves_a_unique_nonzero_playback_device() {
        let root = fixture();
        fs::remove_dir(root.path().join("class/sound/pcmC7D0p")).expect("remove device zero");
        fs::create_dir_all(root.path().join("class/sound/pcmC7D3p")).expect("device three");

        let endpoint = resolve_usb_dac(root.path(), &UsbDacMatcher::any()).expect("USB DAC");
        assert_eq!(endpoint.device(), 3);
        assert_eq!(endpoint.alsa_device(), "hw:CARD=KnownDAC,DEV=3");
    }

    #[test]
    fn automatic_match_prefers_unique_device_zero_when_dac_has_multiple_pcms() {
        let root = fixture();
        fs::create_dir_all(root.path().join("class/sound/pcmC7D3p")).expect("device three");

        let endpoint = resolve_usb_dac(root.path(), &UsbDacMatcher::any()).expect("USB DAC");
        assert_eq!(endpoint.device(), 0);
    }

    #[test]
    fn automatic_match_rejects_multiple_usb_dacs() {
        let root = fixture();
        fs::create_dir_all(root.path().join("bus/usb/devices/2-1/sound/card8"))
            .expect("second USB sound card");
        fs::write(root.path().join("bus/usb/devices/2-1/idVendor"), "5678\n")
            .expect("second vendor");
        fs::write(root.path().join("bus/usb/devices/2-1/idProduct"), "ef01\n")
            .expect("second product");
        fs::write(
            root.path().join("bus/usb/devices/2-1/sound/card8/id"),
            "SecondDAC\n",
        )
        .expect("second card ID");
        symlink(
            root.path().join("bus/usb/devices/2-1/sound/card8"),
            root.path().join("class/sound/card8"),
        )
        .expect("second card link");
        fs::create_dir_all(root.path().join("class/sound/pcmC8D0p")).expect("second playback PCM");
        symlink(
            root.path().join("bus/usb/devices/2-1"),
            root.path().join("bus/usb/devices/2-1/sound/card8/device"),
        )
        .expect("second device link");

        assert!(matches!(
            resolve_usb_dac(root.path(), &UsbDacMatcher::any()),
            Err(Error::Backend(message)) if message.contains("multiple USB DAC")
        ));
    }

    #[test]
    fn serial_is_redacted_from_debug_output() {
        let matcher =
            UsbDacMatcher::new("1234", "abcd", Some("secret".to_owned()), None).expect("matcher");
        let debug = format!("{matcher:?}");
        assert!(!debug.contains("secret"));
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn ambiguous_usb_identity_is_rejected() {
        let root = fixture();
        let second = root.path().join("bus/usb/devices/2-1");
        fs::create_dir_all(&second).expect("second");
        fs::write(second.join("idVendor"), "1234\n").expect("vendor");
        fs::write(second.join("idProduct"), "abcd\n").expect("product");
        let matcher = UsbDacMatcher::new("1234", "abcd", None, None).expect("matcher");
        assert!(matches!(
            resolve_usb_dac(root.path(), &matcher),
            Err(Error::Backend(message)) if message.contains("multiple")
        ));
    }

    #[test]
    fn absent_usb_dac_is_typed_as_not_connected() {
        let root = TempDir::new().expect("tempdir");
        fs::create_dir_all(root.path().join("bus/usb/devices")).expect("USB root");
        let matcher = UsbDacMatcher::new("1234", "abcd", None, None).expect("matcher");
        assert_eq!(
            resolve_usb_dac(root.path(), &matcher),
            Err(Error::NotReady {
                component: "USB DAC",
                state: "not connected",
            })
        );
    }

    #[test]
    fn changed_card_id_fails_revalidation() {
        let root = fixture();
        let matcher = UsbDacMatcher::new("1234", "abcd", None, None).expect("matcher");
        let endpoint = resolve_usb_dac(root.path(), &matcher).expect("endpoint");
        fs::write(
            root.path().join("bus/usb/devices/1-1.2/sound/card7/id"),
            "Replacement\n",
        )
        .expect("change id");
        assert!(revalidate_usb_dac(root.path(), &matcher, &endpoint).is_err());
    }

    #[test]
    fn automatic_match_detects_usb_identity_change_at_the_same_endpoint() {
        let root = fixture();
        let matcher = UsbDacMatcher::any();
        let endpoint = resolve_usb_dac(root.path(), &matcher).expect("endpoint");
        fs::write(root.path().join("bus/usb/devices/1-1.2/idVendor"), "beef\n")
            .expect("replacement vendor");

        assert!(revalidate_usb_dac(root.path(), &matcher, &endpoint).is_err());
    }

    #[test]
    fn usb_audio_card_without_playback_pcm_is_rejected() {
        let root = fixture();
        fs::remove_dir(root.path().join("class/sound/pcmC7D0p")).expect("remove playback");
        let matcher = UsbDacMatcher::new("1234", "abcd", None, None).expect("matcher");
        assert!(matches!(
            resolve_usb_dac(root.path(), &matcher),
            Err(Error::Backend(_))
        ));
    }

    #[test]
    fn builtin_microphone_requires_card_id_and_capture_device_zero() {
        let root = TempDir::new().expect("tempdir");
        fs::create_dir_all(root.path().join("class/sound/card2")).expect("card");
        fs::create_dir_all(root.path().join("class/sound/pcmC2D0c")).expect("capture");
        fs::write(root.path().join("class/sound/card2/id"), "Superbird\n").expect("id");
        let endpoint = resolve_builtin_microphone(root.path()).expect("endpoint");
        assert_eq!(endpoint.alsa_device(), "hw:CARD=Superbird,DEV=0");
    }

    #[test]
    fn sysfs_reader_bounds_actual_content_instead_of_metadata() {
        let root = TempDir::new().expect("tempdir");
        let attribute = root.path().join("attribute");
        fs::write(&attribute, "meson-video-decoder\n").expect("short attribute");
        assert_eq!(
            read_required_attribute(&attribute, "fixture").expect("attribute"),
            "meson-video-decoder"
        );

        fs::write(
            &attribute,
            vec![b'a'; MAXIMUM_SYSFS_ATTRIBUTE_BYTES as usize + 1],
        )
        .expect("oversized attribute");
        assert!(matches!(
            read_required_attribute(&attribute, "fixture"),
            Err(Error::Backend(message)) if message.contains("exceeds")
        ));
    }
}
