use futures_lite::StreamExt;
use simplelog::{Config, LevelFilter, SimpleLogger};
use std::env;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use openaction::device_plugin;
use openaction::global_events::DeviceDidDisconnectEvent;
use openaction::global_events::SetBrightnessEvent;
use openaction::global_events::SetImageEvent;
use openaction::global_events::SystemDidWakeUpEvent;
use openaction::global_events::{self};

static DISPLAYPAD: Mutex<Option<Arc<driver::DisplayPad>>> = Mutex::new(None);
/// Guards against concurrent connect attempts
static CONNECTING: AtomicBool = AtomicBool::new(false);

static DEVICE_PATH: Mutex<Option<String>> = Mutex::new(None);

const DEVICE_NAMESPACE: &str = "50";
const DEVICE_LOCAL_ID: &str = "displaypad-0";

fn device_id() -> String {
    format!("{}-{}", DEVICE_NAMESPACE, DEVICE_LOCAL_ID)
}

fn list_hid() {
    match hidapi::HidApi::new() {
        Ok(api) => {
            println!("Enumerating HID devices:");
            for dev in api.device_list() {
                println!(
                    "vid=0x{:04x} pid=0x{:04x} interface={} usage_page=0x{:04x} usage=0x{:04x} path={} product={:?} manufacturer={:?}",
                    dev.vendor_id(),
                    dev.product_id(),
                    dev.interface_number(),
                    dev.usage_page(),
                    dev.usage(),
                    dev.path().to_string_lossy(),
                    dev.product_string(),
                    dev.manufacturer_string()
                );
            }
        }
        Err(e) => eprintln!("hidapi init error: {}", e),
    }
}

/// Create a new DisplayPad instance, wire up key callbacks, and store it globally.
fn connect_displaypad(rt_handle: tokio::runtime::Handle) -> Result<(), String> {
    let dp_inst = driver::DisplayPad::new()?;
    let dp = Arc::new(dp_inst);

    let did = device_id();
    let rt1 = rt_handle.clone();
    dp.on_down(move |idx| {
        let did = did.clone();
        let rt = rt1.clone();
        rt.spawn(async move {
            if let Err(err) = device_plugin::key_down(did.clone(), idx as u8).await {
                log::error!("key_down send error: {}", err);
            }
        });
    });

    let did2 = device_id();
    let rt2 = rt_handle.clone();
    dp.on_up(move |idx| {
        let did = did2.clone();
        let rt = rt2.clone();
        rt.spawn(async move {
            if let Err(err) = device_plugin::key_up(did.clone(), idx as u8).await {
                log::error!("key_up send error: {}", err);
            }
        });
    });

    // On HID error (e.g. USB unplug via write failure): clear the dead instance
    // and unregister from OpenDeck. The watcher will handle reconnection.
    let rt3 = rt_handle;
    dp.on_error(move |err| {
        log::error!("DisplayPad HID error: {}", err);
        *DISPLAYPAD.lock().unwrap() = None;
        let rt = rt3.clone();
        rt.spawn(async move {
            if let Err(e) = device_plugin::unregister_device(device_id()).await {
                log::error!("unregister_device error: {}", e);
            }
        });
    });

    *DISPLAYPAD.lock().unwrap() = Some(dp);
    Ok(())
}

/// Connect to DisplayPad, wait for INIT handshake, and register with OpenDeck.
/// Retries connection attempts since the device's HID interfaces take time to become ready after USB plug-in.
async fn handle_device_connected() {
    // Guard against duplicate connect attempts (multiple HID interfaces trigger multiple events)
    if CONNECTING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }

    // Already connected?
    if DISPLAYPAD.lock().unwrap().is_some() {
        CONNECTING.store(false, Ordering::SeqCst);
        return;
    }

    log::info!("DisplayPad USB detected, attempting to connect...");

    let rt_handle = tokio::runtime::Handle::current();
    let result = tokio::task::spawn_blocking(move || {
        // Phase 1: Retry opening HID interfaces until they're available
        let max_open_attempts = 15;
        for attempt in 1..=max_open_attempts {
            match connect_displaypad(rt_handle.clone()) {
                Ok(()) => {
                    log::info!("HID interfaces opened on attempt {}", attempt);
                    break;
                }
                Err(e) => {
                    if attempt == max_open_attempts {
                        return Err(format!(
                            "Could not open HID interfaces after {} attempts: {}",
                            max_open_attempts, e
                        ));
                    }
                    log::debug!("Open attempt {}/{}: {}", attempt, max_open_attempts, e);
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }
            }
        }

        // Phase 2: Retry INIT handshake on the connected instance
        let dp = DISPLAYPAD.lock().unwrap().clone();
        if let Some(dp) = dp {
            let max_init_attempts = 5;
            for attempt in 1..=max_init_attempts {
                let timeout = std::time::Duration::from_secs(3);
                if dp.wait_for_ready(timeout) {
                    log::info!("INIT handshake succeeded on attempt {}", attempt);
                    return Ok(());
                }
                if attempt < max_init_attempts {
                    log::debug!(
                        "INIT attempt {}/{} timed out, retrying...",
                        attempt,
                        max_init_attempts
                    );
                }
            }
            // All INIT attempts failed — tear down
            *DISPLAYPAD.lock().unwrap() = None;
            return Err("Device did not become ready (no INIT response)".to_string());
        }

        Err("No DisplayPad instance after connect".to_string())
    })
    .await
    .unwrap_or(Err("spawn_blocking failed".into()));

    match result {
        Ok(()) => {
            if let Err(e) = device_plugin::register_device(
                device_id(),
                "DisplayPad".to_string(),
                2u8,
                6u8,
                0u8,
                0u8,
            )
            .await
            {
                log::error!("register_device error: {}", e);
            } else {
                log::info!("DisplayPad connected and registered with OpenDeck");
            }
        }
        Err(e) => {
            log::error!("Failed to initialize DisplayPad: {}", e);
        }
    }

    CONNECTING.store(false, Ordering::SeqCst);
}

/// Clear instance and unregister from OpenDeck.
async fn handle_device_disconnected() {
    log::info!("DisplayPad USB disconnected");
    *DISPLAYPAD.lock().unwrap() = None;
    if let Err(e) = device_plugin::unregister_device(device_id()).await {
        log::error!("unregister_device error: {}", e);
    }
}

/// Check if a device matches our DisplayPad VID/PID.
fn is_our_device(dev: &async_hid::Device) -> bool {
    dev.vendor_id == driver::VENDOR_ID && driver::PRODUCT_IDS.contains(&dev.product_id)
}

/// Event-driven USB device watcher using async-hid.
/// Watches for DisplayPad connect/disconnect events — no polling.
async fn device_watcher_task() {
    let backend = async_hid::HidBackend::default();

    // Check for already-connected device
    match backend.enumerate().await {
        Ok(mut stream) => {
            let mut found = false;
            while let Some(dev) = stream.next().await {
                if is_our_device(&dev) {
                    found = true;
                    break;
                }
            }
            if found {
                handle_device_connected().await;
            } else {
                log::info!("DisplayPad not connected, watching for USB events...");
            }
        }
        Err(e) => {
            log::error!("HID enumeration error: {}", e);
        }
    }

    // Watch for hotplug events (event-driven, no polling)
    let mut events = match backend.watch() {
        Ok(stream) => stream,
        Err(e) => {
            log::error!("Failed to start USB watcher: {}", e);
            return;
        }
    };

    log::info!("USB device watcher started");

    while let Some(event) = events.next().await {
        match event {
            async_hid::DeviceEvent::Connected(id) => {
                // Resolve the device ID to check VID/PID
                if DISPLAYPAD.lock().unwrap().is_some() {
                    // Already connected — ignore additional interface events
                    continue;
                }
                match backend.query_devices(&id).await {
                    Ok(devices) => {
                        let is_ours = devices.into_iter().any(|dev| is_our_device(&dev));
                        if is_ours {
                            handle_device_connected().await;
                        }
                    }
                    Err(e) => {
                        log::warn!("Failed to query connected device: {}", e);
                    }
                }
            }
            async_hid::DeviceEvent::Disconnected(_id) => {
                // If we have an active instance, check if our device is still present
                if DISPLAYPAD.lock().unwrap().is_some() {
                    // On Linux, rusb claiming interfaces causes spurious disconnect events.
                    // Check if the driver itself reports the device as still connected.
                    let dp_connected = DISPLAYPAD
                        .lock()
                        .unwrap()
                        .as_ref()
                        .is_some_and(|dp| dp.is_connected());

                    if !dp_connected {
                        handle_device_disconnected().await;
                    }
                }
            }
        }
    }

    log::info!("USB device watcher ended");
}

struct MyGlobalHandler;

#[async_trait::async_trait]
impl global_events::GlobalEventHandler for MyGlobalHandler {
    async fn plugin_ready(&self) -> openaction::OpenActionResult<()> {
        log::info!("openaction plugin_ready called");

        // Spawn the event-driven device watcher (runs in background)
        tokio::spawn(device_watcher_task());

        Ok(())
    }

    async fn device_plugin_set_image(
        &self,
        event: SetImageEvent,
    ) -> openaction::OpenActionResult<()> {
        if event.device != device_id() {
            return Ok(());
        }

        if let Some(ctrl) = &event.controller {
            if ctrl.to_lowercase().contains("encoder") {
                return Ok(());
            }
        }

        let position = match event.position {
            Some(p) => p as usize,
            None => return Ok(()),
        };

        if let Some(img) = event.image {
            match data_url::DataUrl::process(&img) {
                Ok(url) => match url.decode_to_vec() {
                    Ok((body, _frag)) => {
                        let dp = DISPLAYPAD.lock().unwrap().clone();
                        if let Some(dp) = dp {
                            tokio::task::spawn_blocking(move || {
                                match image::load_from_memory(&body) {
                                    Ok(img) => {
                                        let resized = img.resize_exact(
                                            driver::ICON_SIZE as u32,
                                            driver::ICON_SIZE as u32,
                                            image::imageops::FilterType::Triangle,
                                        );
                                        let rgb = resized.to_rgb8().into_raw();
                                        if let Err(e) = dp.set_key_image(position, rgb) {
                                            log::error!(
                                                "Failed to send image for key {}: {}",
                                                position,
                                                e
                                            );
                                        }
                                    }
                                    Err(e) => {
                                        log::error!("Failed to decode image: {}", e);
                                    }
                                }
                            });
                        }
                    }
                    Err(e) => log::error!("Failed to decode data URL image: {}", e),
                },
                Err(e) => log::error!("Invalid data URL for image: {}", e),
            }
        } else {
            let dp = DISPLAYPAD.lock().unwrap().clone();
            if let Some(dp) = dp {
                tokio::task::spawn_blocking(move || {
                    if let Err(e) = dp.clear_key(position) {
                        log::error!("Failed to clear key: {}", e);
                    }
                });
            }
        }

        Ok(())
    }

    async fn device_plugin_set_brightness(
        &self,
        event: SetBrightnessEvent,
    ) -> openaction::OpenActionResult<()> {
        if event.device != device_id() {
            return Ok(());
        }
        log::info!(
            "Brightness set to {} (not supported by hardware)",
            event.brightness
        );
        Ok(())
    }

    async fn device_did_disconnect(
        &self,
        event: DeviceDidDisconnectEvent,
    ) -> openaction::OpenActionResult<()> {
        if event.device != device_id() {
            return Ok(());
        }
        log::info!("OpenAction device disconnected: {}", event.device);
        Ok(())
    }

    async fn system_did_wake_up(
        &self,
        _event: SystemDidWakeUpEvent,
    ) -> openaction::OpenActionResult<()> {
        log::info!("System woke up, re-initializing DisplayPad");
        let dp = DISPLAYPAD.lock().unwrap().clone();
        if let Some(dp) = dp {
            if let Err(e) = dp.reset_device() {
                log::error!("Failed to re-init DisplayPad after wake: {}", e);
            }
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    SimpleLogger::init(LevelFilter::Info, Config::default()).ok();

    println!("OpenDeck DisplayPad runner");

    let args: Vec<String> = env::args().collect();
    if args.iter().any(|a| a == "--list-hid") {
        list_hid();
        return Ok(());
    }

    if let Some(idx) = args.iter().position(|a| a == "--device-path") {
        if args.len() > idx + 1 {
            let p = args[idx + 1].clone();
            println!("Using device path from CLI: {}", p);
            *DEVICE_PATH.lock().unwrap() = Some(p);
        } else {
            eprintln!("--device-path requires a path argument");
            return Ok(());
        }
    }

    // Windows-only: Spawn a watchdog thread. OpenDeck on Windows doesn't reliably
    // signal child processes on shutdown — instead stdin closes. Detect that and force-exit.
    // On Linux, ctrl_c/SIGTERM works properly so this isn't needed.
    #[cfg(target_os = "windows")]
    std::thread::spawn(|| {
        use std::io::Read;
        let mut buf = [0u8; 1];
        let _ = std::io::stdin().read(&mut buf);
        log::info!("Stdin closed, shutting down");
        std::process::exit(0);
    });

    global_events::set_global_event_handler(&MyGlobalHandler {});
    tokio::select! {
        result = openaction::run(args) => {
            if let Err(e) = result {
                eprintln!("openaction run error: {}", e);
            }
        }
        _ = tokio::signal::ctrl_c() => {
            log::info!("Received shutdown signal");
        }
    }

    // Force exit — kills the watcher task immediately
    std::process::exit(0);
}
