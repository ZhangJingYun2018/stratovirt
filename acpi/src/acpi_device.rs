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

use std::time::Instant;

use address_space::GuestAddress;
use byteorder::{ByteOrder, LittleEndian};

// Frequency of PM Timer in HZ.
const PM_TIMER_FREQUENCY: u128 = 3_579_545;
const NANOSECONDS_PER_SECOND: u128 = 1_000_000_000;
pub const ACPI_BITMASK_SLEEP_ENABLE: u16 = 0x2000;

/// ACPI Power Management Timer
#[allow(clippy::upper_case_acronyms)]
pub struct AcpiPMTimer {
    start: Instant,
}

impl Default for AcpiPMTimer {
    fn default() -> Self {
        Self::new()
    }
}

impl AcpiPMTimer {
    pub fn new() -> AcpiPMTimer {
        AcpiPMTimer {
            start: Instant::now(),
        }
    }

    pub fn read(&mut self, data: &mut [u8], _base: GuestAddress, _offset: u64) -> bool {
        if data.len() != 4 {
            error!(
                "PM Timer read: invalid data length {}, required length is 4",
                data.len()
            );
        }
        let now = Instant::now();
        let time_nanos = now.duration_since(self.start).as_nanos();
        let counter: u128 = (time_nanos * PM_TIMER_FREQUENCY) / NANOSECONDS_PER_SECOND;

        data.copy_from_slice(&((counter & 0xFFFF_FFFF) as u32).to_le_bytes());
        true
    }
}

#[derive(Default)]
pub struct AcpiPmEvent {
    // PM1 Status Registers, location: PM1a_EVT_BLK.
    status: u16,
    // PM1Enable Registers, location: PM1a_EVT_BLK + PM1_EVT_LEN / 2.
    enable: u16,
}

impl AcpiPmEvent {
    pub fn new() -> AcpiPmEvent {
        AcpiPmEvent {
            status: 0,
            enable: 0,
        }
    }

    pub fn read(&mut self, data: &mut [u8], _base: GuestAddress, offset: u64) -> bool {
        match offset {
            0 => match data.len() {
                1 => data[0] = self.status as u8,
                2 => LittleEndian::write_u16(data, self.status),
                n => {
                    error!(
                        "Invalid data length {} for reading PM status register, offset is {}",
                        n, offset
                    );
                    return false;
                }
            },
            2 => match data.len() {
                1 => data[0] = self.enable as u8,
                2 => LittleEndian::write_u16(data, self.enable),
                n => {
                    error!(
                        "Invalid data length {} for reading PM enable register, offset is {}",
                        n, offset
                    );
                    return false;
                }
            },
            _ => {
                error!("Invalid offset");
                return false;
            }
        }
        true
    }

    pub fn write(&mut self, data: &[u8], _base: GuestAddress, offset: u64) -> bool {
        match offset {
            0 => {
                let value: u16 = match data.len() {
                    1 => data[0] as u16,
                    2 => LittleEndian::read_u16(data),
                    n => {
                        error!(
                            "Invalid data length {} for writing PM status register, offset is {}",
                            n, offset
                        );
                        return false;
                    }
                };
                self.status &= !value;
            }
            2 => {
                let value: u16 = match data.len() {
                    1 => data[0] as u16,
                    2 => LittleEndian::read_u16(data),
                    n => {
                        error!(
                            "Invalid data length {} for writing PM enable register, offset is {}",
                            n, offset
                        );
                        return false;
                    }
                };
                self.enable = value;
            }
            _ => {
                error!("Invalid offset");
                return false;
            }
        }
        true
    }
}

#[derive(Default)]
pub struct AcpiPmCtrl {
    control: u16,
}

impl AcpiPmCtrl {
    pub fn new() -> AcpiPmCtrl {
        AcpiPmCtrl { control: 0 }
    }

    pub fn read(&mut self, data: &mut [u8], _base: GuestAddress, _offset: u64) -> bool {
        match data.len() {
            1 => data[0] = self.control as u8,
            2 => LittleEndian::write_u16(data, self.control),
            n => {
                error!("Invalid data length {} for reading PM control register", n);
                return false;
            }
        }
        true
    }

    // Return true when guest want poweroff.
    pub fn write(&mut self, data: &[u8], _base: GuestAddress, _offset: u64) -> bool {
        let value: u16 = match data.len() {
            1 => data[0] as u16,
            2 => LittleEndian::read_u16(data),
            n => {
                error!("Invalid data length {} for writing PM control register", n);
                return false;
            }
        };
        self.control = value & !ACPI_BITMASK_SLEEP_ENABLE;
        value & ACPI_BITMASK_SLEEP_ENABLE != 0
    }
}
