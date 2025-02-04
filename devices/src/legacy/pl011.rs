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

use acpi::{
    AmlActiveLevel, AmlBuilder, AmlDevice, AmlEdgeLevel, AmlExtendedInterrupt, AmlIntShare,
    AmlInteger, AmlMemory32Fixed, AmlNameDecl, AmlReadAndWrite, AmlResTemplate, AmlResourceUsage,
    AmlScopeBuilder, AmlString,
};
use address_space::GuestAddress;
use byteorder::{ByteOrder, LittleEndian};
use machine_manager::{
    config::{BootSource, Param, SerialConfig},
    event_loop::EventLoop,
};
use migration::{DeviceStateDesc, FieldDesc, MigrationHook, MigrationManager, StateTransfer};
use sysbus::{SysBus, SysBusDevOps, SysBusDevType, SysRes};
use util::byte_code::ByteCode;
use util::loop_context::EventNotifierHelper;
use vmm_sys_util::eventfd::EventFd;

use super::chardev::{Chardev, InputReceiver};
use super::errors::{ErrorKind, Result, ResultExt};

const PL011_FLAG_TXFE: u8 = 0x80;
const PL011_FLAG_RXFF: u8 = 0x40;
const PL011_FLAG_RXFE: u8 = 0x10;

// Interrupt bits in UARTRIS, UARTMIS and UARTIMSC
// Receive timeout interrupt bit
const INT_RT: u32 = 1 << 6;
// Transmit interrupt bit
const INT_TX: u32 = 1 << 5;
// Receive interrupt bit
const INT_RX: u32 = 1 << 4;
// Framing/Panity/Break/Overrun error bits, bits 7~10.
const INT_E: u32 = 1 << 7 | 1 << 8 | 1 << 9 | 1 << 10;
// nUARTRI/nUARTCTS/nUARTDCD/nUARTDSR modem interrupt bits, bits 0~3.
const INT_MS: u32 = 1 | 1 << 1 | 1 << 2 | 1 << 3;

const PL011_FIFO_SIZE: usize = 16;

/// Device state of PL011.
#[allow(clippy::upper_case_acronyms)]
#[repr(C)]
#[derive(Clone, Copy, Desc, ByteCode)]
#[desc_version(compat_version = "0.1.0")]
pub struct PL011State {
    /// Read FIFO. PL011_FIFO_SIZE is 16.
    rfifo: [u32; 16],
    /// Flag Register.
    flags: u32,
    /// Line Control Register.
    lcr: u32,
    /// Receive Status Register.
    rsr: u32,
    /// Control Register.
    cr: u32,
    /// DMA Control Register.
    dmacr: u32,
    /// IrDA Low-Power Counter Register.
    ilpr: u32,
    /// Integer Baud Rate Register.
    ibrd: u32,
    /// Fractional Baud Rate Register.
    fbrd: u32,
    /// Interrupt FIFO Level Select Register.
    ifl: u32,
    /// Identifier Register. Length is 8.
    id: [u8; 8],
    /// FIFO Status.
    read_pos: u32,
    read_count: u32,
    read_trigger: u32,
    /// Raw Interrupt Status Register.
    int_level: u32,
    /// Interrupt Mask Set/Clean Register.
    int_enabled: u32,
}

impl PL011State {
    fn new() -> Self {
        PL011State {
            rfifo: [0; PL011_FIFO_SIZE],
            flags: (PL011_FLAG_TXFE | PL011_FLAG_RXFE) as u32,
            lcr: 0,
            rsr: 0,
            cr: 0x300,
            dmacr: 0,
            ilpr: 0,
            ibrd: 0,
            fbrd: 0,
            ifl: 0x12, // Receive and transmit enable
            id: [0x11, 0x10, 0x14, 0x00, 0x0d, 0xf0, 0x05, 0xb1],
            read_pos: 0,
            read_count: 0,
            read_trigger: 1,
            int_level: 0,
            int_enabled: 0,
        }
    }
}

#[allow(clippy::upper_case_acronyms)]
pub struct PL011 {
    /// Device state.
    state: PL011State,
    /// Interrupt event file descriptor.
    interrupt_evt: EventFd,
    /// System Resource of device.
    res: SysRes,
    /// Character device for redirection.
    chardev: Arc<Mutex<Chardev>>,
}

impl PL011 {
    /// Create a new `PL011` instance with default parameters.
    pub fn new(cfg: SerialConfig) -> Result<Self> {
        Ok(PL011 {
            state: PL011State::new(),
            interrupt_evt: EventFd::new(libc::EFD_NONBLOCK)?,
            res: SysRes::default(),
            chardev: Arc::new(Mutex::new(Chardev::new(cfg.chardev))),
        })
    }

    fn interrupt(&self) {
        let irq_mask = INT_E | INT_MS | INT_RT | INT_TX | INT_RX;

        let flag = self.state.int_level & self.state.int_enabled;
        if flag & irq_mask != 0 {
            if let Err(e) = self.interrupt_evt.write(1) {
                error!(
                    "Failed to trigger interrupt for PL011, flag is 0x{:x}, error is {}",
                    flag, e,
                )
            }
        }
    }

    pub fn realize(
        mut self,
        sysbus: &mut SysBus,
        region_base: u64,
        region_size: u64,
        bs: &Arc<Mutex<BootSource>>,
    ) -> Result<()> {
        self.chardev
            .lock()
            .unwrap()
            .realize()
            .chain_err(|| "Failed to realize chardev")?;
        self.set_sys_resource(sysbus, region_base, region_size)
            .chain_err(|| "Failed to set system resource for PL011.")?;

        let dev = Arc::new(Mutex::new(self));
        sysbus
            .attach_device(&dev, region_base, region_size)
            .chain_err(|| "Failed to attach PL011 to system bus.")?;

        bs.lock().unwrap().kernel_cmdline.push(Param {
            param_type: "earlycon".to_string(),
            value: format!("pl011,mmio,0x{:08x}", region_base),
        });
        MigrationManager::register_device_instance_mutex(PL011State::descriptor(), dev.clone());
        let locked_dev = dev.lock().unwrap();
        locked_dev.chardev.lock().unwrap().set_input_callback(&dev);
        EventLoop::update_event(
            EventNotifierHelper::internal_notifiers(locked_dev.chardev.clone()),
            None,
        )
        .chain_err(|| ErrorKind::RegNotifierErr)?;
        Ok(())
    }
}

impl InputReceiver for PL011 {
    fn input_handle(&mut self, data: &[u8]) {
        self.state.flags &= !PL011_FLAG_RXFE as u32;
        for val in data {
            let mut slot = (self.state.read_pos + self.state.read_count) as usize;
            if slot >= PL011_FIFO_SIZE {
                slot -= PL011_FIFO_SIZE;
            }
            self.state.rfifo[slot] = *val as u32;
            self.state.read_count += 1;
        }

        // If in character-mode, or in FIFO-mode and FIFO is full, trigger the interrupt.
        if ((self.state.lcr & 0x10) == 0) || (self.state.read_count as usize == PL011_FIFO_SIZE) {
            self.state.flags |= PL011_FLAG_RXFF as u32;
        }
        if self.state.read_count >= self.state.read_trigger {
            self.state.int_level |= INT_RX as u32;
            self.interrupt();
        }
    }

    fn get_remain_space_size(&mut self) -> usize {
        PL011_FIFO_SIZE - self.state.read_count as usize
    }
}

impl SysBusDevOps for PL011 {
    fn read(&mut self, data: &mut [u8], _base: GuestAddress, offset: u64) -> bool {
        if data.len() > 4 {
            error!("Fail to read PL011, illegal data length {}", data.len());
            return false;
        }
        let ret;

        match offset >> 2 {
            0 => {
                // Data register.
                self.state.flags &= !(PL011_FLAG_RXFF as u32);
                let c = self.state.rfifo[self.state.read_pos as usize];

                if self.state.read_count > 0 {
                    self.state.read_count -= 1;
                    self.state.read_pos += 1;
                    if self.state.read_pos as usize == PL011_FIFO_SIZE {
                        self.state.read_pos = 0;
                    }
                }
                if self.state.read_count == 0 {
                    self.state.flags |= PL011_FLAG_RXFE as u32;
                }
                if self.state.read_count == self.state.read_trigger - 1 {
                    self.state.int_level &= !(INT_RX as u32);
                }
                self.state.rsr = c >> 8;
                self.interrupt();
                ret = c;
            }
            1 => {
                ret = self.state.rsr;
            }
            6 => {
                ret = self.state.flags;
            }
            8 => {
                ret = self.state.ilpr;
            }
            9 => {
                ret = self.state.ibrd;
            }
            10 => {
                ret = self.state.fbrd;
            }
            11 => {
                ret = self.state.lcr;
            }
            12 => {
                ret = self.state.cr;
            }
            13 => {
                ret = self.state.ifl;
            }
            14 => {
                // Interrupt Mask Set/Clear Register
                ret = self.state.int_enabled;
            }
            15 => {
                // Raw Interrupt Status Register
                ret = self.state.int_level;
            }
            16 => {
                // Masked Interrupt Status Register
                ret = self.state.int_level & self.state.int_enabled;
            }
            18 => {
                ret = self.state.dmacr;
            }
            0x3f8..=0x400 => {
                // Register 0xFE0~0xFFC is UART Peripheral Identification Registers
                // and PrimeCell Identification Registers.
                ret = *self.state.id.get(((offset - 0xfe0) >> 2) as usize).unwrap() as u32;
            }
            _ => {
                error!("Failed to read pl011: Invalid offset 0x{:x}", offset);
                return false;
            }
        }
        data.copy_from_slice(&ret.as_bytes()[0..data.len()]);

        true
    }

    fn write(&mut self, data: &[u8], _base: GuestAddress, offset: u64) -> bool {
        let value = match data.len() {
            1 => data[0] as u32,
            2 => LittleEndian::read_u16(data) as u32,
            4 => LittleEndian::read_u32(data) as u32,
            _ => return false,
        };

        match offset >> 2 {
            0 => {
                let ch = value as u8;

                if let Some(output) = &mut self.chardev.lock().unwrap().output {
                    let mut locked_output = output.lock().unwrap();
                    if let Err(e) = locked_output.write_all(&[ch]) {
                        error!("Failed to write to pl011 output fd, error is {}", e);
                    }
                    if let Err(e) = locked_output.flush() {
                        error!("Failed to flush pl011, error is {}", e);
                    }
                } else {
                    debug!("Failed to get output fd");
                    return false;
                }

                self.state.int_level |= INT_TX as u32;
                self.interrupt();
            }
            1 => {
                self.state.rsr = 0;
            }
            8 => {
                self.state.ilpr = value;
            }
            9 => {
                self.state.ibrd = value;
            }
            10 => {
                self.state.fbrd = value;
            }
            11 => {
                // PL011 works in two modes: character mode or FIFO mode.
                // Reset FIFO if the mode is changed.
                if (self.state.lcr ^ value) & 0x10 != 0 {
                    self.state.read_count = 0;
                    self.state.read_pos = 0;
                }
                self.state.lcr = value;
                self.state.read_trigger = 1;
            }
            12 => {
                self.state.cr = value;
            }
            13 => {
                self.state.ifl = value;
                self.state.read_trigger = 1;
            }
            14 => {
                self.state.int_enabled = value;
                self.interrupt();
            }
            17 => {
                // Interrupt Clear Register, write only
                self.state.int_level &= !value;
                self.interrupt();
            }
            18 => {
                self.state.dmacr = value;
                if value & 3 != 0 {
                    error!("pl011: DMA not implemented");
                }
            }
            _ => {
                error!("Failed to write pl011: Invalid offset 0x{:x}", offset);
                return false;
            }
        }

        true
    }

    fn interrupt_evt(&self) -> Option<&EventFd> {
        Some(&self.interrupt_evt)
    }

    fn get_sys_resource(&mut self) -> Option<&mut SysRes> {
        Some(&mut self.res)
    }

    fn get_type(&self) -> SysBusDevType {
        SysBusDevType::PL011
    }
}

impl StateTransfer for PL011 {
    fn get_state_vec(&self) -> migration::errors::Result<Vec<u8>> {
        Ok(self.state.as_bytes().to_vec())
    }

    fn set_state_mut(&mut self, state: &[u8]) -> migration::errors::Result<()> {
        self.state = *PL011State::from_bytes(state)
            .ok_or(migration::errors::ErrorKind::FromBytesError("PL011"))?;

        Ok(())
    }

    fn get_device_alias(&self) -> u64 {
        if let Some(alias) = MigrationManager::get_desc_alias(&PL011State::descriptor().name) {
            alias
        } else {
            !0
        }
    }
}

impl MigrationHook for PL011 {}

impl AmlBuilder for PL011 {
    fn aml_bytes(&self) -> Vec<u8> {
        let mut acpi_dev = AmlDevice::new("COM0");
        acpi_dev.append_child(AmlNameDecl::new("_HID", AmlString("ARMH0001".to_string())));
        acpi_dev.append_child(AmlNameDecl::new("_UID", AmlInteger(0)));

        let mut res = AmlResTemplate::new();
        res.append_child(AmlMemory32Fixed::new(
            AmlReadAndWrite::ReadWrite,
            self.res.region_base as u32,
            self.res.region_size as u32,
        ));
        // SPI start at interrupt number 32 on aarch64 platform.
        let irq_base = 32_u32;
        res.append_child(AmlExtendedInterrupt::new(
            AmlResourceUsage::Consumer,
            AmlEdgeLevel::Level,
            AmlActiveLevel::High,
            AmlIntShare::Exclusive,
            vec![self.res.irq as u32 + irq_base],
        ));
        acpi_dev.append_child(AmlNameDecl::new("_CRS", res));

        acpi_dev.aml_bytes()
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use machine_manager::config::{ChardevConfig, ChardevType};

    #[test]
    fn test_receive() {
        let chardev_cfg = ChardevConfig {
            id: "chardev".to_string(),
            backend: ChardevType::Stdio,
        };
        let mut pl011_dev = PL011::new(SerialConfig {
            chardev: chardev_cfg,
        })
        .unwrap();
        assert_eq!(pl011_dev.state.rfifo, [0; PL011_FIFO_SIZE]);
        assert_eq!(pl011_dev.state.flags, 0x90);
        assert_eq!(pl011_dev.state.lcr, 0);
        assert_eq!(pl011_dev.state.rsr, 0);
        assert_eq!(pl011_dev.state.cr, 0x300);
        assert_eq!(pl011_dev.state.dmacr, 0);
        assert_eq!(pl011_dev.state.ilpr, 0);
        assert_eq!(pl011_dev.state.ibrd, 0);
        assert_eq!(pl011_dev.state.fbrd, 0);
        assert_eq!(pl011_dev.state.ifl, 0x12);
        assert_eq!(
            pl011_dev.state.id,
            [0x11, 0x10, 0x14, 0x00, 0x0d, 0xf0, 0x05, 0xb1]
        );
        assert_eq!(pl011_dev.state.read_pos, 0);
        assert_eq!(pl011_dev.state.read_count, 0);
        assert_eq!(pl011_dev.state.read_trigger, 1);
        assert_eq!(pl011_dev.state.int_level, 0);
        assert_eq!(pl011_dev.state.int_enabled, 0);

        let data = vec![0x12, 0x34, 0x56, 0x78, 0x90];
        pl011_dev.input_handle(&data);
        assert_eq!(pl011_dev.state.read_count, data.len() as u32);
        for i in 0..data.len() {
            assert_eq!(pl011_dev.state.rfifo[i], data[i] as u32);
        }
        assert_eq!(pl011_dev.state.flags, 0xC0);
        assert_eq!(pl011_dev.state.int_level, INT_RX);
    }
}
