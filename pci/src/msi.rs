// Copyright © 2019 Intel Corporation
//
// SPDX-License-Identifier: Apache-2.0 OR BSD-3-Clause
//

extern crate byteorder;
extern crate vm_memory;

use crate::{set_kvm_routes, InterruptRoute};
use byteorder::{ByteOrder, LittleEndian};
use kvm_bindings::{kvm_irq_routing_entry, KVM_IRQ_ROUTING_MSI};
use kvm_ioctls::VmFd;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use vm_allocator::SystemAllocator;

// MSI control masks
const MSI_CTL_ENABLE: u16 = 0x1;
const MSI_CTL_MULTI_MSG_ENABLE: u16 = 0x70;
const MSI_CTL_64_BITS: u16 = 0x80;
const MSI_CTL_PER_VECTOR: u16 = 0x100;

// MSI message offsets
const MSI_MSG_CTL_OFFSET: u64 = 0x2;
const MSI_MSG_ADDR_LO_OFFSET: u64 = 0x4;

// MSI message masks
const MSI_MSG_ADDR_LO_MASK: u32 = 0xffff_fffc;

#[derive(Clone, Copy, Default)]
pub struct MsiCap {
    // Message Control Register
    //   0:     MSI enable.
    //   3-1;   Multiple message capable.
    //   6-4:   Multiple message enable.
    //   7:     64 bits address capable.
    //   8:     Per-vector masking capable.
    //   15-9:  Reserved.
    pub msg_ctl: u16,
    // Message Address (LSB)
    //   1-0:  Reserved.
    //   31-2: Message address.
    pub msg_addr_lo: u32,
    // Message Upper Address (MSB)
    //   31-0: Message address.
    pub msg_addr_hi: u32,
    // Message Data
    //   15-0: Message data.
    pub msg_data: u16,
    // Mask Bits
    //   31-0: Mask bits.
    pub mask_bits: u32,
    // Pending Bits
    //   31-0: Pending bits.
    pub pending_bits: u32,
}

impl MsiCap {
    fn addr_64_bits(&self) -> bool {
        self.msg_ctl & MSI_CTL_64_BITS == MSI_CTL_64_BITS
    }

    fn per_vector_mask(&self) -> bool {
        self.msg_ctl & MSI_CTL_PER_VECTOR == MSI_CTL_PER_VECTOR
    }

    fn enabled(&self) -> bool {
        self.msg_ctl & MSI_CTL_ENABLE == MSI_CTL_ENABLE
    }

    fn num_enabled_vectors(&self) -> usize {
        let field = (self.msg_ctl >> 4) & 0x7;

        if field > 5 {
            return 0;
        }

        1 << field
    }

    fn vector_masked(&self, vector: usize) -> bool {
        if !self.per_vector_mask() {
            return false;
        }

        (self.mask_bits >> vector) & 0x1 == 0x1
    }

    fn size(&self) -> u64 {
        let mut size: u64 = 0xa;

        if self.addr_64_bits() {
            size += 0x4;
        }
        if self.per_vector_mask() {
            size += 0xa;
        }

        size
    }

    fn update(&mut self, offset: u64, data: &[u8]) {
        // Calculate message data offset depending on the address being 32 or
        // 64 bits.
        // Calculate upper address offset if the address is 64 bits.
        // Calculate mask bits offset based on the address being 32 or 64 bits
        // and based on the per vector masking being enabled or not.
        let (msg_data_offset, addr_hi_offset, mask_bits_offset): (u64, Option<u64>, Option<u64>) =
            if self.addr_64_bits() {
                let mask_bits = if self.per_vector_mask() {
                    Some(0x10)
                } else {
                    None
                };
                (0xc, Some(0x8), mask_bits)
            } else {
                let mask_bits = if self.per_vector_mask() {
                    Some(0xc)
                } else {
                    None
                };
                (0x8, None, mask_bits)
            };

        // Update cache without overriding the read-only bits.
        match data.len() {
            2 => {
                let value = LittleEndian::read_u16(data);
                match offset {
                    MSI_MSG_CTL_OFFSET => {
                        self.msg_ctl = (self.msg_ctl & !(MSI_CTL_ENABLE | MSI_CTL_MULTI_MSG_ENABLE))
                            | (value & (MSI_CTL_ENABLE | MSI_CTL_MULTI_MSG_ENABLE))
                    }
                    x if x == msg_data_offset => self.msg_data = value,
                    _ => error!("invalid offset"),
                }
            }
            4 => {
                let value = LittleEndian::read_u32(data);
                match offset {
                    0x0 => {
                        self.msg_ctl = (self.msg_ctl & !(MSI_CTL_ENABLE | MSI_CTL_MULTI_MSG_ENABLE))
                            | ((value >> 16) as u16 & (MSI_CTL_ENABLE | MSI_CTL_MULTI_MSG_ENABLE))
                    }
                    MSI_MSG_ADDR_LO_OFFSET => self.msg_addr_lo = value & MSI_MSG_ADDR_LO_MASK,
                    x if x == msg_data_offset => self.msg_data = value as u16,
                    x if addr_hi_offset.is_some() && x == addr_hi_offset.unwrap() => {
                        self.msg_addr_hi = value
                    }
                    x if mask_bits_offset.is_some() && x == mask_bits_offset.unwrap() => {
                        self.mask_bits = value
                    }
                    _ => error!("invalid offset"),
                }
            }
            _ => error!("invalid data length"),
        }
    }
}

pub struct MsiConfig {
    pub cap: MsiCap,
    pub irq_routes: Vec<InterruptRoute>,
    vm_fd: Arc<VmFd>,
    gsi_msi_routes: Arc<Mutex<HashMap<u32, kvm_irq_routing_entry>>>,
}

impl MsiConfig {
    pub fn new(
        msg_ctl: u16,
        allocator: &mut SystemAllocator,
        vm_fd: Arc<VmFd>,
        gsi_msi_routes: Arc<Mutex<HashMap<u32, kvm_irq_routing_entry>>>,
    ) -> Self {
        let cap = MsiCap {
            msg_ctl,
            ..Default::default()
        };

        let mut irq_routes: Vec<InterruptRoute> = Vec::new();
        for _ in 0..cap.num_enabled_vectors() {
            irq_routes.push(InterruptRoute::new(allocator).unwrap());
        }

        MsiConfig {
            cap,
            irq_routes,
            vm_fd,
            gsi_msi_routes,
        }
    }

    pub fn enabled(&self) -> bool {
        self.cap.enabled()
    }

    pub fn size(&self) -> u64 {
        self.cap.size()
    }

    pub fn num_enabled_vectors(&self) -> usize {
        self.cap.num_enabled_vectors()
    }

    pub fn vector_masked(&self, vector: usize) -> bool {
        self.cap.vector_masked(vector)
    }

    pub fn update(&mut self, offset: u64, data: &[u8]) {
        let old_enabled = self.cap.enabled();

        self.cap.update(offset, data);

        let mut gsi_msi_routes = self.gsi_msi_routes.lock().unwrap();

        if self.cap.enabled() {
            for (idx, route) in self.irq_routes.iter().enumerate() {
                if !old_enabled {
                    if let Err(e) = self.irq_routes[idx].enable(&self.vm_fd) {
                        error!("Failed enabling irq_fd: {:?}", e);
                    }
                }

                // Ignore MSI vector if masked.
                if self.cap.vector_masked(idx) {
                    continue;
                }

                let mut entry = kvm_irq_routing_entry {
                    gsi: route.gsi,
                    type_: KVM_IRQ_ROUTING_MSI,
                    ..Default::default()
                };

                entry.u.msi.address_lo = self.cap.msg_addr_lo;
                entry.u.msi.address_hi = self.cap.msg_addr_hi;
                entry.u.msi.data = u32::from(self.cap.msg_data) | (idx as u32);

                gsi_msi_routes.insert(route.gsi, entry);
            }
        } else {
            for route in self.irq_routes.iter() {
                if old_enabled {
                    if let Err(e) = route.disable(&self.vm_fd) {
                        error!("Failed disabling irq_fd: {:?}", e);
                    }
                }

                gsi_msi_routes.remove(&route.gsi);
            }
        }

        if let Err(e) = set_kvm_routes(&self.vm_fd, &gsi_msi_routes) {
            error!("Failed updating KVM routes: {:?}", e);
        }
    }
}
