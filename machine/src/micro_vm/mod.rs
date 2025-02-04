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

//! # Micro VM
//!
//! Micro VM is a extremely light machine type.
//! It has a very simple machine model, which benefits to a very short
//! boot-time and tiny memory usage.
//!
//! ## Design
//!
//! This module offers support for:
//! 1. Create and manage lifecycle for `Micro VM`.
//! 2. Set cmdline arguments parameters for `Micro VM`.
//! 3. Manage mainloop to handle events for `Micro VM` and its devices.
//!
//! ## Platform Support
//!
//! - `x86_64`
//! - `aarch64`

pub mod errors {
    error_chain! {
        links {
            Util(util::errors::Error, util::errors::ErrorKind);
            Virtio(virtio::errors::Error, virtio::errors::ErrorKind);
        }
        foreign_links {
            Io(std::io::Error);
            Kvm(kvm_ioctls::Error);
            Nul(std::ffi::NulError);
        }
        errors {
            RplDevLmtErr(dev: String, nr: usize) {
                display("A maximum of {} {} replaceable devices are supported.", nr, dev)
            }
            UpdCfgErr(id: String) {
                display("{}: failed to update config.", id)
            }
            RlzVirtioMmioErr {
                display("Failed to realize virtio mmio.")
            }
        }
    }
}

mod mem_layout;
mod syscall;

use std::fs::metadata;
use std::ops::Deref;
use std::os::linux::fs::MetadataExt;
use std::os::unix::io::RawFd;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::vec::Vec;

use address_space::{AddressSpace, GuestAddress, Region};
use boot_loader::{load_linux, BootLoaderConfig};
use cpu::{CPUBootConfig, CpuLifecycleState, CpuTopology, CPU};
#[cfg(target_arch = "aarch64")]
use devices::legacy::PL031;
#[cfg(target_arch = "x86_64")]
use devices::legacy::SERIAL_ADDR;
use devices::legacy::{FwCfgOps, Serial};
#[cfg(target_arch = "aarch64")]
use devices::{InterruptController, InterruptControllerConfig};
use error_chain::ChainedError;
use hypervisor::kvm::KVM_FDS;
#[cfg(target_arch = "x86_64")]
use kvm_bindings::{kvm_pit_config, KVM_PIT_SPEAKER_DUMMY};
use machine_manager::config::parse_blk;
use machine_manager::config::parse_net;
use machine_manager::config::BlkDevConfig;
use machine_manager::machine::{
    DeviceInterface, KvmVmState, MachineAddressInterface, MachineExternalInterface,
    MachineInterface, MachineLifecycle, MigrateInterface,
};
use machine_manager::{
    config::{BootSource, ConfigCheck, NetworkInterfaceConfig, SerialConfig, VmConfig},
    qmp::{qmp_schema, QmpChannel, Response},
};
use migration::{MigrationManager, MigrationStatus};
use sysbus::SysBus;
#[cfg(target_arch = "aarch64")]
use sysbus::{SysBusDevType, SysRes};
#[cfg(target_arch = "aarch64")]
use util::device_tree::{self, CompileFDT, FdtBuilder};
use util::loop_context::EventLoopManager;
use util::seccomp::BpfRule;
use util::set_termi_canon_mode;
use virtio::{
    create_tap, qmp_balloon, qmp_query_balloon, Block, BlockState, Net, VhostKern, VirtioDevice,
    VirtioMmioDevice, VirtioMmioState, VirtioNetState,
};
use vmm_sys_util::eventfd::EventFd;

use super::{
    errors::{ErrorKind as MachineErrorKind, Result as MachineResult},
    MachineOps,
};
use errors::{ErrorKind, Result};
use mem_layout::{LayoutEntryType, MEM_LAYOUT};
use syscall::syscall_whitelist;

// The replaceable block device maximum count.
const MMIO_REPLACEABLE_BLK_NR: usize = 4;
// The replaceable network device maximum count.
const MMIO_REPLACEABLE_NET_NR: usize = 2;

// The config of replaceable device.
struct MmioReplaceableConfig {
    // Device id.
    id: String,
    // The dev_config of the related backend device.
    dev_config: Arc<dyn ConfigCheck>,
}

// The device information of replaceable device.
struct MmioReplaceableDevInfo {
    // The related MMIO device.
    device: Arc<Mutex<dyn VirtioDevice>>,
    // Device id.
    id: String,
    // Identify if this device is be used.
    used: bool,
}

// The gather of config, info and count of all replaceable devices.
struct MmioReplaceableInfo {
    // The arrays of all replaceable configs.
    configs: Arc<Mutex<Vec<MmioReplaceableConfig>>>,
    // The arrays of all replaceable device information.
    devices: Arc<Mutex<Vec<MmioReplaceableDevInfo>>>,
    // The count of block device which is plugin.
    block_count: usize,
    // The count of network device which is plugin.
    net_count: usize,
}

impl MmioReplaceableInfo {
    fn new() -> Self {
        MmioReplaceableInfo {
            configs: Arc::new(Mutex::new(Vec::new())),
            devices: Arc::new(Mutex::new(Vec::new())),
            block_count: 0_usize,
            net_count: 0_usize,
        }
    }
}

/// A wrapper around creating and using a kvm-based micro VM.
pub struct LightMachine {
    // `vCPU` topology, support sockets, cores, threads.
    cpu_topo: CpuTopology,
    // `vCPU` devices.
    cpus: Vec<Arc<CPU>>,
    // Interrupt controller device.
    #[cfg(target_arch = "aarch64")]
    irq_chip: Option<Arc<InterruptController>>,
    // Memory address space.
    sys_mem: Arc<AddressSpace>,
    // IO address space.
    #[cfg(target_arch = "x86_64")]
    sys_io: Arc<AddressSpace>,
    // System bus.
    sysbus: SysBus,
    // All replaceable device information.
    replaceable_info: MmioReplaceableInfo,
    // VM running state.
    vm_state: Arc<(Mutex<KvmVmState>, Condvar)>,
    // Vm boot_source config.
    boot_source: Arc<Mutex<BootSource>>,
    // VM power button, handle VM `Shutdown` event.
    power_button: EventFd,
}

impl LightMachine {
    /// Constructs a new `LightMachine`.
    ///
    /// # Arguments
    ///
    /// * `vm_config` - Represents the configuration for VM.
    pub fn new(vm_config: &VmConfig) -> MachineResult<Self> {
        use crate::errors::ResultExt;

        let sys_mem = AddressSpace::new(Region::init_container_region(u64::max_value()))
            .chain_err(|| MachineErrorKind::CrtMemSpaceErr)?;
        #[cfg(target_arch = "x86_64")]
        let sys_io = AddressSpace::new(Region::init_container_region(1 << 16))
            .chain_err(|| MachineErrorKind::CrtIoSpaceErr)?;
        #[cfg(target_arch = "x86_64")]
        let free_irqs: (i32, i32) = (5, 15);
        #[cfg(target_arch = "aarch64")]
        let free_irqs: (i32, i32) = (32, 191);
        let mmio_region: (u64, u64) = (
            MEM_LAYOUT[LayoutEntryType::Mmio as usize].0,
            MEM_LAYOUT[LayoutEntryType::Mmio as usize + 1].0,
        );
        let sysbus = SysBus::new(
            #[cfg(target_arch = "x86_64")]
            &sys_io,
            &sys_mem,
            free_irqs,
            mmio_region,
        );

        // Machine state init
        let vm_state = Arc::new((Mutex::new(KvmVmState::Created), Condvar::new()));
        let power_button = EventFd::new(libc::EFD_NONBLOCK)
            .chain_err(|| MachineErrorKind::InitEventFdErr("power_button".to_string()))?;

        if let Err(e) = MigrationManager::set_status(MigrationStatus::Setup) {
            error!("{}", e);
        }

        Ok(LightMachine {
            cpu_topo: CpuTopology::new(vm_config.machine_config.nr_cpus),
            cpus: Vec::new(),
            #[cfg(target_arch = "aarch64")]
            irq_chip: None,
            sys_mem,
            #[cfg(target_arch = "x86_64")]
            sys_io,
            sysbus,
            replaceable_info: MmioReplaceableInfo::new(),
            boot_source: Arc::new(Mutex::new(vm_config.clone().boot_source)),
            vm_state,
            power_button,
        })
    }

    #[cfg(target_arch = "x86_64")]
    fn arch_init() -> MachineResult<()> {
        use crate::errors::ResultExt;

        let kvm_fds = KVM_FDS.load();
        let vm_fd = kvm_fds.vm_fd.as_ref().unwrap();
        vm_fd
            .set_tss_address(0xfffb_d000_usize)
            .chain_err(|| MachineErrorKind::SetTssErr)?;

        let pit_config = kvm_pit_config {
            flags: KVM_PIT_SPEAKER_DUMMY,
            pad: Default::default(),
        };
        vm_fd
            .create_pit2(pit_config)
            .chain_err(|| MachineErrorKind::CrtPitErr)?;

        Ok(())
    }

    fn create_replaceable_devices(&mut self) -> Result<()> {
        use errors::ResultExt;

        let mut rpl_devs: Vec<VirtioMmioDevice> = Vec::new();
        for _ in 0..MMIO_REPLACEABLE_BLK_NR {
            let block = Arc::new(Mutex::new(Block::default()));
            let virtio_mmio = VirtioMmioDevice::new(&self.sys_mem, block.clone());
            rpl_devs.push(virtio_mmio);

            MigrationManager::register_device_instance_mutex(BlockState::descriptor(), block);
        }
        for _ in 0..MMIO_REPLACEABLE_NET_NR {
            let net = Arc::new(Mutex::new(Net::default()));
            let virtio_mmio = VirtioMmioDevice::new(&self.sys_mem, net.clone());
            rpl_devs.push(virtio_mmio);

            MigrationManager::register_device_instance_mutex(VirtioNetState::descriptor(), net);
        }

        let mut region_base = self.sysbus.min_free_base;
        let region_size = MEM_LAYOUT[LayoutEntryType::Mmio as usize].1;
        for dev in rpl_devs {
            self.replaceable_info
                .devices
                .lock()
                .unwrap()
                .push(MmioReplaceableDevInfo {
                    device: dev.device.clone(),
                    id: "".to_string(),
                    used: false,
                });

            MigrationManager::register_device_instance_mutex(
                VirtioMmioState::descriptor(),
                VirtioMmioDevice::realize(
                    dev,
                    &mut self.sysbus,
                    region_base,
                    MEM_LAYOUT[LayoutEntryType::Mmio as usize].1,
                    #[cfg(target_arch = "x86_64")]
                    &self.boot_source,
                )
                .chain_err(|| ErrorKind::RlzVirtioMmioErr)?,
            );
            region_base += region_size;
        }
        self.sysbus.min_free_base = region_base;
        Ok(())
    }

    fn fill_replaceable_device(
        &mut self,
        id: &str,
        dev_config: Arc<dyn ConfigCheck>,
        index: usize,
    ) -> Result<()> {
        use errors::ResultExt;

        let mut replaceable_devices = self.replaceable_info.devices.lock().unwrap();
        if let Some(device_info) = replaceable_devices.get_mut(index) {
            if device_info.used {
                bail!("{}: index {} is already used.", id, index);
            }

            device_info.id = id.to_string();
            device_info.used = true;
            device_info
                .device
                .lock()
                .unwrap()
                .update_config(Some(dev_config.clone()))
                .chain_err(|| ErrorKind::UpdCfgErr(id.to_string()))?;
        }

        self.add_replaceable_config(id, dev_config)?;
        Ok(())
    }

    fn add_replaceable_config(&self, id: &str, dev_config: Arc<dyn ConfigCheck>) -> Result<()> {
        let mut configs_lock = self.replaceable_info.configs.lock().unwrap();
        let limit = MMIO_REPLACEABLE_BLK_NR + MMIO_REPLACEABLE_NET_NR;
        if configs_lock.len() >= limit {
            return Err(ErrorKind::RplDevLmtErr("".to_string(), limit).into());
        }

        for config in configs_lock.iter() {
            if config.id == id {
                bail!("{} is already registered.", id);
            }
        }

        let config = MmioReplaceableConfig {
            id: id.to_string(),
            dev_config,
        };
        configs_lock.push(config);
        Ok(())
    }

    fn add_replaceable_device(&self, id: &str, driver: &str, slot: usize) -> Result<()> {
        use errors::ResultExt;

        let index = if driver.contains("net") {
            if slot >= MMIO_REPLACEABLE_NET_NR {
                return Err(
                    ErrorKind::RplDevLmtErr("net".to_string(), MMIO_REPLACEABLE_NET_NR).into(),
                );
            }
            slot + MMIO_REPLACEABLE_BLK_NR
        } else if driver.contains("blk") {
            if slot >= MMIO_REPLACEABLE_BLK_NR {
                return Err(
                    ErrorKind::RplDevLmtErr("block".to_string(), MMIO_REPLACEABLE_BLK_NR).into(),
                );
            }
            slot
        } else {
            bail!("Unsupported replaceable device type.");
        };

        // Find the configuration by id.
        let configs_lock = self.replaceable_info.configs.lock().unwrap();
        let mut dev_config = None;
        for config in configs_lock.iter() {
            if config.id == id {
                dev_config = Some(config.dev_config.clone());
            }
        }
        if dev_config.is_none() {
            bail!("Failed to find device configuration.");
        }

        // Find the replaceable device and replace it.
        let mut replaceable_devices = self.replaceable_info.devices.lock().unwrap();
        if let Some(device_info) = replaceable_devices.get_mut(index) {
            if device_info.used {
                bail!("The slot {} is occupied already.", slot);
            }

            device_info.id = id.to_string();
            device_info.used = true;
            device_info
                .device
                .lock()
                .unwrap()
                .update_config(dev_config)
                .chain_err(|| ErrorKind::UpdCfgErr(id.to_string()))?;
        }
        Ok(())
    }

    fn del_replaceable_device(&self, id: &str) -> Result<String> {
        use errors::ResultExt;

        // find the index of configuration by name and remove it
        let mut is_exist = false;
        let mut configs_lock = self.replaceable_info.configs.lock().unwrap();
        for (index, config) in configs_lock.iter().enumerate() {
            if config.id == id {
                configs_lock.remove(index);
                is_exist = true;
                break;
            }
        }

        // set the status of the device to 'unused'
        let mut replaceable_devices = self.replaceable_info.devices.lock().unwrap();
        for device_info in replaceable_devices.iter_mut() {
            if device_info.id == id {
                device_info.id = "".to_string();
                device_info.used = false;
                device_info
                    .device
                    .lock()
                    .unwrap()
                    .update_config(None)
                    .chain_err(|| ErrorKind::UpdCfgErr(id.to_string()))?;
            }
        }

        if !is_exist {
            bail!("Device {} not found", id);
        }
        Ok(id.to_string())
    }
}

impl MachineOps for LightMachine {
    fn arch_ram_ranges(&self, mem_size: u64) -> Vec<(u64, u64)> {
        #[allow(unused_mut)]
        let mut ranges: Vec<(u64, u64)>;

        #[cfg(target_arch = "aarch64")]
        {
            let mem_start = MEM_LAYOUT[LayoutEntryType::Mem as usize].0;
            ranges = vec![(mem_start, mem_size)];
        }
        #[cfg(target_arch = "x86_64")]
        {
            let gap_start = MEM_LAYOUT[LayoutEntryType::MemBelow4g as usize].0
                + MEM_LAYOUT[LayoutEntryType::MemBelow4g as usize].1;
            ranges = vec![(0, std::cmp::min(gap_start, mem_size))];
            if mem_size > gap_start {
                let gap_end = MEM_LAYOUT[LayoutEntryType::MemAbove4g as usize].0;
                ranges.push((gap_end, mem_size - gap_start));
            }
        }
        ranges
    }

    #[cfg(target_arch = "x86_64")]
    fn init_interrupt_controller(&mut self, _vcpu_count: u64) -> MachineResult<()> {
        use crate::errors::ResultExt;

        KVM_FDS
            .load()
            .vm_fd
            .as_ref()
            .unwrap()
            .create_irq_chip()
            .chain_err(|| MachineErrorKind::CrtIrqchipErr)?;
        Ok(())
    }

    #[cfg(target_arch = "aarch64")]
    fn init_interrupt_controller(&mut self, vcpu_count: u64) -> MachineResult<()> {
        // Interrupt Controller Chip init
        let intc_conf = InterruptControllerConfig {
            version: kvm_bindings::kvm_device_type_KVM_DEV_TYPE_ARM_VGIC_V3,
            vcpu_count,
            max_irq: 192,
            msi: true,
            dist_range: MEM_LAYOUT[LayoutEntryType::GicDist as usize],
            redist_region_ranges: vec![
                MEM_LAYOUT[LayoutEntryType::GicRedist as usize],
                MEM_LAYOUT[LayoutEntryType::HighGicRedist as usize],
            ],
            its_range: Some(MEM_LAYOUT[LayoutEntryType::GicIts as usize]),
        };
        let irq_chip = InterruptController::new(&intc_conf)?;
        self.irq_chip = Some(Arc::new(irq_chip));
        self.irq_chip.as_ref().unwrap().realize()?;
        Ok(())
    }

    #[cfg(target_arch = "x86_64")]
    fn load_boot_source(
        &self,
        fwcfg: Option<&Arc<Mutex<dyn FwCfgOps>>>,
    ) -> MachineResult<CPUBootConfig> {
        use crate::errors::ResultExt;

        let boot_source = self.boot_source.lock().unwrap();
        let initrd = boot_source.initrd.as_ref().map(|b| b.initrd_file.clone());

        let gap_start = MEM_LAYOUT[LayoutEntryType::MemBelow4g as usize].0
            + MEM_LAYOUT[LayoutEntryType::MemBelow4g as usize].1;
        let gap_end = MEM_LAYOUT[LayoutEntryType::MemAbove4g as usize].0;
        let bootloader_config = BootLoaderConfig {
            kernel: boot_source.kernel_file.clone(),
            initrd,
            kernel_cmdline: boot_source.kernel_cmdline.to_string(),
            cpu_count: self.cpu_topo.nrcpus,
            gap_range: (gap_start, gap_end - gap_start),
            ioapic_addr: MEM_LAYOUT[LayoutEntryType::IoApic as usize].0 as u32,
            lapic_addr: MEM_LAYOUT[LayoutEntryType::LocalApic as usize].0 as u32,
            ident_tss_range: None,
            prot64_mode: true,
        };
        let layout = load_linux(&bootloader_config, &self.sys_mem, fwcfg)
            .chain_err(|| MachineErrorKind::LoadKernErr)?;

        Ok(CPUBootConfig {
            prot64_mode: true,
            boot_ip: layout.boot_ip,
            boot_sp: layout.boot_sp,
            boot_selector: layout.boot_selector,
            zero_page: layout.zero_page_addr,
            code_segment: layout.segments.code_segment,
            data_segment: layout.segments.data_segment,
            gdt_base: layout.segments.gdt_base,
            gdt_size: layout.segments.gdt_limit,
            idt_base: layout.segments.idt_base,
            idt_size: layout.segments.idt_limit,
            pml4_start: layout.boot_pml4_addr,
        })
    }

    #[cfg(target_arch = "aarch64")]
    fn load_boot_source(
        &self,
        fwcfg: Option<&Arc<Mutex<dyn FwCfgOps>>>,
    ) -> MachineResult<CPUBootConfig> {
        use crate::errors::ResultExt;

        let mut boot_source = self.boot_source.lock().unwrap();
        let initrd = boot_source.initrd.as_ref().map(|b| b.initrd_file.clone());

        let bootloader_config = BootLoaderConfig {
            kernel: boot_source.kernel_file.clone(),
            initrd,
            mem_start: MEM_LAYOUT[LayoutEntryType::Mem as usize].0,
        };
        let layout = load_linux(&bootloader_config, &self.sys_mem, fwcfg)
            .chain_err(|| MachineErrorKind::LoadKernErr)?;
        if let Some(rd) = &mut boot_source.initrd {
            rd.initrd_addr = layout.initrd_start;
            rd.initrd_size = layout.initrd_size;
        }

        Ok(CPUBootConfig {
            fdt_addr: layout.dtb_start,
            boot_pc: layout.boot_pc,
        })
    }

    fn realize_virtio_mmio_device(
        &mut self,
        dev: VirtioMmioDevice,
    ) -> MachineResult<Arc<Mutex<VirtioMmioDevice>>> {
        use errors::ResultExt;

        let region_base = self.sysbus.min_free_base;
        let region_size = MEM_LAYOUT[LayoutEntryType::Mmio as usize].1;
        let realized_virtio_mmio_device = VirtioMmioDevice::realize(
            dev,
            &mut self.sysbus,
            region_base,
            region_size,
            #[cfg(target_arch = "x86_64")]
            &self.boot_source,
        )
        .chain_err(|| ErrorKind::RlzVirtioMmioErr)?;
        self.sysbus.min_free_base += region_size;
        Ok(realized_virtio_mmio_device)
    }

    fn get_sys_mem(&mut self) -> &Arc<AddressSpace> {
        &self.sys_mem
    }

    #[cfg(target_arch = "aarch64")]
    fn add_rtc_device(&mut self) -> MachineResult<()> {
        use crate::errors::ResultExt;

        PL031::realize(
            PL031::default(),
            &mut self.sysbus,
            MEM_LAYOUT[LayoutEntryType::Rtc as usize].0,
            MEM_LAYOUT[LayoutEntryType::Rtc as usize].1,
        )
        .chain_err(|| "Failed to realize pl031.")?;
        Ok(())
    }

    #[cfg(target_arch = "x86_64")]
    fn add_rtc_device(&mut self, _mem_size: u64) -> MachineResult<()> {
        Ok(())
    }

    fn add_serial_device(&mut self, config: &SerialConfig) -> MachineResult<()> {
        use crate::errors::ResultExt;

        #[cfg(target_arch = "x86_64")]
        let region_base: u64 = SERIAL_ADDR;
        #[cfg(target_arch = "aarch64")]
        let region_base: u64 = MEM_LAYOUT[LayoutEntryType::Uart as usize].0;
        #[cfg(target_arch = "x86_64")]
        let region_size: u64 = 8;
        #[cfg(target_arch = "aarch64")]
        let region_size: u64 = MEM_LAYOUT[LayoutEntryType::Uart as usize].1;

        let serial = Serial::new(config.clone());
        serial
            .realize(
                &mut self.sysbus,
                region_base,
                region_size,
                #[cfg(target_arch = "aarch64")]
                &self.boot_source,
            )
            .chain_err(|| "Failed to realize serial device.")?;
        Ok(())
    }

    fn add_virtio_mmio_net(
        &mut self,
        vm_config: &mut VmConfig,
        cfg_args: &str,
    ) -> MachineResult<()> {
        let device_cfg = parse_net(vm_config, cfg_args)?;
        if device_cfg.vhost_type.is_some() {
            let net = Arc::new(Mutex::new(VhostKern::Net::new(&device_cfg, &self.sys_mem)));
            let device = VirtioMmioDevice::new(&self.sys_mem, net);
            self.realize_virtio_mmio_device(device)?;
        } else {
            let index = MMIO_REPLACEABLE_BLK_NR + self.replaceable_info.net_count;
            if index >= MMIO_REPLACEABLE_BLK_NR + MMIO_REPLACEABLE_NET_NR {
                bail!(
                    "A maximum of {} net replaceable devices are supported.",
                    MMIO_REPLACEABLE_NET_NR
                );
            }
            self.fill_replaceable_device(&device_cfg.id, Arc::new(device_cfg.clone()), index)?;
            self.replaceable_info.net_count += 1;
        }
        Ok(())
    }

    fn add_virtio_mmio_block(
        &mut self,
        vm_config: &mut VmConfig,
        cfg_args: &str,
    ) -> MachineResult<()> {
        let device_cfg = parse_blk(vm_config, cfg_args)?;
        if self.replaceable_info.block_count >= MMIO_REPLACEABLE_BLK_NR {
            bail!(
                "A maximum of {} block replaceable devices are supported.",
                MMIO_REPLACEABLE_BLK_NR
            );
        }
        let index = self.replaceable_info.block_count;
        self.fill_replaceable_device(&device_cfg.id, Arc::new(device_cfg.clone()), index)?;
        self.replaceable_info.block_count += 1;
        Ok(())
    }

    fn syscall_whitelist(&self) -> Vec<BpfRule> {
        syscall_whitelist()
    }

    fn realize(
        vm: &Arc<Mutex<Self>>,
        vm_config: &mut VmConfig,
        is_migrate: bool,
    ) -> MachineResult<()> {
        use crate::errors::ResultExt;

        let mut locked_vm = vm.lock().unwrap();

        locked_vm.init_memory(
            &vm_config.machine_config.mem_config,
            #[cfg(target_arch = "x86_64")]
            &locked_vm.sys_io,
            &locked_vm.sys_mem,
            is_migrate,
            vm_config.machine_config.nr_cpus,
        )?;

        #[cfg(target_arch = "x86_64")]
        {
            locked_vm.init_interrupt_controller(u64::from(vm_config.machine_config.nr_cpus))?;
            LightMachine::arch_init()?;
        }
        let mut vcpu_fds = vec![];
        for vcpu_id in 0..vm_config.machine_config.nr_cpus {
            vcpu_fds.push(Arc::new(
                KVM_FDS
                    .load()
                    .vm_fd
                    .as_ref()
                    .unwrap()
                    .create_vcpu(vcpu_id)?,
            ));
        }
        #[cfg(target_arch = "aarch64")]
        locked_vm.init_interrupt_controller(u64::from(vm_config.machine_config.nr_cpus))?;

        // Add mmio devices
        locked_vm
            .create_replaceable_devices()
            .chain_err(|| "Failed to create replaceable devices.")?;
        locked_vm.add_devices(vm_config)?;

        let boot_config = if !is_migrate {
            Some(locked_vm.load_boot_source(None)?)
        } else {
            None
        };
        locked_vm.cpus.extend(<Self as MachineOps>::init_vcpu(
            vm.clone(),
            vm_config.machine_config.nr_cpus,
            &vcpu_fds,
            &boot_config,
        )?);

        #[cfg(target_arch = "aarch64")]
        if let Some(boot_cfg) = boot_config {
            let mut fdt_helper = FdtBuilder::new();
            locked_vm
                .generate_fdt_node(&mut fdt_helper)
                .chain_err(|| MachineErrorKind::GenFdtErr)?;
            let fdt_vec = fdt_helper.finish()?;
            locked_vm
                .sys_mem
                .write(
                    &mut fdt_vec.as_slice(),
                    GuestAddress(boot_cfg.fdt_addr as u64),
                    fdt_vec.len() as u64,
                )
                .chain_err(|| MachineErrorKind::WrtFdtErr(boot_cfg.fdt_addr, fdt_vec.len()))?;
        }
        locked_vm
            .register_power_event(&locked_vm.power_button)
            .chain_err(|| MachineErrorKind::InitEventFdErr("power_button".to_string()))?;
        Ok(())
    }

    fn run(&self, paused: bool) -> MachineResult<()> {
        <Self as MachineOps>::vm_start(paused, &self.cpus, &mut self.vm_state.0.lock().unwrap())
    }
}

impl MachineLifecycle for LightMachine {
    fn pause(&self) -> bool {
        if self.notify_lifecycle(KvmVmState::Running, KvmVmState::Paused) {
            event!(Stop);
            true
        } else {
            false
        }
    }

    fn resume(&self) -> bool {
        if !self.notify_lifecycle(KvmVmState::Paused, KvmVmState::Running) {
            return false;
        }

        event!(Resume);
        true
    }

    fn destroy(&self) -> bool {
        let vmstate = {
            let state = self.vm_state.deref().0.lock().unwrap();
            *state
        };

        if !self.notify_lifecycle(vmstate, KvmVmState::Shutdown) {
            return false;
        }

        self.power_button.write(1).unwrap();
        true
    }

    fn reset(&mut self) -> bool {
        // For micro vm, the reboot command is equivalent to the shutdown command.
        for cpu in self.cpus.iter() {
            let (cpu_state, _) = cpu.state();
            *cpu_state.lock().unwrap() = CpuLifecycleState::Stopped;
        }

        self.destroy()
    }

    fn notify_lifecycle(&self, old: KvmVmState, new: KvmVmState) -> bool {
        <Self as MachineOps>::vm_state_transfer(
            &self.cpus,
            #[cfg(target_arch = "aarch64")]
            &self.irq_chip,
            &mut self.vm_state.0.lock().unwrap(),
            old,
            new,
        )
        .is_ok()
    }
}

impl MachineAddressInterface for LightMachine {
    #[cfg(target_arch = "x86_64")]
    fn pio_in(&self, addr: u64, mut data: &mut [u8]) -> bool {
        // The function pit_calibrate_tsc() in kernel gets stuck if data read from
        // io-port 0x61 is not 0x20.
        // This problem only happens before Linux version 4.18 (fixed by 368a540e0)
        if addr == 0x61 {
            data[0] = 0x20;
            return true;
        }
        let length = data.len() as u64;
        self.sys_io
            .read(&mut data, GuestAddress(addr), length)
            .is_ok()
    }

    #[cfg(target_arch = "x86_64")]
    fn pio_out(&self, addr: u64, mut data: &[u8]) -> bool {
        let count = data.len() as u64;
        self.sys_io
            .write(&mut data, GuestAddress(addr), count)
            .is_ok()
    }

    fn mmio_read(&self, addr: u64, mut data: &mut [u8]) -> bool {
        let length = data.len() as u64;
        self.sys_mem
            .read(&mut data, GuestAddress(addr), length)
            .is_ok()
    }

    fn mmio_write(&self, addr: u64, mut data: &[u8]) -> bool {
        let count = data.len() as u64;
        self.sys_mem
            .write(&mut data, GuestAddress(addr), count)
            .is_ok()
    }
}

impl DeviceInterface for LightMachine {
    fn query_status(&self) -> Response {
        let vmstate = self.vm_state.deref().0.lock().unwrap();
        let qmp_state = match *vmstate {
            KvmVmState::Running => qmp_schema::StatusInfo {
                singlestep: false,
                running: true,
                status: qmp_schema::RunState::running,
            },
            KvmVmState::Paused => qmp_schema::StatusInfo {
                singlestep: false,
                running: true,
                status: qmp_schema::RunState::paused,
            },
            _ => Default::default(),
        };

        Response::create_response(serde_json::to_value(&qmp_state).unwrap(), None)
    }

    fn query_cpus(&self) -> Response {
        let mut cpu_vec: Vec<serde_json::Value> = Vec::new();
        for cpu_index in 0..self.cpu_topo.max_cpus {
            if self.cpu_topo.get_mask(cpu_index as usize) == 1 {
                let thread_id = self.cpus[cpu_index as usize].tid();
                let (socketid, coreid, threadid) = self.cpu_topo.get_topo(cpu_index as usize);
                let cpu_instance = qmp_schema::CpuInstanceProperties {
                    node_id: None,
                    socket_id: Some(socketid as isize),
                    core_id: Some(coreid as isize),
                    thread_id: Some(threadid as isize),
                };
                #[cfg(target_arch = "x86_64")]
                {
                    let cpu_info = qmp_schema::CpuInfo::x86 {
                        current: true,
                        qom_path: String::from("/machine/unattached/device[")
                            + &cpu_index.to_string()
                            + &"]".to_string(),
                        halted: false,
                        props: Some(cpu_instance),
                        CPU: cpu_index as isize,
                        thread_id: thread_id as isize,
                        x86: qmp_schema::CpuInfoX86 {},
                    };
                    cpu_vec.push(serde_json::to_value(cpu_info).unwrap());
                }
                #[cfg(target_arch = "aarch64")]
                {
                    let cpu_info = qmp_schema::CpuInfo::Arm {
                        current: true,
                        qom_path: String::from("/machine/unattached/device[")
                            + &cpu_index.to_string()
                            + &"]".to_string(),
                        halted: false,
                        props: Some(cpu_instance),
                        CPU: cpu_index as isize,
                        thread_id: thread_id as isize,
                        arm: qmp_schema::CpuInfoArm {},
                    };
                    cpu_vec.push(serde_json::to_value(cpu_info).unwrap());
                }
            }
        }
        Response::create_response(cpu_vec.into(), None)
    }

    fn query_hotpluggable_cpus(&self) -> Response {
        let mut hotplug_vec: Vec<serde_json::Value> = Vec::new();
        #[cfg(target_arch = "x86_64")]
        let cpu_type = String::from("host-x86-cpu");
        #[cfg(target_arch = "aarch64")]
        let cpu_type = String::from("host-aarch64-cpu");

        for cpu_index in 0..self.cpu_topo.max_cpus {
            if self.cpu_topo.get_mask(cpu_index as usize) == 0 {
                let (socketid, coreid, threadid) = self.cpu_topo.get_topo(cpu_index as usize);
                let cpu_instance = qmp_schema::CpuInstanceProperties {
                    node_id: None,
                    socket_id: Some(socketid as isize),
                    core_id: Some(coreid as isize),
                    thread_id: Some(threadid as isize),
                };
                let hotpluggable_cpu = qmp_schema::HotpluggableCPU {
                    type_: cpu_type.clone(),
                    vcpus_count: 1,
                    props: cpu_instance,
                    qom_path: None,
                };
                hotplug_vec.push(serde_json::to_value(hotpluggable_cpu).unwrap());
            } else {
                let (socketid, coreid, threadid) = self.cpu_topo.get_topo(cpu_index as usize);
                let cpu_instance = qmp_schema::CpuInstanceProperties {
                    node_id: None,
                    socket_id: Some(socketid as isize),
                    core_id: Some(coreid as isize),
                    thread_id: Some(threadid as isize),
                };
                let hotpluggable_cpu = qmp_schema::HotpluggableCPU {
                    type_: cpu_type.clone(),
                    vcpus_count: 1,
                    props: cpu_instance,
                    qom_path: Some(
                        String::from("/machine/unattached/device[")
                            + &cpu_index.to_string()
                            + &"]".to_string(),
                    ),
                };
                hotplug_vec.push(serde_json::to_value(hotpluggable_cpu).unwrap());
            }
        }
        Response::create_response(hotplug_vec.into(), None)
    }

    fn balloon(&self, value: u64) -> Response {
        if qmp_balloon(value) {
            return Response::create_empty_response();
        }
        Response::create_error_response(
            qmp_schema::QmpErrorClass::DeviceNotActive(
                "No balloon device has been activated".to_string(),
            ),
            None,
        )
    }

    fn query_balloon(&self) -> Response {
        if let Some(actual) = qmp_query_balloon() {
            let ret = qmp_schema::BalloonInfo { actual };
            return Response::create_response(serde_json::to_value(&ret).unwrap(), None);
        }
        Response::create_error_response(
            qmp_schema::QmpErrorClass::DeviceNotActive(
                "No balloon device has been activated".to_string(),
            ),
            None,
        )
    }

    fn device_add(&mut self, args: Box<qmp_schema::DeviceAddArgument>) -> Response {
        // get slot of bus by addr or lun
        let mut slot = 0;
        if let Some(addr) = args.addr {
            let slot_str = addr.as_str().trim_start_matches("0x");

            if let Ok(n) = usize::from_str_radix(slot_str, 16) {
                slot = n;
            }
        } else if let Some(lun) = args.lun {
            slot = lun + 1;
        }

        match self.add_replaceable_device(&args.id, &args.driver, slot) {
            Ok(()) => Response::create_empty_response(),
            Err(ref e) => {
                error!("{}", e.display_chain());
                error!("Failed to add device: id {}, type {}", args.id, args.driver);
                Response::create_error_response(
                    qmp_schema::QmpErrorClass::GenericError(e.to_string()),
                    None,
                )
            }
        }
    }

    fn device_del(&mut self, device_id: String) -> Response {
        match self.del_replaceable_device(&device_id) {
            Ok(path) => {
                let block_del_event = qmp_schema::DeviceDeleted {
                    device: Some(device_id),
                    path,
                };
                event!(DeviceDeleted; block_del_event);

                Response::create_empty_response()
            }
            Err(ref e) => {
                error!("Failed to delete device: {}", e.display_chain());
                Response::create_error_response(
                    qmp_schema::QmpErrorClass::GenericError(e.to_string()),
                    None,
                )
            }
        }
    }

    fn blockdev_add(&self, args: Box<qmp_schema::BlockDevAddArgument>) -> Response {
        const MAX_STRING_LENGTH: usize = 255;
        let read_only = args.read_only.unwrap_or(false);

        let direct = if let Some(cache) = args.cache {
            match cache.direct {
                Some(direct) => direct,
                _ => true,
            }
        } else {
            true
        };

        let blk = Path::new(&args.file.filename);
        match metadata(blk) {
            Ok(meta) => {
                if (meta.st_mode() & libc::S_IFREG != libc::S_IFREG)
                    && (meta.st_mode() & libc::S_IFBLK != libc::S_IFBLK)
                {
                    error!("File {:?} is not a regular file or block device", blk);
                    return Response::create_error_response(
                        qmp_schema::QmpErrorClass::GenericError(
                            "File is not a regular file or block device".to_string(),
                        ),
                        None,
                    );
                }
            }
            Err(ref e) => {
                error!("Blockdev_add failed: {}", e);
                return Response::create_error_response(
                    qmp_schema::QmpErrorClass::GenericError(e.to_string()),
                    None,
                );
            }
        }

        if let Some(file_name) = blk.file_name() {
            if file_name.len() > MAX_STRING_LENGTH {
                error!("File name {:?} is illegal", file_name);
                return Response::create_error_response(
                    qmp_schema::QmpErrorClass::GenericError("Illegal block name".to_string()),
                    None,
                );
            }
        } else {
            error!("Path: {:?} is not valid", blk);
            return Response::create_error_response(
                qmp_schema::QmpErrorClass::GenericError("Invalid block path".to_string()),
                None,
            );
        }

        let config = BlkDevConfig {
            id: args.node_name.clone(),
            path_on_host: args.file.filename,
            read_only,
            direct,
            serial_num: None,
            iothread: None,
            iops: None,
        };
        match self.add_replaceable_config(&args.node_name, Arc::new(config)) {
            Ok(()) => Response::create_empty_response(),
            Err(ref e) => {
                error!("{}", e.display_chain());
                Response::create_error_response(
                    qmp_schema::QmpErrorClass::GenericError(e.to_string()),
                    None,
                )
            }
        }
    }

    fn blockdev_del(&self, _node_name: String) -> Response {
        Response::create_error_response(
            qmp_schema::QmpErrorClass::GenericError("blockdev_del not support yet".to_string()),
            None,
        )
    }

    fn netdev_add(&mut self, args: Box<qmp_schema::NetDevAddArgument>) -> Response {
        let mut config = NetworkInterfaceConfig {
            id: args.id.clone(),
            host_dev_name: "".to_string(),
            mac: None,
            tap_fd: None,
            vhost_type: None,
            vhost_fd: None,
            iothread: None,
        };

        if let Some(fds) = args.fds {
            let netdev_fd = if fds.contains(':') {
                let col: Vec<_> = fds.split(':').collect();
                String::from(col[col.len() - 1])
            } else {
                String::from(&fds)
            };

            if let Some(fd_num) = QmpChannel::get_fd(&netdev_fd) {
                config.tap_fd = Some(fd_num);
            } else {
                // try to convert string to RawFd
                let fd_num = match netdev_fd.parse::<i32>() {
                    Ok(fd) => fd,
                    _ => {
                        error!(
                            "Add netdev error: failed to convert {} to RawFd.",
                            netdev_fd
                        );
                        return Response::create_error_response(
                            qmp_schema::QmpErrorClass::GenericError(
                                "Add netdev error: failed to convert {} to RawFd.".to_string(),
                            ),
                            None,
                        );
                    }
                };
                config.tap_fd = Some(fd_num);
            }
        } else if let Some(if_name) = args.if_name {
            config.host_dev_name = if_name.clone();
            if create_tap(None, Some(&if_name)).is_err() {
                return Response::create_error_response(
                    qmp_schema::QmpErrorClass::GenericError(
                        "Tap device already in use".to_string(),
                    ),
                    None,
                );
            }
        }

        match self.add_replaceable_config(&args.id, Arc::new(config)) {
            Ok(()) => Response::create_empty_response(),
            Err(ref e) => {
                error!("{}", e.display_chain());
                Response::create_error_response(
                    qmp_schema::QmpErrorClass::GenericError(e.to_string()),
                    None,
                )
            }
        }
    }

    fn netdev_del(&mut self, _node_name: String) -> Response {
        Response::create_error_response(
            qmp_schema::QmpErrorClass::GenericError("netdev_del not support yet".to_string()),
            None,
        )
    }

    fn getfd(&self, fd_name: String, if_fd: Option<RawFd>) -> Response {
        if let Some(fd) = if_fd {
            QmpChannel::set_fd(fd_name, fd);
            Response::create_empty_response()
        } else {
            let err_resp =
                qmp_schema::QmpErrorClass::GenericError("Invalid SCM message".to_string());
            Response::create_error_response(err_resp, None)
        }
    }
}

impl MigrateInterface for LightMachine {
    fn migrate(&self, uri: String) -> Response {
        use util::unix::{parse_uri, UnixPath};

        match parse_uri(&uri) {
            Ok((UnixPath::File, path)) => {
                if let Err(e) = MigrationManager::save_snapshot(&path) {
                    error!(
                        "Failed to migrate to path \'{:?}\': {}",
                        path,
                        e.display_chain()
                    );
                    let _ = MigrationManager::set_status(MigrationStatus::Failed)
                        .map_err(|e| error!("{}", e));
                    return Response::create_error_response(
                        qmp_schema::QmpErrorClass::GenericError(e.to_string()),
                        None,
                    );
                }
            }
            _ => {
                return Response::create_error_response(
                    qmp_schema::QmpErrorClass::GenericError(format!("Invalid uri: {}", uri)),
                    None,
                );
            }
        }

        Response::create_empty_response()
    }

    fn query_migrate(&self) -> Response {
        let status_str = MigrationManager::migration_get_status().to_string();
        let migration_info = qmp_schema::MigrationInfo {
            status: Some(status_str),
        };

        Response::create_response(serde_json::to_value(migration_info).unwrap(), None)
    }
}

impl MachineInterface for LightMachine {}
impl MachineExternalInterface for LightMachine {}

impl EventLoopManager for LightMachine {
    fn loop_should_exit(&self) -> bool {
        let vmstate = self.vm_state.deref().0.lock().unwrap();
        *vmstate == KvmVmState::Shutdown
    }

    fn loop_cleanup(&self) -> util::errors::Result<()> {
        use util::errors::ResultExt;

        set_termi_canon_mode().chain_err(|| "Failed to set terminal to canonical mode")?;
        Ok(())
    }
}

// Function that helps to generate serial node in device-tree.
//
// # Arguments
//
// * `dev_info` - Device resource info of serial device.
// * `fdt` - Flatted device-tree blob where serial node will be filled into.
#[cfg(target_arch = "aarch64")]
fn generate_serial_device_node(fdt: &mut FdtBuilder, res: &SysRes) -> util::errors::Result<()> {
    let node = format!("uart@{:x}", res.region_base);
    let serial_node_dep = fdt.begin_node(&node)?;
    fdt.set_property_string("compatible", "ns16550a")?;
    fdt.set_property_string("clock-names", "apb_pclk")?;
    fdt.set_property_u32("clocks", device_tree::CLK_PHANDLE)?;
    fdt.set_property_array_u64("reg", &[res.region_base, res.region_size])?;
    fdt.set_property_array_u32(
        "interrupts",
        &[
            device_tree::GIC_FDT_IRQ_TYPE_SPI,
            res.irq as u32,
            device_tree::IRQ_TYPE_EDGE_RISING,
        ],
    )?;
    fdt.end_node(serial_node_dep)?;

    Ok(())
}

// Function that helps to generate RTC node in device-tree.
//
// # Arguments
//
// * `dev_info` - Device resource info of RTC device.
// * `fdt` - Flatted device-tree blob where RTC node will be filled into.
#[cfg(target_arch = "aarch64")]
fn generate_rtc_device_node(fdt: &mut FdtBuilder, res: &SysRes) -> util::errors::Result<()> {
    let node = format!("pl031@{:x}", res.region_base);
    let rtc_node_dep = fdt.begin_node(&node)?;
    fdt.set_property_string("compatible", "arm,pl031\0arm,primecell\0")?;
    fdt.set_property_string("clock-names", "apb_pclk")?;
    fdt.set_property_u32("clocks", device_tree::CLK_PHANDLE)?;
    fdt.set_property_array_u64("reg", &[res.region_base, res.region_size])?;
    fdt.set_property_array_u32(
        "interrupts",
        &[
            device_tree::GIC_FDT_IRQ_TYPE_SPI,
            res.irq as u32,
            device_tree::IRQ_TYPE_LEVEL_HIGH,
        ],
    )?;
    fdt.end_node(rtc_node_dep)?;

    Ok(())
}

// Function that helps to generate Virtio-Mmio device's node in device-tree.
//
// # Arguments
//
// * `dev_info` - Device resource info of Virtio-Mmio device.
// * `fdt` - Flatted device-tree blob where node will be filled into.
#[cfg(target_arch = "aarch64")]
fn generate_virtio_devices_node(fdt: &mut FdtBuilder, res: &SysRes) -> util::errors::Result<()> {
    let node = format!("virtio_mmio@{:x}", res.region_base);
    let virtio_node_dep = fdt.begin_node(&node)?;
    fdt.set_property_string("compatible", "virtio,mmio")?;
    fdt.set_property_u32("interrupt-parent", device_tree::GIC_PHANDLE)?;
    fdt.set_property_array_u64("reg", &[res.region_base, res.region_size])?;
    fdt.set_property_array_u32(
        "interrupts",
        &[
            device_tree::GIC_FDT_IRQ_TYPE_SPI,
            res.irq as u32,
            device_tree::IRQ_TYPE_EDGE_RISING,
        ],
    )?;
    fdt.end_node(virtio_node_dep)?;
    Ok(())
}

/// Trait that helps to generate all nodes in device-tree.
#[allow(clippy::upper_case_acronyms)]
#[cfg(target_arch = "aarch64")]
trait CompileFDTHelper {
    /// Function that helps to generate cpu nodes.
    fn generate_cpu_nodes(&self, fdt: &mut FdtBuilder) -> util::errors::Result<()>;
    /// Function that helps to generate memory nodes.
    fn generate_memory_node(&self, fdt: &mut FdtBuilder) -> util::errors::Result<()>;
    /// Function that helps to generate Virtio-mmio devices' nodes.
    fn generate_devices_node(&self, fdt: &mut FdtBuilder) -> util::errors::Result<()>;
    /// Function that helps to generate the chosen node.
    fn generate_chosen_node(&self, fdt: &mut FdtBuilder) -> util::errors::Result<()>;
}

#[cfg(target_arch = "aarch64")]
impl CompileFDTHelper for LightMachine {
    fn generate_cpu_nodes(&self, fdt: &mut FdtBuilder) -> util::errors::Result<()> {
        let node = "cpus";

        let cpus_node_dep = fdt.begin_node(node)?;
        fdt.set_property_u32("#address-cells", 0x02)?;
        fdt.set_property_u32("#size-cells", 0x0)?;

        // Generate CPU topology
        if self.cpu_topo.max_cpus > 0 && self.cpu_topo.max_cpus % 8 == 0 {
            let cpu_map_node_dep = fdt.begin_node("cpu-map")?;

            let sockets = self.cpu_topo.max_cpus / 8;
            for cluster in 0..u32::from(sockets) {
                let clster = format!("cluster{}", cluster);
                let cluster_node_dep = fdt.begin_node(&clster)?;

                for i in 0..2_u32 {
                    let sub_cluster = format!("cluster{}", i);
                    let sub_cluster_node_dep = fdt.begin_node(&sub_cluster)?;

                    let core0 = "core0".to_string();
                    let core0_node_dep = fdt.begin_node(&core0)?;

                    let thread0 = "thread0".to_string();
                    let thread0_node_dep = fdt.begin_node(&thread0)?;
                    fdt.set_property_u32("cpu", cluster * 8 + i * 4 + 10)?;
                    fdt.end_node(thread0_node_dep)?;

                    let thread1 = "thread1".to_string();
                    let thread1_node_dep = fdt.begin_node(&thread1)?;
                    fdt.set_property_u32("cpu", cluster * 8 + i * 4 + 10 + 1)?;
                    fdt.end_node(thread1_node_dep)?;

                    fdt.end_node(core0_node_dep)?;

                    let core1 = "core1".to_string();
                    let core1_node_dep = fdt.begin_node(&core1)?;

                    let thread0 = "thread0".to_string();
                    let thread0_node_dep = fdt.begin_node(&thread0)?;
                    fdt.set_property_u32("cpu", cluster * 8 + i * 4 + 10 + 2)?;
                    fdt.end_node(thread0_node_dep)?;

                    let thread1 = "thread1".to_string();
                    let thread1_node_dep = fdt.begin_node(&thread1)?;
                    fdt.set_property_u32("cpu", cluster * 8 + i * 4 + 10 + 3)?;
                    fdt.end_node(thread1_node_dep)?;

                    fdt.end_node(core1_node_dep)?;

                    fdt.end_node(sub_cluster_node_dep)?;
                }
                fdt.end_node(cluster_node_dep)?;
            }
            fdt.end_node(cpu_map_node_dep)?;
        }

        for cpu_index in 0..self.cpu_topo.max_cpus {
            let mpidr = self.cpus[cpu_index as usize].arch().lock().unwrap().mpidr();

            let node = format!("cpu@{:x}", mpidr);
            let mpidr_node_dep = fdt.begin_node(&node)?;
            fdt.set_property_u32(
                "phandle",
                u32::from(cpu_index) + device_tree::CPU_PHANDLE_START,
            )?;
            fdt.set_property_string("device_type", "cpu")?;
            fdt.set_property_string("compatible", "arm,arm-v8")?;
            if self.cpu_topo.max_cpus > 1 {
                fdt.set_property_string("enable-method", "psci")?;
            }
            fdt.set_property_u64("reg", mpidr & 0x007F_FFFF)?;
            fdt.end_node(mpidr_node_dep)?;
        }

        fdt.end_node(cpus_node_dep)?;

        Ok(())
    }

    fn generate_memory_node(&self, fdt: &mut FdtBuilder) -> util::errors::Result<()> {
        let mem_base = MEM_LAYOUT[LayoutEntryType::Mem as usize].0;
        let mem_size = self.sys_mem.memory_end_address().raw_value()
            - MEM_LAYOUT[LayoutEntryType::Mem as usize].0;
        let node = "memory";
        let memory_node_dep = fdt.begin_node(node)?;
        fdt.set_property_string("device_type", "memory")?;
        fdt.set_property_array_u64("reg", &[mem_base, mem_size as u64])?;
        fdt.end_node(memory_node_dep)?;

        Ok(())
    }

    fn generate_devices_node(&self, fdt: &mut FdtBuilder) -> util::errors::Result<()> {
        // timer
        let mut cells: Vec<u32> = Vec::new();
        for &irq in [13, 14, 11, 10].iter() {
            cells.push(device_tree::GIC_FDT_IRQ_TYPE_PPI);
            cells.push(irq);
            cells.push(device_tree::IRQ_TYPE_LEVEL_HIGH);
        }
        let node = "timer";
        let timer_node_dep = fdt.begin_node(node)?;
        fdt.set_property_string("compatible", "arm,armv8-timer")?;
        fdt.set_property("always-on", &Vec::new())?;
        fdt.set_property_array_u32("interrupts", &cells)?;
        fdt.end_node(timer_node_dep)?;

        // clock
        let node = "apb-pclk";
        let clock_node_dep = fdt.begin_node(node)?;
        fdt.set_property_string("compatible", "fixed-clock")?;
        fdt.set_property_string("clock-output-names", "clk24mhz")?;
        fdt.set_property_u32("#clock-cells", 0x0)?;
        fdt.set_property_u32("clock-frequency", 24_000_000)?;
        fdt.set_property_u32("phandle", device_tree::CLK_PHANDLE)?;
        fdt.end_node(clock_node_dep)?;

        // psci
        let node = "psci";
        let psci_node_dep = fdt.begin_node(node)?;
        fdt.set_property_string("compatible", "arm,psci-0.2")?;
        fdt.set_property_string("method", "hvc")?;
        fdt.end_node(psci_node_dep)?;

        for dev in self.sysbus.devices.iter() {
            let mut locked_dev = dev.lock().unwrap();
            let dev_type = locked_dev.get_type();
            let sys_res = locked_dev.get_sys_resource().unwrap();
            match dev_type {
                SysBusDevType::Serial => generate_serial_device_node(fdt, sys_res)?,
                SysBusDevType::Rtc => generate_rtc_device_node(fdt, sys_res)?,
                SysBusDevType::VirtioMmio => generate_virtio_devices_node(fdt, sys_res)?,
                _ => (),
            }
        }
        Ok(())
    }

    fn generate_chosen_node(&self, fdt: &mut FdtBuilder) -> util::errors::Result<()> {
        let node = "chosen";
        let boot_source = self.boot_source.lock().unwrap();

        let chosen_node_dep = fdt.begin_node(node)?;
        let cmdline = &boot_source.kernel_cmdline.to_string();
        fdt.set_property_string("bootargs", cmdline.as_str())?;

        match &boot_source.initrd {
            Some(initrd) => {
                fdt.set_property_u64("linux,initrd-start", initrd.initrd_addr)?;
                fdt.set_property_u64("linux,initrd-end", initrd.initrd_addr + initrd.initrd_size)?;
            }
            None => {}
        }
        fdt.end_node(chosen_node_dep)?;

        Ok(())
    }
}

#[cfg(target_arch = "aarch64")]
impl device_tree::CompileFDT for LightMachine {
    fn generate_fdt_node(&self, fdt: &mut FdtBuilder) -> util::errors::Result<()> {
        let node_dep = fdt.begin_node("")?;

        fdt.set_property_string("compatible", "linux,dummy-virt")?;
        fdt.set_property_u32("#address-cells", 0x2)?;
        fdt.set_property_u32("#size-cells", 0x2)?;
        fdt.set_property_u32("interrupt-parent", device_tree::GIC_PHANDLE)?;

        self.generate_cpu_nodes(fdt)?;
        self.generate_memory_node(fdt)?;
        self.generate_devices_node(fdt)?;
        self.generate_chosen_node(fdt)?;
        self.irq_chip.as_ref().unwrap().generate_fdt_node(fdt)?;

        fdt.end_node(node_dep)?;

        Ok(())
    }
}
