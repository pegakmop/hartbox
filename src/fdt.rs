//! Flattened device tree (DTB) builder and the description of this machine.
//!
//! The tree mirrors QEMU's `virt` machine (same device addresses and
//! compatible strings) so that stock kernels bind to the devices without
//! any changes.

use std::collections::HashMap;

use crate::arch::*;

const FDT_MAGIC: u32 = 0xd00d_feed;
const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
const FDT_END: u32 = 9;

/// Low-level FDT writer: nodes, properties and the string table.
#[derive(Default)]
pub struct FdtBuilder {
    structure: Vec<u8>,
    strings: Vec<u8>,
    string_offsets: HashMap<String, u32>,
}

impl FdtBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    fn token(&mut self, t: u32) {
        self.structure.extend_from_slice(&t.to_be_bytes());
    }

    fn pad4(buf: &mut Vec<u8>) {
        while !buf.len().is_multiple_of(4) {
            buf.push(0);
        }
    }

    fn name_offset(&mut self, name: &str) -> u32 {
        if let Some(&off) = self.string_offsets.get(name) {
            return off;
        }
        let off = self.strings.len() as u32;
        self.strings.extend_from_slice(name.as_bytes());
        self.strings.push(0);
        self.string_offsets.insert(name.to_string(), off);
        off
    }

    pub fn begin_node(&mut self, name: &str) -> &mut Self {
        self.token(FDT_BEGIN_NODE);
        self.structure.extend_from_slice(name.as_bytes());
        self.structure.push(0);
        Self::pad4(&mut self.structure);
        self
    }

    pub fn end_node(&mut self) -> &mut Self {
        self.token(FDT_END_NODE);
        self
    }

    pub fn prop(&mut self, name: &str, data: &[u8]) -> &mut Self {
        let nameoff = self.name_offset(name);
        self.token(FDT_PROP);
        self.structure.extend_from_slice(&(data.len() as u32).to_be_bytes());
        self.structure.extend_from_slice(&nameoff.to_be_bytes());
        self.structure.extend_from_slice(data);
        Self::pad4(&mut self.structure);
        self
    }

    pub fn prop_empty(&mut self, name: &str) -> &mut Self {
        self.prop(name, &[])
    }

    pub fn prop_u32(&mut self, name: &str, v: u32) -> &mut Self {
        self.prop(name, &v.to_be_bytes())
    }

    /// A 64-bit value as two big-endian cells.
    pub fn prop_u64(&mut self, name: &str, v: u64) -> &mut Self {
        self.prop(name, &v.to_be_bytes())
    }

    /// A list of 32-bit cells.
    pub fn prop_cells(&mut self, name: &str, cells: &[u32]) -> &mut Self {
        let mut buf = Vec::with_capacity(cells.len() * 4);
        for c in cells {
            buf.extend_from_slice(&c.to_be_bytes());
        }
        self.prop(name, &buf)
    }

    /// A string property.
    pub fn prop_str(&mut self, name: &str, s: &str) -> &mut Self {
        let mut buf = s.as_bytes().to_vec();
        buf.push(0);
        self.prop(name, &buf)
    }

    /// A string-list property.
    pub fn prop_strs(&mut self, name: &str, items: &[&str]) -> &mut Self {
        let mut buf = Vec::new();
        for s in items {
            buf.extend_from_slice(s.as_bytes());
            buf.push(0);
        }
        self.prop(name, &buf)
    }

    /// Serialize the tree (header, empty memory reservation block, structure
    /// block, strings block).
    pub fn finish(mut self) -> Vec<u8> {
        self.token(FDT_END);
        let header_size = 40u32;
        let rsvmap_size = 16u32;
        let off_rsvmap = header_size;
        let off_struct = off_rsvmap + rsvmap_size;
        let off_strings = off_struct + self.structure.len() as u32;
        let total = off_strings + self.strings.len() as u32;

        let mut out = Vec::with_capacity(total as usize);
        for v in [
            FDT_MAGIC,
            total,
            off_struct,
            off_strings,
            off_rsvmap,
            17,                       // version
            16,                       // last compatible version
            0,                        // boot CPU id
            self.strings.len() as u32,
            self.structure.len() as u32,
        ] {
            out.extend_from_slice(&v.to_be_bytes());
        }
        out.extend_from_slice(&[0u8; 16]); // empty reservation map terminator
        out.extend_from_slice(&self.structure);
        out.extend_from_slice(&self.strings);
        out
    }
}

/// What the device tree needs to know about the machine at boot.
pub struct MachineDesc<'a> {
    pub harts: usize,
    pub ram_size: u64,
    pub bootargs: &'a str,
    /// (start, end) of the initial RAM disk.
    pub initrd: Option<(u64, u64)>,
    /// Memory owned by M-mode firmware that the kernel must not use.
    pub firmware: Option<(u64, u64)>,
    /// Number of virtio-mmio devices attached (slots 0..n).
    pub virtio_devices: usize,
}

/// Build the device tree for this machine.
pub fn build_machine_dtb(desc: &MachineDesc) -> Vec<u8> {
    let n = desc.harts as u32;
    // Phandles: CPU interrupt controllers are 1..=n, the PLIC is n+1.
    let cpu_intc = |h: u32| h + 1;
    let plic_ph = n + 1;

    let mut b = FdtBuilder::new();
    b.begin_node("");
    b.prop_u32("#address-cells", 2)
        .prop_u32("#size-cells", 2)
        .prop_str("compatible", "rvemu,virt")
        .prop_str("model", "rvemu,virt");

    b.begin_node("chosen");
    b.prop_str("bootargs", desc.bootargs);
    b.prop_str("stdout-path", "/soc/serial@10000000");
    if let Some((start, end)) = desc.initrd {
        b.prop_u64("linux,initrd-start", start);
        b.prop_u64("linux,initrd-end", end);
    }
    b.end_node();

    b.begin_node("aliases");
    b.prop_str("serial0", "/soc/serial@10000000");
    b.end_node();

    let mem_name = format!("memory@{:x}", RAM_BASE);
    b.begin_node(&mem_name);
    b.prop_str("device_type", "memory");
    b.prop_cells(
        "reg",
        &[(RAM_BASE >> 32) as u32, RAM_BASE as u32, (desc.ram_size >> 32) as u32, desc.ram_size as u32],
    );
    b.end_node();

    if let Some((start, size)) = desc.firmware {
        b.begin_node("reserved-memory");
        b.prop_u32("#address-cells", 2).prop_u32("#size-cells", 2).prop_empty("ranges");
        let name = format!("firmware@{:x}", start);
        b.begin_node(&name);
        b.prop_cells("reg", &[(start >> 32) as u32, start as u32, (size >> 32) as u32, size as u32]);
        b.prop_empty("no-map");
        b.end_node();
        b.end_node();
    }

    b.begin_node("cpus");
    b.prop_u32("#address-cells", 1).prop_u32("#size-cells", 0).prop_u32("timebase-frequency", TIMEBASE_HZ as u32);
    for h in 0..n {
        let name = format!("cpu@{}", h);
        b.begin_node(&name);
        b.prop_str("device_type", "cpu")
            .prop_u32("reg", h)
            .prop_str("compatible", "riscv")
            .prop_str("riscv,isa", "rv64imafdc_zicntr_zicsr_zifencei")
            .prop_str("riscv,isa-base", "rv64i")
            .prop_strs(
                "riscv,isa-extensions",
                &["i", "m", "a", "f", "d", "c", "zicntr", "zicsr", "zifencei"],
            )
            .prop_str("mmu-type", "riscv,sv39")
            .prop_str("status", "okay");
        // The CPU-local interrupt controller is what `interrupts-extended` names.
        b.begin_node("interrupt-controller");
        b.prop_u32("#interrupt-cells", 1)
            .prop_empty("interrupt-controller")
            .prop_str("compatible", "riscv,cpu-intc")
            .prop_u32("phandle", cpu_intc(h));
        b.end_node();
        b.end_node();
    }
    b.end_node();

    b.begin_node("soc");
    b.prop_u32("#address-cells", 2)
        .prop_u32("#size-cells", 2)
        .prop_str("compatible", "simple-bus")
        .prop_empty("ranges");

    // CLINT: software and timer interrupts for each hart.
    let mut clint_ext = Vec::new();
    for h in 0..n {
        clint_ext.extend_from_slice(&[cpu_intc(h), 3, cpu_intc(h), 7]);
    }
    b.begin_node(&format!("clint@{:x}", CLINT_BASE));
    b.prop_strs("compatible", &["sifive,clint0", "riscv,clint0"])
        .prop_cells("reg", &[0, CLINT_BASE as u32, 0, CLINT_SIZE as u32])
        .prop_cells("interrupts-extended", &clint_ext);
    b.end_node();

    // PLIC: machine and supervisor external interrupts for each hart.
    let mut plic_ext = Vec::new();
    for h in 0..n {
        plic_ext.extend_from_slice(&[cpu_intc(h), 11, cpu_intc(h), 9]);
    }
    b.begin_node(&format!("plic@{:x}", PLIC_BASE));
    b.prop_strs("compatible", &["sifive,plic-1.0.0", "riscv,plic0"])
        .prop_cells("reg", &[0, PLIC_BASE as u32, 0, PLIC_SIZE as u32])
        .prop_cells("interrupts-extended", &plic_ext)
        .prop_empty("interrupt-controller")
        .prop_u32("#interrupt-cells", 1)
        .prop_u32("riscv,ndev", 63)
        .prop_u32("phandle", plic_ph);
    b.end_node();

    b.begin_node(&format!("rtc@{:x}", RTC_BASE));
    b.prop_str("compatible", "google,goldfish-rtc")
        .prop_cells("reg", &[0, RTC_BASE as u32, 0, RTC_SIZE as u32])
        .prop_cells("interrupts", &[RTC_IRQ])
        .prop_u32("interrupt-parent", plic_ph);
    b.end_node();

    for slot in 0..desc.virtio_devices {
        let base = VIRTIO_BASE + slot as u64 * VIRTIO_STRIDE;
        b.begin_node(&format!("virtio_mmio@{:x}", base));
        b.prop_str("compatible", "virtio,mmio")
            .prop_cells("reg", &[0, base as u32, 0, VIRTIO_STRIDE as u32])
            .prop_cells("interrupts", &[VIRTIO_IRQ_BASE + slot as u32])
            .prop_u32("interrupt-parent", plic_ph);
        b.end_node();
    }

    b.begin_node(&format!("serial@{:x}", UART_BASE));
    b.prop_str("compatible", "ns16550a")
        .prop_cells("reg", &[0, UART_BASE as u32, 0, UART_SIZE as u32])
        .prop_u32("clock-frequency", 3_686_400)
        .prop_u32("interrupt-parent", plic_ph)
        .prop_cells("interrupts", &[UART_IRQ]);
    b.end_node();

    // Test finisher: firmware and kernels use it to power the machine off.
    let syscon_ph = plic_ph + 1;
    b.begin_node(&format!("test@{:x}", SIFIVE_TEST_BASE));
    b.prop_strs("compatible", &["sifive,test1", "sifive,test0", "syscon"])
        .prop_cells("reg", &[0, SIFIVE_TEST_BASE as u32, 0, SIFIVE_TEST_SIZE as u32])
        .prop_u32("phandle", syscon_ph);
    b.end_node();
    // Generic syscon-based poweroff/reboot, as used by OpenSBI and Linux.
    b.begin_node("poweroff");
    b.prop_str("compatible", "syscon-poweroff")
        .prop_u32("regmap", syscon_ph)
        .prop_u32("offset", 0)
        .prop_u32("value", 0x5555);
    b.end_node();
    b.begin_node("reboot");
    b.prop_str("compatible", "syscon-reboot")
        .prop_u32("regmap", syscon_ph)
        .prop_u32("offset", 0)
        .prop_u32("value", 0x7777);
    b.end_node();

    b.end_node(); // soc
    b.end_node(); // root
    b.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_is_valid() {
        let dtb = build_machine_dtb(&MachineDesc {
            harts: 2,
            ram_size: 1 << 28,
            bootargs: "console=ttyS0",
            initrd: Some((0x8100_0000, 0x8110_0000)),
            firmware: None,
            virtio_devices: 2,
        });
        assert_eq!(u32::from_be_bytes(dtb[0..4].try_into().unwrap()), FDT_MAGIC);
        let total = u32::from_be_bytes(dtb[4..8].try_into().unwrap()) as usize;
        assert_eq!(total, dtb.len());
    }
}
