#![allow(non_snake_case)]

//! miui_lights - backlight bridge + AIDL lights HAL for MIUI/HyperOS ports.
//!
//! Modes:
//!   (default) --sync          follow settings screen_brightness_float -> sysfs
//!   --hal                     register android.hardware.light.ILights/default
//!   --probe                   query the *running* lights HAL and print lights
//!   --value <0.0..1.0>        write one brightness value, then exit
//!
//! Why a bridge: on ported MTK vendor trees the framework picks
//! `BacklightAdapter [useSurfaceControl=false]` but never actually calls the
//! LightImpl (mColor stays 0), and SurfaceFlinger rejects display brightness
//! (`displayBrightness=nullopt`). The panel node is writable though, so this
//! service simply mirrors the framework's brightness setting into
//! /sys/class/leds/lcd-backlight/brightness.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc::{channel, RecvTimeoutError};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

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
const BACKLIGHT_MAX: &str = "/sys/class/leds/lcd-backlight/max_brightness";
const SETTINGS_BIN: &str = "/system/bin/settings";

type BinderResult<T> = rsbinder::status::Result<T>;

// ---------------------------------------------------------------- panel I/O

#[derive(Clone, Copy)]
struct Panel {
    min: i64,
    max: i64,
    gamma: f64,
    /// gain applied to the framework brightness (MIUI caps normal brightness
    /// around 0.45-0.5 before HBM, so the raw float needs scaling)
    gain: f64,
    /// minimum fraction of the range that is still visible on AMOLED
    floor: f64,
}

impl Panel {
    fn detect(gamma: f64, floor: f64, gain: f64) -> Self {
        let max = read_i64(BACKLIGHT_MAX_HW)
            .or_else(|| read_i64(BACKLIGHT_MAX))
            .unwrap_or(255);
        let min = read_i64(BACKLIGHT_MIN_HW).unwrap_or(1).max(1);
        tracing::info!(
            "[Panel] node={} range={}..{} gamma={} gain={} floor={}",
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
    fn raw(&self, level: f64) -> i64 {
        let v = (level * self.gain).clamp(0.0, 1.0);
        let curved = if (self.gamma - 1.0).abs() < f64::EPSILON {
            v
        } else {
            v.powf(self.gamma)
        };
        // floor is a fraction of the raw range, so the panel never goes
        // pitch black even at brightness 0.
        let curved = curved.max(self.floor);
        let span = (self.max - self.min) as f64;
        (self.min as f64 + span * curved).round() as i64
    }

    fn write(&self, raw: i64) -> std::io::Result<()> {
        let raw = raw.clamp(self.min, self.max);
        let mut f = std::fs::OpenOptions::new().write(true).open(BACKLIGHT_NODE)?;
        write!(f, "{}", raw)
    }
}

fn read_i64(path: &str) -> Option<i64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

// ------------------------------------------------------------------ settings

fn settings_get(key: &str) -> Option<String> {
    let out = Command::new(SETTINGS_BIN)
        .args(["get", "system", key])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() || s == "null" {
        None
    } else {
        Some(s)
    }
}

/// Framework brightness as 0.0..1.0.
///
/// `screen_brightness_float` is authoritative; MIUI keeps it in sync even in
/// auto-brightness mode. `screen_brightness` is only a fallback for older
/// builds (0..255 or 0..2047).
fn cmd_display_brightness() -> Option<f64> {
    // This is the value the ROM brightness slider actually modifies
    // (DisplayManager.setBrightness -> DisplayPowerController). It does NOT
    // update Settings.screen_brightness_float on this MIUI build.
    let out = Command::new("/system/bin/cmd")
        .args(["display", "get-brightness"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().last()?.trim();
    let v: f64 = line.parse().ok()?;
    if v.is_finite() {
        Some(v.clamp(0.0, 1.0))
    } else {
        None
    }
}

fn framework_brightness() -> Option<f64> {
    if let Some(v) = cmd_display_brightness() {
        return Some(v);
    }
    if let Some(v) = settings_get("screen_brightness_float") {
        if let Ok(f) = v.parse::<f64>() {
            if f.is_finite() {
                return Some(f.clamp(0.0, 1.0));
            }
        }
    }
    let v = settings_get("screen_brightness")?.parse::<f64>().ok()?;
    let div = if v > 255.0 { 2047.0 } else { 255.0 };
    Some((v / div).clamp(0.0, 1.0))
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

fn run_hal(panel: Panel, gamma: f64) -> Result<()> {
    let _ = gamma;
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

fn parse_float_after<'a>(line: &'a str, marker: &str) -> Option<f64> {
    let idx = line.find(marker)?;
    let rest = &line[idx + marker.len()..];
    let start = rest.find(|c: char| c.is_ascii_digit() || c == '.' || c == '-')?;
    let rest = &rest[start..];
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == 'E' || c == 'e' || c == '+'))
        .unwrap_or(rest.len());
    let v: f64 = rest[..end].parse().ok()?;
    if v.is_finite() {
        Some(v.clamp(0.0, 1.0))
    } else {
        None
    }
}

fn parse_brightness_event(line: &str) -> Option<f64> {
    // Control Center drag: DisplayManagerService.setTemporaryBrightness
    // (fires on every slider movement, ~10ms granularity)
    if line.contains("setTemporaryBrightness") {
        return parse_float_after(line, "brightness:");
    }
    // DPC impl: targetBrightness on every change, also during temporary drag
    if line.contains("updateBrightnessChangeStatus") {
        return parse_float_after(line, "targetBrightness:");
    }
    // Committed value (slider release, settings set, auto brightness, dim)
    parse_float_after(line, "BrightnessEvent: brt=")
}

/// Stream the framework's applied brightness from logcat.
///
/// `DisplayManager`/`cmd display get-brightness` on this MIUI build only
/// reports the *committed* value (updates on slider release). The
/// DisplayPowerController keeps logging every applied value while the slider
/// is dragged, so we follow those lines instead.
fn spawn_logcat_brightness(tx: std::sync::mpsc::Sender<f64>) {
    std::thread::spawn(move || loop {
        let child = Command::new("/system/bin/logcat")
            .args([
                "-T",
                "1",
                "-v",
                "brief",
                "DisplayManagerService:V",
                "DisplayPowerControllerImpl:V",
                "DisplayPowerController[0]:I",
                "*:S",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();

        match child {
            Ok(mut child) => {
                if let Some(out) = child.stdout.take() {
                    for line in BufReader::new(out).lines().map_while(Result::ok) {
                        if let Some(v) = parse_brightness_event(&line) {
                            if tx.send(v).is_err() {
                                return;
                            }
                        }
                    }
                }
                let _ = child.wait();
                tracing::warn!("[Sync] logcat stream ended, restarting");
            }
            Err(e) => tracing::warn!("[Sync] cannot start logcat: {}", e),
        }
        std::thread::sleep(Duration::from_secs(1));
    });
}

fn run_sync(panel: Panel, interval_ms: u64) -> Result<()> {
    tracing::info!("[Sync] started (idle tick={}ms, logcat realtime)", interval_ms);
    let (tx, rx) = channel::<f64>();
    spawn_logcat_brightness(tx);

    let mut last_raw: i64 = i64::MIN;
    let mut was_auto = -1;
    // logcat is the realtime source; the cmd fallback is only a safety net
    let fallback_every = Duration::from_secs(5);
    let mut last_fallback = Instant::now() - fallback_every;

    let mut apply = |level: f64, last_raw: &mut i64| {
        let raw = panel.raw(level);
        if raw == *last_raw {
            return;
        }
        // What is on the node right now? Someone else may have changed it.
        let current = read_i64(BACKLIGHT_NODE);
        if current == Some(raw) {
            *last_raw = raw;
            return;
        }
        match panel.write(raw) {
            Ok(()) => {
                tracing::info!(
                    "[Sync] brightness {:.4} -> raw {} (range {}..{})",
                    level,
                    raw,
                    panel.min,
                    panel.max
                );
                *last_raw = raw;
            }
            Err(e) => tracing::warn!("[Sync] write failed: {}", e),
        }
    };

    loop {
        match rx.recv_timeout(Duration::from_millis(interval_ms)) {
            Ok(level) => apply(level, &mut last_raw),
            Err(RecvTimeoutError::Timeout) => {
                if let Some(mode) = settings_get("screen_brightness_mode") {
                    let auto = if mode == "1" { 1 } else { 0 };
                    if auto != was_auto {
                        tracing::info!(
                            "[Sync] screen_brightness_mode={} ({})",
                            auto,
                            if auto == 1 { "auto" } else { "manual" }
                        );
                        was_auto = auto;
                    }
                }
                if last_fallback.elapsed() >= fallback_every {
                    if let Some(level) = framework_brightness() {
                        apply(level, &mut last_raw);
                    }
                    last_fallback = Instant::now();
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                tracing::warn!("[Sync] logcat channel closed, restarting");
                let (tx, rx2) = channel::<f64>();
                spawn_logcat_brightness(tx);
                let level = framework_brightness();
                if let Some(level) = level {
                    apply(level, &mut last_raw);
                }
                // continue with the new receiver
                loop {
                    match rx2.recv_timeout(Duration::from_millis(interval_ms)) {
                        Ok(level) => apply(level, &mut last_raw),
                        Err(RecvTimeoutError::Timeout) => {
                            if let Some(level) = framework_brightness() {
                                apply(level, &mut last_raw);
                            }
                        }
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------- main

fn usage() -> ! {
    eprintln!(
        "miui_lights {}\n\
         usage:\n\
           miui_lights [--sync] [--interval MS] [--gamma G] [--gain K] [--floor F]\n\
           miui_lights --hal [--gamma G] [--gain K] [--floor F]\n\
           miui_lights --probe\n\
           miui_lights --value 0..1 [--gamma G] [--floor F]",
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
    let mut interval_ms: u64 = 1000;
    let mut gamma: f64 = std::env::var("MIUI_LIGHTS_GAMMA")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1.0);
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
                i += 1;
                interval_ms = args.get(i).and_then(|v| v.parse().ok()).unwrap_or(1000);
            }
            "--gamma" => {
                i += 1;
                gamma = args.get(i).and_then(|v| v.parse().ok()).unwrap_or(1.0);
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
            if let Err(e) = run_hal(panel, gamma) {
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
            if let Err(e) = run_sync(panel, interval_ms) {
                tracing::error!("sync failed: {e:#}");
                std::process::exit(1);
            }
        }
    }
}
