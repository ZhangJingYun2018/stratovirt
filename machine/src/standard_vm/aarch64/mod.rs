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

mod pci_host_root;
mod syscall;

use std::fs::OpenOptions;
use std::ops::Deref;
use std::sync::{Arc, Condvar, Mutex};

use address_space::{AddressSpace, GuestAddress, Region};
use boot_loader::{load_linux, BootLoaderConfig};
use cpu::{CPUBootConfig, CPUInterface, CpuTopology, CPU};
use devices::legacy::{
    errors::ErrorKind as DevErrorKind, FwCfgEntryType, FwCfgMem, FwCfgOps, PFlash, PL011, PL031,
};
use devices::{InterruptController, InterruptControllerConfig};
use error_chain::ChainedError;
use hypervisor::kvm::KVM_FDS;
use machine_manager::config::{BootSource, PFlashConfig, SerialConfig, VmConfig};
use machine_manager::machine::{
    KvmVmState, MachineAddressInterface, MachineExternalInterface, MachineInterface,
    MachineLifecycle, MigrateInterface,
};
use machine_manager::qmp::{qmp_schema, QmpChannel, Response};
use migration::{MigrationManager, MigrationStatus};
use pci::{PciDevOps, PciHost};
use sysbus::{SysBus, SysBusDevOps, SysBusDevType, SysRes};
use util::byte_code::ByteCode;
use util::device_tree::{self, CompileFDT, FdtBuilder};
use util::loop_context::EventLoopManager;
use util::seccomp::BpfRule;
use util::set_termi_canon_mode;
use vmm_sys_util::eventfd::EventFd;

use super::{errors::Result as StdResult, AcpiBuilder, StdMachineOps};
use crate::errors::{ErrorKind, Result};
use crate::MachineOps;
use pci_host_root::PciHostRoot;
use syscall::syscall_whitelist;

/// The type of memory layout entry on aarch64
#[allow(dead_code)]
pub enum LayoutEntryType {
    Flash = 0,
    GicDist,
    GicCpu,
    GicIts,
    GicRedist,
    Uart,
    Rtc,
    FwCfg,
    Mmio,
    PcieMmio,
    PciePio,
    PcieEcam,
    Mem,
    HighGicRedist,
    HighPcieEcam,
    HighPcieMmio,
}

/// Layout of aarch64
pub const MEM_LAYOUT: &[(u64, u64)] = &[
    (0, 0x0800_0000),              // Flash
    (0x0800_0000, 0x0001_0000),    // GicDist
    (0x0801_0000, 0x0001_0000),    // GicCpu
    (0x0808_0000, 0x0002_0000),    // GicIts
    (0x080A_0000, 0x00F6_0000),    // GicRedist (max 123 redistributors)
    (0x0900_0000, 0x0000_1000),    // Uart
    (0x0901_0000, 0x0000_1000),    // Rtc
    (0x0902_0000, 0x0000_0018),    // FwCfg
    (0x0A00_0000, 0x0000_0200),    // Mmio
    (0x1000_0000, 0x2EFF_0000),    // PcieMmio
    (0x3EFF_0000, 0x0001_0000),    // PciePio
    (0x3F00_0000, 0x0100_0000),    // PcieEcam
    (0x4000_0000, 0x80_0000_0000), // Mem
    (256 << 30, 0x200_0000),       // HighGicRedist, (where remaining redistributors locates)
    (257 << 30, 0x1000_0000),      // HighPcieEcam
    (258 << 30, 512 << 30),        // HighPcieMmio
];

/// Standard machine structure.
pub struct StdMachine {
    /// `vCPU` topology, support sockets, cores, threads.
    cpu_topo: CpuTopology,
    /// `vCPU` devices.
    cpus: Vec<Arc<CPU>>,
    // Interrupt controller device.
    #[cfg(target_arch = "aarch64")]
    irq_chip: Option<Arc<InterruptController>>,
    /// Memory address space.
    pub sys_mem: Arc<AddressSpace>,
    /// System bus.
    sysbus: SysBus,
    /// PCI/PCIe host bridge.
    pci_host: Arc<Mutex<PciHost>>,
    /// VM running state.
    vm_state: Arc<(Mutex<KvmVmState>, Condvar)>,
    /// Vm boot_source config.
    boot_source: Arc<Mutex<BootSource>>,
    /// VM power button, handle VM `Shutdown` event.
    power_button: EventFd,
    vm_config: Mutex<VmConfig>,
    /// Reset request, handle VM `Reset` event.
    reset_req: EventFd,
    /// Device Tree Blob.
    dtb_vec: Vec<u8>,
}

impl StdMachine {
    pub fn new(vm_config: &VmConfig) -> Result<Self> {
        use crate::errors::ResultExt;

        let cpu_topo = CpuTopology::new(vm_config.machine_config.nr_cpus);
        let sys_mem = AddressSpace::new(Region::init_container_region(u64::max_value()))
            .chain_err(|| ErrorKind::CrtIoSpaceErr)?;
        let sysbus = SysBus::new(
            &sys_mem,
            (32, 192),
            (
                MEM_LAYOUT[LayoutEntryType::Mmio as usize].0,
                MEM_LAYOUT[LayoutEntryType::Mmio as usize + 1].0,
            ),
        );

        Ok(StdMachine {
            cpu_topo,
            cpus: Vec::new(),
            irq_chip: None,
            sys_mem: sys_mem.clone(),
            sysbus,
            pci_host: Arc::new(Mutex::new(PciHost::new(
                &sys_mem,
                MEM_LAYOUT[LayoutEntryType::PcieEcam as usize],
                MEM_LAYOUT[LayoutEntryType::PcieMmio as usize],
            ))),
            boot_source: Arc::new(Mutex::new(vm_config.clone().boot_source)),
            vm_state: Arc::new((Mutex::new(KvmVmState::Created), Condvar::new())),
            power_button: EventFd::new(libc::EFD_NONBLOCK)
                .chain_err(|| ErrorKind::InitEventFdErr("power_button".to_string()))?,
            vm_config: Mutex::new(vm_config.clone()),
            reset_req: EventFd::new(libc::EFD_NONBLOCK)
                .chain_err(|| ErrorKind::InitEventFdErr("reset_req".to_string()))?,
            dtb_vec: Vec::new(),
        })
    }

    /// Run `LightMachine` with `paused` flag.
    ///
    /// # Arguments
    ///
    /// * `paused` - Flag for `paused` when `LightMachine` starts to run.
    pub fn run(&self, paused: bool) -> Result<()> {
        <Self as MachineOps>::vm_start(paused, &self.cpus, &mut self.vm_state.0.lock().unwrap())
    }

    pub fn handle_reset_request(vm: &Arc<Mutex<Self>>) -> Result<()> {
        use crate::errors::ResultExt;

        let locked_vm = vm.lock().unwrap();
        let mut fdt_addr: u64 = 0;

        for (cpu_index, cpu) in locked_vm.cpus.iter().enumerate() {
            cpu.pause()
                .chain_err(|| format!("Failed to pause vcpu{}", cpu_index))?;

            cpu.set_to_boot_state();
            if cpu_index == 0 {
                fdt_addr = cpu.arch().lock().unwrap().core_regs().regs.regs[0];
            }
            cpu.fd()
                .vcpu_init(&cpu.arch().lock().unwrap().kvi())
                .chain_err(|| "Failed to init vcpu fd")?;
        }

        locked_vm
            .sys_mem
            .write(
                &mut locked_vm.dtb_vec.as_slice(),
                GuestAddress(fdt_addr as u64),
                locked_vm.dtb_vec.len() as u64,
            )
            .chain_err(|| "Fail to write dtb into sysmem")?;

        for dev in locked_vm.sysbus.devices.iter() {
            dev.lock()
                .unwrap()
                .reset()
                .chain_err(|| "Fail to reset sysbus device")?;
        }
        locked_vm
            .pci_host
            .lock()
            .unwrap()
            .reset()
            .chain_err(|| "Fail to reset pci host")?;

        for (cpu_index, cpu) in locked_vm.cpus.iter().enumerate() {
            cpu.resume()
                .chain_err(|| format!("Failed to resume vcpu{}", cpu_index))?;
        }

        Ok(())
    }
}

impl StdMachineOps for StdMachine {
    fn init_pci_host(&self) -> StdResult<()> {
        use super::errors::ResultExt;

        let root_bus = Arc::downgrade(&self.pci_host.lock().unwrap().root_bus);
        let mmconfig_region_ops = PciHost::build_mmconfig_ops(self.pci_host.clone());
        let mmconfig_region = Region::init_io_region(
            MEM_LAYOUT[LayoutEntryType::PcieEcam as usize].1,
            mmconfig_region_ops,
        );
        self.sys_mem
            .root()
            .add_subregion(
                mmconfig_region,
                MEM_LAYOUT[LayoutEntryType::PcieEcam as usize].0,
            )
            .chain_err(|| "Failed to register ECAM in memory space.")?;

        let pcihost_root = PciHostRoot::new(root_bus);
        pcihost_root
            .realize()
            .chain_err(|| "Failed to realize pcihost root device.")?;

        Ok(())
    }

    fn add_fwcfg_device(&mut self) -> StdResult<Arc<Mutex<dyn FwCfgOps>>> {
        use super::errors::ResultExt;

        let mut fwcfg = FwCfgMem::new(self.sys_mem.clone());
        let ncpus = self.cpus.len();
        fwcfg
            .add_data_entry(FwCfgEntryType::NbCpus, ncpus.as_bytes().to_vec())
            .chain_err(|| DevErrorKind::AddEntryErr("NbCpus".to_string()))?;

        let cmdline = self.boot_source.lock().unwrap().kernel_cmdline.to_string();
        fwcfg
            .add_data_entry(
                FwCfgEntryType::CmdlineSize,
                (cmdline.len() + 1).as_bytes().to_vec(),
            )
            .chain_err(|| DevErrorKind::AddEntryErr("CmdlineSize".to_string()))?;
        fwcfg
            .add_string_entry(FwCfgEntryType::CmdlineData, cmdline.as_str())
            .chain_err(|| DevErrorKind::AddEntryErr("CmdlineData".to_string()))?;

        let boot_order = Vec::<u8>::new();
        fwcfg
            .add_file_entry("bootorder", boot_order)
            .chain_err(|| DevErrorKind::AddEntryErr("bootorder".to_string()))?;

        let bios_geometry = Vec::<u8>::new();
        fwcfg
            .add_file_entry("bios-geometry", bios_geometry)
            .chain_err(|| DevErrorKind::AddEntryErr("bios-geometry".to_string()))?;

        let fwcfg_dev = FwCfgMem::realize(
            fwcfg,
            &mut self.sysbus,
            MEM_LAYOUT[LayoutEntryType::FwCfg as usize].0,
            MEM_LAYOUT[LayoutEntryType::FwCfg as usize].1,
        )
        .chain_err(|| "Failed to realize fwcfg device")?;

        Ok(fwcfg_dev)
    }

    fn get_vm_state(&self) -> &Arc<(Mutex<KvmVmState>, Condvar)> {
        &self.vm_state
    }

    fn get_cpu_topo(&self) -> &CpuTopology {
        &self.cpu_topo
    }

    fn get_cpus(&self) -> &Vec<Arc<CPU>> {
        &self.cpus
    }

    fn get_vm_config(&self) -> &Mutex<VmConfig> {
        &self.vm_config
    }
}

impl MachineOps for StdMachine {
    fn arch_ram_ranges(&self, mem_size: u64) -> Vec<(u64, u64)> {
        vec![(MEM_LAYOUT[LayoutEntryType::Mem as usize].0, mem_size)]
    }

    fn init_interrupt_controller(&mut self, vcpu_count: u64) -> Result<()> {
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
        KVM_FDS
            .load()
            .irq_route_table
            .lock()
            .unwrap()
            .init_irq_route_table();
        KVM_FDS.load().commit_irq_routing()?;
        Ok(())
    }

    fn load_boot_source(&self, fwcfg: Option<&Arc<Mutex<dyn FwCfgOps>>>) -> Result<CPUBootConfig> {
        use crate::errors::ResultExt;

        let mut boot_source = self.boot_source.lock().unwrap();
        let initrd = boot_source.initrd.as_ref().map(|b| b.initrd_file.clone());

        let bootloader_config = BootLoaderConfig {
            kernel: boot_source.kernel_file.clone(),
            initrd,
            mem_start: MEM_LAYOUT[LayoutEntryType::Mem as usize].0,
        };
        let layout = load_linux(&bootloader_config, &self.sys_mem, fwcfg)
            .chain_err(|| ErrorKind::LoadKernErr)?;
        if let Some(rd) = &mut boot_source.initrd {
            rd.initrd_addr = layout.initrd_start;
            rd.initrd_size = layout.initrd_size;
        }

        Ok(CPUBootConfig {
            fdt_addr: layout.dtb_start,
            boot_pc: layout.boot_pc,
        })
    }

    fn add_rtc_device(&mut self) -> Result<()> {
        use crate::errors::ResultExt;

        let rtc = PL031::default();
        PL031::realize(
            rtc,
            &mut self.sysbus,
            MEM_LAYOUT[LayoutEntryType::Rtc as usize].0,
            MEM_LAYOUT[LayoutEntryType::Rtc as usize].1,
        )
        .chain_err(|| "Failed to realize PL031")?;
        Ok(())
    }

    fn add_serial_device(&mut self, config: &SerialConfig) -> Result<()> {
        use crate::errors::ResultExt;

        let region_base: u64 = MEM_LAYOUT[LayoutEntryType::Uart as usize].0;
        let region_size: u64 = MEM_LAYOUT[LayoutEntryType::Uart as usize].1;

        let pl011 = PL011::new(config.clone()).chain_err(|| "Failed to create PL011")?;
        pl011
            .realize(
                &mut self.sysbus,
                region_base,
                region_size,
                &self.boot_source,
            )
            .chain_err(|| "Failed to realize PL011")?;
        Ok(())
    }

    fn syscall_whitelist(&self) -> Vec<BpfRule> {
        syscall_whitelist()
    }

    fn realize(vm: &Arc<Mutex<Self>>, vm_config: &mut VmConfig, is_migrate: bool) -> Result<()> {
        use super::errors::ErrorKind as StdErrorKind;
        use crate::errors::ResultExt;

        let clone_vm = vm.clone();
        let mut locked_vm = vm.lock().unwrap();
        locked_vm.init_global_config(vm_config)?;
        locked_vm
            .register_reset_event(&locked_vm.reset_req, clone_vm)
            .chain_err(|| "Fail to register reset event")?;
        locked_vm.init_memory(
            &vm_config.machine_config.mem_config,
            &locked_vm.sys_mem,
            is_migrate,
            vm_config.machine_config.nr_cpus,
        )?;

        let vcpu_fds = {
            let mut fds = vec![];
            for vcpu_id in 0..vm_config.machine_config.nr_cpus {
                fds.push(Arc::new(
                    KVM_FDS
                        .load()
                        .vm_fd
                        .as_ref()
                        .unwrap()
                        .create_vcpu(vcpu_id)?,
                ));
            }
            fds
        };

        // Interrupt Controller Chip init
        locked_vm.init_interrupt_controller(u64::from(vm_config.machine_config.nr_cpus))?;
        locked_vm
            .init_pci_host()
            .chain_err(|| StdErrorKind::InitPCIeHostErr)?;
        locked_vm
            .add_devices(vm_config)
            .chain_err(|| "Failed to add devices")?;

        let boot_config = if !is_migrate {
            let fwcfg = locked_vm.add_fwcfg_device()?;
            Some(locked_vm.load_boot_source(Some(&fwcfg))?)
        } else {
            None
        };

        locked_vm.cpus.extend(<Self as MachineOps>::init_vcpu(
            vm.clone(),
            vm_config.machine_config.nr_cpus,
            &vcpu_fds,
            &boot_config,
        )?);

        if let Some(boot_cfg) = boot_config {
            let mut fdt_helper = FdtBuilder::new();
            locked_vm
                .generate_fdt_node(&mut fdt_helper)
                .chain_err(|| ErrorKind::GenFdtErr)?;
            let fdt_vec = fdt_helper.finish()?;
            locked_vm.dtb_vec = fdt_vec.clone();
            locked_vm
                .sys_mem
                .write(
                    &mut fdt_vec.as_slice(),
                    GuestAddress(boot_cfg.fdt_addr as u64),
                    fdt_vec.len() as u64,
                )
                .chain_err(|| ErrorKind::WrtFdtErr(boot_cfg.fdt_addr, fdt_vec.len()))?;
        }

        locked_vm.register_power_event(&locked_vm.power_button)?;

        if let Err(e) = MigrationManager::set_status(MigrationStatus::Setup) {
            bail!("Failed to set migration status {}", e);
        }
        Ok(())
    }

    fn add_pflash_device(&mut self, configs: &[PFlashConfig]) -> Result<()> {
        use super::errors::ErrorKind as StdErrorKind;
        use crate::errors::ResultExt;

        let mut configs_vec = configs.to_vec();
        configs_vec.sort_by_key(|c| c.unit);
        let sector_len: u32 = 1024 * 256;
        let mut flash_base: u64 = MEM_LAYOUT[LayoutEntryType::Flash as usize].0;
        let flash_size: u64 = MEM_LAYOUT[LayoutEntryType::Flash as usize].1 / 2;
        for i in 0..=1 {
            let (fd, read_only) = if i < configs_vec.len() {
                let path = &configs_vec[i].path_on_host;
                let read_only = configs_vec[i].read_only;
                let fd = OpenOptions::new()
                    .read(true)
                    .write(!read_only)
                    .open(path)
                    .chain_err(|| StdErrorKind::OpenFileErr(path.to_string()))?;
                (Some(fd), read_only)
            } else {
                (None, false)
            };

            let pflash = PFlash::new(flash_size, &fd, sector_len, 4, 2, read_only)
                .chain_err(|| StdErrorKind::InitPflashErr)?;
            PFlash::realize(pflash, &mut self.sysbus, flash_base, flash_size, fd)
                .chain_err(|| StdErrorKind::RlzPflashErr)?;
            flash_base += flash_size;
        }

        Ok(())
    }

    fn run(&self, paused: bool) -> Result<()> {
        <Self as MachineOps>::vm_start(paused, &self.cpus, &mut self.vm_state.0.lock().unwrap())
    }

    fn get_sys_mem(&mut self) -> &Arc<AddressSpace> {
        &self.sys_mem
    }

    fn get_pci_host(&mut self) -> StdResult<&Arc<Mutex<PciHost>>> {
        Ok(&self.pci_host)
    }
}

impl AcpiBuilder for StdMachine {}

impl MachineLifecycle for StdMachine {
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
        true
    }

    fn reset(&mut self) -> bool {
        self.reset_req.write(1).unwrap();
        true
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

impl MachineAddressInterface for StdMachine {
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

impl MigrateInterface for StdMachine {
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

impl MachineInterface for StdMachine {}
impl MachineExternalInterface for StdMachine {}

impl EventLoopManager for StdMachine {
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

// Function that helps to generate pci node in device-tree.
//
// # Arguments
//
// * `fdt` - Flatted device-tree blob where node will be filled into.
fn generate_pci_host_node(fdt: &mut FdtBuilder) -> util::errors::Result<()> {
    let pcie_ecam_base = MEM_LAYOUT[LayoutEntryType::PcieEcam as usize].0;
    let pcie_ecam_size = MEM_LAYOUT[LayoutEntryType::PcieEcam as usize].1;
    let pcie_buses_num = MEM_LAYOUT[LayoutEntryType::PcieEcam as usize].1 >> 20;
    let node = format!("pcie@{:x}", pcie_ecam_base);
    let pci_node_dep = fdt.begin_node(&node)?;
    fdt.set_property_string("compatible", "pci-host-ecam-generic")?;
    fdt.set_property_string("device_type", "pci")?;
    fdt.set_property_array_u64("reg", &[pcie_ecam_base, pcie_ecam_size])?;
    fdt.set_property_array_u32("bus-range", &[0, (pcie_buses_num - 1) as u32])?;
    fdt.set_property_u32("linux,pci-domain", 0)?;
    fdt.set_property_u32("#address-cells", 3)?;
    fdt.set_property_u32("#size-cells", 2)?;

    let pcie_mmio_base = MEM_LAYOUT[LayoutEntryType::PcieMmio as usize].0;
    let pcie_mmio_size = MEM_LAYOUT[LayoutEntryType::PcieMmio as usize].1;
    let fdt_pci_mmio_type: u32 = 0x0200_0000;
    let mmio_base_hi: u32 = (pcie_mmio_base >> 32) as u32;
    let mmio_base_lo: u32 = (pcie_mmio_base & 0xffff_ffff) as u32;
    let mmio_size_hi: u32 = (pcie_mmio_size >> 32) as u32;
    let mmio_size_lo: u32 = (pcie_mmio_size & 0xffff_ffff) as u32;

    let pcie_pio_base = MEM_LAYOUT[LayoutEntryType::PciePio as usize].0;
    let pcie_pio_size = MEM_LAYOUT[LayoutEntryType::PciePio as usize].1;
    let fdt_pci_pio_type: u32 = 0x0100_0000;
    let pio_base_hi: u32 = (pcie_pio_base >> 32) as u32;
    let pio_base_lo: u32 = (pcie_pio_base & 0xffff_ffff) as u32;
    let pio_size_hi: u32 = (pcie_pio_size >> 32) as u32;
    let pio_size_lo: u32 = (pcie_pio_size & 0xffff_ffff) as u32;

    fdt.set_property_array_u32(
        "ranges",
        &[
            fdt_pci_pio_type,
            0,
            0,
            pio_base_hi,
            pio_base_lo,
            pio_size_hi,
            pio_size_lo,
            fdt_pci_mmio_type,
            mmio_base_hi,
            mmio_base_lo,
            mmio_base_hi,
            mmio_base_lo,
            mmio_size_hi,
            mmio_size_lo,
        ],
    )?;

    fdt.set_property_u32("msi-parent", device_tree::GIC_ITS_PHANDLE)?;
    fdt.end_node(pci_node_dep)?;
    Ok(())
}

// Function that helps to generate Virtio-Mmio device's node in device-tree.
//
// # Arguments
//
// * `dev_info` - Device resource info of Virtio-Mmio device.
// * `fdt` - Flatted device-tree blob where node will be filled into.
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

/// Function that helps to generate flash node in device-tree.
///
/// # Arguments
///
/// * `dev_info` - Device resource info of fw-cfg device.
/// * `flash` - Flatted device-tree blob where fw-cfg node will be filled into.
fn generate_flash_device_node(fdt: &mut FdtBuilder) -> util::errors::Result<()> {
    let flash_base = MEM_LAYOUT[LayoutEntryType::Flash as usize].0;
    let flash_size = MEM_LAYOUT[LayoutEntryType::Flash as usize].1 / 2;
    let node = format!("flash@{:x}", flash_base);
    let flash_node_dep = fdt.begin_node(&node)?;
    fdt.set_property_string("compatible", "cfi-flash")?;
    fdt.set_property_array_u64(
        "reg",
        &[flash_base, flash_size, flash_base + flash_size, flash_size],
    )?;
    fdt.set_property_u32("bank-width", 4)?;
    fdt.end_node(flash_node_dep)?;
    Ok(())
}

/// Function that helps to generate fw-cfg node in device-tree.
///
/// # Arguments
///
/// * `dev_info` - Device resource info of fw-cfg device.
/// * `fdt` - Flatted device-tree blob where fw-cfg node will be filled into.
fn generate_fwcfg_device_node(fdt: &mut FdtBuilder, res: &SysRes) -> util::errors::Result<()> {
    let node = format!("fw-cfg@{:x}", res.region_base);
    let fwcfg_node_dep = fdt.begin_node(&node)?;
    fdt.set_property_string("compatible", "qemu,fw-cfg-mmio")?;
    fdt.set_property_array_u64("reg", &[res.region_base, res.region_size])?;
    fdt.end_node(fwcfg_node_dep)?;

    Ok(())
}

// Function that helps to generate serial node in device-tree.
//
// # Arguments
//
// * `dev_info` - Device resource info of serial device.
// * `fdt` - Flatted device-tree blob where serial node will be filled into.
fn generate_serial_device_node(fdt: &mut FdtBuilder, res: &SysRes) -> util::errors::Result<()> {
    let node = format!("pl011@{:x}", res.region_base);
    let serial_node_dep = fdt.begin_node(&node)?;
    fdt.set_property_string("compatible", "arm,pl011\0arm,primecell")?;
    fdt.set_property_string("clock-names", "uartclk\0apb_pclk")?;
    fdt.set_property_array_u32(
        "clocks",
        &[device_tree::CLK_PHANDLE, device_tree::CLK_PHANDLE],
    )?;
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

/// Trait that helps to generate all nodes in device-tree.
#[allow(clippy::upper_case_acronyms)]
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

impl CompileFDTHelper for StdMachine {
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
            match locked_dev.get_type() {
                SysBusDevType::PL011 => {
                    generate_serial_device_node(fdt, locked_dev.get_sys_resource().unwrap())?
                }
                SysBusDevType::Rtc => {
                    generate_rtc_device_node(fdt, locked_dev.get_sys_resource().unwrap())?
                }
                SysBusDevType::VirtioMmio => {
                    generate_virtio_devices_node(fdt, locked_dev.get_sys_resource().unwrap())?
                }
                SysBusDevType::FwCfg => {
                    generate_fwcfg_device_node(fdt, locked_dev.get_sys_resource().unwrap())?;
                }
                _ => (),
            }
        }
        generate_flash_device_node(fdt)?;

        generate_pci_host_node(fdt)?;

        Ok(())
    }

    fn generate_chosen_node(&self, fdt: &mut FdtBuilder) -> util::errors::Result<()> {
        let node = "chosen";

        let boot_source = self.boot_source.lock().unwrap();

        let chosen_node_dep = fdt.begin_node(node)?;
        let cmdline = &boot_source.kernel_cmdline.to_string();
        fdt.set_property_string("bootargs", cmdline.as_str())?;

        let pl011_property_string =
            format!("/pl011@{:x}", MEM_LAYOUT[LayoutEntryType::Uart as usize].0);
        fdt.set_property_string("stdout-path", &pl011_property_string)?;

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

impl device_tree::CompileFDT for StdMachine {
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
