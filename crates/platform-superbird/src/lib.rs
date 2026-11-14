#![forbid(unsafe_code)]

mod ambient_light;
mod audio_route;
mod heartbeat;
mod identity;
mod input;
#[cfg(target_os = "linux")]
mod linux_input;
mod max20332;
mod usb_role;

pub use ambient_light::{
    AmbientLightConfig, AmbientLightControl, AmbientLightError, AmbientLightMonitor,
    AmbientLightState, BRIGHTNESS_PERCENT_RANGE, DEFAULT_BRIGHTNESS_PERCENT, InitialBacklight,
    apply_initial_backlight, apply_manual_backlight,
};
pub use audio_route::{AudioRoutePreference, AudioRouteStateError};
pub use heartbeat::{HeartbeatStamp, HeartbeatStampError};
pub use identity::{
    AccessoryIdentity, AccessoryIdentitySource, IdentityError, SuperbirdIdentitySource,
};
pub use input::{
    ABS_MT_POSITION_X, ABS_MT_POSITION_Y, ABS_MT_SLOT, ABS_MT_TRACKING_ID, DeviceRole, EV_ABS,
    EV_KEY, EV_REL, EV_SYN, GpioKey, InputDeviceProfile, InputParser, InputParserConfig,
    InputParserError, KnobInputButton, LocalInputAction, LocalPreset, MonotonicMicros,
    PlatformInputEvent, PlatformInputEventKind, REL_HWHEEL, RawInputEvent, ResetReason,
    RotaryDirection, SYN_DROPPED, SYN_REPORT, SiriLongPressPolicy, TouchInputPhase,
    transform_rotate_270,
};
#[cfg(target_os = "linux")]
pub use linux_input::{
    DeviceSelector, GrabPolicy, LinuxGpioKeyReader, LinuxInputBackend, LinuxInputBatch,
    LinuxInputConfig, LinuxInputError, LinuxInputTerminal,
};
#[cfg(target_os = "linux")]
pub use max20332::LinuxRegisterBus;
pub use max20332::{
    CONTROL1, CONTROL1_POWER_ON_DEFAULT, CONTROL2, CONTROL2_POWER_ON_DEFAULT, CONTROL3,
    CONTROL3_POWER_ON_DEFAULT, DATA_SWITCH_MASK, DEVICE_ID, Max20332, Max20332Error,
    Max20332RegisterSnapshot, Max20332Status, RegisterBus, STATUS1, USB_DATA_PATH, VB_VALID_MASK,
};
pub use usb_role::{
    HostBootOutcome, PlatformPaths, ProcessRoleCommand, RoleCommand, UsbRole, UsbRoleError,
    UsbRoleManager, UsbRoleTransaction, process_output_with_timeout,
};
