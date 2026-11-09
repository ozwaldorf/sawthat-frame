//! USB drive mode: exposes the raw SD card to a computer as a USB mass storage device, so the
//! whole filesystem (config, font, cached artwork) can be edited.
//!
//! The firmware must not use the filesystem while the drive is exposed. Switching the USB PHY to
//! OTG disconnects the USB-Serial-JTAG console until [`restore_phy`] and a reset.

use embassy_futures::yield_now;
use embassy_time::{Duration, Instant};
use embedded_sdmmc::{Block, BlockCount, BlockDevice, BlockIdx};
use esp_hal::otg_fs::{Usb, UsbBus};
use log::info;

use crate::fat_label;
use static_cell::StaticCell;
use usb_device::prelude::*;
use usbd_storage::subclass::Command;
use usbd_storage::subclass::scsi::{Scsi, ScsiCommand};
use usbd_storage::transport::TransportError;
use usbd_storage::transport::bbb::{BulkOnly, BulkOnlyError};

/// Espressif VID with the esp-tinyusb mass storage PID
const VID_PID: UsbVidPid = UsbVidPid(0x303a, 0x4002);
/// FAT volume label (11 bytes, space padded), so the drive mounts under a fixed name
const VOLUME_LABEL: &[u8; 11] = b"SAWTHAT-SD ";
/// Full speed bulk packet size
const PACKET_SIZE: u16 = 64;
/// Bulk-only transport IO buffer size
const IO_BUF_SIZE: usize = 4096;
/// Attempts per SD card block read or write; cards occasionally fail one transiently
const IO_ATTEMPTS: usize = 3;
/// Time to enumerate before giving up on the host
const ENUMERATE_TIMEOUT: Duration = Duration::from_secs(10);
/// Time suspended (unplugged or host asleep) before leaving drive mode
const SUSPEND_TIMEOUT: Duration = Duration::from_secs(2);
/// Time to send the final status after an eject
const EJECT_DELAY: Duration = Duration::from_millis(200);

/// SCSI opcodes handled beyond those parsed by `usbd-storage`
const START_STOP_UNIT: u8 = 0x1b;
const PREVENT_ALLOW_MEDIUM_REMOVAL: u8 = 0x1e;
const VERIFY_10: u8 = 0x2f;
const SYNCHRONIZE_CACHE_10: u8 = 0x35;

/// Sense keys and additional sense codes
const NO_SENSE: (u8, u8) = (0x00, 0x00);
const NOT_READY_NO_MEDIUM: (u8, u8) = (0x02, 0x3a);
const MEDIUM_ERROR_READ: (u8, u8) = (0x03, 0x11);
const MEDIUM_ERROR_WRITE: (u8, u8) = (0x03, 0x0c);
const ILLEGAL_OPCODE: (u8, u8) = (0x05, 0x20);
const ILLEGAL_LBA: (u8, u8) = (0x05, 0x21);

/// Why drive mode ended
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// The computer ejected the drive
    Ejected,
    /// `stop` returned true
    Stopped,
    /// The drive was unplugged, or the computer went to sleep
    Disconnected,
    /// No computer enumerated the drive
    NoHost,
}

type ScsiCmd<'a, 'alloc> =
    Command<'a, ScsiCommand, Scsi<BulkOnly<'alloc, UsbBus<Usb<'static>>, alloc::vec::Vec<u8>>>>;
type CmdResult = Result<(), TransportError<BulkOnlyError>>;

/// SCSI command state over a block device
struct Drive<D: BlockDevice> {
    device: D,
    blocks: u32,
    /// Bytes of the current READ or WRITE moved so far
    done: u32,
    /// Staging block and the index it holds, if loaded from the device
    block: Block,
    loaded: Option<u32>,
    /// Sense reported for the last failed command
    sense: (u8, u8),
    ejected: Option<Instant>,
}

impl<D: BlockDevice> Drive<D> {
    /// Read `index` into the staging block, retrying transient card errors
    fn read_block(&mut self, index: u32) -> bool {
        let ok = (0..IO_ATTEMPTS).any(|_| {
            let blocks = core::slice::from_mut(&mut self.block);
            self.device.read(blocks, BlockIdx(index), "usb").is_ok()
        });
        self.loaded = ok.then_some(index);
        ok
    }

    /// Write the staging block to `index`, retrying transient card errors
    fn write_block(&mut self, index: u32) -> bool {
        let ok = (0..IO_ATTEMPTS).any(|_| {
            let blocks = core::slice::from_ref(&self.block);
            self.device.write(blocks, BlockIdx(index)).is_ok()
        });
        self.loaded = ok.then_some(index);
        ok
    }

    fn fail(&mut self, cmd: ScsiCmd, sense: (u8, u8)) {
        self.sense = sense;
        cmd.fail();
        self.done = 0;
    }

    /// Respond with `data`, truncated to what the host asked for
    fn respond(mut cmd: ScsiCmd, data: &[u8]) -> CmdResult {
        cmd.write_data(data)?;
        cmd.pass();
        Ok(())
    }

    fn handle(&mut self, mut cmd: ScsiCmd) -> CmdResult {
        match cmd.kind {
            ScsiCommand::TestUnitReady if self.ejected.is_some() => {
                self.fail(cmd, NOT_READY_NO_MEDIUM)
            }
            ScsiCommand::TestUnitReady => cmd.pass(),
            ScsiCommand::Inquiry { .. } => {
                let mut data = [0u8; 36];
                data[1] = 0x80; // removable
                data[2] = 0x04; // SPC-2
                data[3] = 0x02;
                data[4] = 32; // additional length
                data[8..16].copy_from_slice(b"SawThat ");
                data[16..32].copy_from_slice(b"Frame SD card   ");
                data[32..36].copy_from_slice(b"1.0 ");
                Self::respond(cmd, &data)?;
            }
            ScsiCommand::RequestSense { .. } => {
                let mut data = [0u8; 18];
                data[0] = 0x70; // current error, fixed format
                data[2] = self.sense.0;
                data[7] = 10; // additional length
                data[12] = self.sense.1;
                self.sense = NO_SENSE;
                Self::respond(cmd, &data)?;
            }
            ScsiCommand::ModeSense6 { .. } => Self::respond(cmd, &[3, 0, 0, 0])?,
            ScsiCommand::ModeSense10 { .. } => Self::respond(cmd, &[0, 6, 0, 0, 0, 0, 0, 0])?,
            ScsiCommand::ReadCapacity10 => {
                let mut data = [0u8; 8];
                data[0..4].copy_from_slice(&(self.blocks - 1).to_be_bytes());
                data[4..8].copy_from_slice(&(Block::LEN_U32).to_be_bytes());
                Self::respond(cmd, &data)?;
            }
            ScsiCommand::ReadCapacity16 { .. } => {
                let mut data = [0u8; 32];
                data[0..8].copy_from_slice(&(self.blocks as u64 - 1).to_be_bytes());
                data[8..12].copy_from_slice(&(Block::LEN_U32).to_be_bytes());
                Self::respond(cmd, &data)?;
            }
            ScsiCommand::ReadFormatCapacities { .. } => {
                let mut data = [0u8; 12];
                data[3] = 8; // capacity list length
                data[4..8].copy_from_slice(&self.blocks.to_be_bytes());
                data[8] = 0x02; // formatted media
                data[9..12].copy_from_slice(&Block::LEN_U32.to_be_bytes()[1..]);
                Self::respond(cmd, &data)?;
            }
            ScsiCommand::Read { lba, len } => {
                let total = len as u32 * Block::LEN_U32;
                if lba as u64 + len as u64 > self.blocks as u64 {
                    self.fail(cmd, ILLEGAL_LBA);
                    return Ok(());
                }
                while self.done < total {
                    let index = lba + self.done / Block::LEN_U32;
                    if self.loaded != Some(index) && !self.read_block(index) {
                        self.fail(cmd, MEDIUM_ERROR_READ);
                        return Ok(());
                    }
                    let offset = (self.done % Block::LEN_U32) as usize;
                    match cmd.write_data(&self.block.contents[offset..])? {
                        0 => return Ok(()),
                        n => self.done += n as u32,
                    }
                }
                cmd.pass();
                self.done = 0;
            }
            ScsiCommand::Write { lba, len } => {
                let total = len as u32 * Block::LEN_U32;
                if lba as u64 + len as u64 > self.blocks as u64 {
                    self.fail(cmd, ILLEGAL_LBA);
                    return Ok(());
                }
                while self.done < total {
                    let offset = (self.done % Block::LEN_U32) as usize;
                    if offset == 0 {
                        self.loaded = None;
                    }
                    match cmd.read_data(&mut self.block.contents[offset..])? {
                        0 => return Ok(()),
                        n => self.done += n as u32,
                    }
                    if self.done.is_multiple_of(Block::LEN_U32)
                        && !self.write_block(lba + self.done / Block::LEN_U32 - 1)
                    {
                        self.fail(cmd, MEDIUM_ERROR_WRITE);
                        return Ok(());
                    }
                }
                cmd.pass();
                self.done = 0;
            }
            ScsiCommand::Unknown {
                cmd: START_STOP_UNIT,
            } => {
                info!("USB drive ejected");
                self.ejected = Some(Instant::now());
                cmd.pass();
            }
            ScsiCommand::Unknown {
                cmd: PREVENT_ALLOW_MEDIUM_REMOVAL | VERIFY_10 | SYNCHRONIZE_CACHE_10,
            } => cmd.pass(),
            _ => self.fail(cmd, ILLEGAL_OPCODE),
        }
        Ok(())
    }
}

/// Expose `device` as a USB drive until it is ejected, unplugged, or `stop` returns true
pub async fn serve<D: BlockDevice>(device: D, usb: Usb<'static>, stop: impl Fn() -> bool) -> Exit {
    static EP_MEMORY: StaticCell<[u32; 1024]> = StaticCell::new();

    let blocks = match device.num_blocks() {
        Ok(BlockCount(n)) if n > 0 => n,
        _ => {
            info!("SD card size unknown, not starting USB drive");
            return Exit::NoHost;
        }
    };

    match fat_label::set_label(&device, VOLUME_LABEL) {
        Ok(true) => info!("Set volume label"),
        Ok(false) => {}
        Err(e) => info!("Volume label not set: {:?}", e),
    }

    let bus = UsbBus::new(usb, EP_MEMORY.init([0; 1024]));
    let mut scsi = Scsi::new(&bus, PACKET_SIZE, 0, alloc::vec![0u8; IO_BUF_SIZE])
        .expect("USB storage buffer too small");
    let mut usb_dev = UsbDeviceBuilder::new(&bus, VID_PID)
        .strings(&[StringDescriptors::default()
            .manufacturer("SawThat")
            .product("SawThat Frame")
            .serial_number("SAWTHAT")])
        .expect("too many USB strings")
        .self_powered(true)
        .build();

    let mut drive = Drive {
        device,
        blocks,
        done: 0,
        block: Block::new(),
        loaded: None,
        sense: NO_SENSE,
        ejected: None,
    };

    info!("USB drive mode: exposing {} MB", blocks / 2048);
    let start = Instant::now();
    let mut configured = false;
    let mut suspended: Option<Instant> = None;

    loop {
        if usb_dev.poll(&mut [&mut scsi]) && usb_dev.state() == UsbDeviceState::Default {
            drive.done = 0;
        }
        // Driven on every pass: a status queued behind a stalled endpoint is sent once the host
        // clears the stall, which is control traffic that `poll` does not report
        let _ = scsi.poll(|cmd| {
            if let Err(e) = drive.handle(cmd) {
                info!("USB drive command error: {:?}", e);
            }
        });

        match usb_dev.state() {
            UsbDeviceState::Configured => {
                configured = true;
                suspended = None;
            }
            UsbDeviceState::Suspend if configured => {
                if suspended.get_or_insert_with(Instant::now).elapsed() > SUSPEND_TIMEOUT {
                    return Exit::Disconnected;
                }
            }
            _ => {}
        }

        if !configured && start.elapsed() > ENUMERATE_TIMEOUT {
            return Exit::NoHost;
        }
        if drive.ejected.is_some_and(|t| t.elapsed() > EJECT_DELAY) {
            return Exit::Ejected;
        }
        if stop() {
            return Exit::Stopped;
        }
        yield_now().await;
    }
}

/// Hand the USB PHY back to USB-Serial-JTAG, so the console and flashing work after a reset
pub fn restore_phy() {
    esp_hal::peripherals::LPWR::regs()
        .usb_conf()
        .modify(|_, w| w.sw_hw_usb_phy_sel().clear_bit());
}
