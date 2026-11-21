//! Push a [`Config`] to the mouse. Every setting is read first and only written
//! when it differs, so repeated runs do not wear the onboard flash.

use crate::config::{Config, button_target, parse_button, spec_for};
use crate::device::{ButtonFunction, DongleKind, Mouse, dongle_led_slot_name_checked};
use crate::hid::{HidError, Result};
use crate::protocol::Status;
use crate::sensor;

/// The function a button has now, or None when the mouse has no such button:
/// a layout shared by several rows may list one this unit lacks (the read
/// answers status 03 or 05), and such a button is skipped unless the config
/// binds it, which is an error worth seeing.
pub fn current_button(mouse: &Mouse, code: u8, name: &str, hypershift: bool, bound: bool) -> Result<Option<ButtonFunction>> {
    match mouse.button(mouse.profile(), code, hypershift) {
        Ok(f) => Ok(Some(f)),
        Err(HidError::Status(Status::Failure | Status::NotSupported)) if !bound => Ok(None),
        Err(HidError::Status(Status::Failure | Status::NotSupported)) => {
            Err(HidError::Invalid(format!("the config binds button {name}, which this mouse does not have")))
        }
        Err(e) => Err(e),
    }
}

/// Build a config from what the mouse currently has stored, so the first run
/// keeps the settings Synapse left behind instead of replacing them.
pub fn import(mouse: &Mouse) -> Result<Config> {
    let mut cfg = Config::default();

    let st = mouse.dpi_stages()?;
    if !st.stages.is_empty() {
        cfg.dpi.stages = st.stages.iter().map(|s| s.0).collect();
        cfg.dpi.active = st.active.clamp(1, st.stages.len() as u8);
        if st.stages.iter().any(|(x, y)| x != y) {
            cfg.dpi.y_stages = st.stages.iter().map(|s| s.1).collect();
        }
    }
    if let Ok((on, mode)) = mouse.acceleration() {
        cfg.acceleration.enabled = on;
        cfg.acceleration.mode = mode.name().into();
        // The stored custom curve cannot be turned back into control points; the
        // default template stays in the config and is written on the next apply.
    }
    if let Ok(p) = mouse.polling_rate() {
        cfg.polling.rate = p;
    }
    if let Ok(s) = mouse.idle_time() {
        cfg.power.idle_seconds = s.clamp(60, 900);
    }
    if let Ok(p) = mouse.low_battery_threshold() {
        cfg.power.low_battery_percent = p.clamp(5, 100);
    }

    // Sensor: Smart Tracking can be read back; the asymmetric levels cannot, so
    // that mode is kept as is until the user picks values in the window. On a
    // mouse whose sensor page is not known, a failed read means "keep" too.
    let s = match mouse.sensor() {
        Ok(s) => Some(s),
        Err(e) if mouse.spec().sensor_tracking => return Err(e),
        Err(_) => None,
    };
    let sc = &mut cfg.sensor;
    sc.tracking = "keep".into();
    if let Some(s) = s {
        if s.mode == sensor::LIFT_MODE_SMART && !s.use_calibration {
            sc.tracking = "smart".into();
            sc.asymmetric = false;
            sc.lift = (s.height + 1).clamp(1, 3);
        } else if s.mode == sensor::LIFT_MODE_ASYMMETRIC
            && s.use_calibration
            && let Some((lift, land)) = s.asym_levels
        {
            sc.tracking = "smart".into();
            sc.asymmetric = true;
            sc.lift = lift;
            sc.land = land;
        }
    }
    if let Ok((on, deg)) = mouse.rotation() {
        sc.rotation = if on { deg.clamp(-30, 30) } else { 0 };
    }

    match mouse.spec().dongle {
        DongleKind::HyperPolling => {
            if let Ok(led) = mouse.dongle_led_mode() {
                if (1..=3).contains(&led) {
                    cfg.dongle.led = led;
                }
            }
        }
        DongleKind::Gen2Dome => {
            let leds = mouse.dongle_leds()?;
            let mut names = Vec::with_capacity(3);
            for c in leds {
                let n = dongle_led_slot_name_checked(c).ok_or_else(|| HidError::Invalid(format!("unknown dongle LED slot code 0x{c:02x}")))?;
                names.push(n.to_string());
            }
            cfg.dongle.leds = names;
            cfg.dongle.brightness = Some(mouse.dongle_led_brightness()?);
        }
        DongleKind::None => {}
    }

    for (zone, _) in mouse.spec().lighting {
        if let (Ok(Some(e)), Some(slot)) = (mouse.lighting(zone), cfg.lighting.zone_mut(zone)) {
            *slot = Some(e.spec());
        }
        if cfg.lighting.brightness.is_none() {
            cfg.lighting.brightness = mouse.brightness(zone).ok();
        }
    }

    cfg.profile.active = mouse.profile();
    for b in mouse.spec().buttons {
        let name = b.name;
        for hs in [false, true] {
            let Ok(f) = mouse.button(mouse.profile(), b.code, hs) else {
                continue;
            };
            if f == b.default {
                continue;
            }
            if let Some(spec) = spec_for(&f) {
                let key = if hs { format!("hypershift.{name}") } else { name.to_string() };
                cfg.buttons.insert(key, spec);
            }
        }
    }

    if cfg.validate().is_err() {
        // Something odd on the mouse: keep the parts that validate on their own.
        let mut safe = Config { dpi: cfg.dpi.clone(), ..Default::default() };
        safe.polling.rate = cfg.polling.rate;
        if safe.validate().is_err() {
            safe = Config::default();
        }
        return Ok(safe);
    }
    Ok(cfg)
}

/// Whether the mouse's acceleration state differs from the config. A custom
/// curve is checked on its first and last packets only (reading all 32 takes
/// seconds through the dongle).
fn accel_differs(mouse: &Mouse, cfg: &Config) -> Result<bool> {
    let (on, mode) = mouse.acceleration()?;
    let a = &cfg.acceleration;
    if on != a.enabled {
        return Ok(true);
    }
    if !on {
        return Ok(false);
    }
    let want = a.mode().map_err(|_| HidError::Unsupported("invalid [acceleration] section"))?;
    if mode != want {
        return Ok(true);
    }
    if want == crate::accel::Mode::Custom {
        let samples = a.samples();
        for offset in [0usize, crate::accel::POINTS - crate::accel::CHUNK] {
            let stored = mouse.acceleration_curve_chunk(offset as u8, crate::accel::CHUNK as u8)?;
            if stored != samples[offset..offset + crate::accel::CHUNK] {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Apply the whole configuration. `polling_rate` lets the caller pick between
/// the normal and the in-game rate. Returns a line per change made.
pub fn apply(mouse: &Mouse, cfg: &Config, polling_rate: u16) -> Result<Vec<String>> {
    let mut changes = Vec::new();
    let spec = mouse.spec();
    cfg.validate_for(spec).map_err(HidError::Invalid)?;

    // Everything profile-bound below (buttons, acceleration) goes to this profile.
    if mouse.profile() != cfg.profile.active {
        mouse.select_profile(cfg.profile.active)?;
        changes.push(format!("onboard profile -> {}", cfg.profile.active));
    }

    // Write both the stage table and the configured `active` stage when either differs,
    // and always keep the live `(x, y)` sensor DPI in sync with the active stage.
    let want_stages = cfg.stage_pairs();
    let want_active = cfg.dpi.active.clamp(1, want_stages.len() as u8);
    let cur = mouse.dpi_stages()?;
    if cur.stages != want_stages || cur.active != want_active {
        mouse.set_dpi_stages(want_active, &want_stages)?;
        changes.push(format!("dpi stages -> {:?} (active {})", cfg.dpi.stages, want_active));
    } else {
        let want = want_stages[want_active as usize - 1];
        if mouse.dpi()? != want {
            mouse.set_dpi(want.0, want.1)?;
            changes.push(format!("dpi -> {}", want.0));
        }
    }

    if mouse.polling_rate()? != polling_rate {
        mouse.set_polling_rate(polling_rate)?;
        changes.push(format!("polling -> {polling_rate} Hz"));
    }

    if mouse.spec().battery && mouse.idle_time()? != cfg.power.idle_seconds {
        mouse.set_idle_time(cfg.power.idle_seconds)?;
        changes.push(format!("idle sleep -> {} s", cfg.power.idle_seconds));
    }

    if mouse.spec().battery && mouse.low_battery_threshold()? != cfg.power.low_battery_percent {
        mouse.set_low_battery_threshold(cfg.power.low_battery_percent)?;
        changes.push(format!("low battery -> {}%", cfg.power.low_battery_percent));
    }

    let tracking = cfg.tracking().map_err(|_| HidError::Unsupported("invalid [sensor] section"))?;
    // The tracking page is only written where it is known to apply (src/sensor.rs).
    if let Some(tracking) = tracking.filter(|_| spec.sensor_tracking) {
        let plan = tracking.plan().map_err(|_| HidError::Unsupported("invalid [sensor] section"))?;
        if mouse.sync_tracking(&plan)? {
            changes.push(format!("tracking -> {tracking:?}"));
        }
    }

    if spec.supports_acceleration && accel_differs(mouse, cfg)? {
        let mode = cfg.acceleration.mode().map_err(|_| HidError::Unsupported("invalid [acceleration] section"))?;
        let samples = cfg.acceleration.samples();
        mouse.set_acceleration(cfg.acceleration.enabled, mode, Some(&samples))?;
        changes.push(if cfg.acceleration.enabled { format!("dynamic sensitivity -> {}", mode.name()) } else { "dynamic sensitivity -> off".into() });
    }

    let want_rot = cfg.sensor.rotation;
    if spec.supports_rotation {
        match mouse.rotation() {
            Ok((rot_on, rot_deg)) => {
                if rot_on != (want_rot != 0) || (rot_on && rot_deg != want_rot) {
                    mouse.set_rotation(want_rot)?;
                    changes.push(format!("rotation -> {want_rot} deg"));
                }
            }
            Err(e) if spec.sensor_tracking => return Err(e),
            // Angle tune is part of the sensor page: left alone where that page is unknown.
            Err(_) => {}
        }
    }

    match spec.dongle {
        DongleKind::HyperPolling => {
            if mouse.dongle_led_mode()? != cfg.dongle.led {
                mouse.set_dongle_led_mode(cfg.dongle.led)?;
                changes.push(format!("dongle led -> {}", cfg.dongle.led));
            }
        }
        DongleKind::Gen2Dome => {
            if let Some(want_leds) = cfg.dongle.led_codes().map_err(HidError::Invalid)? {
                if mouse.sync_dongle_leds(want_leds)? {
                    changes.push(format!("dongle leds -> {:?}", cfg.dongle.leds));
                }
            }
            if let Some(want_br) = cfg.dongle.brightness {
                if mouse.sync_dongle_led_brightness(want_br)? {
                    changes.push(format!("dongle led brightness -> {}%", want_br * 25));
                }
            }
        }
        DongleKind::None => {}
    }

    changes.extend(apply_lighting(mouse, cfg)?);

    // Every button of both layers: a key missing from [buttons] means its default function.
    for b in mouse.spec().buttons {
        let (name, code) = (b.name, b.code);
        for hs in [false, true] {
            let key = if hs { format!("hypershift.{name}") } else { name.to_string() };
            let spec = cfg.buttons.get(&key);
            let func = spec.and_then(|s| parse_button(s).ok().flatten()).unwrap_or(b.default);
            let Some(current) = current_button(mouse, code, name, hs, spec.is_some())? else {
                continue;
            };
            if current != func {
                mouse.set_button(mouse.profile(), code, hs, func)?;
                changes.push(format!("button {key} -> {}", spec.map(String::as_str).unwrap_or("default")));
            }
        }
    }
    let _ = button_target;

    Ok(changes)
}

/// Write the `[lighting]` table: each zone the config names and the mouse has,
/// and the brightness of every zone. Zones the config leaves out are untouched.
pub fn apply_lighting(mouse: &Mouse, cfg: &Config) -> Result<Vec<String>> {
    let mut changes = Vec::new();
    for (zone, _) in mouse.spec().lighting {
        if let Some(spec) = cfg.lighting.zone(zone) {
            let Ok(e) = crate::lighting::Effect::parse(spec) else {
                continue;
            };
            if mouse.lighting(zone)? != Some(e) {
                mouse.set_lighting(zone, e)?;
                changes.push(format!("lighting {zone} -> {spec}"));
            }
        }
        if let Some(b) = cfg.lighting.brightness
            && mouse.brightness(zone)? != b
        {
            mouse.set_brightness(zone, b)?;
            changes.push(format!("brightness {zone} -> {b}%"));
        }
    }
    Ok(changes)
}

/// Desired normal-layer [`ButtonFunction`] for `b` (with optional `game` overrides)
/// and whether the config explicitly names `b.name`.
pub fn normal_button_target(cfg: &Config, game: Option<&crate::config::GameOverrides>, b: &crate::device::PhysicalButton) -> (ButtonFunction, bool) {
    let spec = game.and_then(|g| g.buttons.get(b.name)).or_else(|| cfg.buttons.get(b.name));
    let func = spec.and_then(|s| parse_button(s).ok().flatten()).unwrap_or(b.default);
    (func, spec.is_some())
}

/// Write the button layout of a game (or the base layout when `game` is None)
/// for the normal layer. Returns true when anything was written.
pub fn apply_game_buttons(mouse: &Mouse, cfg: &Config, game: Option<&crate::config::GameOverrides>) -> Result<bool> {
    let mut written = false;
    for b in mouse.spec().buttons {
        let (func, bound) = normal_button_target(cfg, game, b);
        let Some(current) = current_button(mouse, b.code, b.name, false, bound)? else {
            continue;
        };
        if current != func {
            mouse.set_button(mouse.profile(), b.code, false, func)?;
            written = true;
        }
    }
    Ok(written)
}
