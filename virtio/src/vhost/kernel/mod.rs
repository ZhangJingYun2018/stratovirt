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

mod net;
mod vsock;

pub use net::Net;
pub use vsock::{Vsock, VsockState};

use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::sync::{Arc, Mutex};

use address_space::{
    AddressSpace, FlatRange, GuestAddress, Listener, ListenerReqType, RegionIoEventFd, RegionType,
};
use util::byte_code::ByteCode;
use util::loop_context::{
    read_fd, EventNotifier, EventNotifierHelper, NotifierCallback, NotifierOperation,
};
use vmm_sys_util::epoll::EventSet;
use vmm_sys_util::eventfd::EventFd;
use vmm_sys_util::ioctl::{ioctl, ioctl_with_mut_ref, ioctl_with_ptr, ioctl_with_ref};

use super::super::errors::{ErrorKind, Result, ResultExt};
use super::super::{QueueConfig, VirtioInterrupt, VirtioInterruptType};
use super::{VhostNotify, VhostOps};
use crate::error_chain::ChainedError;

/// Refer to VHOST_VIRTIO in
/// https://github.com/torvalds/linux/blob/master/include/uapi/linux/vhost.h.
const VHOST: u32 = 0xaf;
ioctl_ior_nr!(VHOST_GET_FEATURES, VHOST, 0x00, u64);
ioctl_iow_nr!(VHOST_SET_FEATURES, VHOST, 0x00, u64);
ioctl_io_nr!(VHOST_SET_OWNER, VHOST, 0x01);
ioctl_io_nr!(VHOST_RESET_OWNER, VHOST, 0x02);
ioctl_iow_nr!(VHOST_SET_MEM_TABLE, VHOST, 0x03, VhostMemory);
ioctl_iow_nr!(VHOST_SET_VRING_NUM, VHOST, 0x10, VhostVringState);
ioctl_iow_nr!(VHOST_SET_VRING_ADDR, VHOST, 0x11, VhostVringAddr);
ioctl_iow_nr!(VHOST_SET_VRING_BASE, VHOST, 0x12, VhostVringState);
ioctl_iowr_nr!(VHOST_GET_VRING_BASE, VHOST, 0x12, VhostVringState);
ioctl_iow_nr!(VHOST_SET_VRING_KICK, VHOST, 0x20, VhostVringFile);
ioctl_iow_nr!(VHOST_SET_VRING_CALL, VHOST, 0x21, VhostVringFile);
ioctl_iow_nr!(VHOST_NET_SET_BACKEND, VHOST, 0x30, VhostVringFile);
ioctl_iow_nr!(VHOST_VSOCK_SET_GUEST_CID, VHOST, 0x60, u64);
ioctl_iow_nr!(VHOST_VSOCK_SET_RUNNING, VHOST, 0x61, i32);

/// Refer to vhost_vring_file in
/// https://github.com/torvalds/linux/blob/master/include/uapi/linux/vhost.h.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct VhostVringFile {
    /// Vring index.
    pub index: u32,
    /// File fd.
    pub fd: RawFd,
}

/// Refer to vhost_vring_state in
/// https://github.com/torvalds/linux/blob/master/include/uapi/linux/vhost.h.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
struct VhostVringState {
    /// Vring index.
    index: u32,
    /// Vring size.
    num: u32,
}

/// Refer to vhost_vring_addr in
/// https://github.com/torvalds/linux/blob/master/include/uapi/linux/vhost.h.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
struct VhostVringAddr {
    /// Vring index.
    index: u32,
    /// Option flags.
    flags: u32,
    /// Base address of descriptor table.
    desc_user_addr: u64,
    /// Base address of used vring.
    used_user_addr: u64,
    /// Base address of available vring.
    avail_user_addr: u64,
    /// Address where to write logs.
    log_guest_addr: u64,
}

/// Refer to vhost_memory_region in
/// https://github.com/torvalds/linux/blob/master/include/uapi/linux/vhost.h.
#[repr(C)]
#[derive(Debug, Copy, Clone, Default)]
struct VhostMemoryRegion {
    /// GPA.
    guest_phys_addr: u64,
    /// Size of the memory region.
    memory_size: u64,
    /// HVA.
    userspace_addr: u64,
    /// No flags specified for now.
    flags_padding: u64,
}

impl ByteCode for VhostMemoryRegion {}

/// Refer to vhost_memory in
/// https://github.com/torvalds/linux/blob/master/include/uapi/linux/vhost.h.
#[repr(C)]
#[derive(Debug, Copy, Clone, Default)]
struct VhostMemory {
    nregions: u32,
    padding: u32,
}

impl ByteCode for VhostMemory {}

#[derive(Clone)]
struct VhostMemInfo {
    regions: Arc<Mutex<Vec<VhostMemoryRegion>>>,
}

impl VhostMemInfo {
    fn new() -> VhostMemInfo {
        VhostMemInfo {
            regions: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn addr_to_host(&self, addr: GuestAddress) -> Option<u64> {
        let addr = addr.raw_value();
        for region in self.regions.lock().unwrap().iter() {
            if addr >= region.guest_phys_addr && addr < region.guest_phys_addr + region.memory_size
            {
                let offset = addr - region.guest_phys_addr;
                return Some(region.userspace_addr + offset);
            }
        }
        None
    }

    fn check_vhost_mem_range(fr: &FlatRange) -> bool {
        fr.owner.region_type() == RegionType::Ram
    }

    fn add_mem_range(&self, fr: &FlatRange) {
        let guest_phys_addr = fr.addr_range.base.raw_value();
        let memory_size = fr.addr_range.size;
        let userspace_addr = fr.owner.get_host_address().unwrap() + fr.offset_in_region;

        self.regions.lock().unwrap().push(VhostMemoryRegion {
            guest_phys_addr,
            memory_size,
            userspace_addr,
            flags_padding: 0_u64,
        });
    }

    fn delete_mem_range(&self, fr: &FlatRange) {
        let mut mem_regions = self.regions.lock().unwrap();
        let target = VhostMemoryRegion {
            guest_phys_addr: fr.addr_range.base.raw_value(),
            memory_size: fr.addr_range.size,
            userspace_addr: fr.owner.get_host_address().unwrap() + fr.offset_in_region,
            flags_padding: 0_u64,
        };
        for (index, mr) in mem_regions.iter().enumerate() {
            if mr.guest_phys_addr == target.guest_phys_addr
                && mr.memory_size == target.memory_size
                && mr.userspace_addr == target.userspace_addr
                && mr.flags_padding == target.flags_padding
            {
                mem_regions.remove(index);
                return;
            }
        }
        debug!("Vhost: deleting mem region failed: not matched");
    }
}

impl Listener for VhostMemInfo {
    fn priority(&self) -> i32 {
        0
    }

    fn handle_request(
        &self,
        range: Option<&FlatRange>,
        _evtfd: Option<&RegionIoEventFd>,
        req_type: ListenerReqType,
    ) -> std::result::Result<(), address_space::errors::Error> {
        match req_type {
            ListenerReqType::AddRegion => {
                if Self::check_vhost_mem_range(&range.unwrap()) {
                    self.add_mem_range(range.unwrap());
                }
            }
            ListenerReqType::DeleteRegion => {
                let fr = range.unwrap();
                if fr.owner.region_type() == RegionType::Ram {
                    self.delete_mem_range(&fr);
                }
            }
            _ => {}
        }
        Ok(())
    }
}

pub struct VhostBackend {
    fd: File,
    mem_info: Arc<Mutex<VhostMemInfo>>,
}

impl VhostBackend {
    pub fn new(
        mem_space: &Arc<AddressSpace>,
        path: &str,
        rawfd: Option<RawFd>,
    ) -> Result<VhostBackend> {
        let fd = match rawfd {
            Some(rawfd) => unsafe { File::from_raw_fd(rawfd) },
            None => OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_CLOEXEC | libc::O_NONBLOCK)
                .open(path)
                .chain_err(|| format!("Failed to open {} for vhost backend.", path))?,
        };
        let mem_info = Arc::new(Mutex::new(VhostMemInfo::new()));
        mem_space.register_listener(mem_info.clone())?;

        Ok(VhostBackend { fd, mem_info })
    }
}

impl AsRawFd for VhostBackend {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

impl VhostOps for VhostBackend {
    fn set_owner(&self) -> Result<()> {
        let ret = unsafe { ioctl(self, VHOST_SET_OWNER()) };
        if ret < 0 {
            return Err(ErrorKind::VhostIoctl("VHOST_SET_OWNER".to_string()).into());
        }
        Ok(())
    }

    fn reset_owner(&self) -> Result<()> {
        let ret = unsafe { ioctl(self, VHOST_RESET_OWNER()) };
        if ret < 0 {
            return Err(ErrorKind::VhostIoctl("VHOST_RESET_OWNER".to_string()).into());
        }
        Ok(())
    }

    fn get_features(&self) -> Result<u64> {
        let mut avail_features: u64 = 0;
        let ret = unsafe { ioctl_with_mut_ref(self, VHOST_GET_FEATURES(), &mut avail_features) };
        if ret < 0 {
            return Err(ErrorKind::VhostIoctl("VHOST_GET_FEATURES".to_string()).into());
        }
        Ok(avail_features)
    }

    fn set_features(&self, features: u64) -> Result<()> {
        let ret = unsafe { ioctl_with_ref(self, VHOST_SET_FEATURES(), &features) };
        if ret < 0 {
            return Err(ErrorKind::VhostIoctl("VHOST_SET_FEATURES".to_string()).into());
        }
        Ok(())
    }

    fn set_mem_table(&self) -> Result<()> {
        let regions = self.mem_info.lock().unwrap().regions.lock().unwrap().len();
        let vm_size = std::mem::size_of::<VhostMemory>();
        let vmr_size = std::mem::size_of::<VhostMemoryRegion>();
        let mut bytes: Vec<u8> = vec![0; vm_size + regions * vmr_size];

        bytes[0..vm_size].copy_from_slice(
            VhostMemory {
                nregions: regions as u32,
                padding: 0,
            }
            .as_bytes(),
        );

        let locked_mem_info = self.mem_info.lock().unwrap();
        let locked_regions = locked_mem_info.regions.lock().unwrap();
        for (index, region) in locked_regions.iter().enumerate() {
            bytes[(vm_size + index * vmr_size)..(vm_size + (index + 1) * vmr_size)]
                .copy_from_slice(region.as_bytes());
        }

        let ret = unsafe { ioctl_with_ptr(self, VHOST_SET_MEM_TABLE(), bytes.as_ptr()) };
        if ret < 0 {
            return Err(ErrorKind::VhostIoctl("VHOST_SET_MEM_TABLE".to_string()).into());
        }
        Ok(())
    }

    fn set_vring_num(&self, queue_idx: usize, num: u16) -> Result<()> {
        let vring_state = VhostVringState {
            index: queue_idx as u32,
            num: u32::from(num),
        };
        let ret = unsafe { ioctl_with_ref(self, VHOST_SET_VRING_NUM(), &vring_state) };
        if ret < 0 {
            return Err(ErrorKind::VhostIoctl("VHOST_SET_VRING_NUM".to_string()).into());
        }
        Ok(())
    }

    fn set_vring_addr(&self, queue_config: &QueueConfig, index: usize, flags: u32) -> Result<()> {
        let locked_mem_info = self.mem_info.lock().unwrap();
        let desc_user_addr = locked_mem_info
            .addr_to_host(queue_config.desc_table)
            .ok_or_else(|| {
                ErrorKind::Msg(format!(
                    "Failed to transform desc-table address {}",
                    queue_config.desc_table.0
                ))
            })?;
        let used_user_addr = locked_mem_info
            .addr_to_host(queue_config.used_ring)
            .ok_or_else(|| {
                ErrorKind::Msg(format!(
                    "Failed to transform used ring address {}",
                    queue_config.used_ring.0
                ))
            })?;
        let avail_user_addr = locked_mem_info
            .addr_to_host(queue_config.avail_ring)
            .ok_or_else(|| {
                ErrorKind::Msg(format!(
                    "Failed to transform avail ring address {}",
                    queue_config.avail_ring.0
                ))
            })?;

        let vring_addr = VhostVringAddr {
            index: index as u32,
            flags,
            desc_user_addr,
            used_user_addr,
            avail_user_addr,
            log_guest_addr: 0_u64,
        };

        let ret = unsafe { ioctl_with_ref(self, VHOST_SET_VRING_ADDR(), &vring_addr) };
        if ret < 0 {
            return Err(ErrorKind::VhostIoctl("VHOST_SET_VRING_ADDR".to_string()).into());
        }
        Ok(())
    }

    fn set_vring_base(&self, queue_idx: usize, num: u16) -> Result<()> {
        let vring_state = VhostVringState {
            index: queue_idx as u32,
            num: u32::from(num),
        };
        let ret = unsafe { ioctl_with_ref(self, VHOST_SET_VRING_BASE(), &vring_state) };
        if ret < 0 {
            return Err(ErrorKind::VhostIoctl("VHOST_SET_VRING_BASE".to_string()).into());
        }
        Ok(())
    }

    fn get_vring_base(&self, queue_idx: usize) -> Result<u16> {
        let vring_state = VhostVringState {
            index: queue_idx as u32,
            num: 0,
        };

        let ret = unsafe { ioctl_with_ref(self, VHOST_GET_VRING_BASE(), &vring_state) };
        if ret < 0 {
            return Err(ErrorKind::VhostIoctl("VHOST_GET_VRING_BASE".to_string()).into());
        }
        Ok(vring_state.num as u16)
    }

    fn set_vring_call(&self, queue_idx: usize, fd: &EventFd) -> Result<()> {
        let vring_file = VhostVringFile {
            index: queue_idx as u32,
            fd: fd.as_raw_fd(),
        };
        let ret = unsafe { ioctl_with_ref(self, VHOST_SET_VRING_CALL(), &vring_file) };
        if ret < 0 {
            return Err(ErrorKind::VhostIoctl("VHOST_SET_VRING_CALL".to_string()).into());
        }
        Ok(())
    }

    fn set_vring_kick(&self, queue_idx: usize, fd: &EventFd) -> Result<()> {
        let vring_file = VhostVringFile {
            index: queue_idx as u32,
            fd: fd.as_raw_fd(),
        };
        let ret = unsafe { ioctl_with_ref(self, VHOST_SET_VRING_KICK(), &vring_file) };
        if ret < 0 {
            return Err(ErrorKind::VhostIoctl("VHOST_SET_VRING_KICK".to_string()).into());
        }
        Ok(())
    }
}

pub struct VhostIoHandler {
    interrupt_cb: Arc<VirtioInterrupt>,
    host_notifies: Vec<VhostNotify>,
    deactivate_evt: RawFd,
}

impl VhostIoHandler {
    fn deactivate_evt_handler(&mut self) -> Vec<EventNotifier> {
        let mut notifiers = Vec::new();
        for host_notify in self.host_notifies.iter() {
            notifiers.push(EventNotifier::new(
                NotifierOperation::Delete,
                host_notify.notify_evt.as_raw_fd(),
                None,
                EventSet::IN,
                Vec::new(),
            ));
        }

        notifiers.push(EventNotifier::new(
            NotifierOperation::Delete,
            self.deactivate_evt,
            None,
            EventSet::IN,
            Vec::new(),
        ));

        notifiers
    }
}

impl EventNotifierHelper for VhostIoHandler {
    fn internal_notifiers(vhost_handler: Arc<Mutex<Self>>) -> Vec<EventNotifier> {
        let mut notifiers = Vec::new();
        let vhost = vhost_handler.clone();

        let handler: Box<dyn Fn(EventSet, RawFd) -> Option<Vec<EventNotifier>>> =
            Box::new(move |_, fd: RawFd| {
                read_fd(fd);

                let locked_vhost_handler = vhost.lock().unwrap();

                for host_notify in locked_vhost_handler.host_notifies.iter() {
                    if let Err(e) = (locked_vhost_handler.interrupt_cb)(
                        &VirtioInterruptType::Vring,
                        Some(&host_notify.queue.lock().unwrap()),
                    ) {
                        error!(
                            "Failed to trigger interrupt for vhost device, error is {}",
                            e.display_chain()
                        );
                    }
                }

                None as Option<Vec<EventNotifier>>
            });
        let h = Arc::new(Mutex::new(handler));

        for host_notify in vhost_handler.lock().unwrap().host_notifies.iter() {
            notifiers.push(EventNotifier::new(
                NotifierOperation::AddShared,
                host_notify.notify_evt.as_raw_fd(),
                None,
                EventSet::IN,
                vec![h.clone()],
            ));
        }

        // Register event notifier for deactivate_evt.
        let vhost = vhost_handler.clone();
        let handler: Box<NotifierCallback> = Box::new(move |_, fd: RawFd| {
            read_fd(fd);
            Some(vhost.lock().unwrap().deactivate_evt_handler())
        });
        notifiers.push(EventNotifier::new(
            NotifierOperation::AddShared,
            vhost_handler.lock().unwrap().deactivate_evt,
            None,
            EventSet::IN,
            vec![Arc::new(Mutex::new(handler))],
        ));

        notifiers
    }
}
