pub mod msbc;

use std::{
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    time::Duration,
};

const BTPROTO_SCO: libc::c_int = 2;
const SOL_BLUETOOTH: libc::c_int = 274;
const BT_DEFER_SETUP: libc::c_int = 7;
const BT_VOICE: libc::c_int = 11;
const BT_VOICE_TRANSPARENT: u16 = 0x0003;
const BT_VOICE_CVSD_16BIT: u16 = 0x0060;
const SOL_SCO: libc::c_int = 17;
const SCO_OPTIONS: libc::c_int = 1;
const LISTEN_BACKLOG: libc::c_int = 1;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct SockaddrSco {
    family: libc::sa_family_t,
    address: [u8; 6],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct ScoOptions {
    mtu: u16,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct BtVoice {
    setting: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AirMode {
    Cvsd,
    Transparent,
}

impl AirMode {
    const fn voice_setting(self) -> u16 {
        match self {
            Self::Cvsd => BT_VOICE_CVSD_16BIT,
            Self::Transparent => BT_VOICE_TRANSPARENT,
        }
    }
}

#[derive(Debug)]
pub struct ScoListener {
    fd: OwnedFd,
}

#[derive(Debug)]
pub struct IncomingSco {
    fd: OwnedFd,
    peer: [u8; 6],
}

#[derive(Debug)]
pub struct ScoLink {
    fd: OwnedFd,
    peer: [u8; 6],
    mtu: usize,
}

impl ScoListener {
    pub fn bind() -> io::Result<Self> {
        let fd = sco_socket()?;
        let local = SockaddrSco {
            family: bluetooth_family(),
            address: [0; 6],
        };
        check(unsafe {
            libc::bind(
                fd.as_raw_fd(),
                (&raw const local).cast(),
                socklen::<SockaddrSco>(),
            )
        })?;
        set_option(
            &fd,
            SOL_BLUETOOTH,
            BT_VOICE,
            &BtVoice {
                setting: BT_VOICE_CVSD_16BIT,
            },
        )?;
        set_option(&fd, SOL_BLUETOOTH, BT_DEFER_SETUP, &1_u32)?;
        check(unsafe { libc::listen(fd.as_raw_fd(), LISTEN_BACKLOG) })?;
        Ok(Self { fd })
    }

    pub fn accept(&self, timeout: Duration) -> io::Result<Option<IncomingSco>> {
        if !wait_readable(&self.fd, timeout)? {
            return Ok(None);
        }
        let mut remote = SockaddrSco::default();
        let mut length = socklen::<SockaddrSco>();
        let raw = match check(accept_raw(&self.fd, &mut remote, &mut length)) {
            Ok(raw) => raw,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let mut peer = remote.address;
        peer.reverse();
        Ok(Some(IncomingSco { fd, peer }))
    }
}

impl IncomingSco {
    pub const fn peer(&self) -> [u8; 6] {
        self.peer
    }

    pub fn accept(self, air_mode: AirMode) -> io::Result<ScoLink> {
        let mut options = ScoOptions::default();
        get_option(&self.fd, SOL_SCO, SCO_OPTIONS, &mut options)?;
        set_option(
            &self.fd,
            SOL_BLUETOOTH,
            BT_VOICE,
            &BtVoice {
                setting: air_mode.voice_setting(),
            },
        )?;
        let mut trigger = [0_u8; 1];
        check_size(unsafe {
            libc::recv(
                self.fd.as_raw_fd(),
                trigger.as_mut_ptr().cast(),
                trigger.len(),
                libc::MSG_DONTWAIT,
            )
        })?;
        Ok(ScoLink {
            fd: self.fd,
            peer: self.peer,
            mtu: usize::from(options.mtu),
        })
    }
}

impl ScoLink {
    pub const fn peer(&self) -> [u8; 6] {
        self.peer
    }

    pub const fn mtu(&self) -> usize {
        self.mtu
    }

    pub fn set_timeouts(&self, timeout: Duration) -> io::Result<()> {
        let value = libc::timeval {
            tv_sec: libc::time_t::try_from(timeout.as_secs()).unwrap_or(libc::time_t::MAX),
            tv_usec: timeout.subsec_micros() as libc::suseconds_t,
        };
        set_option(&self.fd, libc::SOL_SOCKET, libc::SO_RCVTIMEO, &value)?;
        set_option(&self.fd, libc::SOL_SOCKET, libc::SO_SNDTIMEO, &value)
    }

    pub fn recv(&self, buffer: &mut [u8]) -> io::Result<usize> {
        check_size(unsafe {
            libc::recv(
                self.fd.as_raw_fd(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                0,
            )
        })
    }

    pub fn send(&self, packet: &[u8]) -> io::Result<usize> {
        check_size(unsafe {
            libc::send(
                self.fd.as_raw_fd(),
                packet.as_ptr().cast(),
                packet.len(),
                send_flags(),
            )
        })
    }
}

#[cfg(target_os = "linux")]
fn sco_socket() -> io::Result<OwnedFd> {
    let raw = check(unsafe {
        libc::socket(
            libc::AF_BLUETOOTH,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            BTPROTO_SCO,
        )
    })?;
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

#[cfg(not(target_os = "linux"))]
fn sco_socket() -> io::Result<OwnedFd> {
    let _ = BTPROTO_SCO;
    Err(io::Error::from(io::ErrorKind::Unsupported))
}

#[cfg(target_os = "linux")]
fn bluetooth_family() -> libc::sa_family_t {
    libc::sa_family_t::try_from(libc::AF_BLUETOOTH).unwrap_or_default()
}

#[cfg(not(target_os = "linux"))]
fn bluetooth_family() -> libc::sa_family_t {
    0
}

#[cfg(target_os = "linux")]
fn accept_raw(
    listener: &OwnedFd,
    remote: &mut SockaddrSco,
    length: &mut libc::socklen_t,
) -> libc::c_int {
    unsafe {
        libc::accept4(
            listener.as_raw_fd(),
            std::ptr::from_mut(remote).cast(),
            length,
            libc::SOCK_CLOEXEC,
        )
    }
}

#[cfg(not(target_os = "linux"))]
fn accept_raw(
    listener: &OwnedFd,
    remote: &mut SockaddrSco,
    length: &mut libc::socklen_t,
) -> libc::c_int {
    unsafe {
        libc::accept(
            listener.as_raw_fd(),
            std::ptr::from_mut(remote).cast(),
            length,
        )
    }
}

#[cfg(target_os = "linux")]
const fn send_flags() -> libc::c_int {
    libc::MSG_NOSIGNAL
}

#[cfg(not(target_os = "linux"))]
const fn send_flags() -> libc::c_int {
    0
}

fn wait_readable(fd: &OwnedFd, timeout: Duration) -> io::Result<bool> {
    let mut poll = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let millis = libc::c_int::try_from(timeout.as_millis()).unwrap_or(libc::c_int::MAX);
    match check(unsafe { libc::poll(&raw mut poll, 1, millis) }) {
        Ok(ready) => Ok(ready > 0),
        Err(error) if error.kind() == io::ErrorKind::Interrupted => Ok(false),
        Err(error) => Err(error),
    }
}

fn set_option<T>(fd: &OwnedFd, level: libc::c_int, name: libc::c_int, value: &T) -> io::Result<()> {
    check(unsafe {
        libc::setsockopt(
            fd.as_raw_fd(),
            level,
            name,
            std::ptr::from_ref(value).cast(),
            socklen::<T>(),
        )
    })
    .map(drop)
}

fn get_option<T>(
    fd: &OwnedFd,
    level: libc::c_int,
    name: libc::c_int,
    value: &mut T,
) -> io::Result<()> {
    let mut length = socklen::<T>();
    check(unsafe {
        libc::getsockopt(
            fd.as_raw_fd(),
            level,
            name,
            std::ptr::from_mut(value).cast(),
            &raw mut length,
        )
    })
    .map(drop)
}

fn socklen<T>() -> libc::socklen_t {
    libc::socklen_t::try_from(size_of::<T>()).unwrap_or(libc::socklen_t::MAX)
}

fn check(result: libc::c_int) -> io::Result<libc::c_int> {
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}

fn check_size(result: libc::ssize_t) -> io::Result<usize> {
    usize::try_from(result).map_err(|_| io::Error::last_os_error())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn kernel_structures_match_the_bluetooth_abi() {
        assert_eq!(size_of::<SockaddrSco>(), 8);
        assert_eq!(size_of::<ScoOptions>(), 2);
        assert_eq!(size_of::<BtVoice>(), 2);
    }

    #[test]
    fn air_modes_use_the_kernel_voice_settings() {
        assert_eq!(AirMode::Cvsd.voice_setting(), 0x0060);
        assert_eq!(AirMode::Transparent.voice_setting(), 0x0003);
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn listening_is_unsupported_off_linux() {
        assert_eq!(
            ScoListener::bind().map(drop).map_err(|error| error.kind()),
            Err(io::ErrorKind::Unsupported)
        );
    }
}
