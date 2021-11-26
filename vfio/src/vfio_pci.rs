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

use std::collections::HashMap;
use std::mem::size_of;
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex, Weak};

use address_space::{AddressSpace, FileBackend, GuestAddress, HostMemMapping, Region, RegionOps};
use byteorder::{ByteOrder, LittleEndian};
use error_chain::ChainedError;
use hypervisor::{MsiVector, KVM_FDS};
#[cfg(target_arch = "aarch64")]
use pci::config::SECONDARY_BUS_NUM;
use pci::config::{
    PciConfig, RegionType, BAR_0, BAR_5, BAR_IO_SPACE, BAR_MEM_64BIT, BAR_SPACE_UNMAPPED, COMMAND,
    COMMAND_BUS_MASTER, COMMAND_INTERRUPT_DISABLE, COMMAND_IO_SPACE, COMMAND_MEMORY_SPACE,
    HEADER_TYPE, IO_BASE_ADDR_MASK, MEM_BASE_ADDR_MASK, PCIE_CONFIG_SPACE_SIZE, REG_SIZE,
};
use pci::errors::Result as PciResult;
use pci::msix::{
    is_msix_enabled, update_dev_id, Msix, MSIX_CAP_CONTROL, MSIX_CAP_ENABLE, MSIX_CAP_FUNC_MASK,
    MSIX_CAP_ID, MSIX_CAP_SIZE, MSIX_CAP_TABLE, MSIX_TABLE_BIR, MSIX_TABLE_ENTRY_SIZE,
    MSIX_TABLE_OFFSET, MSIX_TABLE_SIZE_MAX,
};
use pci::{
    init_multifunction, le_read_u16, le_read_u32, le_write_u16, le_write_u32, ranges_overlap,
    PciBus, PciDevOps,
};
use util::unix::host_page_size;
use vfio_bindings::bindings::vfio;
use vmm_sys_util::eventfd::EventFd;
use vmm_sys_util::ioctl::ioctl_with_mut_ref;

use super::errors::{ErrorKind, Result, ResultExt};
use crate::vfio_dev::*;
use crate::{CONTAINERS, GROUPS};

const PCI_NUM_BARS: u8 = 6;
const PCI_ROM_SLOT: u8 = 6;

struct MsixTable {
    table_bar: u8,
    table_offset: u64,
    table_size: u64,
}

struct VfioMsixInfo {
    // Table bar, table offset and table size info.
    table: MsixTable,
    // Msix enteries.
    enteries: u16,
    // Vfio device irq info
    #[allow(dead_code)]
    vfio_irq: HashMap<u32, VfioIrq>,
}

struct VfioBar {
    vfio_region: VfioRegion,
    region_type: RegionType,
    size: u64,
}

struct GsiMsiRoute {
    irq_fd: Option<EventFd>,
    gsi: i32,
}

/// VfioPciDevice is a VFIO PCI device. It implements PciDevOps trait for a PCI device.
/// And it is bound to a VFIO device.
pub struct VfioPciDevice {
    pci_config: PciConfig,
    config_size: u64,
    // Offset of pci config space region within vfio device fd.
    config_offset: u64,
    // Vfio device which is bound to.
    vfio_device: Arc<Mutex<VfioDevice>>,
    // Cache of MSI-X setup.
    msix_info: Option<VfioMsixInfo>,
    // Bars information without ROM.
    vfio_bars: Arc<Mutex<Vec<VfioBar>>>,
    // Maintains a list of GSI with irqfds that are registered to kvm.
    gsi_msi_routes: Arc<Mutex<Vec<GsiMsiRoute>>>,
    devfn: u8,
    dev_id: Arc<AtomicU16>,
    name: String,
    parent_bus: Weak<Mutex<PciBus>>,
    // Multi-Function flag.
    multi_func: bool,
    mem_as: Arc<AddressSpace>,
}

impl VfioPciDevice {
    /// New a VFIO PCI device structure for the vfio device created by VMM.
    pub fn new(
        vfio_device: Arc<Mutex<VfioDevice>>,
        devfn: u8,
        name: String,
        parent_bus: Weak<Mutex<PciBus>>,
        multi_func: bool,
        mem_as: Arc<AddressSpace>,
    ) -> Self {
        Self {
            // Unknown PCI or PCIe type here, allocate enough space to match the two types.
            pci_config: PciConfig::new(PCIE_CONFIG_SPACE_SIZE, PCI_NUM_BARS),
            config_size: 0,
            config_offset: 0,
            vfio_device,
            msix_info: None,
            vfio_bars: Arc::new(Mutex::new(Vec::with_capacity(PCI_NUM_BARS as usize))),
            gsi_msi_routes: Arc::new(Mutex::new(Vec::new())),
            devfn,
            dev_id: Arc::new(AtomicU16::new(0)),
            name,
            parent_bus,
            multi_func,
            mem_as,
        }
    }

    fn get_pci_config(&mut self) -> Result<()> {
        let argsz: u32 = size_of::<vfio::vfio_region_info>() as u32;
        let mut info = vfio::vfio_region_info {
            argsz,
            flags: 0,
            index: vfio::VFIO_PCI_CONFIG_REGION_INDEX,
            cap_offset: 0,
            size: 0,
            offset: 0,
        };

        let locked_dev = self.vfio_device.lock().unwrap();
        // Safe as device is the owner of file, and we will verify the result is valid.
        let ret =
            unsafe { ioctl_with_mut_ref(&locked_dev.fd, VFIO_DEVICE_GET_REGION_INFO(), &mut info) };
        if ret < 0 {
            return Err(ErrorKind::VfioIoctl(
                "VFIO_GET_PCI_CONFIG_INFO".to_string(),
                std::io::Error::last_os_error(),
            )
            .into());
        }

        self.config_size = info.size;
        self.config_offset = info.offset;
        let mut config_data = vec![0_u8; self.config_size as usize];
        locked_dev.read_region(config_data.as_mut_slice(), self.config_offset, 0)?;
        self.pci_config.config = config_data;

        Ok(())
    }

    /// Disable I/O, MMIO, bus master and INTx states, And clear host device bar size information.
    /// Guest OS can get residual addresses from the host if not clear bar size.
    fn pci_config_reset(&mut self) -> Result<()> {
        let mut cmd = le_read_u16(&self.pci_config.config, COMMAND as usize)?;
        cmd &= !(COMMAND_IO_SPACE
            | COMMAND_MEMORY_SPACE
            | COMMAND_BUS_MASTER
            | COMMAND_INTERRUPT_DISABLE);
        le_write_u16(&mut self.pci_config.config, COMMAND as usize, cmd)?;

        let mut data = vec![0u8; 2];
        LittleEndian::write_u16(&mut data, cmd);
        self.vfio_device.lock().unwrap().write_region(
            data.as_slice(),
            self.config_offset,
            COMMAND as u64,
        )?;

        for i in 0..PCI_ROM_SLOT {
            let offset = BAR_0 as usize + REG_SIZE * i as usize;
            let v = le_read_u32(&self.pci_config.config, offset)?;
            if v & BAR_IO_SPACE as u32 != 0 {
                le_write_u32(&mut self.pci_config.config, offset, v & !IO_BASE_ADDR_MASK)?;
            } else {
                le_write_u32(
                    &mut self.pci_config.config,
                    offset,
                    v & !MEM_BASE_ADDR_MASK as u32,
                )?;
            }
        }

        Ok(())
    }

    /// Get MSI-X table, vfio_irq and entry information from vfio device.
    fn get_msix_info(&mut self) -> Result<VfioMsixInfo> {
        let locked_dev = self.vfio_device.lock().unwrap();
        let n = locked_dev.dev_info.num_irqs;
        let vfio_irq = locked_dev
            .get_irqs_info(n)
            .chain_err(|| "Failed to get vfio irqs info")?;

        let cap_offset = self.pci_config.find_pci_cap(MSIX_CAP_ID);
        let table = le_read_u32(
            &self.pci_config.config,
            cap_offset + MSIX_CAP_TABLE as usize,
        )?;

        let ctrl = le_read_u16(
            &self.pci_config.config,
            cap_offset + MSIX_CAP_CONTROL as usize,
        )?;
        let enteries = (ctrl & MSIX_TABLE_SIZE_MAX) + 1;
        // Make sure that if enteries less than 1 or greater than (0x7ff + 1) is error value.
        if !(1..=(MSIX_TABLE_SIZE_MAX + 1)).contains(&enteries) {
            bail!(
                "The number of MSI-X vectors is invalid, MSI-X vectors are {}",
                enteries,
            );
        }

        Ok(VfioMsixInfo {
            table: MsixTable {
                table_bar: (table as u16 & MSIX_TABLE_BIR) as u8,
                table_offset: (table & MSIX_TABLE_OFFSET) as u64,
                table_size: (enteries * MSIX_TABLE_ENTRY_SIZE) as u64,
            },
            enteries: enteries as u16,
            vfio_irq,
        })
    }

    /// Get vfio bars information. Vfio device won't allow to mmap the MSI-X table area,
    /// we need to separate MSI-X table area and region mmap area.
    fn bar_region_info(&mut self) -> Result<Vec<VfioBar>> {
        let mut vfio_bars: Vec<VfioBar> = Vec::new();
        let locked_dev = self.vfio_device.lock().unwrap();
        let mut infos = locked_dev
            .get_regions_info()
            .chain_err(|| "Failed get vfio device regions info")?;

        for i in 0..PCI_ROM_SLOT {
            let mut data = vec![0_u8; 4];
            locked_dev.read_region(
                data.as_mut_slice(),
                self.config_offset,
                (BAR_0 + (REG_SIZE as u8) * i) as u64,
            )?;
            let mut region_type = RegionType::Mem32Bit;
            let pci_bar = LittleEndian::read_u32(&data);
            if pci_bar & BAR_IO_SPACE as u32 != 0 {
                region_type = RegionType::Io;
            } else if pci_bar & BAR_MEM_64BIT as u32 != 0 {
                region_type = RegionType::Mem64Bit;
            }
            let vfio_region = infos.remove(0);
            let size = vfio_region.size;

            vfio_bars.push(VfioBar {
                vfio_region,
                region_type,
                size,
            });
        }

        self.fixup_msix_region(&mut vfio_bars)?;

        Ok(vfio_bars)
    }

    fn fixup_msix_region(&self, vfio_bars: &mut Vec<VfioBar>) -> Result<()> {
        let msix_info = self
            .msix_info
            .as_ref()
            .chain_err(|| "Failed to get MSIX info")?;

        let vfio_bar = vfio_bars
            .get_mut(msix_info.table.table_bar as usize)
            .chain_err(|| "Failed to get vfio bar info")?;
        let region = &mut vfio_bar.vfio_region;
        // If MSI-X area already setups or does not support mapping, we shall just return.
        if region.mmaps.len() != 1
            || region.mmaps[0].offset != 0
            || region.size != region.mmaps[0].size
        {
            return Ok(());
        }

        // Align MSI-X table start and end to host page size.
        let page_size = host_page_size();
        let start: u64 = ((msix_info.table.table_offset as i64) & (0 - page_size as i64)) as u64;
        let end: u64 = (((msix_info.table.table_offset + msix_info.table.table_size)
            + (page_size - 1)) as i64
            & (0 - page_size as i64)) as u64;

        // The remaining area of the BAR before or after MSI-X table is remappable.
        if start == 0 {
            if end >= region.size {
                region.mmaps.clear();
            } else {
                region.mmaps[0].offset = end;
                region.mmaps[0].size = region.size - end;
            }
        } else if end >= region.size {
            region.mmaps[0].size = start;
        } else {
            region.mmaps[0].offset = 0;
            region.mmaps[0].size = start;
            region.mmaps.push(MmapInfo {
                offset: end,
                size: region.size - end,
            });
        }

        Ok(())
    }

    fn register_bars(&mut self) -> Result<()> {
        let msix_info = self
            .msix_info
            .as_ref()
            .chain_err(|| "Failed to get MSIX info")?;
        let table_bar = msix_info.table.table_bar;
        let table_offset = msix_info.table.table_offset;
        let table_size = msix_info.table.table_size;
        // Create a separate region for MSI-X table, VFIO won't allow to map the MSI-X table area.
        let table_ops = self
            .get_table_region_ops()
            .chain_err(|| "Failed to get table region ops")?;
        let bar_ops = self.get_bar_region_ops();

        for i in 0..PCI_ROM_SLOT {
            {
                let mut bars = self.vfio_bars.lock().unwrap();
                let bar = bars
                    .get_mut(i as usize)
                    .chain_err(|| "Failed to get bar info")?;
                // Skip unimplemented bar and the upper half of 64 bit bar.
                if bar.size == 0 {
                    continue;
                }
            }

            let mut vfio_bars = self.vfio_bars.lock().unwrap();
            let vfio_bar = vfio_bars
                .get_mut(i as usize)
                .chain_err(|| "Failed to get vfio bar info")?;
            let size = vfio_bar.size;

            let region = Region::init_container_region(size);
            let bar_region = if i == table_bar {
                region
                    .add_subregion(
                        Region::init_io_region(table_size as u64, table_ops.clone()),
                        table_offset,
                    )
                    .chain_err(|| ErrorKind::AddRegBar(i as usize))?;

                if table_offset > 0 {
                    region
                        .add_subregion(Region::init_io_region(table_offset, bar_ops.clone()), 0)
                        .chain_err(|| ErrorKind::AddRegBar(i as usize))?;
                }

                if table_offset + table_size < size {
                    region
                        .add_subregion(
                            Region::init_io_region(
                                size - table_offset - table_size,
                                bar_ops.clone(),
                            ),
                            table_offset + table_size,
                        )
                        .chain_err(|| ErrorKind::AddRegBar(i as usize))?;
                }
                region
            } else {
                region
                    .add_subregion(Region::init_io_region(size, bar_ops.clone()), 0)
                    .chain_err(|| ErrorKind::AddRegBar(i as usize))?;
                region
            };

            self.pci_config
                .register_bar(i as usize, bar_region, vfio_bar.region_type, false, size);
        }

        Ok(())
    }

    fn unregister_bars(&mut self) -> Result<()> {
        let bus = self.parent_bus.upgrade().unwrap();
        self.pci_config.unregister_bars(&bus)?;
        Ok(())
    }

    /// Create region ops for MSI-X table.
    fn get_table_region_ops(&mut self) -> Result<RegionOps> {
        let msix_info = self
            .msix_info
            .as_ref()
            .chain_err(|| "Failed to get MSIX info")?;
        let table_size = msix_info.table.table_size as u32;
        let cap_offset = self.pci_config.find_pci_cap(MSIX_CAP_ID);

        let offset: usize = cap_offset + MSIX_CAP_CONTROL as usize;
        le_write_u16(
            &mut self.pci_config.write_mask,
            offset,
            MSIX_CAP_FUNC_MASK | MSIX_CAP_ENABLE,
        )?;
        let msix = Arc::new(Mutex::new(Msix::new(
            table_size,
            table_size / 128,
            cap_offset as u16,
            self.dev_id.load(Ordering::Acquire),
        )));
        self.pci_config.msix = Some(msix.clone());

        let cloned_msix = msix.clone();
        let read = move |data: &mut [u8], _: GuestAddress, offset: u64| -> bool {
            data.copy_from_slice(
                &cloned_msix.lock().unwrap().table[offset as usize..(offset as usize + data.len())],
            );
            true
        };

        let cloned_dev = self.vfio_device.clone();
        let cloned_gsi_routes = self.gsi_msi_routes.clone();
        let parent_bus = self.parent_bus.clone();
        let dev_id = self.dev_id.clone();
        let devfn = self.devfn;
        let write = move |data: &[u8], _: GuestAddress, offset: u64| -> bool {
            let mut locked_msix = msix.lock().unwrap();
            locked_msix.table[offset as usize..(offset as usize + data.len())]
                .copy_from_slice(&data);
            let vector = offset / MSIX_TABLE_ENTRY_SIZE as u64;
            if locked_msix.is_vector_masked(vector as u16) {
                return true;
            }
            let entry = locked_msix.get_message(vector as u16);

            update_dev_id(&parent_bus, devfn, &dev_id);
            let msix_vector = MsiVector {
                msg_addr_lo: entry.address_lo,
                msg_addr_hi: entry.address_hi,
                msg_data: entry.data,
                masked: false,
                #[cfg(target_arch = "aarch64")]
                dev_id: dev_id.load(Ordering::Acquire) as u32,
            };

            let mut locked_gsi_routes = cloned_gsi_routes.lock().unwrap();
            let mut gsi_route = locked_gsi_routes.get_mut(vector as usize).unwrap();
            if gsi_route.irq_fd.is_none() {
                let irq_fd = EventFd::new(libc::EFD_NONBLOCK).unwrap();
                gsi_route.irq_fd = Some(irq_fd);
            }
            if gsi_route.gsi == -1 {
                gsi_route.gsi = match KVM_FDS
                    .load()
                    .irq_route_table
                    .lock()
                    .unwrap()
                    .allocate_gsi()
                {
                    Ok(g) => g as i32,
                    Err(e) => {
                        error!("Failed to allocate gsi, error is {}", e);
                        return true;
                    }
                };

                KVM_FDS
                    .load()
                    .irq_route_table
                    .lock()
                    .unwrap()
                    .add_msi_route(gsi_route.gsi as u32, msix_vector)
                    .unwrap_or_else(|e| error!("Failed to add MSI-X route, error is {}", e));
                KVM_FDS
                    .load()
                    .commit_irq_routing()
                    .unwrap_or_else(|e| error!("{}", e));
                KVM_FDS
                    .load()
                    .register_irqfd(gsi_route.irq_fd.as_ref().unwrap(), gsi_route.gsi as u32)
                    .unwrap_or_else(|e| error!("{}", e));
            } else {
                KVM_FDS
                    .load()
                    .irq_route_table
                    .lock()
                    .unwrap()
                    .update_msi_route(gsi_route.gsi as u32, msix_vector)
                    .unwrap_or_else(|e| error!("Failed to update MSI-X route, error is {}", e));
                KVM_FDS
                    .load()
                    .commit_irq_routing()
                    .unwrap_or_else(|e| error!("{}", e));
            }

            let mut locked_dev = cloned_dev.lock().unwrap();
            locked_dev
                .disable_irqs()
                .unwrap_or_else(|e| error!("Failed to disable irq, error is {}", e));
            locked_dev
                .enable_irqs(get_irq_rawfds(&locked_gsi_routes))
                .unwrap_or_else(|e| error!("Failed to enable irq, error is {}", e));
            true
        };

        Ok(RegionOps {
            read: Arc::new(read),
            write: Arc::new(write),
        })
    }

    /// Create region ops for BARs.
    fn get_bar_region_ops(&self) -> RegionOps {
        let cloned_dev = self.vfio_device.clone();
        let cloned_bars = self.vfio_bars.clone();
        let read = move |data: &mut [u8], addr: GuestAddress, offset: u64| -> bool {
            for locked_bar in cloned_bars.lock().unwrap().iter() {
                if locked_bar.size == 0 {
                    continue;
                }

                let r = &locked_bar.vfio_region;
                if r.guest_phys_addr != 0
                    && addr.0 >= r.guest_phys_addr
                    && addr.0 < (r.guest_phys_addr + r.size)
                {
                    if let Err(e) =
                        cloned_dev
                            .lock()
                            .unwrap()
                            .read_region(data, r.region_offset, offset)
                    {
                        error!(
                            "Failed to read bar region, address is {}, offset is {}, error is {}",
                            addr.0, offset, e,
                        );
                    }
                    return true;
                }
            }
            true
        };

        let cloned_dev = self.vfio_device.clone();
        let cloned_bars = self.vfio_bars.clone();
        let write = move |data: &[u8], addr: GuestAddress, offset: u64| -> bool {
            for locked_bar in cloned_bars.lock().unwrap().iter() {
                if locked_bar.size == 0 {
                    continue;
                }

                let r = &locked_bar.vfio_region;
                if r.guest_phys_addr != 0
                    && addr.0 >= r.guest_phys_addr
                    && addr.0 < (r.guest_phys_addr + r.size)
                {
                    if let Err(e) =
                        cloned_dev
                            .lock()
                            .unwrap()
                            .write_region(data, r.region_offset, offset)
                    {
                        error!(
                            "Failed to write bar region, address is {}, offset is {}, error is {}",
                            addr.0, offset, e,
                        );
                    }
                    return true;
                }
            }
            true
        };

        RegionOps {
            read: Arc::new(read),
            write: Arc::new(write),
        }
    }

    /// Avoid VM exits when guest OS read or write device MMIO regions, it maps bar regions into
    /// the guest OS.
    fn setup_bars_mmap(&mut self) -> Result<()> {
        for i in vfio::VFIO_PCI_BAR0_REGION_INDEX..vfio::VFIO_PCI_ROM_REGION_INDEX {
            let gpa = self.pci_config.get_bar_address(i as usize);
            if gpa == BAR_SPACE_UNMAPPED || gpa == 0 {
                continue;
            }

            let mut bars = self.vfio_bars.lock().unwrap();
            let bar = bars
                .get_mut(i as usize)
                .chain_err(|| "Failed to get bar info")?;
            let region = &mut bar.vfio_region;
            // If bar region already setups or does not support mapping, just process the nest.
            if region.size == 0 || region.mmaps.is_empty() || region.guest_phys_addr == gpa {
                continue;
            } else {
                region.guest_phys_addr = gpa;
            }

            let mut read_only = true;
            if region.flags & vfio::VFIO_REGION_INFO_FLAG_WRITE != 0 {
                read_only = false;
            }

            for mmap in region.mmaps.iter() {
                let dev = self.vfio_device.lock().unwrap().fd.try_clone().unwrap();
                let fb = Some(FileBackend {
                    file: Arc::new(dev),
                    offset: region.region_offset + mmap.offset,
                    page_size: host_page_size(),
                });
                let host_mmap = HostMemMapping::new(
                    GuestAddress(gpa + mmap.offset),
                    None,
                    mmap.size,
                    fb,
                    false,
                    true,
                    read_only,
                )?;

                let ram_device = Region::init_ram_device_region(Arc::new(host_mmap));
                let bar = self
                    .pci_config
                    .bars
                    .get_mut(i as usize)
                    .chain_err(|| "Failed to get pci bar info")?;
                bar.region
                    .as_ref()
                    .unwrap()
                    .add_subregion(ram_device, mmap.offset)
                    .chain_err(|| ErrorKind::AddRegBar(i as usize))?;
            }
        }
        Ok(())
    }

    fn vfio_enable_msix(&mut self) -> Result<()> {
        let mut gsi_routes = self.gsi_msi_routes.lock().unwrap();
        if gsi_routes.len() == 0 {
            let irq_fd = EventFd::new(libc::EFD_NONBLOCK).unwrap();
            let gsi_route = GsiMsiRoute {
                irq_fd: Some(irq_fd),
                gsi: -1,
            };
            gsi_routes.push(gsi_route);

            let entries = self.msix_info.as_ref().unwrap().enteries;
            for _ in 1..entries {
                let gsi_route = GsiMsiRoute {
                    irq_fd: None,
                    gsi: -1,
                };
                gsi_routes.push(gsi_route);
            }
        }
        // Register a vector of irqfd to kvm interrupts. If one of the device interrupt vector is
        // triggered, the corresponding irqfd is written, and interrupt is injected into VM finally.
        self.vfio_device
            .lock()
            .unwrap()
            .enable_irqs(get_irq_rawfds(&gsi_routes))
            .chain_err(|| "Failed enable irqfds in kvm")?;

        Ok(())
    }

    fn vfio_disable_msix(&mut self) -> Result<()> {
        self.vfio_device
            .lock()
            .unwrap()
            .disable_irqs()
            .chain_err(|| "Failed disable irqfds in kvm")?;
        let routes = self.gsi_msi_routes.lock().unwrap();
        for route in routes.iter() {
            if let Some(fd) = &route.irq_fd.as_ref() {
                KVM_FDS.load().unregister_irqfd(fd, route.gsi as u32)?;
                KVM_FDS
                    .load()
                    .irq_route_table
                    .lock()
                    .unwrap()
                    .release_gsi(route.gsi as u32)?;
            }
        }
        Ok(())
    }

    fn unrealize(&mut self) -> Result<()> {
        self.vfio_disable_msix()?;
        self.unregister_bars()?;

        let locked_dev = self.vfio_device.lock().unwrap();
        let group = locked_dev.group.upgrade().unwrap();
        let mut devices = group.devices.lock().unwrap();
        devices.remove(&locked_dev.fd.as_raw_fd());
        if devices.is_empty() {
            group.del_from_kvm_device()?;
            GROUPS.lock().unwrap().remove(&group.id);

            let container = locked_dev.container.upgrade().unwrap();
            let locked_container = container.lock().unwrap();
            let container_fd = locked_container.fd.as_raw_fd();
            let mut groups = locked_container.groups.lock().unwrap();
            groups.remove(&group.id);
            if groups.is_empty() {
                drop(groups);
                drop(locked_container);
                self.mem_as.unregister_listener(container.clone())?;
                CONTAINERS.lock().unwrap().remove(&container_fd);
            }
        }
        Ok(())
    }
}

impl PciDevOps for VfioPciDevice {
    fn init_write_mask(&mut self) -> PciResult<()> {
        self.pci_config.init_common_write_mask()
    }

    fn init_write_clear_mask(&mut self) -> PciResult<()> {
        self.pci_config.init_common_write_clear_mask()
    }

    fn realize(mut self) -> PciResult<()> {
        use pci::errors::ResultExt as PciResultExt;

        self.init_write_mask()?;
        self.init_write_clear_mask()?;
        PciResultExt::chain_err(self.vfio_device.lock().unwrap().reset(), || {
            "Failed to reset vfio device"
        })?;

        PciResultExt::chain_err(self.get_pci_config(), || {
            "Failed to get vfio device pci config space"
        })?;
        PciResultExt::chain_err(self.pci_config_reset(), || {
            "Failed to reset vfio device pci config space"
        })?;
        PciResultExt::chain_err(
            init_multifunction(
                self.multi_func,
                &mut self.pci_config.config,
                self.devfn,
                self.parent_bus.clone(),
            ),
            || "Failed to init vfio device multifunction.",
        )?;

        #[cfg(target_arch = "aarch64")]
        {
            let bus_num = self
                .parent_bus
                .upgrade()
                .unwrap()
                .lock()
                .unwrap()
                .number(SECONDARY_BUS_NUM as usize);
            self.dev_id = Arc::new(AtomicU16::new(self.set_dev_id(bus_num, self.devfn)));
        }

        self.msix_info = Some(PciResultExt::chain_err(self.get_msix_info(), || {
            "Failed to get MSI-X info"
        })?);
        self.vfio_bars = Arc::new(Mutex::new(PciResultExt::chain_err(
            self.bar_region_info(),
            || "Failed to get bar region info",
        )?));
        PciResultExt::chain_err(self.register_bars(), || "Failed to register bars")?;

        let devfn = self.devfn;
        let dev = Arc::new(Mutex::new(self));
        let pci_bus = dev.lock().unwrap().parent_bus.upgrade().unwrap();
        let mut locked_pci_bus = pci_bus.lock().unwrap();
        let pci_device = locked_pci_bus.devices.get(&devfn);
        if pci_device.is_none() {
            locked_pci_bus.devices.insert(devfn, dev);
        } else {
            bail!(
                "Devfn {:?} has been used by {:?}",
                &devfn,
                pci_device.unwrap().lock().unwrap().name()
            );
        }

        Ok(())
    }

    fn unrealize(&mut self) -> PciResult<()> {
        if let Err(e) = VfioPciDevice::unrealize(self) {
            error!("{}", e.display_chain());
            bail!("Failed to unrealize vfio-pci.");
        }
        Ok(())
    }

    fn devfn(&self) -> Option<u8> {
        Some(self.devfn)
    }

    /// Read pci data from pci config if it emulate, otherwise read from vfio device.
    fn read_config(&self, offset: usize, data: &mut [u8]) {
        let size = data.len();
        let end = offset + size;
        if end > (self.config_size as usize) || size > 4 {
            error!(
                "Failed to read pci config space at offset {} with data size {}",
                offset, size
            );
            return;
        }

        // BAR and header_type are always controlled by StratoVirt.
        if ranges_overlap(offset, end, BAR_0 as usize, (BAR_5 as usize) + REG_SIZE)
            || ranges_overlap(
                offset,
                end,
                HEADER_TYPE as usize,
                (HEADER_TYPE as usize) + 2,
            )
        {
            self.pci_config.read(offset, data);
            return;
        }

        if let Err(e) =
            self.vfio_device
                .lock()
                .unwrap()
                .read_region(data, self.config_offset, offset as u64)
        {
            error!("Failed to read device pci config, error is {}", e);
            return;
        }
        for (i, data) in data.iter_mut().enumerate().take(size) {
            if i + offset == 0x3d {
                // Clear INIx
                *data &= 0;
            }
        }
    }

    /// Write data to pci config and vfio device at the same time
    fn write_config(&mut self, offset: usize, data: &[u8]) {
        let size = data.len();
        let end = offset + size;
        if end > (self.config_size as usize) || size > 4 {
            error!(
                "Failed to write pci config space at offset {} with data size {}",
                offset, size
            );
            return;
        }

        // Let vfio device filter data to write.
        if let Err(e) =
            self.vfio_device
                .lock()
                .unwrap()
                .write_region(data, self.config_offset, offset as u64)
        {
            error!("Failed to write device pci config, error is {}", e);
            return;
        }

        let cap_offset = self
            .pci_config
            .msix
            .as_ref()
            .map_or(0, |m| m.lock().unwrap().msix_cap_offset as usize);
        if ranges_overlap(offset, end, COMMAND as usize, COMMAND as usize + REG_SIZE) {
            self.pci_config
                .write(offset, data, self.dev_id.load(Ordering::Acquire));

            if le_read_u32(&self.pci_config.config, offset).unwrap() & COMMAND_MEMORY_SPACE as u32
                != 0
            {
                let parent_bus = self.parent_bus.upgrade().unwrap();
                let locked_parent_bus = parent_bus.lock().unwrap();
                if let Err(e) = self.pci_config.update_bar_mapping(
                    #[cfg(target_arch = "x86_64")]
                    &locked_parent_bus.io_region,
                    &locked_parent_bus.mem_region,
                ) {
                    error!("Failed to update bar, error is {}", e.display_chain());
                    return;
                }
                drop(locked_parent_bus);

                if let Err(e) = self.setup_bars_mmap() {
                    error!("Failed to map bar regions, error is {}", e.display_chain());
                    return;
                }
            }
        } else if ranges_overlap(offset, end, BAR_0 as usize, (BAR_5 as usize) + REG_SIZE) {
            self.pci_config
                .write(offset, data, self.dev_id.load(Ordering::Acquire));

            if size == 4 && LittleEndian::read_u32(data) != 0xffff_ffff {
                let parent_bus = self.parent_bus.upgrade().unwrap();
                let locked_parent_bus = parent_bus.lock().unwrap();
                if let Err(e) = self.pci_config.update_bar_mapping(
                    #[cfg(target_arch = "x86_64")]
                    &locked_parent_bus.io_region,
                    &locked_parent_bus.mem_region,
                ) {
                    error!("Failed to update bar, error is {}", e.display_chain());
                    return;
                }
            }
        } else if ranges_overlap(offset, end, cap_offset, cap_offset + MSIX_CAP_SIZE as usize) {
            let was_enable = is_msix_enabled(cap_offset, &self.pci_config.config);
            self.pci_config
                .write(offset, data, self.dev_id.load(Ordering::Acquire));
            let is_enable = is_msix_enabled(cap_offset, &self.pci_config.config);

            if !was_enable && is_enable {
                if let Err(e) = self.vfio_enable_msix() {
                    error!("{}\nFailed to enable MSI-X.", e.display_chain());
                    return;
                }
            } else if was_enable && !is_enable {
                if let Err(e) = self.vfio_disable_msix() {
                    error!("{}\nFailed to disable MSI-X.", e.display_chain());
                    return;
                }
            }
        } else {
            self.pci_config
                .write(offset, data, self.dev_id.load(Ordering::Acquire));
        }
    }

    fn name(&self) -> String {
        self.name.clone()
    }
}

fn get_irq_rawfds(gsi_msi_routes: &[GsiMsiRoute]) -> Vec<RawFd> {
    let mut rawfds: Vec<RawFd> = Vec::new();
    for r in gsi_msi_routes.iter() {
        if let Some(fd) = r.irq_fd.as_ref() {
            rawfds.push(fd.as_raw_fd());
        }
    }
    rawfds
}
