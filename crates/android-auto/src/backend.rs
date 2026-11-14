use std::{fmt::Debug, sync::Arc, time::Duration};

use crate::{
    ConfigurationDescriptor, EndpointDescriptor, InterfaceDescriptor, Result, TransferKind,
    UsbDeviceSnapshot, UsbTopology,
};

pub trait UsbHandle: Debug + Send {
    fn control_read(
        &mut self,
        request: u8,
        value: u16,
        index: u16,
        bytes: &mut [u8],
        timeout: Duration,
    ) -> std::result::Result<usize, rusb::Error>;

    fn control_write(
        &mut self,
        request: u8,
        value: u16,
        index: u16,
        bytes: &[u8],
        timeout: Duration,
    ) -> std::result::Result<usize, rusb::Error>;

    fn set_active_configuration(
        &mut self,
        configuration: u8,
    ) -> std::result::Result<(), rusb::Error>;

    fn claim_interface(&mut self, interface: u8) -> std::result::Result<(), rusb::Error>;

    fn set_alternate_setting(
        &mut self,
        interface: u8,
        alternate_setting: u8,
    ) -> std::result::Result<(), rusb::Error>;

    fn release_interface(&mut self, interface: u8) -> std::result::Result<(), rusb::Error>;

    fn reset(&mut self) -> std::result::Result<(), rusb::Error>;

    fn clear_halt(&mut self, endpoint: u8) -> std::result::Result<(), rusb::Error>;

    fn read_bulk(
        &mut self,
        endpoint: u8,
        bytes: &mut [u8],
        timeout: Duration,
    ) -> std::result::Result<usize, rusb::Error>;

    fn write_bulk(
        &mut self,
        endpoint: u8,
        bytes: &[u8],
        timeout: Duration,
    ) -> std::result::Result<usize, rusb::Error>;
}

pub trait UsbBackend: Debug + Send {
    type Handle: UsbHandle;

    fn devices(&mut self) -> Result<Vec<UsbDeviceSnapshot>>;

    fn open(
        &mut self,
        device: &UsbDeviceSnapshot,
    ) -> std::result::Result<Self::Handle, rusb::Error>;

    fn sleep(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

#[derive(Debug, Default)]
pub struct RusbBackend;

#[derive(Debug, Clone)]
pub struct RusbHandle(Arc<rusb::DeviceHandle<rusb::GlobalContext>>);

impl UsbBackend for RusbBackend {
    type Handle = RusbHandle;

    fn devices(&mut self) -> Result<Vec<UsbDeviceSnapshot>> {
        let devices =
            rusb::devices().map_err(|source| crate::Error::usb("listing USB devices", source))?;
        let mut snapshots = Vec::new();

        for device in devices.iter() {
            let Ok(descriptor) = device.device_descriptor() else {
                continue;
            };
            let Ok(port_path) = device.port_numbers() else {
                continue;
            };
            if port_path.is_empty() {
                continue;
            }

            let active_configuration = device
                .active_config_descriptor()
                .ok()
                .map(|configuration| configuration.number());
            let mut configurations = Vec::new();
            for index in 0..descriptor.num_configurations() {
                let Ok(configuration) = device.config_descriptor(index) else {
                    continue;
                };
                let mut interfaces = Vec::new();
                for interface in configuration.interfaces() {
                    for descriptor in interface.descriptors() {
                        let endpoints = descriptor
                            .endpoint_descriptors()
                            .map(|endpoint| EndpointDescriptor {
                                address: endpoint.address(),
                                transfer_kind: if endpoint.transfer_type()
                                    == rusb::TransferType::Bulk
                                {
                                    TransferKind::Bulk
                                } else {
                                    TransferKind::Other
                                },
                            })
                            .collect();
                        interfaces.push(InterfaceDescriptor {
                            number: descriptor.interface_number(),
                            alternate_setting: descriptor.setting_number(),
                            class: descriptor.class_code(),
                            subclass: descriptor.sub_class_code(),
                            protocol: descriptor.protocol_code(),
                            endpoints,
                        });
                    }
                }
                configurations.push(ConfigurationDescriptor {
                    value: configuration.number(),
                    interfaces,
                });
            }

            snapshots.push(UsbDeviceSnapshot {
                topology: UsbTopology {
                    bus: device.bus_number(),
                    port_path,
                },
                address: device.address(),
                vendor_id: descriptor.vendor_id(),
                product_id: descriptor.product_id(),
                device_class: descriptor.class_code(),
                active_configuration,
                configurations,
            });
        }

        snapshots.sort_by(|left, right| {
            left.topology
                .cmp(&right.topology)
                .then(left.address.cmp(&right.address))
        });
        Ok(snapshots)
    }

    fn open(
        &mut self,
        expected: &UsbDeviceSnapshot,
    ) -> std::result::Result<Self::Handle, rusb::Error> {
        let devices = rusb::devices()?;
        for device in devices.iter() {
            if device.bus_number() != expected.topology.bus
                || device.port_numbers().ok().as_deref()
                    != Some(expected.topology.port_path.as_slice())
            {
                continue;
            }
            let descriptor = device.device_descriptor()?;
            if descriptor.vendor_id() != expected.vendor_id
                || descriptor.product_id() != expected.product_id
            {
                return Err(rusb::Error::NoDevice);
            }
            return device.open().map(|handle| RusbHandle(Arc::new(handle)));
        }
        Err(rusb::Error::NoDevice)
    }
}

impl UsbHandle for RusbHandle {
    fn control_read(
        &mut self,
        request: u8,
        value: u16,
        index: u16,
        bytes: &mut [u8],
        timeout: Duration,
    ) -> std::result::Result<usize, rusb::Error> {
        let request_type = rusb::request_type(
            rusb::Direction::In,
            rusb::RequestType::Vendor,
            rusb::Recipient::Device,
        );
        self.0
            .read_control(request_type, request, value, index, bytes, timeout)
    }

    fn control_write(
        &mut self,
        request: u8,
        value: u16,
        index: u16,
        bytes: &[u8],
        timeout: Duration,
    ) -> std::result::Result<usize, rusb::Error> {
        let request_type = rusb::request_type(
            rusb::Direction::Out,
            rusb::RequestType::Vendor,
            rusb::Recipient::Device,
        );
        self.0
            .write_control(request_type, request, value, index, bytes, timeout)
    }

    fn set_active_configuration(
        &mut self,
        configuration: u8,
    ) -> std::result::Result<(), rusb::Error> {
        self.0.set_active_configuration(configuration)
    }

    fn claim_interface(&mut self, interface: u8) -> std::result::Result<(), rusb::Error> {
        self.0.claim_interface(interface)
    }

    fn set_alternate_setting(
        &mut self,
        interface: u8,
        alternate_setting: u8,
    ) -> std::result::Result<(), rusb::Error> {
        self.0.set_alternate_setting(interface, alternate_setting)
    }

    fn release_interface(&mut self, interface: u8) -> std::result::Result<(), rusb::Error> {
        self.0.release_interface(interface)
    }

    fn reset(&mut self) -> std::result::Result<(), rusb::Error> {
        self.0.reset()
    }

    fn clear_halt(&mut self, endpoint: u8) -> std::result::Result<(), rusb::Error> {
        self.0.clear_halt(endpoint)
    }

    fn read_bulk(
        &mut self,
        endpoint: u8,
        bytes: &mut [u8],
        timeout: Duration,
    ) -> std::result::Result<usize, rusb::Error> {
        self.0.read_bulk(endpoint, bytes, timeout)
    }

    fn write_bulk(
        &mut self,
        endpoint: u8,
        bytes: &[u8],
        timeout: Duration,
    ) -> std::result::Result<usize, rusb::Error> {
        self.0.write_bulk(endpoint, bytes, timeout)
    }
}
