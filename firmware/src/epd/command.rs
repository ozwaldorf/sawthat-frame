//! Command definitions for GDEP073E01 / Spectra 6 display controller

/// Display commands
#[derive(Debug, Clone, Copy)]
#[repr(u8)]
#[allow(clippy::upper_case_acronyms)]
pub enum Command {
    /// Panel Setting
    PSR = 0x00,
    /// Power Setting
    PWRR = 0x01,
    /// Power Off
    POF = 0x02,
    /// Power Off Sequence Setting
    POFS = 0x03,
    /// Power On
    PON = 0x04,
    /// Booster Soft Start 1
    BTST1 = 0x05,
    /// Booster Soft Start 2
    BTST2 = 0x06,
    /// Deep Sleep
    DSLP = 0x07,
    /// Booster Soft Start 3
    BTST3 = 0x08,
    /// Data Start Transmission
    DTM = 0x10,
    /// Display Refresh
    DRF = 0x12,
    /// IPC (Image Process Command - fast mode)
    IPC = 0x13,
    /// PLL Control
    PLL = 0x30,
    /// Temperature Sensor Enable
    TSE = 0x41,
    /// VCOM and Data Interval Setting
    CDI = 0x50,
    /// TCON Setting
    TCON = 0x60,
    /// Resolution Setting
    TRES = 0x61,
    /// Revision
    _REV = 0x70,
    /// VCOM DC Setting
    VDCS = 0x82,
    /// Temperature VCOM DC Setting
    #[allow(non_camel_case_types)]
    T_VDCS = 0x84,
    /// AGID (fast mode)
    AGID = 0x86,
    /// Command Header
    CMDH = 0xAA,
    /// Power Saving Setting
    PWS = 0xE3,
    /// CCSET (Cascade Setting - fast mode)
    CCSET = 0xE0,
    /// TSSET (Temperature Sensor Setting - fast mode)
    TSSET = 0xE6,
    /// Partial Window Setting
    PTLW = 0x83,
}

impl Command {
    /// Get the command address byte
    #[inline]
    pub fn addr(self) -> u8 {
        self as u8
    }
}
