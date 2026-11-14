use std::{
    io::{self, Read, Write},
    time::{Duration, Instant},
};

use crate::{
    CancellationToken, EndpointDescriptor, Error, Result, TransferKind, UsbDeviceSnapshot,
    UsbHandle, aoap::check_cancelled,
};

const MIN_TRANSFER_TIMEOUT: Duration = Duration::from_millis(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BulkEndpoints {
    pub configuration: u8,
    pub interface: u8,
    pub alternate_setting: u8,
    pub bulk_in: u8,
    pub bulk_out: u8,
}

pub fn select_bulk_endpoints(device: &UsbDeviceSnapshot) -> Result<BulkEndpoints> {
    for configuration in device
        .configurations
        .iter()
        .filter(|configuration| configuration.value == 1)
    {
        for interface in &configuration.interfaces {
            if interface.class != 0xff || interface.subclass != 0xff || interface.protocol != 0 {
                continue;
            }
            let bulk_in = interface
                .endpoints
                .iter()
                .find(|endpoint| is_bulk_direction(endpoint, true));
            let bulk_out = interface
                .endpoints
                .iter()
                .find(|endpoint| is_bulk_direction(endpoint, false));
            if let (Some(bulk_in), Some(bulk_out)) = (bulk_in, bulk_out) {
                return Ok(BulkEndpoints {
                    configuration: configuration.value,
                    interface: interface.number,
                    alternate_setting: interface.alternate_setting,
                    bulk_in: bulk_in.address,
                    bulk_out: bulk_out.address,
                });
            }
        }
    }
    Err(Error::BulkEndpointsMissing)
}

fn is_bulk_direction(endpoint: &EndpointDescriptor, incoming: bool) -> bool {
    endpoint.transfer_kind == TransferKind::Bulk && (endpoint.address & 0x80 != 0) == incoming
}

#[derive(Debug)]
pub struct UsbAoapTransport<H: UsbHandle> {
    handle: Option<H>,
    device: UsbDeviceSnapshot,
    endpoints: BulkEndpoints,
    reset_on_close: bool,
    release_on_close: bool,
    poll_interval: Duration,
}

impl<H: UsbHandle> UsbAoapTransport<H> {
    pub(crate) fn new(
        handle: H,
        device: UsbDeviceSnapshot,
        endpoints: BulkEndpoints,
        poll_interval: Duration,
    ) -> Self {
        Self {
            handle: Some(handle),
            device,
            endpoints,
            reset_on_close: true,
            release_on_close: true,
            poll_interval,
        }
    }

    pub fn device(&self) -> &UsbDeviceSnapshot {
        &self.device
    }

    pub const fn endpoints(&self) -> BulkEndpoints {
        self.endpoints
    }

    pub fn set_reset_on_close(&mut self, reset_on_close: bool) {
        self.reset_on_close = reset_on_close;
    }

    pub fn set_release_on_close(&mut self, release_on_close: bool) {
        self.release_on_close = release_on_close;
    }

    pub fn into_io(
        self,
        cancellation: CancellationToken,
        read_timeout: Duration,
        write_timeout: Duration,
    ) -> AoapIo<H> {
        AoapIo {
            transport: self,
            cancellation,
            read_timeout,
            write_timeout,
        }
    }

    pub fn read(
        &mut self,
        bytes: &mut [u8],
        timeout: Duration,
        cancellation: &CancellationToken,
    ) -> Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let operation = "reading Android accessory bulk endpoint";
        let started = Instant::now();
        let endpoint = self.endpoints.bulk_in;
        loop {
            check_cancelled(cancellation, operation)?;
            let Some(transfer_timeout) = self.transfer_timeout(started, timeout) else {
                return Err(Error::Timeout { operation, timeout });
            };
            match self
                .handle_mut()?
                .read_bulk(endpoint, bytes, transfer_timeout)
            {
                Ok(0) => {}
                Ok(length) => return Ok(length),
                Err(rusb::Error::Timeout | rusb::Error::Interrupted) => {}
                Err(rusb::Error::Pipe) => {
                    self.handle_mut()?.clear_halt(endpoint).map_err(|source| {
                        Error::usb("clearing stalled Android accessory input", source)
                    })?
                }
                Err(source) => return Err(Error::usb(operation, source)),
            }
        }
    }

    pub fn write_all(
        &mut self,
        bytes: &[u8],
        timeout: Duration,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        let operation = "writing Android accessory bulk endpoint";
        let started = Instant::now();
        let endpoint = self.endpoints.bulk_out;
        let mut offset = 0;
        while offset < bytes.len() {
            check_cancelled(cancellation, operation)?;
            let Some(transfer_timeout) = self.transfer_timeout(started, timeout) else {
                return Err(Error::Timeout { operation, timeout });
            };
            match self
                .handle_mut()?
                .write_bulk(endpoint, &bytes[offset..], transfer_timeout)
            {
                Ok(0) => return Err(Error::NoProgress { operation }),
                Ok(length) => offset += length,
                Err(rusb::Error::Timeout | rusb::Error::Interrupted) => {}
                Err(rusb::Error::Pipe) => {
                    self.handle_mut()?.clear_halt(endpoint).map_err(|source| {
                        Error::usb("clearing stalled Android accessory output", source)
                    })?
                }
                Err(source) => return Err(Error::usb(operation, source)),
            }
        }
        Ok(())
    }

    pub fn close(mut self) -> Result<()> {
        self.cleanup()
    }

    fn transfer_timeout(&self, started: Instant, timeout: Duration) -> Option<Duration> {
        let remaining = timeout.saturating_sub(started.elapsed());
        (remaining >= MIN_TRANSFER_TIMEOUT)
            .then(|| self.poll_interval.min(remaining).max(MIN_TRANSFER_TIMEOUT))
    }

    fn handle_mut(&mut self) -> Result<&mut H> {
        self.handle.as_mut().ok_or(Error::Disconnected {
            operation: "accessing closed Android accessory",
        })
    }

    fn cleanup(&mut self) -> Result<()> {
        let Some(mut handle) = self.handle.take() else {
            return Ok(());
        };
        let mut first_error = None;
        if self.release_on_close {
            if let Err(source) = handle.release_interface(self.endpoints.interface) {
                first_error = Some(Error::usb("releasing Android accessory interface", source));
            }
        }
        if self.reset_on_close {
            if let Err(source) = handle.reset() {
                if first_error.is_none() {
                    first_error = Some(Error::usb("resetting Android accessory", source));
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

impl<H: UsbHandle + Clone> UsbAoapTransport<H> {
    pub fn try_clone(&self) -> Result<Self> {
        let handle = self.handle.as_ref().cloned().ok_or(Error::Disconnected {
            operation: "cloning closed Android accessory transport",
        })?;
        Ok(Self {
            handle: Some(handle),
            device: self.device.clone(),
            endpoints: self.endpoints,
            reset_on_close: false,
            release_on_close: false,
            poll_interval: self.poll_interval,
        })
    }
}

impl<H: UsbHandle> Drop for UsbAoapTransport<H> {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

#[derive(Debug)]
pub struct AoapIo<H: UsbHandle> {
    transport: UsbAoapTransport<H>,
    cancellation: CancellationToken,
    read_timeout: Duration,
    write_timeout: Duration,
}

impl<H: UsbHandle> AoapIo<H> {
    pub fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    pub fn transport(&self) -> &UsbAoapTransport<H> {
        &self.transport
    }

    pub fn transport_mut(&mut self) -> &mut UsbAoapTransport<H> {
        &mut self.transport
    }

    pub fn into_transport(self) -> UsbAoapTransport<H> {
        self.transport
    }
}

impl<H: UsbHandle> Read for AoapIo<H> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.transport
            .read(bytes, self.read_timeout, &self.cancellation)
            .map_err(io_error)
    }
}

impl<H: UsbHandle> Write for AoapIo<H> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.transport
            .write_all(bytes, self.write_timeout, &self.cancellation)
            .map(|()| bytes.len())
            .map_err(io_error)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn io_error(error: Error) -> io::Error {
    let kind = match error {
        Error::Timeout { .. } => io::ErrorKind::TimedOut,
        Error::Cancelled { .. } => io::ErrorKind::Interrupted,
        Error::Disconnected { .. } => io::ErrorKind::BrokenPipe,
        _ => io::ErrorKind::Other,
    };
    io::Error::new(kind, error)
}
