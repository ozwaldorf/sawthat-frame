//! USB drive mode: exposes the raw SD card to a computer as a USB mass storage device, so the
//! whole filesystem (config, font, cached artwork) can be edited.
//!
//! Implements the Bulk-Only Transport with a minimal SCSI command set on embassy-usb. Endpoints
//! are never stalled: a data phase that ends early is closed with a short packet (to the host) or
//! drained (from the host), and the residue is reported in the status.
//!
//! The firmware must not use the filesystem while the drive is exposed. Switching the USB PHY to
//! OTG disconnects the USB-Serial-JTAG console until [`restore_phy`] and a reset.

use core::cell::Cell;

use embassy_futures::select::{Either, Either3, select, select3};
use embassy_futures::yield_now;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant};
use embassy_usb::control::{InResponse, OutResponse, Recipient, Request, RequestType};
use embassy_usb::driver::{EndpointError, EndpointIn, EndpointOut};
use embassy_usb::{Builder, Handler};
use embedded_sdmmc::{Block, BlockCount, BlockDevice, BlockIdx};
use esp_hal::usb::otg::Usb;
use esp_hal::usb::otg::embassy_usb_device::{Config as OtgConfig, Driver};
use log::info;

use crate::fat_label;

/// Espressif VID with the esp-tinyusb mass storage PID
const VID: u16 = 0x303a;
const PID: u16 = 0x4002;
/// FAT volume label (11 bytes, space padded), so the drive mounts under a fixed name
const VOLUME_LABEL: &[u8; 11] = b"SAWTHAT-SD ";
/// Full speed bulk packet size
const PACKET_SIZE: u16 = 64;
/// Attempts per SD card block read or write; cards occasionally fail one transiently
const IO_ATTEMPTS: usize = 3;
/// Time to enumerate before giving up on the host
const ENUMERATE_TIMEOUT: Duration = Duration::from_secs(10);
/// Time suspended (unplugged or host asleep) before leaving drive mode
const SUSPEND_TIMEOUT: Duration = Duration::from_secs(2);
/// Time to send the final status after an eject
const EJECT_DELAY: Duration = Duration::from_millis(200);

/// Mass storage class, SCSI transparent command set, Bulk-Only Transport
const CLASS_MSC: u8 = 0x08;
const SUBCLASS_SCSI: u8 = 0x06;
const PROTOCOL_BBB: u8 = 0x50;
/// Bulk-Only class requests
const GET_MAX_LUN: u8 = 0xfe;
const BULK_ONLY_RESET: u8 = 0xff;
/// Command block and command status wrappers
const CBW_SIGNATURE: u32 = 0x4342_5355;
const CBW_LEN: usize = 31;
const CSW_SIGNATURE: u32 = 0x5342_5355;
const CSW_PASSED: u8 = 0;
const CSW_FAILED: u8 = 1;

/// SCSI opcodes
const TEST_UNIT_READY: u8 = 0x00;
const REQUEST_SENSE: u8 = 0x03;
const INQUIRY: u8 = 0x12;
const MODE_SENSE_6: u8 = 0x1a;
const START_STOP_UNIT: u8 = 0x1b;
const PREVENT_ALLOW_MEDIUM_REMOVAL: u8 = 0x1e;
const READ_FORMAT_CAPACITIES: u8 = 0x23;
const READ_CAPACITY_10: u8 = 0x25;
const READ_10: u8 = 0x28;
const WRITE_10: u8 = 0x2a;
const VERIFY_10: u8 = 0x2f;
const SYNCHRONIZE_CACHE_10: u8 = 0x35;
const MODE_SENSE_10: u8 = 0x5a;
const READ_CAPACITY_16: u8 = 0x9e;

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

/// State shared between the USB stack, the transport, and the exit monitor
#[derive(Default)]
struct Shared {
    configured: Cell<bool>,
    suspended: Cell<bool>,
    ejected: Cell<Option<Instant>>,
    /// Bus or Bulk-Only reset, aborting the command in progress
    reset: Signal<NoopRawMutex, ()>,
}

/// Bus events and Bulk-Only class requests
struct Control<'a> {
    shared: &'a Shared,
    interface: u16,
}

impl Control<'_> {
    fn is_class_request(&self, req: &Request) -> bool {
        req.request_type == RequestType::Class
            && req.recipient == Recipient::Interface
            && req.index == self.interface
    }
}

impl Handler for Control<'_> {
    fn reset(&mut self) {
        // The stack clears its own suspend state on a reset without reporting it
        self.shared.configured.set(false);
        self.shared.suspended.set(false);
        self.shared.reset.signal(());
    }

    fn configured(&mut self, configured: bool) {
        self.shared.configured.set(configured);
    }

    fn suspended(&mut self, suspended: bool) {
        self.shared.suspended.set(suspended);
    }

    fn control_out(&mut self, req: Request, _data: &[u8]) -> Option<OutResponse> {
        (self.is_class_request(&req) && req.request == BULK_ONLY_RESET).then(|| {
            self.shared.reset.signal(());
            OutResponse::Accepted
        })
    }

    fn control_in<'a>(&'a mut self, req: Request, buf: &'a mut [u8]) -> Option<InResponse<'a>> {
        (self.is_class_request(&req) && req.request == GET_MAX_LUN).then(|| {
            buf[0] = 0;
            InResponse::Accepted(&buf[..1])
        })
    }
}

/// Data phase of one command, bounded by the length the host announced
struct Transfer<'a, I, O> {
    ep_in: &'a mut I,
    ep_out: &'a mut O,
    len: u32,
    to_host: bool,
    done: u32,
}

impl<I: EndpointIn, O: EndpointOut> Transfer<'_, I, O> {
    fn remaining(&self) -> u32 {
        self.len - self.done
    }

    /// Send `data` to the host, truncated to what it asked for
    async fn write(&mut self, data: &[u8]) -> Result<(), EndpointError> {
        if !self.to_host {
            return Ok(());
        }
        let n = data.len().min(self.remaining() as usize);
        self.ep_in.write_transfer(&data[..n], false).await?;
        self.done += n as u32;
        Ok(())
    }

    /// Receive into `buf` until it is full or the host ends the data phase
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, EndpointError> {
        if self.to_host {
            return Ok(0);
        }
        let n = buf.len().min(self.remaining() as usize);
        if n == 0 {
            return Ok(0);
        }
        let read = self.ep_out.read_transfer(&mut buf[..n]).await?;
        self.done += read as u32;
        Ok(read)
    }

    /// End the data phase: a short packet ends what the host reads, what it still sends is dropped
    async fn finish(&mut self) -> Result<(), EndpointError> {
        if self.remaining() == 0 {
            return Ok(());
        }
        if self.to_host {
            if self.done.is_multiple_of(PACKET_SIZE as u32) {
                self.ep_in.write(&[]).await?;
            }
        } else {
            let mut packet = [0u8; PACKET_SIZE as usize];
            while self.remaining() > 0 {
                let n = self.ep_out.read(&mut packet).await?;
                self.done += n as u32;
                if n < packet.len() {
                    break;
                }
            }
        }
        Ok(())
    }
}

/// SCSI command state over a block device
struct Drive<'a, D: BlockDevice> {
    device: D,
    blocks: u32,
    /// Staging block and the index it holds, if loaded from the device
    block: Block,
    loaded: Option<u32>,
    /// Sense reported for the last failed command
    sense: (u8, u8),
    shared: &'a Shared,
}

impl<D: BlockDevice> Drive<'_, D> {
    /// Read `index` into the staging block, retrying transient card errors
    fn read_block(&mut self, index: u32) -> bool {
        let ok = (0..IO_ATTEMPTS).any(|_| {
            let blocks = core::slice::from_mut(&mut self.block);
            self.device.read(blocks, BlockIdx(index)).is_ok()
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

    fn fail(&mut self, sense: (u8, u8)) -> bool {
        self.sense = sense;
        false
    }

    /// Run the command in `cb`, returning whether it passed
    async fn handle<I: EndpointIn, O: EndpointOut>(
        &mut self,
        cb: &[u8; 16],
        xfer: &mut Transfer<'_, I, O>,
    ) -> Result<bool, EndpointError> {
        let be32 = |i: usize| u32::from_be_bytes([cb[i], cb[i + 1], cb[i + 2], cb[i + 3]]);
        let be16 = |i: usize| u16::from_be_bytes([cb[i], cb[i + 1]]);

        match cb[0] {
            TEST_UNIT_READY if self.shared.ejected.get().is_some() => {
                return Ok(self.fail(NOT_READY_NO_MEDIUM));
            }
            TEST_UNIT_READY | PREVENT_ALLOW_MEDIUM_REMOVAL | VERIFY_10 | SYNCHRONIZE_CACHE_10 => {}
            INQUIRY => {
                let mut data = [0u8; 36];
                data[1] = 0x80; // removable
                data[2] = 0x04; // SPC-2
                data[3] = 0x02;
                data[4] = 32; // additional length
                data[8..16].copy_from_slice(b"SawThat ");
                data[16..32].copy_from_slice(b"Frame SD card   ");
                data[32..36].copy_from_slice(b"1.0 ");
                xfer.write(&data).await?;
            }
            REQUEST_SENSE => {
                let mut data = [0u8; 18];
                data[0] = 0x70; // current error, fixed format
                data[2] = self.sense.0;
                data[7] = 10; // additional length
                data[12] = self.sense.1;
                self.sense = NO_SENSE;
                xfer.write(&data).await?;
            }
            MODE_SENSE_6 => xfer.write(&[3, 0, 0, 0]).await?,
            MODE_SENSE_10 => xfer.write(&[0, 6, 0, 0, 0, 0, 0, 0]).await?,
            READ_CAPACITY_10 => {
                let mut data = [0u8; 8];
                data[0..4].copy_from_slice(&(self.blocks - 1).to_be_bytes());
                data[4..8].copy_from_slice(&Block::LEN_U32.to_be_bytes());
                xfer.write(&data).await?;
            }
            READ_CAPACITY_16 => {
                let mut data = [0u8; 32];
                data[0..8].copy_from_slice(&(self.blocks as u64 - 1).to_be_bytes());
                data[8..12].copy_from_slice(&Block::LEN_U32.to_be_bytes());
                xfer.write(&data).await?;
            }
            READ_FORMAT_CAPACITIES => {
                let mut data = [0u8; 12];
                data[3] = 8; // capacity list length
                data[4..8].copy_from_slice(&self.blocks.to_be_bytes());
                data[8] = 0x02; // formatted media
                data[9..12].copy_from_slice(&Block::LEN_U32.to_be_bytes()[1..]);
                xfer.write(&data).await?;
            }
            READ_10 => {
                let (lba, count) = (be32(2), be16(7) as u32);
                if lba as u64 + count as u64 > self.blocks as u64 {
                    return Ok(self.fail(ILLEGAL_LBA));
                }
                for index in lba..lba + count {
                    if xfer.remaining() == 0 {
                        break;
                    }
                    if self.loaded != Some(index) && !self.read_block(index) {
                        return Ok(self.fail(MEDIUM_ERROR_READ));
                    }
                    xfer.write(&self.block.contents).await?;
                }
            }
            WRITE_10 => {
                let (lba, count) = (be32(2), be16(7) as u32);
                if lba as u64 + count as u64 > self.blocks as u64 {
                    return Ok(self.fail(ILLEGAL_LBA));
                }
                for index in lba..lba + count {
                    self.loaded = None;
                    if xfer.read(&mut self.block.contents).await? < Block::LEN {
                        return Ok(self.fail(MEDIUM_ERROR_WRITE));
                    }
                    if !self.write_block(index) {
                        return Ok(self.fail(MEDIUM_ERROR_WRITE));
                    }
                }
            }
            START_STOP_UNIT => {
                info!("USB drive ejected");
                self.shared.ejected.set(Some(Instant::now()));
            }
            _ => return Ok(self.fail(ILLEGAL_OPCODE)),
        }
        Ok(true)
    }

    /// Serve Bulk-Only commands forever
    async fn serve<I: EndpointIn, O: EndpointOut>(&mut self, ep_in: &mut I, ep_out: &mut O) -> ! {
        let shared = self.shared;
        let mut cbw = [0u8; PACKET_SIZE as usize];
        loop {
            match ep_out.read(&mut cbw).await {
                Ok(CBW_LEN) => {}
                // Not a command block; wait for the next one
                Ok(_) => continue,
                Err(_) => {
                    ep_out.wait_enabled().await;
                    continue;
                }
            }
            let le32 = |i: usize| u32::from_le_bytes([cbw[i], cbw[i + 1], cbw[i + 2], cbw[i + 3]]);
            if le32(0) != CBW_SIGNATURE {
                continue;
            }
            let mut cb = [0u8; 16];
            let cb_len = (cbw[14] as usize & 0x1f).min(cb.len());
            cb[..cb_len].copy_from_slice(&cbw[15..15 + cb_len]);
            let mut xfer = Transfer {
                ep_in: &mut *ep_in,
                ep_out: &mut *ep_out,
                len: le32(8),
                to_host: cbw[12] & 0x80 != 0,
                done: 0,
            };

            shared.reset.reset();
            let command = async {
                let passed = self.handle(&cb, &mut xfer).await?;
                xfer.finish().await?;
                Ok::<_, EndpointError>((passed, xfer.remaining()))
            };
            let (passed, residue) = match select(command, shared.reset.wait()).await {
                Either::First(Ok(result)) => result,
                Either::First(Err(_)) | Either::Second(()) => continue,
            };

            let mut csw = [0u8; 13];
            csw[0..4].copy_from_slice(&CSW_SIGNATURE.to_le_bytes());
            csw[4..8].copy_from_slice(&cbw[4..8]);
            csw[8..12].copy_from_slice(&residue.to_le_bytes());
            csw[12] = if passed { CSW_PASSED } else { CSW_FAILED };
            let _ = ep_in.write(&csw).await;
        }
    }
}

/// Expose `device` as a USB drive until it is ejected, unplugged, or `stop` returns true.
///
/// `tick` runs on every pass of the polling loop, for work that must continue meanwhile.
pub async fn serve<D: BlockDevice>(
    device: D,
    usb: Usb<'static>,
    stop: impl Fn() -> bool,
    mut tick: impl FnMut(),
) -> Exit {
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

    let shared = Shared::default();
    let mut control = Control {
        shared: &shared,
        interface: 0,
    };
    let mut ep_out_buffer = [0u8; 256];
    let mut config_descriptor = [0u8; 64];
    let mut bos_descriptor = [0u8; 32];
    let mut control_buf = [0u8; 64];

    let mut config = embassy_usb::Config::new(VID, PID);
    config.manufacturer = Some("SawThat");
    config.product = Some("SawThat Frame");
    config.serial_number = Some("SAWTHAT");
    config.self_powered = true;
    config.device_class = 0;
    config.device_sub_class = 0;
    config.device_protocol = 0;
    config.composite_with_iads = false;

    let mut builder = Builder::new(
        Driver::new(usb, &mut ep_out_buffer, OtgConfig::default()),
        config,
        &mut config_descriptor,
        &mut bos_descriptor,
        &mut [],
        &mut control_buf,
    );
    let (mut ep_in, mut ep_out) = {
        let mut function = builder.function(CLASS_MSC, SUBCLASS_SCSI, PROTOCOL_BBB);
        let mut interface = function.interface();
        control.interface = interface.interface_number().0 as u16;
        let mut alt = interface.alt_setting(CLASS_MSC, SUBCLASS_SCSI, PROTOCOL_BBB, None);
        (
            alt.endpoint_bulk_in(None, PACKET_SIZE),
            alt.endpoint_bulk_out(None, PACKET_SIZE),
        )
    };
    builder.handler(&mut control);
    let mut usb_dev = builder.build();

    let mut drive = Drive {
        device,
        blocks,
        block: Block::new(),
        loaded: None,
        sense: NO_SENSE,
        shared: &shared,
    };

    info!("USB drive mode: exposing {} MB", blocks / 2048);
    let monitor = async {
        let start = Instant::now();
        let mut configured = false;
        let mut suspended: Option<Instant> = None;
        loop {
            configured |= shared.configured.get();
            if configured && shared.suspended.get() {
                if suspended.get_or_insert_with(Instant::now).elapsed() > SUSPEND_TIMEOUT {
                    return Exit::Disconnected;
                }
            } else {
                suspended = None;
            }

            if !configured && start.elapsed() > ENUMERATE_TIMEOUT {
                return Exit::NoHost;
            }
            if shared
                .ejected
                .get()
                .is_some_and(|t| t.elapsed() > EJECT_DELAY)
            {
                return Exit::Ejected;
            }
            tick();
            if stop() {
                return Exit::Stopped;
            }
            yield_now().await;
        }
    };

    let exit = match select3(usb_dev.run(), drive.serve(&mut ep_in, &mut ep_out), monitor).await {
        Either3::Third(exit) => exit,
        Either3::First(never) => never,
        Either3::Second(never) => never,
    };
    usb_dev.disable().await;
    exit
}

/// Hand the USB PHY back to USB-Serial-JTAG, so the console and flashing work after a reset
pub fn restore_phy() {
    esp_hal::peripherals::LPWR::regs()
        .usb_conf()
        .modify(|_, w| w.sw_hw_usb_phy_sel().clear_bit());
}
