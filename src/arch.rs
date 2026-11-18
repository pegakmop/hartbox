//! Architectural constants: privilege levels, CSR numbers, trap causes,
//! status-register bit layouts and the machine memory map.
//!
//! The memory map deliberately mirrors QEMU's `virt` machine so that stock
//! distribution kernels (Alpine, Debian, ...) and OpenSBI work unmodified.

// ---------------------------------------------------------------- privilege
pub const PRV_U: u8 = 0;
pub const PRV_S: u8 = 1;
pub const PRV_M: u8 = 3;

// -------------------------------------------------------------- memory map
pub const RAM_BASE: u64 = 0x8000_0000;
pub const CLINT_BASE: u64 = 0x0200_0000;
pub const CLINT_SIZE: u64 = 0x1_0000;
pub const PLIC_BASE: u64 = 0x0c00_0000;
pub const PLIC_SIZE: u64 = 0x0400_0000;
pub const UART_BASE: u64 = 0x1000_0000;
pub const UART_SIZE: u64 = 0x100;
pub const UART_IRQ: u32 = 10;
pub const SIFIVE_TEST_BASE: u64 = 0x0010_0000;
pub const SIFIVE_TEST_SIZE: u64 = 0x1000;
pub const RTC_BASE: u64 = 0x0010_1000;
pub const RTC_SIZE: u64 = 0x1000;
pub const RTC_IRQ: u32 = 11;
/// virtio-mmio device slots; slot `i` lives at `VIRTIO_BASE + i * VIRTIO_STRIDE`
/// and interrupts PLIC source `VIRTIO_IRQ_BASE + i`.
pub const VIRTIO_BASE: u64 = 0x1000_1000;
pub const VIRTIO_STRIDE: u64 = 0x1000;
pub const VIRTIO_SLOTS: usize = 8;
pub const VIRTIO_IRQ_BASE: u32 = 1;

/// Timebase advertised to the guest (`timebase-frequency`).
pub const TIMEBASE_HZ: u64 = 10_000_000;
/// The `time` CSR advances by one tick per this many retired instructions.
pub const INSNS_PER_TICK: u64 = 16;

// ------------------------------------------------------- exception causes
pub const EXC_IFETCH_MISALIGN: u64 = 0;
pub const EXC_IFETCH_ACCESS: u64 = 1;
pub const EXC_ILLEGAL: u64 = 2;
pub const EXC_BREAKPOINT: u64 = 3;
pub const EXC_LOAD_MISALIGN: u64 = 4;
pub const EXC_LOAD_ACCESS: u64 = 5;
pub const EXC_STORE_MISALIGN: u64 = 6;
pub const EXC_STORE_ACCESS: u64 = 7;
pub const EXC_ECALL_U: u64 = 8;
pub const EXC_ECALL_S: u64 = 9;
pub const EXC_ECALL_M: u64 = 11;
pub const EXC_FETCH_PAGE: u64 = 12;
pub const EXC_LOAD_PAGE: u64 = 13;
pub const EXC_STORE_PAGE: u64 = 15;

/// Set in `cause` values that describe an interrupt rather than an exception.
pub const IRQ_FLAG: u64 = 1 << 63;
pub const INT_SSI: u64 = 1;
pub const INT_MSI: u64 = 3;
pub const INT_STI: u64 = 5;
pub const INT_MTI: u64 = 7;
pub const INT_SEI: u64 = 9;
pub const INT_MEI: u64 = 11;

/// Interrupt priority order, highest first (privileged spec 3.1.9).
pub const INT_PRIORITY: [u64; 6] = [INT_MEI, INT_MSI, INT_MTI, INT_SEI, INT_SSI, INT_STI];

pub const MIP_SSIP: u64 = 1 << 1;
pub const MIP_MSIP: u64 = 1 << 3;
pub const MIP_STIP: u64 = 1 << 5;
pub const MIP_MTIP: u64 = 1 << 7;
pub const MIP_SEIP: u64 = 1 << 9;
pub const MIP_MEIP: u64 = 1 << 11;
/// Supervisor-level interrupt bits visible through `sip`/`sie`.
pub const MIP_S_MASK: u64 = MIP_SSIP | MIP_STIP | MIP_SEIP;
/// Writable bits of `mie`.
pub const MIE_MASK: u64 = 0xAAA;

// ------------------------------------------------------- mstatus layout
pub const MSTATUS_SIE: u64 = 1 << 1;
pub const MSTATUS_MIE: u64 = 1 << 3;
pub const MSTATUS_SPIE: u64 = 1 << 5;
pub const MSTATUS_MPIE: u64 = 1 << 7;
pub const MSTATUS_SPP: u64 = 1 << 8;
pub const MSTATUS_MPP: u64 = 3 << 11;
pub const MSTATUS_FS: u64 = 3 << 13;
pub const MSTATUS_MPRV: u64 = 1 << 17;
pub const MSTATUS_SUM: u64 = 1 << 18;
pub const MSTATUS_MXR: u64 = 1 << 19;
pub const MSTATUS_TVM: u64 = 1 << 20;
pub const MSTATUS_TW: u64 = 1 << 21;
pub const MSTATUS_TSR: u64 = 1 << 22;
pub const MSTATUS_SD: u64 = 1 << 63;

/// Bits of `mstatus` that software may write.
pub const MSTATUS_WMASK: u64 = MSTATUS_SIE
    | MSTATUS_MIE
    | MSTATUS_SPIE
    | MSTATUS_MPIE
    | MSTATUS_SPP
    | MSTATUS_MPP
    | MSTATUS_FS
    | MSTATUS_MPRV
    | MSTATUS_SUM
    | MSTATUS_MXR
    | MSTATUS_TVM
    | MSTATUS_TW
    | MSTATUS_TSR;
/// Bits of `sstatus`, a restricted view of `mstatus`.
pub const SSTATUS_WMASK: u64 =
    MSTATUS_SIE | MSTATUS_SPIE | MSTATUS_SPP | MSTATUS_FS | MSTATUS_SUM | MSTATUS_MXR;

/// UXL = SXL = 2 (64-bit) — hard-wired read-only fields.
pub const MSTATUS_XL_FIXED: u64 = (2 << 32) | (2 << 34);

// ----------------------------------------------------------- misa
/// RV64 with A, C, D, F, I, M, S and U.
pub const MISA_VALUE: u64 = (2u64 << 62)
    | (1 << 0)   // A
    | (1 << 2)   // C
    | (1 << 3)   // D
    | (1 << 5)   // F
    | (1 << 8)   // I
    | (1 << 12)  // M
    | (1 << 18)  // S
    | (1 << 20); // U

// ------------------------------------------------------------ CSR numbers
pub const CSR_FFLAGS: u16 = 0x001;
pub const CSR_FRM: u16 = 0x002;
pub const CSR_FCSR: u16 = 0x003;
pub const CSR_CYCLE: u16 = 0xC00;
pub const CSR_TIME: u16 = 0xC01;
pub const CSR_INSTRET: u16 = 0xC02;
pub const CSR_SSTATUS: u16 = 0x100;
pub const CSR_SIE: u16 = 0x104;
pub const CSR_STVEC: u16 = 0x105;
pub const CSR_SCOUNTEREN: u16 = 0x106;
pub const CSR_SENVCFG: u16 = 0x10A;
pub const CSR_SSCRATCH: u16 = 0x140;
pub const CSR_SEPC: u16 = 0x141;
pub const CSR_SCAUSE: u16 = 0x142;
pub const CSR_STVAL: u16 = 0x143;
pub const CSR_SIP: u16 = 0x144;
pub const CSR_SATP: u16 = 0x180;
pub const CSR_MSTATUS: u16 = 0x300;
pub const CSR_MISA: u16 = 0x301;
pub const CSR_MEDELEG: u16 = 0x302;
pub const CSR_MIDELEG: u16 = 0x303;
pub const CSR_MIE: u16 = 0x304;
pub const CSR_MTVEC: u16 = 0x305;
pub const CSR_MCOUNTEREN: u16 = 0x306;
pub const CSR_MENVCFG: u16 = 0x30A;
pub const CSR_MCOUNTINHIBIT: u16 = 0x320;
pub const CSR_MSCRATCH: u16 = 0x340;
pub const CSR_MEPC: u16 = 0x341;
pub const CSR_MCAUSE: u16 = 0x342;
pub const CSR_MTVAL: u16 = 0x343;
pub const CSR_MIP: u16 = 0x344;
pub const CSR_PMPCFG0: u16 = 0x3A0;
pub const CSR_PMPCFG2: u16 = 0x3A2;
pub const CSR_PMPADDR0: u16 = 0x3B0;
pub const CSR_PMPADDR15: u16 = 0x3BF;
pub const CSR_MCYCLE: u16 = 0xB00;
pub const CSR_MINSTRET: u16 = 0xB02;
pub const CSR_MVENDORID: u16 = 0xF11;
pub const CSR_MARCHID: u16 = 0xF12;
pub const CSR_MIMPID: u16 = 0xF13;
pub const CSR_MHARTID: u16 = 0xF14;
