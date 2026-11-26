#![allow(non_snake_case)]

//! miui_lights - backlight bridge + AIDL lights HAL for MIUI/HyperOS ports.
//!
//! Modes:
//!   (default) --sync          follow debug.tracing.screen_brightness + screen_state (futex) -> sysfs
//!   --hal                     register android.hardware.light.ILights/default
//!   --probe                   query the *running* lights HAL and print lights
//!   --value <0.0..1.0>        write one brightness value, then exit

use std::ffi::{CStr, CString};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::raw::{c_char, c_int, c_uchar, c_void};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use rsbinder::Interface;

include!(concat!(env!("OUT_DIR"), "/light.rs"));

use crate::android::hardware::light::HwLight::HwLight;
use crate::android::hardware::light::HwLightState::HwLightState;
use crate::android::hardware::light::ILights::{BnLights, ILights};

const HAL_SERVICE_NAME: &str = "android.hardware.light.ILights/default";
const BACKLIGHT_NODE: &str = "/sys/class/leds/lcd-backlight/brightness";
const BACKLIGHT_MAX_HW: &str = "/sys/class/leds/lcd-backlight/max_hw_brightness";
const BACKLIGHT_MIN_HW: &str = "/sys/class/leds/lcd-backlight/min_hw_brightness";
const BACKLIGHT_MIN: &str = "/sys/class/leds/lcd-backlight/min_brightness";
const BACKLIGHT_MAX: &str = "/sys/class/leds/lcd-backlight/max_brightness";

const BRIGHTNESS_PROP: &str = "debug.tracing.screen_brightness";
const SCREEN_STATE_PROP: &str = "debug.tracing.screen_state";

const BRIGHTNESS_OFF: i64 = 0;
const STATE_CHANGE_LEVEL: f64 = 10.0;
const STATE_CHANGE_DELAY_MS: u64 = 30;

const SCREEN_STATE_OFF: i32 = 0;
const SCREEN_STATE_OFF_AOD: i32 = 1;
const SCREEN_STATE_ON: i32 = 2;
const SCREEN_STATE_DOZE: i32 = 3;
const SCREEN_STATE_DOZE_SUSPEND: i32 = 4;

type BinderResult<T> = rsbinder::status::Result<T>;

// ------------------------------------------------------------ Bionic prop FFI

#[cfg(target_os = "android")]
extern "C" {
    fn __system_property_get(name: *const c_uchar, value: *mut c_uchar) -> c_int;
    fn __system_property_find(name: *const c_char) -> *const c_void;
    fn __system_property_read_callback(
        pi: *const c_void,
        callback: extern "C" fn(
            cookie: *mut c_void,
            name: *const c_char,
            value: *const c_char,
            serial: u32,
        ),
        cookie: *mut c_void,
    );
    fn __system_property_wait(
        pi: *const c_void,
        old_serial: u32,
        new_serial_ptr: *mut u32,
        relative_timeout: *const c_void,
    ) -> bool;
}

#[cfg(not(target_os = "android"))]
unsafe fn __system_property_get(_name: *const c_uchar, _value: *mut c_uchar) -> c_int {
    0
}

#[cfg(not(target_os = "android"))]
unsafe fn __system_property_find(_name: *const c_char) -> *const c_void {
    std::ptr::null()
}

#[cfg(not(target_os = "android"))]
unsafe fn __system_property_read_callback(
    _pi: *const c_void,
    _callback: extern "C" fn(*mut c_void, *const c_char, *const c_char, u32),
    _cookie: *mut c_void,
) {
}

#[cfg(not(target_os = "android"))]
unsafe fn __system_property_wait(
    _pi: *const c_void,
    _old_serial: u32,
    _new_serial_ptr: *mut u32,
    _relative_timeout: *const c_void,
) -> bool {
    false
}

struct PropValue {
    value: String,
    serial: u32,
}

extern "C" fn read_callback_trampoline(
    cookie: *mut c_void,
    _name: *const c_char,
    value: *const c_char,
    serial: u32,
) {
    unsafe {
        let pv = &mut *(cookie as *mut PropValue);
        pv.serial = serial;
        if !value.is_null() {
            let s = CStr::from_ptr(value).to_string_lossy();
            pv.value = s.into_owned();
        }
    }
}

fn find_prop(name: &str) -> Option<*const c_void> {
    let c_name = CString::new(name).ok()?;
    let pi = unsafe { __system_property_find(c_name.as_ptr()) };
    if pi.is_null() {
        None
    } else {
        Some(pi)
    }
}

fn read_prop_pi(pi: *const c_void) -> PropValue {
    let mut pv = PropValue {
        value: String::new(),
        serial: 0,
    };
    unsafe {
        __system_property_read_callback(
            pi,
            read_callback_trampoline,
            &mut pv as *mut PropValue as *mut c_void,
        );
    }
    pv
}

fn wait_prop_pi(pi: *const c_void, old_serial: u32) -> Option<u32> {
    let mut new_serial = 0u32;
    let ok = unsafe { __system_property_wait(pi, old_serial, &mut new_serial, std::ptr::null()) };
    if ok {
        Some(new_serial)
    } else {
        None
    }
}

fn wait_any_prop(old_global_serial: u32) -> Option<u32> {
    let mut new_serial = 0u32;
    let ok = unsafe {
        __system_property_wait(
            std::ptr::null(),
            old_global_serial,
            &mut new_serial,
            std::ptr::null(),
        )
    };
    if ok {
        Some(new_serial)
    } else {
        None
    }
}

fn wait_for_prop(name: &str) -> *const c_void {
    let mut pi = find_prop(name);
    let mut global_serial = 0u32;
    while pi.is_none() {
        if let Some(s) = wait_any_prop(global_serial) {
            global_serial = s;
        } else {
            std::thread::sleep(Duration::from_millis(200));
        }
        pi = find_prop(name);
    }
    pi.unwrap()
}

fn get_prop(key: &str) -> Option<String> {
    const PROP_VALUE_MAX: usize = 92;
    let c_key = CString::new(key).ok()?;
    let mut buffer = vec![0u8; PROP_VALUE_MAX];
    let len = unsafe { __system_property_get(c_key.as_ptr() as *const c_uchar, buffer.as_mut_ptr()) };
    if len > 0 {
        let c_str = unsafe { CStr::from_ptr(buffer.as_ptr() as *const c_char) };
        Some(c_str.to_string_lossy().into_owned())
    } else {
        None
    }
}

// ---------------------------------------------------------------- panel I/O

#[derive(Clone, Copy)]
struct Panel {
    min: i64,
    max: i64,
    gamma: f64,
    /// gain applied to the framework brightness (MIUI caps normal brightness
    /// around 0.45-0.5 before HBM, so the raw float needs scaling)
    gain: f64,
    /// minimum fraction of the range that is still visible when > 0
    floor: f64,
}

fn compute_dynamic_gamma(_hw_max: i64) -> f64 {
    // Framework (DisplayPowerController) already applies its perceptual
    // curve to the float we receive. Adding a second gamma here makes the
    // lower half dim and the upper half jump, so pass through linearly.
    1.0
}

impl Panel {
    fn detect(gamma: Option<f64>, floor: f64, gain: f64) -> Self {
        let max = read_i64(BACKLIGHT_MAX_HW)
            .or_else(|| read_i64(BACKLIGHT_MAX))
            .unwrap_or(255);
        let min = read_i64(BACKLIGHT_MIN_HW)
            .or_else(|| read_i64(BACKLIGHT_MIN))
            .unwrap_or(1)
            .max(1);
        let gamma = gamma.unwrap_or_else(|| compute_dynamic_gamma(max));
        tracing::info!(
            "[Panel] node={} range={}..{} gamma={:.2} gain={} floor={}",
            BACKLIGHT_NODE,
            min,
            max,
            gamma,
            gain,
            floor
        );
        Self {
            min: min.min(max),
            max: max.max(min + 1),
            gamma,
            gain,
            floor,
        }
    }

    /// 0.0..1.0 (framework brightness) -> raw panel code.
    /// Values <= 0.0 map to 0 (backlight off) without applying floor.
    fn raw(&self, level: f64) -> i64 {
        if !level.is_finite() || level <= 0.0 {
            return BRIGHTNESS_OFF;
        }
        let v = (level * self.gain).clamp(0.0, 1.0);
        let curved = if (self.gamma - 1.0).abs() < f64::EPSILON {
            v
        } else {
            v.powf(self.gamma)
        };
        let curved = curved.max(self.floor);
        let span = (self.max - self.min) as f64;
        (self.min as f64 + span * curved).round() as i64
    }

    /// Intermediate level written before the target when waking the panel,
    /// so the kernel LED core never skips a write whose value matches cache.
    fn state_change_level(&self) -> i64 {
        let level = ((self.max.max(1) as f64) * (STATE_CHANGE_LEVEL / 511.0)).round() as i64;
        level.clamp(self.min, self.max)
    }

    fn write(&self, raw: i64) -> std::io::Result<()> {
        let clamped = if raw <= 0 {
            BRIGHTNESS_OFF
        } else {
            raw.clamp(self.min, self.max)
        };
        let mut f = OpenOptions::new().write(true).open(BACKLIGHT_NODE)?;
        // This MTK leds-mtk node rejects a trailing newline (EINVAL).
        write!(f, "{}", clamped)
    }
}

fn read_i64(path: &str) -> Option<i64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

struct BacklightWriter {
    panel: Panel,
    file: Option<File>,
    last_raw: i64,
}

impl BacklightWriter {
    fn new(panel: Panel) -> Self {
        let file = OpenOptions::new().write(true).open(BACKLIGHT_NODE).ok();
        if file.is_none() {
            tracing::warn!("[Sync] failed to open {} at startup, will retry on write", BACKLIGHT_NODE);
        }
        Self {
            panel,
            file,
            last_raw: i64::MIN,
        }
    }

    fn write_raw(&mut self, raw: i64, force: bool) {
        let clamped = if raw <= 0 {
            BRIGHTNESS_OFF
        } else {
            raw.clamp(self.panel.min, self.panel.max)
        };

        if !force && clamped == self.last_raw {
            return;
        }

        if self.file.is_none() {
            self.file = OpenOptions::new().write(true).open(BACKLIGHT_NODE).ok();
        }

        let res = match self.file.as_mut() {
            Some(f) => {
                use std::io::{Seek, SeekFrom};
                if let Err(e) = f.seek(SeekFrom::Start(0)) {
                    Err(e)
                } else {
                    write!(f, "{}", clamped)
                }
            }
            None => self.panel.write(clamped),
        };

        match res {
            Ok(()) => {
                tracing::info!(
                    "[Sync] write raw={} (prev={}, force={})",
                    clamped,
                    if self.last_raw == i64::MIN { -1 } else { self.last_raw },
                    force
                );
                self.last_raw = clamped;
            }
            Err(e) => {
                tracing::warn!("[Sync] write {} failed: {}", clamped, e);
                self.file = None;
            }
        }
    }
}

// --------------------------------------------------------------- HAL service

struct LightsHal {
    panel: Panel,
    /// last raw value written, so repeated identical requests are cheap
    last: AtomicU64,
}

impl Interface for LightsHal {}

impl LightsHal {
    fn apply_color(&self, color: i32) {
        // AOSP luminance formula for an ARGB "color" brightness request.
        let r = ((color >> 16) & 0xff) as i64;
        let g = ((color >> 8) & 0xff) as i64;
        let b = (color & 0xff) as i64;
        let level = (((77 * r) + (150 * g) + (29 * b)) >> 8) as f64 / 255.0;
        let raw = self.panel.raw(level);
        let prev = self.last.swap(raw as u64, Ordering::SeqCst) as i64;
        if prev == raw {
            return;
        }
        match self.panel.write(raw) {
            Ok(()) => tracing::debug!("[HAL] setLightState color=0x{:08x} -> raw={}", color, raw),
            Err(e) => tracing::warn!("[HAL] write {} failed: {}", BACKLIGHT_NODE, e),
        }
    }
}

impl ILights for LightsHal {
    // tx 1
    fn r#setLightState(&self, id: i32, state: &HwLightState) -> BinderResult<()> {
        tracing::info!(
            "[HAL] setLightState id={} color=0x{:08x} flashMode={} flashOn={} flashOff={} brightnessMode={}",
            id,
            state.color as u32,
            state.flashMode,
            state.flashOnMs,
            state.flashOffMs,
            state.brightnessMode
        );
        if id == 0 {
            self.apply_color(state.color);
        }
        Ok(())
    }

    // tx 2
    fn r#getLights(&self) -> BinderResult<Vec<HwLight>> {
        tracing::info!("[HAL] getLights()");
        Ok(vec![HwLight {
            id: 0,
            ordinal: 0,
            r#type: 0,
        }])
    }
}

fn run_hal(panel: Panel) -> Result<()> {
    rsbinder::ProcessState::init_default()
        .map_err(|e| anyhow!("ProcessState::init_default: {e}"))?;
    rsbinder::ProcessState::start_thread_pool();

    let hal = LightsHal {
        panel,
        last: AtomicU64::new(u64::MAX),
    };
    let binder = BnLights::new_binder(hal);
    rsbinder::hub::add_service(HAL_SERVICE_NAME, binder.as_binder())
        .with_context(|| format!("cannot register {HAL_SERVICE_NAME} (is vendor.light-default still running?)"))?;

    tracing::info!("[HAL] registered {}", HAL_SERVICE_NAME);
    rsbinder::ProcessState::join_thread_pool()?;
    Ok(())
}

// --------------------------------------------------------------------- probe

fn run_probe() -> Result<()> {
    rsbinder::ProcessState::init_default()
        .map_err(|e| anyhow!("ProcessState::init_default: {e}"))?;
    let binder = rsbinder::hub::get_service(HAL_SERVICE_NAME)
        .ok_or_else(|| anyhow!("{HAL_SERVICE_NAME} not found"))?;
    let hal: rsbinder::Strong<dyn ILights> = binder
        .into_interface()
        .map_err(|e| anyhow!("cast to ILights: {e}"))?;

    let lights = hal.r#getLights().map_err(|e| anyhow!("getLights: {e}"))?;
    tracing::info!("[Probe] {} lights", lights.len());
    for l in &lights {
        tracing::info!(
            "[Probe] light id={} ordinal={} type={}",
            l.id,
            l.ordinal,
            l.r#type
        );
    }

    for l in &lights {
        let state = HwLightState {
            color: 0xffc8_0000u32 as i32,
            flashMode: 0,
            flashOnMs: 0,
            flashOffMs: 0,
            brightnessMode: 0,
        };
        match hal.r#setLightState(l.id, &state) {
            Ok(()) => tracing::info!("[Probe] setLightState({}) ok", l.id),
            Err(e) => tracing::warn!("[Probe] setLightState({}) failed: {}", l.id, e),
        }
    }
    tracing::info!(
        "[Probe] sysfs now = {:?}",
        std::fs::read_to_string(BACKLIGHT_NODE).map(|s| s.trim().to_string())
    );
    Ok(())
}

// ---------------------------------------------------------------------- sync

struct Shared {
    writer: BacklightWriter,
    screen_state: i32,
    last_level: f64,
    has_brightness: bool,
}

struct SyncState {
    panel: Panel,
    /// One lock covers state + writer, so a brightness event racing a screen
    /// transition can never write a stale target to the panel node.
    inner: Mutex<Shared>,
}

impl SyncState {
    fn new(panel: Panel, initial_state: i32) -> Self {
        Self {
            panel,
            inner: Mutex::new(Shared {
                writer: BacklightWriter::new(panel),
                screen_state: initial_state,
                last_level: 0.0,
                has_brightness: false,
            }),
        }
    }

    fn off_level(&self, state: i32) -> i64 {
        match state {
            SCREEN_STATE_OFF_AOD | SCREEN_STATE_DOZE | SCREEN_STATE_DOZE_SUSPEND => self.panel.min,
            _ => BRIGHTNESS_OFF,
        }
    }

    fn apply_brightness(&self, level: f64) {
        let level = if level.is_finite() {
            level.clamp(0.0, 1.0)
        } else {
            return;
        };
        let mut inner = match self.inner.lock() {
            Ok(v) => v,
            Err(_) => return,
        };

        // Only remember a lit level: the framework sends 0.0 on screen off,
        // and keeping that would leave nothing to restore on wake.
        if level > 0.0 {
            inner.last_level = level;
            inner.has_brightness = true;
        }

        if inner.screen_state != SCREEN_STATE_ON {
            let raw = self.off_level(inner.screen_state);
            inner.writer.write_raw(raw, false);
            return;
        }

        let raw = self.panel.raw(level);
        inner.writer.write_raw(raw, false);
    }

    fn apply_screen_state(&self, state: i32) {
        let mut inner = match self.inner.lock() {
            Ok(v) => v,
            Err(_) => return,
        };
        let prev = inner.screen_state;
        inner.screen_state = state;
        if prev == state {
            return;
        }

        if state == SCREEN_STATE_ON {
            tracing::info!("[Sync] screen_state {} -> ON, restoring brightness", prev);
            if inner.has_brightness && inner.last_level > 0.0 {
                let target = self.panel.raw(inner.last_level);
                let intermediate = self.panel.state_change_level();
                inner.writer.write_raw(intermediate, true);
                std::thread::sleep(Duration::from_millis(STATE_CHANGE_DELAY_MS));
                inner.writer.write_raw(target, true);
            }
            return;
        }

        tracing::info!("[Sync] screen_state {} -> {}, dropping brightness", prev, state);
        inner.writer.write_raw(self.off_level(state), false);
    }
}

fn parse_prop_brightness(s: &str) -> Option<f64> {
    let v: f64 = s.trim().parse().ok()?;
    if v.is_finite() {
        Some(v.clamp(0.0, 1.0))
    } else {
        None
    }
}

fn run_sync(panel: Panel) -> Result<()> {
    let initial_state = get_prop(SCREEN_STATE_PROP)
        .and_then(|s| s.trim().parse::<i32>().ok())
        .unwrap_or(SCREEN_STATE_ON);

    tracing::info!(
        "[Sync] started (futex listener on {} + {}, initial screen_state={})",
        BRIGHTNESS_PROP,
        SCREEN_STATE_PROP,
        initial_state
    );

    // Start from an unknown state (-1) so the real initial state is applied,
    // e.g. the daemon starting while the screen is already off.
    let state = Arc::new(SyncState::new(panel, -1));
    state.apply_screen_state(initial_state);

    // Thread 1: futex listener for debug.tracing.screen_state
    let state_screen = Arc::clone(&state);
    std::thread::spawn(move || {
        let pi = wait_for_prop(SCREEN_STATE_PROP);
        let initial = read_prop_pi(pi);
        let mut serial = initial.serial;
        if let Ok(s) = initial.value.trim().parse::<i32>() {
            state_screen.apply_screen_state(s);
        }

        loop {
            let new_serial = match wait_prop_pi(pi, serial) {
                Some(s) => s,
                None => continue,
            };
            serial = new_serial;
            let pv = read_prop_pi(pi);
            if pv.serial != 0 {
                serial = pv.serial;
            }
            if let Ok(s) = pv.value.trim().parse::<i32>() {
                state_screen.apply_screen_state(s);
            }
        }
    });

    // Main thread: futex listener for debug.tracing.screen_brightness
    let pi = wait_for_prop(BRIGHTNESS_PROP);
    let initial = read_prop_pi(pi);
    let mut serial = initial.serial;
    if let Some(level) = parse_prop_brightness(&initial.value) {
        state.apply_brightness(level);
    }

    loop {
        let new_serial = match wait_prop_pi(pi, serial) {
            Some(s) => s,
            None => continue,
        };
        serial = new_serial;
        let pv = read_prop_pi(pi);
        if pv.serial != 0 {
            serial = pv.serial;
        }
        if let Some(level) = parse_prop_brightness(&pv.value) {
            state.apply_brightness(level);
        }
    }
}

// ---------------------------------------------------------------------- main

fn usage() -> ! {
    eprintln!(
        "miui_lights {}\n\
         usage:\n\
           miui_lights [--sync] [--gamma G] [--gain K] [--floor F]\n\
           miui_lights --hal [--gamma G] [--gain K] [--floor F]\n\
           miui_lights --probe\n\
           miui_lights --value 0..1 [--gamma G] [--gain K] [--floor F]",
        env!("CARGO_PKG_VERSION")
    );
    std::process::exit(2)
}

fn main() {
    let _ = tracing_log::LogTracer::init();
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_ansi(false)
        .try_init();

    let mut mode = "sync";
    let mut gamma: Option<f64> = std::env::var("MIUI_LIGHTS_GAMMA")
        .ok()
        .and_then(|v| v.parse().ok());
    let mut floor: f64 = std::env::var("MIUI_LIGHTS_FLOOR")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.01);
    let mut gain: f64 = std::env::var("MIUI_LIGHTS_GAIN")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2.0);
    let mut value: Option<f64> = None;

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--sync" => mode = "sync",
            "--hal" => mode = "hal",
            "--probe" => mode = "probe",
            "--value" => {
                i += 1;
                value = args.get(i).and_then(|v| v.parse().ok());
                mode = "value";
            }
            "--interval" => {
                // Kept as a no-op for CLI backwards compatibility with init rc files.
                i += 1;
            }
            "--gamma" => {
                i += 1;
                gamma = args.get(i).and_then(|v| v.parse().ok());
            }
            "--floor" => {
                i += 1;
                floor = args.get(i).and_then(|v| v.parse().ok()).unwrap_or(0.01);
            }
            "--gain" => {
                i += 1;
                gain = args.get(i).and_then(|v| v.parse().ok()).unwrap_or(2.0);
            }
            "-h" | "--help" => usage(),
            other => {
                eprintln!("unknown argument: {other}");
                usage();
            }
        }
        i += 1;
    }

    if mode == "probe" {
        if let Err(e) = run_probe() {
            tracing::error!("probe failed: {e:#}");
            std::process::exit(1);
        }
        return;
    }

    let panel = Panel::detect(gamma, floor, gain);

    match mode {
        "hal" => {
            if let Err(e) = run_hal(panel) {
                tracing::error!("hal mode failed: {e:#}");
                std::process::exit(1);
            }
        }
        "value" => {
            let v = value.unwrap_or(0.5);
            let raw = panel.raw(v);
            match panel.write(raw) {
                Ok(()) => tracing::info!("wrote {} (level {:.3})", raw, v),
                Err(e) => {
                    tracing::error!("write failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        _ => {
            if let Err(e) = run_sync(panel) {
                tracing::error!("sync failed: {e:#}");
                std::process::exit(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_and_negative_never_hit_floor() {
        let panel = Panel {
            min: 1,
            max: 5119,
            gamma: 1.56,
            gain: 2.0,
            floor: 0.01,
        };
        assert_eq!(panel.raw(0.0), 0);
        assert_eq!(panel.raw(-1.0), 0);
        assert_eq!(panel.raw(f64::NAN), 0);
        assert!(panel.raw(0.001) >= 52);
    }

    #[test]
    fn gamma_is_linear_regardless_of_depth() {
        assert_eq!(compute_dynamic_gamma(511), 1.0);
        assert_eq!(compute_dynamic_gamma(4096), 1.0);
        assert_eq!(compute_dynamic_gamma(5119), 1.0);
    }

    #[test]
    fn linear_curve_spreads_brightness_evenly() {
        let panel = Panel {
            min: 1,
            max: 5119,
            gamma: 1.0,
            gain: 2.0,
            floor: 0.01,
        };
        // Framework float tops out around 0.5 before HBM, so gain 2x maps
        // slider 0..100% to raw 0..max. Each quarter of the slider should
        // consume roughly a quarter of the raw range.
        let q1 = panel.raw(0.125);
        let q2 = panel.raw(0.25);
        let q3 = panel.raw(0.375);
        let q4 = panel.raw(0.5);
        assert!((q1 - 1280).abs() < 5, "q1={}", q1);
        assert!((q2 - 2560).abs() < 5, "q2={}", q2);
        assert!((q3 - 3840).abs() < 5, "q3={}", q3);
        assert_eq!(q4, 5119);
    }

    #[test]
    fn state_change_level_scales_with_panel() {
        let p511 = Panel {
            min: 1,
            max: 511,
            gamma: 1.0,
            gain: 2.0,
            floor: 0.01,
        };
        assert_eq!(p511.state_change_level(), 10);

        let p5119 = Panel {
            min: 1,
            max: 5119,
            gamma: 1.56,
            gain: 2.0,
            floor: 0.01,
        };
        assert_eq!(p5119.state_change_level(), 100);
    }

    #[test]
    fn screen_off_ignores_brightness_and_on_restores() {
        let panel = Panel {
            min: 1,
            max: 5119,
            gamma: 1.0,
            gain: 2.0,
            floor: 0.01,
        };
        let sync = SyncState::new(panel, SCREEN_STATE_ON);
        sync.apply_brightness(0.25);
        assert_eq!(sync.inner.lock().unwrap().last_level, 0.25);

        sync.apply_screen_state(SCREEN_STATE_OFF);
        assert_eq!(sync.inner.lock().unwrap().screen_state, SCREEN_STATE_OFF);

        // Screen-off 0.0 must not overwrite the level restored on wake.
        sync.apply_brightness(0.0);
        assert_eq!(sync.inner.lock().unwrap().last_level, 0.25);

        // A lit update while off is remembered but does not wake the panel.
        sync.apply_brightness(0.3);
        assert_eq!(sync.inner.lock().unwrap().screen_state, SCREEN_STATE_OFF);
        assert_eq!(sync.inner.lock().unwrap().last_level, 0.3);

        sync.apply_screen_state(SCREEN_STATE_ON);
        assert_eq!(sync.inner.lock().unwrap().screen_state, SCREEN_STATE_ON);
    }

    #[test]
    fn aod_always_uses_lowest_hw_level() {
        let panel = Panel {
            min: 7,
            max: 5119,
            gamma: 1.0,
            gain: 2.0,
            floor: 0.01,
        };
        let sync = SyncState::new(panel, SCREEN_STATE_ON);
        sync.apply_brightness(0.25);
        assert_eq!(sync.inner.lock().unwrap().last_level, 0.25);

        for state in [
            SCREEN_STATE_OFF_AOD,
            SCREEN_STATE_DOZE,
            SCREEN_STATE_DOZE_SUSPEND,
        ] {
            sync.apply_screen_state(state);
            assert_eq!(sync.inner.lock().unwrap().screen_state, state);
            assert_eq!(sync.off_level(state), panel.min);
        }

        // Brightness arriving while AOD/DOZE is remembered for wake-up, but
        // the panel level itself must stay at the minimum.
        sync.apply_brightness(0.5);
        assert_eq!(sync.inner.lock().unwrap().screen_state, SCREEN_STATE_DOZE_SUSPEND);
        assert_eq!(sync.inner.lock().unwrap().last_level, 0.5);
        assert!(sync.off_level(sync.inner.lock().unwrap().screen_state) <= panel.min);

        sync.apply_screen_state(SCREEN_STATE_OFF);
        assert_eq!(sync.inner.lock().unwrap().screen_state, SCREEN_STATE_OFF);
        assert_eq!(sync.off_level(SCREEN_STATE_OFF), BRIGHTNESS_OFF);
    }
}
