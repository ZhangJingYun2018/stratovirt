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

use std::sync::{Arc, Mutex};

use kvm_ioctls::DeviceFd;

use super::{
    state::{GICv3ItsState, GICv3State},
    GICConfig, GICDevice, UtilResult,
};
use crate::interrupt_controller::errors::{ErrorKind, Result, ResultExt};
use hypervisor::kvm::KVM_FDS;
use machine_manager::machine::{KvmVmState, MachineLifecycle};
use migration::MigrationManager;
use util::device_tree::{self, FdtBuilder};

// See arch/arm64/include/uapi/asm/kvm.h file from the linux kernel.
const SZ_64K: u64 = 0x0001_0000;
const KVM_VGIC_V3_REDIST_SIZE: u64 = 2 * SZ_64K;

/// A wrapper for kvm_based device check and access.
pub struct KvmDevice;

impl KvmDevice {
    fn kvm_device_check(fd: &DeviceFd, group: u32, attr: u64) -> Result<()> {
        let attr = kvm_bindings::kvm_device_attr {
            group,
            attr,
            addr: 0,
            flags: 0,
        };
        fd.has_device_attr(&attr)
            .chain_err(|| "Failed to check device attributes for GIC.")?;
        Ok(())
    }

    fn kvm_device_access(
        fd: &DeviceFd,
        group: u32,
        attr: u64,
        addr: u64,
        write: bool,
    ) -> Result<()> {
        let attr = kvm_bindings::kvm_device_attr {
            group,
            attr,
            addr,
            flags: 0,
        };

        if write {
            fd.set_device_attr(&attr)
                .chain_err(|| "Failed to set device attributes for GIC.")?;
        } else {
            let mut attr = attr;
            fd.get_device_attr(&mut attr)
                .chain_err(|| "Failed to get device attributes for GIC.")?;
        };

        Ok(())
    }
}

/// Access wrapper for GICv3.
pub trait GICv3Access {
    /// Returns `gicr_attr` of `vCPU`.
    fn vcpu_gicr_attr(&self, cpu: usize) -> u64;

    fn access_gic_distributor(&self, offset: u64, gicd_value: &mut u32, write: bool) -> Result<()>;

    fn access_gic_redistributor(
        &self,
        offset: u64,
        cpu: usize,
        gicr_value: &mut u32,
        write: bool,
    ) -> Result<()>;

    fn access_gic_cpu(
        &self,
        offset: u64,
        cpu: usize,
        gicc_value: &mut u64,
        write: bool,
    ) -> Result<()>;

    fn access_gic_line_level(&self, offset: u64, gicll_value: &mut u32, write: bool) -> Result<()>;
}

struct GicRedistRegion {
    /// Base address.
    base: u64,
    /// Size of redistributor region.
    size: u64,
    /// Attribute of redistributor region.
    base_attr: u64,
}

/// A wrapper around creating and managing a `GICv3`.
pub struct GICv3 {
    /// The fd for the GICv3 device.
    fd: DeviceFd,
    /// Number of vCPUs, determines the number of redistributor and CPU interface.
    pub(crate) vcpu_count: u64,
    /// GICv3 ITS device.
    pub(crate) its_dev: Option<Arc<GICv3Its>>,
    /// Maximum irq number.
    pub(crate) nr_irqs: u32,
    /// GICv3 redistributor info, support multiple redistributor regions.
    redist_regions: Vec<GicRedistRegion>,
    /// Base address in the guest physical address space of the GICv3 distributor
    /// register mappings.
    dist_base: u64,
    /// GICv3 distributor region size.
    dist_size: u64,
    /// Lifecycle state for GICv3.
    state: Arc<Mutex<KvmVmState>>,
}

impl GICv3 {
    fn new(config: &GICConfig) -> Result<Self> {
        config.check_sanity()?;

        let mut gic_device = kvm_bindings::kvm_create_device {
            type_: kvm_bindings::kvm_device_type_KVM_DEV_TYPE_ARM_VGIC_V3,
            fd: 0,
            flags: 0,
        };

        let gic_fd = match KVM_FDS
            .load()
            .vm_fd
            .as_ref()
            .unwrap()
            .create_device(&mut gic_device)
        {
            Ok(fd) => fd,
            Err(e) => return Err(ErrorKind::CreateKvmDevice(e).into()),
        };

        // Calculate GIC redistributor regions' address range according to vcpu count.
        let base = config.redist_region_ranges[0].0;
        let size = config.redist_region_ranges[0].1;
        let redist_capability = size / KVM_VGIC_V3_REDIST_SIZE;
        let redist_region_count = std::cmp::min(config.vcpu_count, redist_capability);
        let mut redist_regions = vec![GicRedistRegion {
            base,
            size,
            base_attr: (redist_region_count << 52) | base,
        }];

        if config.vcpu_count > redist_capability {
            let high_redist_base = config.redist_region_ranges[1].0;
            let high_redist_region_count = config.vcpu_count - redist_capability;
            let high_redist_attr = (high_redist_region_count << 52) | high_redist_base | 0x1;

            redist_regions.push(GicRedistRegion {
                base: high_redist_base,
                size: high_redist_region_count * KVM_VGIC_V3_REDIST_SIZE,
                base_attr: high_redist_attr,
            })
        }

        let mut gicv3 = GICv3 {
            fd: gic_fd,
            vcpu_count: config.vcpu_count,
            nr_irqs: config.max_irq,
            its_dev: None,
            redist_regions,
            dist_base: config.dist_range.0,
            dist_size: config.dist_range.1,
            state: Arc::new(Mutex::new(KvmVmState::Created)),
        };

        if let Some(its_range) = config.its_range {
            gicv3.its_dev = Some(Arc::new(
                GICv3Its::new(&its_range).chain_err(|| "Failed to create ITS")?,
            ));
        }

        Ok(gicv3)
    }

    fn realize(&self) -> Result<()> {
        if self.redist_regions.len() > 1 {
            KvmDevice::kvm_device_check(
                &self.fd,
                kvm_bindings::KVM_DEV_ARM_VGIC_GRP_ADDR,
                kvm_bindings::KVM_VGIC_V3_ADDR_TYPE_REDIST_REGION as u64,
            )
            .chain_err(|| {
                "Multiple redistributors are acquired while KVM does not provide support."
            })?;
        }

        if self.redist_regions.len() == 1 {
            KvmDevice::kvm_device_access(
                &self.fd,
                kvm_bindings::KVM_DEV_ARM_VGIC_GRP_ADDR,
                u64::from(kvm_bindings::KVM_VGIC_V3_ADDR_TYPE_REDIST),
                &self.redist_regions.get(0).unwrap().base as *const u64 as u64,
                true,
            )
            .chain_err(|| "Failed to set GICv3 attribute: redistributor address")?;
        } else {
            for redist in &self.redist_regions {
                KvmDevice::kvm_device_access(
                    &self.fd,
                    kvm_bindings::KVM_DEV_ARM_VGIC_GRP_ADDR,
                    u64::from(kvm_bindings::KVM_VGIC_V3_ADDR_TYPE_REDIST_REGION),
                    &redist.base_attr as *const u64 as u64,
                    true,
                )
                .chain_err(|| "Failed to set GICv3 attribute: redistributor region address")?;
            }
        }

        KvmDevice::kvm_device_access(
            &self.fd,
            kvm_bindings::KVM_DEV_ARM_VGIC_GRP_ADDR,
            u64::from(kvm_bindings::KVM_VGIC_V3_ADDR_TYPE_DIST),
            &self.dist_base as *const u64 as u64,
            true,
        )
        .chain_err(|| "Failed to set GICv3 attribute: distributor address")?;

        KvmDevice::kvm_device_check(&self.fd, kvm_bindings::KVM_DEV_ARM_VGIC_GRP_NR_IRQS, 0)?;

        // Init the interrupt number support by the GIC.
        KvmDevice::kvm_device_access(
            &self.fd,
            kvm_bindings::KVM_DEV_ARM_VGIC_GRP_NR_IRQS,
            0,
            &self.nr_irqs as *const u32 as u64,
            true,
        )
        .chain_err(|| "Failed to set GICv3 attribute: irqs")?;

        // Finalize the GIC.
        KvmDevice::kvm_device_access(
            &self.fd,
            kvm_bindings::KVM_DEV_ARM_VGIC_GRP_CTRL,
            u64::from(kvm_bindings::KVM_DEV_ARM_VGIC_CTRL_INIT),
            0,
            true,
        )
        .chain_err(|| "KVM failed to initialize GICv3")?;

        let mut state = self.state.lock().unwrap();
        *state = KvmVmState::Running;

        Ok(())
    }

    fn device_fd(&self) -> &DeviceFd {
        &self.fd
    }
}

impl MachineLifecycle for GICv3 {
    fn pause(&self) -> bool {
        // VM change state will flush REDIST pending tables into guest RAM.
        if KvmDevice::kvm_device_access(
            self.device_fd(),
            kvm_bindings::KVM_DEV_ARM_VGIC_GRP_CTRL,
            kvm_bindings::KVM_DEV_ARM_VGIC_SAVE_PENDING_TABLES as u64,
            0,
            true,
        )
        .is_ok()
        {
            let mut state = self.state.lock().unwrap();
            *state = KvmVmState::Running;

            true
        } else {
            false
        }
    }

    fn notify_lifecycle(&self, old: KvmVmState, new: KvmVmState) -> bool {
        let state = self.state.lock().unwrap();
        if *state != old {
            error!("GICv3 lifecycle error: state check failed.");
            return false;
        }
        drop(state);

        match (old, new) {
            (KvmVmState::Running, KvmVmState::Paused) => self.pause(),
            _ => true,
        }
    }
}

impl GICv3Access for GICv3 {
    fn vcpu_gicr_attr(&self, cpu: usize) -> u64 {
        let clustersz = 16;

        let aff1 = (cpu / clustersz) as u64;
        let aff0 = (cpu % clustersz) as u64;

        let affid = (aff1 << 8) | aff0;
        let cpu_affid: u64 = ((affid & 0xFF_0000_0000) >> 8) | (affid & 0xFF_FFFF);

        let last = if (self.vcpu_count - 1) == cpu as u64 {
            1
        } else {
            0
        };

        ((cpu_affid << 32) | (1 << 24) | (1 << 8) | (last << 4))
            & kvm_bindings::KVM_DEV_ARM_VGIC_V3_MPIDR_MASK as u64
    }

    fn access_gic_distributor(&self, offset: u64, gicd_value: &mut u32, write: bool) -> Result<()> {
        KvmDevice::kvm_device_access(
            &self.fd,
            kvm_bindings::KVM_DEV_ARM_VGIC_GRP_DIST_REGS,
            offset,
            gicd_value as *mut u32 as u64,
            write,
        )
        .chain_err(|| format!("Failed to access gic distributor for offset 0x{:x}", offset))
    }

    fn access_gic_redistributor(
        &self,
        offset: u64,
        cpu: usize,
        gicr_value: &mut u32,
        write: bool,
    ) -> Result<()> {
        KvmDevice::kvm_device_access(
            &self.fd,
            kvm_bindings::KVM_DEV_ARM_VGIC_GRP_REDIST_REGS,
            self.vcpu_gicr_attr(cpu) | offset,
            gicr_value as *mut u32 as u64,
            write,
        )
        .chain_err(|| {
            format!(
                "Failed to access gic redistributor for offset 0x{:x}",
                offset
            )
        })
    }

    fn access_gic_cpu(
        &self,
        offset: u64,
        cpu: usize,
        gicc_value: &mut u64,
        write: bool,
    ) -> Result<()> {
        KvmDevice::kvm_device_access(
            &self.fd,
            kvm_bindings::KVM_DEV_ARM_VGIC_GRP_CPU_SYSREGS,
            self.vcpu_gicr_attr(cpu) | offset,
            gicc_value as *mut u64 as u64,
            write,
        )
        .chain_err(|| format!("Failed to access gic cpu for offset 0x{:x}", offset))
    }

    fn access_gic_line_level(&self, offset: u64, gicll_value: &mut u32, write: bool) -> Result<()> {
        KvmDevice::kvm_device_access(
            &self.fd,
            kvm_bindings::KVM_DEV_ARM_VGIC_GRP_LEVEL_INFO,
            self.vcpu_gicr_attr(0) | offset,
            gicll_value as *mut u32 as u64,
            write,
        )
    }
}

impl GICDevice for GICv3 {
    fn create_device(
        gic_conf: &GICConfig,
    ) -> Result<Arc<dyn GICDevice + std::marker::Send + std::marker::Sync>> {
        let gicv3 = Arc::new(GICv3::new(gic_conf)?);
        if gicv3.its_dev.is_some() {
            MigrationManager::register_device_instance(
                GICv3ItsState::descriptor(),
                gicv3.its_dev.as_ref().unwrap().clone(),
                true,
            );
        }
        MigrationManager::register_device_instance(GICv3State::descriptor(), gicv3.clone(), true);

        Ok(gicv3)
    }

    fn realize(&self) -> Result<()> {
        self.realize().chain_err(|| "Failed to realize GICv3")?;

        if let Some(its) = &self.its_dev {
            its.realize().chain_err(|| "Failed to realize ITS")?;
        }

        Ok(())
    }

    fn generate_fdt(&self, fdt: &mut FdtBuilder) -> UtilResult<()> {
        let redist_count = self.redist_regions.len() as u32;
        let mut gic_reg = vec![self.dist_base, self.dist_size];

        for redist in &self.redist_regions {
            gic_reg.push(redist.base);
            gic_reg.push(redist.size);
        }

        let node = "intc";
        let intc_node_dep = fdt.begin_node(node)?;
        fdt.set_property_string("compatible", "arm,gic-v3")?;
        fdt.set_property("interrupt-controller", &Vec::new())?;
        fdt.set_property_u32("#interrupt-cells", 0x3)?;
        fdt.set_property_u32("phandle", device_tree::GIC_PHANDLE)?;
        fdt.set_property_u32("#address-cells", 0x2)?;
        fdt.set_property_u32("#size-cells", 0x2)?;
        fdt.set_property_u32("#redistributor-regions", redist_count)?;
        fdt.set_property_array_u64("reg", &gic_reg)?;

        let gic_intr = [
            device_tree::GIC_FDT_IRQ_TYPE_PPI,
            0x9,
            device_tree::IRQ_TYPE_LEVEL_HIGH,
        ];
        fdt.set_property_array_u32("interrupts", &gic_intr)?;

        if let Some(its) = &self.its_dev {
            fdt.set_property("ranges", &Vec::new())?;
            let its_reg = [its.msi_base, its.msi_size];
            let node = "its";
            let its_node_dep = fdt.begin_node(node)?;
            fdt.set_property_string("compatible", "arm,gic-v3-its")?;
            fdt.set_property("msi-controller", &Vec::new())?;
            fdt.set_property_u32("phandle", device_tree::GIC_ITS_PHANDLE)?;
            fdt.set_property_array_u64("reg", &its_reg)?;
            fdt.end_node(its_node_dep)?;
        }
        fdt.end_node(intc_node_dep)?;

        Ok(())
    }
}

pub(crate) struct GICv3Its {
    /// The fd for the GICv3Its device
    fd: DeviceFd,

    /// Base address in the guest physical address space of the GICv3 ITS
    /// control register frame.
    msi_base: u64,

    /// GICv3 ITS needs to be 64K aligned and the region covers 128K.
    msi_size: u64,
}

impl GICv3Its {
    fn new(its_range: &(u64, u64)) -> Result<Self> {
        let mut its_device = kvm_bindings::kvm_create_device {
            type_: kvm_bindings::kvm_device_type_KVM_DEV_TYPE_ARM_VGIC_ITS,
            fd: 0,
            flags: 0,
        };

        let its_fd = match KVM_FDS
            .load()
            .vm_fd
            .as_ref()
            .unwrap()
            .create_device(&mut its_device)
        {
            Ok(fd) => fd,
            Err(e) => return Err(ErrorKind::CreateKvmDevice(e).into()),
        };

        Ok(GICv3Its {
            fd: its_fd,
            msi_base: its_range.0,
            msi_size: its_range.1,
        })
    }

    fn realize(&self) -> Result<()> {
        KvmDevice::kvm_device_check(
            &self.fd,
            kvm_bindings::KVM_DEV_ARM_VGIC_GRP_ADDR,
            u64::from(kvm_bindings::KVM_VGIC_ITS_ADDR_TYPE),
        )
        .chain_err(|| "ITS address attribute is not supported for KVM")?;

        KvmDevice::kvm_device_access(
            &self.fd,
            kvm_bindings::KVM_DEV_ARM_VGIC_GRP_ADDR,
            u64::from(kvm_bindings::KVM_VGIC_ITS_ADDR_TYPE),
            &self.msi_base as *const u64 as u64,
            true,
        )
        .chain_err(|| "Failed to set ITS attribute: ITS address")?;

        // Finalize the GIC Its.
        KvmDevice::kvm_device_access(
            &self.fd,
            kvm_bindings::KVM_DEV_ARM_VGIC_GRP_CTRL,
            u64::from(kvm_bindings::KVM_DEV_ARM_VGIC_CTRL_INIT),
            &self.msi_base as *const u64 as u64,
            true,
        )
        .chain_err(|| "KVM failed to initialize ITS")?;

        Ok(())
    }

    pub(crate) fn access_gic_its(&self, attr: u32, its_value: &mut u64, write: bool) -> Result<()> {
        KvmDevice::kvm_device_access(
            &self.fd,
            kvm_bindings::KVM_DEV_ARM_VGIC_GRP_ITS_REGS,
            attr as u64,
            its_value as *const u64 as u64,
            write,
        )
    }

    pub(crate) fn access_gic_its_tables(&self, save: bool) -> Result<()> {
        let attr = if save {
            kvm_bindings::KVM_DEV_ARM_ITS_SAVE_TABLES as u64
        } else {
            kvm_bindings::KVM_DEV_ARM_ITS_RESTORE_TABLES as u64
        };
        KvmDevice::kvm_device_access(
            &self.fd,
            kvm_bindings::KVM_DEV_ARM_VGIC_GRP_CTRL,
            attr,
            std::ptr::null::<u64>() as u64,
            true,
        )
    }
}

#[cfg(test)]
mod tests {
    use hypervisor::kvm::KVMFds;
    use serial_test::serial;

    use super::super::GICConfig;
    use super::*;

    #[test]
    #[serial]
    fn test_create_gicv3() {
        let kvm_fds = KVMFds::new();
        if kvm_fds.vm_fd.is_none() {
            return;
        }
        KVM_FDS.store(Arc::new(kvm_fds));

        let gic_conf = GICConfig {
            version: kvm_bindings::kvm_device_type_KVM_DEV_TYPE_ARM_VGIC_V3.into(),
            vcpu_count: 4,
            max_irq: 192,
            msi: false,
            dist_range: (0x0800_0000, 0x0001_0000),
            redist_region_ranges: vec![(0x080A_0000, 0x00F6_0000)],
            its_range: None,
        };
        assert!(GICv3::new(&gic_conf).is_ok());
    }

    #[test]
    #[serial]
    fn test_create_gic_device() {
        let kvm_fds = KVMFds::new();
        if kvm_fds.vm_fd.is_none() {
            return;
        }
        KVM_FDS.store(Arc::new(kvm_fds));

        let gic_config = GICConfig {
            version: kvm_bindings::kvm_device_type_KVM_DEV_TYPE_ARM_VGIC_V3,
            vcpu_count: 4_u64,
            max_irq: 192_u32,
            msi: false,
            dist_range: (0x0800_0000, 0x0001_0000),
            redist_region_ranges: vec![(0x080A_0000, 0x00F6_0000)],
            its_range: None,
        };
        let gic = GICv3::new(&gic_config).unwrap();
        assert!(gic.its_dev.is_none());
        assert!(GICv3::new(&gic_config).is_err());
    }

    #[test]
    #[serial]
    fn test_gic_redist_regions() {
        let kvm_fds = KVMFds::new();
        if kvm_fds.vm_fd.is_none() {
            return;
        }
        KVM_FDS.store(Arc::new(kvm_fds));

        let gic_config = GICConfig {
            version: kvm_bindings::kvm_device_type_KVM_DEV_TYPE_ARM_VGIC_V3,
            vcpu_count: 210_u64,
            max_irq: 192_u32,
            msi: true,
            dist_range: (0x0800_0000, 0x0001_0000),
            redist_region_ranges: vec![(0x080A_0000, 0x00F6_0000), (256 << 30, 0x200_0000)],
            its_range: Some((0x0808_0000, 0x0002_0000)),
        };
        let gic = GICv3::new(&gic_config).unwrap();

        assert!(gic.its_dev.is_some());
        assert_eq!(gic.redist_regions.len(), 2);
    }
}
