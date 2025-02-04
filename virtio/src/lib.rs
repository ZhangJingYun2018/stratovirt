// Copyright (c) 2020 Huawei Technologies Co.,Ltd. All rights reserved.
//
// StratoVirt is licensed under Mulan PSL v2.
// You can use this software according to the terms and conditions of the Mulan
// PSL v2.
// You may obtain a copy of Mulan PSL v2 at:
//         http://license.coscl.org.cn/MulanPSL2
// THIS SOFTWARE IS PROVIDED ON AN "AS IS" BASIS, WITHOUT WARRANTIES OF ANY
// KIND, EITHER EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO
// NON-INFRINGEMENT, MERCHANTABILITY OR FIT FOR A PARTICULAR PURPOSE.
// See the Mulan PSL v2 for more details.

//! # Virtio
//!
//! This mod is used for virtio device.
//!
//! ## Design
//!
//! This module offers support for:
//! 1. Some Spec specified const variable used by virtio device.
//! 2. Virtio Device trait
//!
//! ## Platform Support
//!
//! - `x86_64`
//! - `aarch64`

#[macro_use]
extern crate error_chain;
#[macro_use]
extern crate log;
#[macro_use]
extern crate machine_manager;
#[macro_use]
extern crate vmm_sys_util;
#[macro_use]
extern crate migration_derive;

pub mod errors {
    error_chain! {
        foreign_links {
            Io(std::io::Error);
        }
        links {
            Util(util::errors::Error, util::errors::ErrorKind);
            AddressSpace(address_space::errors::Error, address_space::errors::ErrorKind);
            SysBus(sysbus::errors::Error, sysbus::errors::ErrorKind);
        }
        errors {
            EventFdCreate {
                display("Failed to create eventfd.")
            }
            EventFdWrite {
                display("Failed to write eventfd.")
            }
            ThreadCreate(name: String) {
                display("Failed to create {} thread", name)
            }
            ChannelSend(value: String) {
                display("Failed to send {} on the channel", value)
            }
            QueueIndex(index: u16, size: u16) {
                display("Queue index {} invalid, queue size is {}", index, size)
            }
            QueueDescInvalid {
                display("Vring descriptor is invalid")
            }
            AddressOverflow(value: &'static str, address: u64, offset: u64) {
                display("Address overflows for {}, address: 0x{:x}, offset: {}", value, address, offset)
            }
            DevConfigOverflow(offset: u64, size: u64) {
                display("Failed to r/w dev config space: overflows, offset {}, space size {}", offset, size)
            }
            InterruptTrigger(dev_ty: &'static str, int_type: super::VirtioInterruptType) {
                display("Failed to trigger interrupt for {}, int-type {:#?}", dev_ty, int_type)
            }
            VhostIoctl(ioctl: String) {
                display("Vhost ioctl failed: {}", ioctl)
            }
            ElementEmpty {
                display("Failed to get iovec from element!")
            }
            IncorrectQueueNum(expect: usize, actual: usize) {
                display("Cannot perform activate. Expected {} queue(s), got {}", expect, actual)
            }
            IncorrectOffset(expect: u64, actual: u64) {
                display("Incorrect offset, expected {}, got {}", expect, actual)
            }
            DeviceNotActivated(devname: String) {
                display("Device {} not activated", devname)
            }
            FailedToWriteConfig {
                display("Failed to write config")
            }
            ReadObjectErr(object: &'static str, address: u64) {
                display("Failed to read object for {}, address: 0x{:x}", object, address)
            }
            DevStatErr(status: u32) {
                display("Invalid device status: 0x{:x}.", status)
            }
            MmioRegErr(offset: u64) {
                display("Unsupported mmio register at offset 0x{:x}.", offset)
            }
        }
    }
}

mod balloon;
mod block;
mod console;
mod net;
mod queue;
mod rng;
mod vhost;
mod virtio_mmio;
#[allow(dead_code)]
mod virtio_pci;

pub use balloon::*;
pub use block::{Block, BlockState};
pub use console::{Console, VirtioConsoleState};
pub use errors::*;
pub use net::*;
pub use queue::*;
pub use rng::{Rng, RngState};
pub use vhost::kernel as VhostKern;
pub use virtio_mmio::{VirtioMmioDevice, VirtioMmioState};
pub use virtio_pci::VirtioPciDevice;

use std::sync::{Arc, Mutex};

use address_space::AddressSpace;
use machine_manager::config::ConfigCheck;
use vmm_sys_util::eventfd::EventFd;

/// Check if the bit of features is configured.
pub fn virtio_has_feature(feature: u64, fbit: u32) -> bool {
    feature & (1 << fbit) != 0
}

/// Identifier of different virtio device, refer to Virtio Spec.
pub const VIRTIO_TYPE_NET: u32 = 1;
pub const VIRTIO_TYPE_BLOCK: u32 = 2;
pub const VIRTIO_TYPE_CONSOLE: u32 = 3;
pub const VIRTIO_TYPE_RNG: u32 = 4;
pub const VIRTIO_TYPE_BALLOON: u32 = 5;
pub const VIRTIO_TYPE_VSOCK: u32 = 19;
pub const _VIRTIO_TYPE_FS: u32 = 26;

// The Status of Virtio Device.
const CONFIG_STATUS_ACKNOWLEDGE: u32 = 0x01;
const CONFIG_STATUS_DRIVER: u32 = 0x02;
const CONFIG_STATUS_DRIVER_OK: u32 = 0x04;
const CONFIG_STATUS_FEATURES_OK: u32 = 0x08;
const CONFIG_STATUS_FAILED: u32 = 0x80;

/// Feature Bits, refer to Virtio Spec.
/// Negotiating this feature indicates that the driver can use descriptors
/// with the VIRTQ_DESC_F_INDIRECT flag set.
pub const VIRTIO_F_RING_INDIRECT_DESC: u32 = 28;
/// This feature enables the used_event and the avail_event fields.
pub const VIRTIO_F_RING_EVENT_IDX: u32 = 29;
/// Indicates compliance with Virtio Spec.
pub const VIRTIO_F_VERSION_1: u32 = 32;
/// This feature indicates that the device can be used on a platform
/// where device access to data in memory is limited and/or translated.
pub const VIRTIO_F_ACCESS_PLATFORM: u32 = 33;
/// This feature indicates support for the packed virtqueue layout.
pub const VIRTIO_F_RING_PACKED: u32 = 34;

/// Device handles packets with partial checksum.
pub const VIRTIO_NET_F_CSUM: u32 = 0;
/// Driver handles packets with partial checksum.
pub const VIRTIO_NET_F_GUEST_CSUM: u32 = 1;
/// Device has given MAC address.
pub const VIRTIO_NET_F_MAC: u32 = 5;
/// Driver can receive TSOv4.
pub const VIRTIO_NET_F_GUEST_TSO4: u32 = 7;
/// Driver can receive UFO.
pub const VIRTIO_NET_F_GUEST_UFO: u32 = 10;
/// Device can receive TSOv4.
pub const VIRTIO_NET_F_HOST_TSO4: u32 = 11;
/// Device can receive UFO.
pub const VIRTIO_NET_F_HOST_UFO: u32 = 14;
/// Configuration cols and rows are valid.
pub const VIRTIO_CONSOLE_F_SIZE: u64 = 0;
/// Maximum size of any single segment is in size_max.
pub const VIRTIO_BLK_F_SIZE_MAX: u32 = 1;
/// Maximum number of segments in a request is in seg_max.
pub const VIRTIO_BLK_F_SEG_MAX: u32 = 2;
/// Device is read-only.
pub const VIRTIO_BLK_F_RO: u32 = 5;
/// Cache flush command support.
pub const VIRTIO_BLK_F_FLUSH: u32 = 9;

/// The IO type of virtio block, refer to Virtio Spec.
/// Read.
pub const VIRTIO_BLK_T_IN: u32 = 0;
/// Write.
pub const VIRTIO_BLK_T_OUT: u32 = 1;
/// Flush.
pub const VIRTIO_BLK_T_FLUSH: u32 = 4;
/// Device id
pub const VIRTIO_BLK_T_GET_ID: u32 = 8;
/// Device id length
pub const VIRTIO_BLK_ID_BYTES: u32 = 20;
/// Success
pub const VIRTIO_BLK_S_OK: u32 = 0;

/// Interrupt status: Used Buffer Notification
pub const VIRTIO_MMIO_INT_VRING: u32 = 0x01;
/// Interrupt status: Configuration Change Notification
pub const VIRTIO_MMIO_INT_CONFIG: u32 = 0x02;

/// The offset between notify reg's address and base MMIO address
/// Guest OS uses notify reg to notify the VMM.
pub const NOTIFY_REG_OFFSET: u32 = 0x50;

/// Packet header, refer to Virtio Spec.
#[repr(C)]
#[derive(Debug, Default, Copy, Clone, PartialEq)]
pub struct VirtioNetHdr {
    pub flags: u8,
    pub gso_type: u8,
    pub hdr_len: u16,
    pub gso_size: u16,
    pub csum_start: u16,
    pub csum_offset: u16,
    pub num_buffers: u16,
}

#[derive(Debug)]
pub enum VirtioInterruptType {
    Config,
    Vring,
}

pub type VirtioInterrupt =
    Box<dyn Fn(&VirtioInterruptType, Option<&Queue>) -> Result<()> + Send + Sync>;

/// The trait for virtio device operations.
pub trait VirtioDevice: Send {
    /// Realize low level device.
    fn realize(&mut self) -> Result<()>;

    /// Unrealize low level device
    fn unrealize(&mut self) -> Result<()> {
        bail!("Unrealize of the virtio device is not implemented");
    }

    /// Get the virtio device type, refer to Virtio Spec.
    fn device_type(&self) -> u32;

    /// Get the count of virtio device queues.
    fn queue_num(&self) -> usize;

    /// Get the queue size of virtio device.
    fn queue_size(&self) -> u16;

    /// Get device features from host.
    fn get_device_features(&self, features_select: u32) -> u32;

    /// Set driver features by guest.
    fn set_driver_features(&mut self, page: u32, value: u32);

    /// Read data of config from guest.
    fn read_config(&self, offset: u64, data: &mut [u8]) -> Result<()>;

    /// Write data to config from guest.
    fn write_config(&mut self, offset: u64, data: &[u8]) -> Result<()>;

    /// Activate the virtio device, this function is called by vcpu thread when frontend
    /// virtio driver is ready and write `DRIVER_OK` to backend.
    ///
    /// # Arguments
    ///
    /// * `mem_space` - System mem.
    /// * `interrupt_evt` - The eventfd used to send interrupt to guest.
    /// * `interrupt_status` - The interrupt status present to guest.
    /// * `queues` - The virtio queues.
    /// * `queue_evts` - The notifier events from guest.
    fn activate(
        &mut self,
        mem_space: Arc<AddressSpace>,
        interrupt_cb: Arc<VirtioInterrupt>,
        queues: &[Arc<Mutex<Queue>>],
        queue_evts: Vec<EventFd>,
    ) -> Result<()>;

    /// Deactivate virtio device, this function remove event fd
    /// of device out of the event loop.
    fn deactivate(&mut self) -> Result<()> {
        bail!(
            "Reset this device is not supported, virtio dev type is {}",
            self.device_type()
        );
    }

    /// Reset virtio device.
    fn reset(&mut self) -> Result<()> {
        Ok(())
    }

    /// Update the low level config of MMIO device,
    /// for example: update the images file fd of virtio block device.
    ///
    /// # Arguments
    ///
    /// * `_file_path` - The related backend file path.
    fn update_config(&mut self, _dev_config: Option<Arc<dyn ConfigCheck>>) -> Result<()> {
        bail!("Unsupported to update configuration")
    }
}
