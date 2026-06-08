//! Driver crate for Mountain DisplayPad - parse HID messages and expose API.
//!
//! Ported from SytxLabs/DisplayPad (Python). The image transfer follows a
//! state-machine protocol:
//!   1. Send INIT_MSG → device replies with 0x11 (ready)
//!   2. Send IMG_MSG (with key index) → device replies with 0x21 0x00 0x00 (send data)
//!   3. Write image_header + pixels in 1024-byte chunks to display interface
//!   4. Write image_header + pixels again (un-chunked) to display interface
//!   5. Device replies with 0x21 0x00 0xFF (transfer complete)
//!   6. Pop queue, repeat from step 2 if more items

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

pub const COLS: usize = 6;
pub const ROWS: usize = 2;
pub const BUTTON_COUNT: usize = COLS * ROWS;

// Device constants from the Python implementation
pub const ICON_SIZE: usize = 102;
pub const NUM_KEYS: usize = 12;
pub const PACKET_SIZE: usize = 31438;
pub const HEADER_SIZE: usize = 306;

pub const VENDOR_ID: u16 = 0x3282;
pub const PRODUCT_IDS: [u16; 1] = [0x0009];

// Default INIT and IMG messages (hex strings from Python repo)
const INIT_MSG_STR: &str = "00118000000100000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";
const IMG_MSG_STR: &str = "0021000000FF3d00006565000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";

#[cfg_attr(target_os = "linux", allow(dead_code))]
const NULLBYTE: &[u8] = &[0x00];

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct ButtonEvent {
    pub row: usize,
    pub col: usize,
    pub index: usize,
    pub pressed: bool,
}

pub fn index_of(row: usize, col: usize) -> usize {
    row * COLS + col
}

/// Parse a device report into the current pressed state for all keys.
/// Returns an empty Vec if the report is not a key-state report.
/// Mapping follows the original Python implementation in SytxLabs/DisplayPad._process_device_event
pub fn parse_report(report: &[u8]) -> Vec<ButtonEvent> {
    let mut events = Vec::new();
    if report.is_empty() {
        return events;
    }
    // Only process reports with leading byte 0x01 (key state)
    if report[0] != 0x01 {
        return events;
    }

    // Guard indices used in mapping (42 and 47)
    if report.len() <= 47 {
        return events;
    }

    let b42 = report[42];
    let b47 = report[47];

    // Row 1 (keys 0..5) use bits in b42
    let row1_bits = [0x02u8, 0x04u8, 0x08u8, 0x10u8, 0x20u8, 0x40u8];
    for (i, &mask) in row1_bits.iter().enumerate() {
        let pressed = (b42 & mask) != 0;
        events.push(ButtonEvent {
            row: 0,
            col: i,
            index: i,
            pressed,
        });
    }

    // Row 2 (keys 6..11) use bits in b42 and b47
    // key 6: b42 & 0x80
    events.push(ButtonEvent {
        row: 1,
        col: 0,
        index: 6,
        pressed: (b42 & 0x80) != 0,
    });
    // keys 7..11 use b47 bits 0x01..0x10
    for i in 0..5 {
        let mask = 1 << i; // 0x01,0x02,0x04,0x08,0x10
        let pressed = (b47 & mask) != 0;
        events.push(ButtonEvent {
            row: 1,
            col: i + 1,
            index: 7 + i,
            pressed,
        });
    }

    events
}

/// Backwards-compatible helper: find a DisplayPad device by inspecting HID devices.
pub fn find_displaypad() -> Option<(u16, u16, String)> {
    match hidapi::HidApi::new() {
        Ok(api) => {
            for dev in api.device_list() {
                let prod = dev.product_string().map(|s| s.to_lowercase());
                let manuf = dev.manufacturer_string().map(|s| s.to_lowercase());
                let matches = prod
                    .as_deref()
                    .is_some_and(|p| p.contains("displaypad") || p.contains("mountain"))
                    || manuf.as_deref().is_some_and(|m| m.contains("mountain"));
                if matches {
                    let path = dev.path().to_string_lossy().into_owned();
                    return Some((dev.vendor_id(), dev.product_id(), path));
                }
            }
            None
        }
        Err(_) => None,
    }
}

/// Backwards-compatible helper: open a device path and read a single report with a timeout.
pub fn read_report_once(path: &str, timeout_ms: i32) -> Result<Option<Vec<ButtonEvent>>, String> {
    let api = hidapi::HidApi::new().map_err(|e| e.to_string())?;
    let cstr = std::ffi::CString::new(path).map_err(|e| e.to_string())?;
    let device = api.open_path(cstr.as_c_str()).map_err(|e| e.to_string())?;
    let mut buf = [0u8; 64];
    match device.read_timeout(&mut buf, timeout_ms) {
        Ok(len) if len > 0 => Ok(Some(parse_report(&buf[..len]))),
        Ok(_) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// DisplayPad: manages HID connections, listener thread, callbacks, and pixel transfer state machine.
pub type KeyCallback = Box<dyn Fn(usize) + Send + 'static>;
pub type ErrCallback = Box<dyn Fn(String) + Send + 'static>;

type UsbHandles = (Box<dyn UsbWriter>, Box<dyn UsbIo>);

/// Abstraction for writing to a USB OUT endpoint (the display interface).
/// Uses hidapi on all platforms — mirroring the Python reference, which opens
/// both the display and command interfaces via hidapi.
pub trait UsbWriter: Send + 'static {
    fn write(&self, data: &[u8]) -> Result<(), String>;
}

/// hidapi-based writer (display interface, non-Linux). On Linux the display
/// interface is written via rusb (see `RusbWriter`) because it has no hidraw node.
#[cfg(not(target_os = "linux"))]
struct HidWriter {
    device: hidapi::HidDevice,
}

#[cfg(not(target_os = "linux"))]
impl UsbWriter for HidWriter {
    fn write(&self, data: &[u8]) -> Result<(), String> {
        self.device
            .write(data)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

/// rusb (libusb) based writer for the display interface on Linux.
///
/// The display interface (interface 1) exposes only an OUT endpoint and is NOT
/// backed by a hidraw node, so the hidapi hidraw backend cannot open it. rusb
/// claims the interface directly and writes to the interrupt/bulk OUT endpoint.
#[cfg(target_os = "linux")]
struct RusbWriter {
    handle: Arc<rusb::DeviceHandle<rusb::GlobalContext>>,
    endpoint: u8,
    transfer_type: EndpointTransferType,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
enum EndpointTransferType {
    Interrupt,
    Bulk,
}

#[cfg(target_os = "linux")]
impl UsbWriter for RusbWriter {
    fn write(&self, data: &[u8]) -> Result<(), String> {
        let timeout = Duration::from_millis(1000);
        let res = match self.transfer_type {
            EndpointTransferType::Interrupt => {
                self.handle.write_interrupt(self.endpoint, data, timeout)
            }
            EndpointTransferType::Bulk => self.handle.write_bulk(self.endpoint, data, timeout),
        };
        res.map(|_| ()).map_err(|e| e.to_string())
    }
}

/// Combined read+write abstraction for the command interface (interface 3).
///
/// CRITICAL: interface 3 must use a SINGLE underlying handle for both reads and
/// writes. On Linux, opening the device twice yields two independent handles, and
/// a command written on one is acknowledged only on that same handle — so a
/// separate reader never observes the device's responses (INIT 0x11, IMG 0x21 ...).
/// One shared handle makes Linux behave like Windows.
pub trait UsbIo: Send + 'static {
    fn write(&self, data: &[u8]) -> Result<(), String>;
    fn read_timeout(&self, buf: &mut [u8], timeout_ms: i32) -> Result<usize, String>;
}

/// hidapi-based read+write handle for interface 3 (non-Linux). On Linux the
/// command interface is driven via raw libusb (`RusbIo`) instead.
#[cfg(not(target_os = "linux"))]
struct HidIo {
    device: hidapi::HidDevice,
}

#[cfg(not(target_os = "linux"))]
impl UsbIo for HidIo {
    fn write(&self, data: &[u8]) -> Result<(), String> {
        self.device
            .write(data)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    fn read_timeout(&self, buf: &mut [u8], timeout_ms: i32) -> Result<usize, String> {
        self.device
            .read_timeout(buf, timeout_ms)
            .map_err(|e| e.to_string())
    }
}

/// rusb (raw libusb) read+write handle for the command interface (interface 3)
/// on Linux. This mirrors the proven Linux reference ReversingForFun/
/// MountainDisplayPadPy, which writes commands to the interrupt OUT endpoint
/// (0x04) as a 64-byte padded packet and reads responses from the interrupt IN
/// endpoint (0x83). The hidapi backends on Linux send a short (63-byte) packet
/// because they strip the leading HID report-ID byte; the device accepts INIT
/// that way but NAKs the IMG_MSG write, so we bypass hidapi entirely here.
#[cfg(target_os = "linux")]
struct RusbIo {
    handle: Arc<rusb::DeviceHandle<rusb::GlobalContext>>,
    out_ep: u8,
    in_ep: u8,
}

#[cfg(target_os = "linux")]
impl UsbIo for RusbIo {
    fn write(&self, data: &[u8]) -> Result<(), String> {
        // Our command buffers carry a leading HID report-ID byte (0x00) by the
        // hidapi convention. Raw libusb must NOT include it, and the device
        // expects a full 64-byte interrupt packet, so strip the report ID and
        // pad/truncate to exactly 64 bytes.
        let payload = if data.first() == Some(&0x00) {
            &data[1..]
        } else {
            data
        };
        let mut buf = [0u8; 64];
        let n = payload.len().min(64);
        buf[..n].copy_from_slice(&payload[..n]);
        self.handle
            .write_interrupt(self.out_ep, &buf, Duration::from_millis(1000))
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    fn read_timeout(&self, buf: &mut [u8], timeout_ms: i32) -> Result<usize, String> {
        match self.handle.read_interrupt(
            self.in_ep,
            buf,
            Duration::from_millis(timeout_ms.max(0) as u64),
        ) {
            Ok(n) => Ok(n),
            Err(rusb::Error::Timeout) => Ok(0),
            Err(e) => Err(e.to_string()),
        }
    }
}

/// Transfer state machine states (mirrors the Python reference's flags)
#[derive(Debug, Clone, PartialEq, Eq)]
enum TransferState {
    /// INIT_MSG sent, waiting for 0x11 response
    Initializing,
    /// No transfer in progress, device is ready for the next queued image
    Idle,
    /// IMG_MSG sent for a key, awaiting the 0x21 responses (ready / complete)
    Transferring,
}

pub struct DisplayPad {
    display: Arc<Mutex<Box<dyn UsbWriter>>>,
    device: Arc<Mutex<Box<dyn UsbIo>>>,
    key_state: Arc<Mutex<Vec<u8>>>,
    on_down: Arc<Mutex<Option<KeyCallback>>>,
    on_up: Arc<Mutex<Option<KeyCallback>>>,
    on_error: Arc<Mutex<Option<ErrCallback>>>,
    connected: Arc<Mutex<bool>>,

    // Transfer queue and state
    queue: Arc<Mutex<std::collections::VecDeque<QueueRequest>>>,
    transfer_state: Arc<Mutex<TransferState>>,
}

struct QueueRequest {
    key_index: usize,
    pixels: Vec<u8>,
}

impl DisplayPad {
    /// Create and connect to the first available DisplayPad. Starts a listener thread
    /// and sends the INIT message to put the device into ready state.
    pub fn new() -> Result<Self, String> {
        let api = hidapi::HidApi::new().map_err(|e| e.to_string())?;

        // enumerate devices and find ones matching vendor/product
        let devices: Vec<_> = api
            .device_list()
            .filter(|d| d.vendor_id() == VENDOR_ID && PRODUCT_IDS.contains(&d.product_id()))
            .collect();
        if devices.is_empty() {
            return Err("No DisplayPad devices found".into());
        }

        // find display (interface_number == 1) and device (interface_number == 3) like Python version
        #[allow(unused_assignments)]
        let mut keyboard_path: Option<String> = None;
        let mut display_path: Option<String> = None;
        let mut device_path: Option<String> = None;
        for d in &devices {
            let ifnum = d.interface_number();
            if ifnum == 0 && keyboard_path.is_none() {
                keyboard_path = Some(d.path().to_string_lossy().into_owned());
            }
            if ifnum == 1 && display_path.is_none() {
                display_path = Some(d.path().to_string_lossy().into_owned());
            }
            if ifnum == 3 && device_path.is_none() {
                device_path = Some(d.path().to_string_lossy().into_owned());
            }
        }

        // On Linux the device sometimes needs a nudge to wake after a cold USB
        // plug-in: sending an LED report to the keyboard interface (0) makes the
        // controller initialize (a known hardware quirk, see
        // https://github.com/ReversingForFun/MountainDisplayPadPy/issues/1).
        //
        // IMPORTANT: that wake makes an ALREADY-AWAKE device reboot, which would
        // drop images we just pushed. So we do NOT wake unconditionally here.
        // Instead we open the interfaces, let the listener poll INIT, and only
        // send the wake further below if the device does not acknowledge quickly
        // (meaning it really is asleep). Matches the no-wake reference behaviour
        // when the device is already present.

        // On Linux the command interface (3) is driven by rusb, which discovers
        // the device by VID/PID — so hidapi does NOT need to enumerate it. This
        // matters for warm restarts: a previous run detaches interfaces 1 and 3
        // from their kernel drivers, so their hidraw nodes disappear and hidapi
        // can no longer see interface 3. We must not hard-fail on that.
        #[cfg(target_os = "linux")]
        let device_path_str = device_path.unwrap_or_default();
        #[cfg(not(target_os = "linux"))]
        let device_path_str =
            device_path.ok_or("No DisplayPad device interface (interface 3) found")?;

        // Open all USB handles — platform-specific
        let (display_writer, device_io) = Self::open_usb(&api, &display_path, &device_path_str)?;

        let dp = DisplayPad {
            display: Arc::new(Mutex::new(display_writer)),
            device: Arc::new(Mutex::new(device_io)),
            key_state: Arc::new(Mutex::new(vec![0u8; NUM_KEYS])),
            on_down: Arc::new(Mutex::new(None)),
            on_up: Arc::new(Mutex::new(None)),
            on_error: Arc::new(Mutex::new(None)),
            connected: Arc::new(Mutex::new(true)),
            queue: Arc::new(Mutex::new(std::collections::VecDeque::new())),
            transfer_state: Arc::new(Mutex::new(TransferState::Initializing)),
        };

        // === LISTENER THREAD ===
        // A single thread owns interface 3 and performs strictly sequential I/O,
        // mirroring the Python reference. Because interface 3 uses ONE handle for
        // both reads and writes, the device's command responses (0x11, 0x21 ...)
        // are observed on the same fd they were requested on — which is what makes
        // Linux behave like Windows. While idle it polls for button/INIT reports;
        // when a transfer is queued it runs the write/read handshake to completion.
        {
            let display_handle = dp.display.clone();
            let device_handle = dp.device.clone();
            let key_state = dp.key_state.clone();
            let on_down = dp.on_down.clone();
            let on_up = dp.on_up.clone();
            let on_error = dp.on_error.clone();
            let connected = dp.connected.clone();
            let queue = dp.queue.clone();
            let transfer_state = dp.transfer_state.clone();

            thread::spawn(move || {
                let mut buf = [0u8; 64];
                // Local watchdog: when a transfer is initiated we record the time.
                // If the device hasn't completed it within 1s, we re-send INIT —
                // exactly like the Python reference's threading.Timer(1.0, reset_device).
                let mut transfer_start: Option<std::time::Instant> = None;
                // Throttle for INIT re-sends while in the Initializing state.
                let mut last_init: Option<std::time::Instant> = None;

                loop {
                    if !*connected.lock().unwrap() {
                        if let Some(cb) = &*on_error.lock().unwrap() {
                            cb("Device disconnected".to_string());
                        }
                        return;
                    }

                    // === READ interface 3 (single shared handle), like Python's
                    // _device_listener. ALL writes happen reactively below. ===
                    let res = {
                        let dev = device_handle.lock().unwrap();
                        dev.read_timeout(&mut buf, 100)
                    };
                    match res {
                        Ok(len) if len > 0 => {
                            let b0 = buf[0];
                            if b0 == 0x01 && len > 47 {
                                Self::process_buttons(&buf[..len], &key_state, &on_down, &on_up);
                            } else if b0 == 0x11 {
                                // INIT acknowledged — device is ready.
                                *transfer_state.lock().unwrap() = TransferState::Idle;
                            } else if b0 == 0x21 && len >= 3 && buf[1] == 0x00 {
                                if buf[2] == 0x00 {
                                    // Device is ready for pixels: stream them to
                                    // the display interface.
                                    let combined = {
                                        let q = queue.lock().unwrap();
                                        q.front().map(|req| {
                                            let mut c = vec![0u8; HEADER_SIZE];
                                            c.extend_from_slice(&req.pixels);
                                            c
                                        })
                                    };
                                    if let Some(combined) = combined {
                                        let disp = display_handle.lock().unwrap();
                                        Self::write_pixels(&**disp, &combined);
                                    }
                                    // Keep the watchdog armed until 0x21 00 FF.
                                } else if buf[2] == 0xFF {
                                    // Transfer complete: pop the queue, back to idle.
                                    queue.lock().unwrap().pop_front();
                                    *transfer_state.lock().unwrap() = TransferState::Idle;
                                    transfer_start = None;
                                }
                            }
                        }
                        Ok(_) => {} // read timeout — fall through to housekeeping
                        Err(e) => {
                            // Only a READ error means the device is truly gone
                            // (Python breaks the listener only on a read exception).
                            *connected.lock().unwrap() = false;
                            if let Some(cb) = &*on_error.lock().unwrap() {
                                cb(e.to_string());
                            }
                            return;
                        }
                    }

                    // === HOUSEKEEPING: initiate the next transfer when idle, and run
                    // the stall watchdog. Write failures here are NON-FATAL. ===
                    let state_now = transfer_state.lock().unwrap().clone();
                    match state_now {
                        TransferState::Idle => {
                            let key = queue.lock().unwrap().front().map(|r| r.key_index);
                            if let Some(key) = key {
                                *transfer_state.lock().unwrap() = TransferState::Transferring;
                                transfer_start = Some(std::time::Instant::now());
                                Self::write_img_msg(&device_handle, key);
                            }
                        }
                        TransferState::Transferring => {
                            if let Some(start) = transfer_start {
                                if start.elapsed() > Duration::from_secs(1) {
                                    // Stalled — go back to Initializing so the
                                    // branch below re-sends INIT immediately.
                                    *transfer_state.lock().unwrap() = TransferState::Initializing;
                                    transfer_start = None;
                                    last_init = None;
                                }
                            }
                        }
                        TransferState::Initializing => {
                            // The listener thread is the SOLE owner of device I/O.
                            // External callers (new(), wait_for_ready) only set the
                            // Initializing state; we perform the actual INIT write
                            // here so a blocking read can never starve it on the
                            // shared device handle. Throttle re-sends to ~500ms.
                            let due =
                                last_init.is_none_or(|t| t.elapsed() > Duration::from_millis(500));
                            if due {
                                last_init = Some(std::time::Instant::now());
                                if let Ok(data) = hex::decode(INIT_MSG_STR) {
                                    let dev = device_handle.lock().unwrap();
                                    let _ = dev.write(&data);
                                }
                            }
                        }
                    }
                }
            });
        }

        // The listener (spawned above) is already sending INIT and watching for the
        // 0x11 ack. Set the Initializing state explicitly for clarity.
        dp.reset_device()?;

        // On Linux only: if the device does not acknowledge INIT within a short
        // window it is asleep (cold plug), so kick it with the keyboard LED wake.
        // An already-awake device acks within a few hundred ms, in which case we
        // skip the wake to avoid the reboot it would otherwise trigger.
        #[cfg(target_os = "linux")]
        {
            let start = std::time::Instant::now();
            let mut awake = false;
            while start.elapsed() < Duration::from_millis(1500) {
                if dp.is_ready() {
                    awake = true;
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            }
            if !awake {
                if let Some(ref kb_path) = keyboard_path {
                    if let Ok(kb_cstr) = std::ffi::CString::new(kb_path.clone()) {
                        if let Ok(kb_dev) = api.open_path(kb_cstr.as_c_str()) {
                            // HID LED report: report ID 0x00, capslock bit
                            let led_on: [u8; 2] = [0x00, 0x02];
                            let led_off: [u8; 2] = [0x00, 0x00];
                            let _ = kb_dev.write(&led_on);
                            thread::sleep(Duration::from_millis(50));
                            let _ = kb_dev.write(&led_off);
                        }
                    }
                }
            }
        }

        Ok(dp)
    }

    /// Open USB handles. The command interface (interface 3) always uses hidapi
    /// (hidraw on Linux) for a single shared read+write handle. The display
    /// interface (interface 1) uses hidapi on non-Linux, and rusb on Linux —
    /// because on Linux that OUT-only interface has no hidraw node to open.
    #[cfg(not(target_os = "linux"))]
    fn open_usb(
        api: &hidapi::HidApi,
        display_path: &Option<String>,
        device_path_str: &str,
    ) -> Result<UsbHandles, String> {
        let disp_path_str = display_path
            .as_ref()
            .ok_or("No DisplayPad display interface (interface 1) found")?;
        let disp_cstr = std::ffi::CString::new(disp_path_str.clone()).map_err(|e| e.to_string())?;
        let display_dev = api
            .open_path(disp_cstr.as_c_str())
            .map_err(|e| e.to_string())?;

        let dev_cstr = std::ffi::CString::new(device_path_str).map_err(|e| e.to_string())?;
        let device_dev = api
            .open_path(dev_cstr.as_c_str())
            .map_err(|e| e.to_string())?;

        let display_writer: Box<dyn UsbWriter> = Box::new(HidWriter {
            device: display_dev,
        });
        let device_io: Box<dyn UsbIo> = Box::new(HidIo { device: device_dev });
        Ok((display_writer, device_io))
    }

    #[cfg(target_os = "linux")]
    fn open_usb(
        _api: &hidapi::HidApi,
        _display_path: &Option<String>,
        _device_path_str: &str,
    ) -> Result<UsbHandles, String> {
        // On Linux, drive BOTH interfaces via raw libusb (rusb), mirroring the
        // proven reference ReversingForFun/MountainDisplayPadPy:
        //   - Interface 1 (display): interrupt OUT endpoint (0x02) for pixel data.
        //   - Interface 3 (device):  interrupt OUT (0x04) for commands, IN (0x83)
        //     for responses.
        // Both share a single libusb device handle.
        let devices = rusb::devices().map_err(|e| e.to_string())?;
        for device in devices.iter() {
            let desc = match device.device_descriptor() {
                Ok(d) => d,
                Err(_) => continue,
            };
            if desc.vendor_id() != VENDOR_ID || !PRODUCT_IDS.contains(&desc.product_id()) {
                continue;
            }
            let handle = device.open().map_err(|e| e.to_string())?;

            // Detach kernel drivers and claim both interfaces we use.
            for ifnum in [1u8, 3u8] {
                if handle.kernel_driver_active(ifnum).unwrap_or(false) {
                    let _ = handle.detach_kernel_driver(ifnum);
                }
                handle
                    .claim_interface(ifnum)
                    .map_err(|e| format!("Failed to claim interface {}: {}", ifnum, e))?;
            }

            let handle = Arc::new(handle);

            // Discover endpoints for interfaces 1 and 3.
            let config = device
                .active_config_descriptor()
                .map_err(|e| e.to_string())?;
            let mut disp_out: Option<(u8, EndpointTransferType)> = None;
            let mut dev_out: Option<u8> = None;
            let mut dev_in: Option<u8> = None;
            for iface in config.interfaces() {
                for iface_desc in iface.descriptors() {
                    let ifnum = iface_desc.interface_number();
                    for ep in iface_desc.endpoint_descriptors() {
                        match (ifnum, ep.direction()) {
                            (1, rusb::Direction::Out) if disp_out.is_none() => {
                                let tt = match ep.transfer_type() {
                                    rusb::TransferType::Bulk => EndpointTransferType::Bulk,
                                    _ => EndpointTransferType::Interrupt,
                                };
                                disp_out = Some((ep.address(), tt));
                            }
                            (3, rusb::Direction::Out) if dev_out.is_none() => {
                                dev_out = Some(ep.address());
                            }
                            (3, rusb::Direction::In) if dev_in.is_none() => {
                                dev_in = Some(ep.address());
                            }
                            _ => {}
                        }
                    }
                }
            }

            let (disp_ep, disp_tt) =
                disp_out.ok_or("No OUT endpoint found on display interface 1")?;
            let dev_out = dev_out.ok_or("No OUT endpoint found on device interface 3")?;
            let dev_in = dev_in.ok_or("No IN endpoint found on device interface 3")?;

            let display_writer: Box<dyn UsbWriter> = Box::new(RusbWriter {
                handle: handle.clone(),
                endpoint: disp_ep,
                transfer_type: disp_tt,
            });
            let device_io: Box<dyn UsbIo> = Box::new(RusbIo {
                handle: handle.clone(),
                out_ep: dev_out,
                in_ep: dev_in,
            });
            return Ok((display_writer, device_io));
        }
        Err("No DisplayPad device found via rusb".into())
    }

    /// Send IMG_MSG (with the key index in byte 5) to start a transfer for one key.
    /// Returns true if the write succeeded.
    fn write_img_msg(device: &Arc<Mutex<Box<dyn UsbIo>>>, key_index: usize) -> bool {
        let mut data = match hex::decode(IMG_MSG_STR) {
            Ok(d) => d,
            Err(_) => return false,
        };
        if data.len() > 5 {
            data[5] = key_index as u8;
        }
        match device.lock() {
            Ok(dev) => dev.write(&data).is_ok(),
            Err(_) => false,
        }
    }

    /// Stream pixel data (header + pixels) to the display interface.
    ///
    /// On Linux the display interface is a raw libusb endpoint, so we send plain
    /// 1024-byte chunks (the device splits them into USB packets itself) with no
    /// HID report-ID prefix and no trailing un-chunked write — mirroring the
    /// reference ReversingForFun/MountainDisplayPadPy.
    ///
    /// On other platforms (hidapi) each chunk is NULLBYTE-prefixed (report ID)
    /// and a final un-chunked write follows, matching SytxLabs/DisplayPad.
    fn write_pixels(disp: &dyn UsbWriter, combined: &[u8]) {
        #[cfg(target_os = "linux")]
        {
            for chunk in combined.chunks(1024) {
                if disp.write(chunk).is_err() {
                    return;
                }
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            for chunk in combined.chunks(1024) {
                let mut cbuf = Vec::with_capacity(1 + chunk.len());
                cbuf.extend_from_slice(NULLBYTE);
                cbuf.extend_from_slice(chunk);
                if disp.write(&cbuf).is_err() {
                    return;
                }
            }
            let _ = disp.write(combined);
        }
    }

    /// Process button events from a 0x01 report
    fn process_buttons(
        report: &[u8],
        key_state: &Arc<Mutex<Vec<u8>>>,
        on_down: &Arc<Mutex<Option<KeyCallback>>>,
        on_up: &Arc<Mutex<Option<KeyCallback>>>,
    ) {
        let b42 = report[42];
        let b47 = report[47];
        let mut ks = key_state.lock().unwrap();
        let row1_bits = [0x02u8, 0x04u8, 0x08u8, 0x10u8, 0x20u8, 0x40u8];
        for (i, &mask) in row1_bits.iter().enumerate() {
            let pressed = (b42 & mask) != 0;
            let prev = ks[i] != 0;
            if pressed != prev {
                ks[i] = if pressed { 1 } else { 0 };
                if pressed {
                    if let Some(cb) = &*on_down.lock().unwrap() {
                        cb(i);
                    }
                } else if let Some(cb) = &*on_up.lock().unwrap() {
                    cb(i);
                }
            }
        }
        // key 6
        let pressed6 = (b42 & 0x80) != 0;
        let prev6 = ks[6] != 0;
        if pressed6 != prev6 {
            ks[6] = if pressed6 { 1 } else { 0 };
            if pressed6 {
                if let Some(cb) = &*on_down.lock().unwrap() {
                    cb(6);
                }
            } else if let Some(cb) = &*on_up.lock().unwrap() {
                cb(6);
            }
        }
        // keys 7..11
        for i in 0..5 {
            let mask = 1 << i;
            let pressed = (b47 & mask) != 0;
            let idx = 7 + i;
            let prev = ks[idx] != 0;
            if pressed != prev {
                ks[idx] = if pressed { 1 } else { 0 };
                if pressed {
                    if let Some(cb) = &*on_down.lock().unwrap() {
                        cb(idx);
                    }
                } else if let Some(cb) = &*on_up.lock().unwrap() {
                    cb(idx);
                }
            }
        }
    }

    /// Register a callback for key down events
    pub fn on_down<F>(&self, cb: F)
    where
        F: Fn(usize) + Send + 'static,
    {
        let mut guard = self.on_down.lock().unwrap();
        *guard = Some(Box::new(cb));
    }

    /// Register a callback for key up events
    pub fn on_up<F>(&self, cb: F)
    where
        F: Fn(usize) + Send + 'static,
    {
        let mut guard = self.on_up.lock().unwrap();
        *guard = Some(Box::new(cb));
    }

    /// Register a callback for errors
    pub fn on_error<F>(&self, cb: F)
    where
        F: Fn(String) + Send + 'static,
    {
        let mut guard = self.on_error.lock().unwrap();
        *guard = Some(Box::new(cb));
    }

    /// Check if the device is still connected
    pub fn is_connected(&self) -> bool {
        *self.connected.lock().unwrap()
    }

    /// Request an INIT/reset by setting the Initializing state. The actual INIT
    /// write is performed by the listener thread (the sole owner of device I/O),
    /// which avoids starving the write behind the listener's blocking read on the
    /// shared device handle.
    pub fn reset_device(&self) -> Result<(), String> {
        *self.transfer_state.lock().unwrap() = TransferState::Initializing;
        Ok(())
    }

    /// Returns true when the device has completed INIT and is ready for image transfers.
    pub fn is_ready(&self) -> bool {
        *self.transfer_state.lock().unwrap() == TransferState::Idle
    }

    /// Send INIT and wait up to `timeout` for the device to become ready (0x11 response).
    /// Returns true if ready, false if timed out.
    pub fn wait_for_ready(&self, timeout: Duration) -> bool {
        // Already ready (e.g. from INIT sent in new())
        if self.is_ready() {
            return true;
        }
        // Try sending INIT — ignore write errors (device may already be initializing)
        let _ = self.reset_device();
        let start = std::time::Instant::now();
        while start.elapsed() < timeout {
            if self.is_ready() {
                return true;
            }
            thread::sleep(Duration::from_millis(100));
        }
        false
    }

    /// Set a key to solid color (r,g,b)
    pub fn set_key_color(&self, key_index: usize, r: u8, g: u8, b: u8) -> Result<(), String> {
        if key_index >= NUM_KEYS {
            return Err("key_index out of range".into());
        }
        // create pixel buffer: PACKET_SIZE bytes where each pixel is BGR
        let pixel = [b, g, r];
        let mut pixels = vec![0u8; PACKET_SIZE];
        for i in 0..(PACKET_SIZE / 3) {
            let off = i * 3;
            pixels[off] = pixel[0];
            pixels[off + 1] = pixel[1];
            pixels[off + 2] = pixel[2];
        }
        self.send_pixel_data(key_index, pixels)
    }

    /// Set a key image from raw RGB buffer (ICON_SIZE * ICON_SIZE * 3 bytes)
    pub fn set_key_image(&self, key_index: usize, image_rgb: Vec<u8>) -> Result<(), String> {
        if key_index >= NUM_KEYS {
            return Err("key_index out of range".into());
        }
        if image_rgb.len() != ICON_SIZE * ICON_SIZE * 3 {
            return Err(format!(
                "Expected image buffer length {}, got {}",
                ICON_SIZE * ICON_SIZE * 3,
                image_rgb.len()
            ));
        }
        // Prepare PACKET_SIZE buffer and convert RGB -> BGR into image area
        let mut pixels = vec![0u8; PACKET_SIZE];
        for y in 0..ICON_SIZE {
            let row_offset = ICON_SIZE * 3 * y;
            for x in 0..ICON_SIZE {
                let img_off = row_offset + 3 * x;
                let r = image_rgb[img_off];
                let g = image_rgb[img_off + 1];
                let b = image_rgb[img_off + 2];
                pixels[img_off] = b;
                pixels[img_off + 1] = g;
                pixels[img_off + 2] = r;
            }
        }
        self.send_pixel_data(key_index, pixels)
    }

    /// Clear a key (set to zeros)
    pub fn clear_key(&self, key_index: usize) -> Result<(), String> {
        if key_index >= NUM_KEYS {
            return Err("key_index out of range".into());
        }
        let pixels = vec![0u8; PACKET_SIZE];
        self.send_pixel_data(key_index, pixels)
    }

    /// Clear all keys (set to black)
    pub fn clear_all_keys(&self) -> Result<(), String> {
        for i in 0..NUM_KEYS {
            self.clear_key(i)?;
        }
        Ok(())
    }

    /// Enqueue raw pixel data for a key. The listener thread's state machine will
    /// process the queue sequentially, waiting for device acknowledgment between transfers.
    /// If a request for the same key is already queued (not actively transferring), it is
    /// replaced with the new data to avoid stale transfers.
    pub fn send_pixel_data(&self, key_index: usize, pixels: Vec<u8>) -> Result<(), String> {
        if !self.is_connected() {
            return Err("Device disconnected".into());
        }

        let mut q = self.queue.lock().unwrap();

        // Replace any existing queued (not in-progress) request for the same key.
        // The front item may be actively transferring, so skip it.
        let mut replaced = false;
        for item in q.iter_mut().skip(1) {
            if item.key_index == key_index {
                item.pixels = pixels.clone();
                replaced = true;
                break;
            }
        }
        if !replaced {
            q.push_back(QueueRequest { key_index, pixels });
        }
        Ok(())
    }

    /// For testing: return current queue length
    #[cfg(test)]
    pub fn queue_len(&self) -> usize {
        self.queue.lock().unwrap().len()
    }

    /// Close devices (drop handles)
    pub fn close_device(&self) {
        // Dropping Arc<Mutex<HidDevice>> will close when last reference disappears. Explicitly try to send reset.
        let _ = self.reset_device();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_index_of() {
        assert_eq!(index_of(0, 0), 0);
        assert_eq!(index_of(0, 5), 5);
        assert_eq!(index_of(1, 0), 6);
        assert_eq!(index_of(1, 5), 11);
    }

    #[test]
    fn test_parse_row1_bits() {
        let mut report = vec![0u8; 64];
        report[0] = 0x01;
        report[42] = 0x04; // key 1 pressed
        let ev = parse_report(&report);
        assert_eq!(ev.len(), 12);
        assert!(ev[1].pressed);
        assert!(!ev[0].pressed);
    }

    #[test]
    fn test_queue_push_pop_order() {
        let q: Mutex<std::collections::VecDeque<QueueRequest>> =
            Mutex::new(std::collections::VecDeque::new());
        {
            let mut g = q.lock().unwrap();
            g.push_back(QueueRequest {
                key_index: 1,
                pixels: vec![1, 2, 3],
            });
            g.push_back(QueueRequest {
                key_index: 2,
                pixels: vec![4, 5, 6],
            });
            assert_eq!(g.len(), 2);
            let a = g.pop_front().expect("first pop");
            assert_eq!(a.key_index, 1);
            let b = g.pop_front().expect("second pop");
            assert_eq!(b.key_index, 2);
            assert!(g.is_empty());
        }
    }
}
