//! High-level API for a Razer mouse (wired, or through its wireless dongle),
//! driven by the [`DeviceSpec`] table below. Rows marked verified were tested
//! write by write by an owner of that mouse; the others carry facts from
//! OpenRazer's device list and refuse writes until an owner confirms them.

use std::borrow::Cow;
use std::thread;
use std::time::Duration;

use crate::accel;
use crate::hid::{HidDevice, HidError, Result};
use crate::lighting;
use crate::protocol::{HID_BUF_LEN, Report, TID_DONGLE, TID_MOUSE, cmd};
use crate::sensor::{self, SensorPlan};

pub const VENDOR_RAZER: u16 = 0x1532;

pub const DPI_MIN: u16 = 100;
/// The rates the extended (HyperPolling) command can carry.
pub const RATES_HYPERPOLLING: [u16; 7] = [125, 250, 500, 1000, 2000, 4000, 8000];
/// The rates of the classic polling command.
pub const RATES_CLASSIC: [u16; 3] = [125, 500, 1000];
/// What the HyperPolling command carries on a wired link (the Viper Mini SE, per OpenRazer).
const RATES_HYPERPOLLING_WIRED: [u16; 3] = [125, 500, 1000];
/// The Viper 8KHz: the extended command without 250 Hz (OpenRazer).
const RATES_VIPER_8K: [u16; 6] = [125, 500, 1000, 2000, 4000, 8000];

/// Which polling-rate command a mouse answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollingStyle {
    /// `00/40` set (sent twice, with argument 0x00 then 0x01), `00/C0` get; codes 8000=0x01 .. 125=0x40.
    HyperPolling,
    /// `00/05` set, `00/85` get; codes 1000=0x01, 500=0x02, 250=0x04, 125=0x08.
    Classic,
}

/// Wireless receiver protocol family, deciding how dongle identity and indicator LEDs work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DongleKind {
    /// Wired link, or standard 1000 Hz HyperSpeed receiver without indicator LED commands.
    None,
    /// HyperPolling wireless dongle (0x00C1, 0x00B3): fw 00/81, serial 00/82, single LED 07/10..07/90.
    HyperPolling,
    /// HyperSpeed Wireless Gen-2 dome dongle (DeathAdder V4 Pro 0x00BF):
    /// fw 00/91, serial 00/92, 3 indicator LEDs 07/15..07/95 + brightness 07/16..07/96.
    Gen2Dome,
}

/// Indicator functions for each of the 3 LEDs on the HyperSpeed Gen-2 dome dongle.
pub const DONGLE_LED_SLOTS: [(u8, &str, &str); 5] =
    [(0x02, "battery", "电量状态"), (0x01, "connection", "连接状态"), (0x03, "polling", "回报率"), (0x04, "dpi", "DPI 档位"), (0x00, "off", "关闭")];

pub fn dongle_led_slot_code(name: &str) -> Option<u8> {
    let s = name.trim().to_ascii_lowercase();
    if let Ok(v) = s.parse::<u8>() {
        if v <= 4 {
            return Some(v);
        }
    }
    DONGLE_LED_SLOTS.iter().find(|(_, n, _)| *n == s).map(|(c, _, _)| *c)
}

pub fn dongle_led_slot_name_checked(code: u8) -> Option<&'static str> {
    DONGLE_LED_SLOTS.iter().find(|(c, _, _)| *c == code).map(|(_, n, _)| *n)
}

pub fn dongle_led_slot_name(code: u8) -> &'static str {
    dongle_led_slot_name_checked(code).unwrap_or("off")
}

pub fn dongle_led_slot_label(code: u8) -> &'static str {
    DONGLE_LED_SLOTS.iter().find(|(c, _, _)| *c == code).map(|(_, _, l)| *l).unwrap_or("关闭")
}

/// What freenapse knows about one product id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceSpec {
    pub product_id: u16,
    pub name: Cow<'static, str>,
    /// The mouse talks through a wireless dongle (dongle firmware, serial and LED commands apply).
    pub wireless: bool,
    /// Transaction id that addresses the mouse (0x1f on current firmware; 0x3f and 0xff on older lines).
    pub transaction_id: u8,
    pub polling: PollingStyle,
    pub polling_rates: &'static [u16],
    pub dpi_max: u16,
    /// The Focus Pro tracking page of `src/sensor.rs` (Smart Tracking / asymmetric cut-off) is known to apply.
    pub sensor_tracking: bool,
    /// `0b/85` returns `[00, 04, 02, lift-1, land-1, ...]` so asymmetric levels can be read back (DeathAdder V4 Pro).
    pub asym_readback: bool,
    /// Dynamic Sensitivity commands are available on this row.
    pub supports_acceleration: bool,
    /// Sensor angle commands are available on this row.
    pub supports_rotation: bool,
    /// The mouse has a battery (and with it idle sleep and the low-battery mode).
    pub battery: bool,
    /// LED zones: (config name, LED id of the lighting commands). Empty: no lights.
    pub lighting: &'static [(&'static str, u8)],
    /// Its own config file, `config-<name>.toml`; None shares `config.toml`.
    pub config: Option<&'static str>,
    /// Dongle pairing (and the single dongle indicator LED) as Synapse offers them for the HyperPolling dongle.
    pub pairing: bool,
    /// Wireless dongle hardware generation for firmware, serial and indicator LEDs.
    pub dongle: DongleKind,
    /// An owner confirmed writes on this mouse. Until then every write is refused
    /// unless `[device] allow_unverified` in the config names its product id.
    pub verified: bool,
    /// The physical buttons, with the function each one has out of the box.
    pub buttons: &'static [PhysicalButton],
}

macro_rules! spec {
    ($pid:expr, $name:expr, wireless $w:expr, tid $tid:expr, $style:ident, $rates:expr, dpi $dpi:expr, sensor $s:expr, pairing $p:expr, verified $v:expr) => {
        spec!($pid, $name, wireless $w, tid $tid, $style, $rates, dpi $dpi, sensor $s, pairing $p, verified $v, acceleration true, rotation true)
    };
    ($pid:expr, $name:expr, wireless $w:expr, tid $tid:expr, $style:ident, $rates:expr, dpi $dpi:expr, sensor $s:expr, pairing $p:expr, verified $v:expr, acceleration $a:expr, rotation $r:expr) => {
        spec!($pid, $name, wireless $w, tid $tid, $style, $rates, dpi $dpi, sensor $s, pairing $p, verified $v, acceleration $a, rotation $r, buttons BUTTONS)
    };
    ($pid:expr, $name:expr, wireless $w:expr, tid $tid:expr, $style:ident, $rates:expr, dpi $dpi:expr, sensor $s:expr, pairing $p:expr, verified $v:expr, acceleration $a:expr, rotation $r:expr, buttons $b:expr) => {
        DeviceSpec {
            product_id: $pid,
            name: Cow::Borrowed($name),
            wireless: $w,
            transaction_id: $tid,
            polling: PollingStyle::$style,
            polling_rates: &$rates,
            dpi_max: $dpi,
            sensor_tracking: $s,
            asym_readback: false,
            supports_acceleration: $a,
            supports_rotation: $r,
            battery: true,
            lighting: &[],
            config: None,
            pairing: $p,
            dongle: if $p { DongleKind::HyperPolling } else { DongleKind::None },
            verified: $v,
            buttons: $b,
        }
    };
}

/// The device table: one row per product id, so a mouse with a cable and a
/// dongle has two. Verified rows were tested on the mouse by an owner; the
/// other rows take product id, transaction id, polling command and DPI maximum
/// from OpenRazer's device list (facts, no code) and stay unverified until an
/// owner's `freenapse probe` output confirms them (see CONTRIBUTING.md).
pub const DEVICES: &[DeviceSpec] = &[
    spec!(0x00C0, "Razer Viper V3 Pro (wired)", wireless false, tid 0x1f, Classic, RATES_CLASSIC, dpi 35000, sensor true, pairing false, verified true),
    spec!(0x00C1, "Razer Viper V3 Pro (wireless)", wireless true, tid 0x1f, HyperPolling, RATES_HYPERPOLLING, dpi 35000, sensor true, pairing true, verified true),
    // From its owner's probes over the cable and through the HyperSpeed 1000 Hz receiver (issue 8):
    // classic polling on both (00/C0 not supported), angle and Dynamic Sensitivity getters answer, the
    // common eight buttons, no macro memory, one onboard profile. That owner confirmed DPI stages,
    // polling and button writes on both layers, through the receiver and over the cable.
    // The sensor reads answer like the Viper V3 Pro's, but nobody has tried the tracking writes, so
    // that page stays off.
    spec!(0x00DE, "Razer Viper V3 Pro SE (wired)", wireless false, tid 0x1f, Classic, RATES_CLASSIC, dpi 35000, sensor false, pairing false, verified true),
    spec!(0x00DF, "Razer Viper V3 Pro SE (wireless)", wireless true, tid 0x1f, Classic, RATES_CLASSIC, dpi 35000, sensor false, pairing false, verified true),
    spec!(0x00B3, "Razer HyperPolling wireless dongle", wireless true, tid 0x1f, HyperPolling, RATES_HYPERPOLLING, dpi 30000, sensor false, pairing true, verified false),
    // DeathAdder V4 Pro (wired 0x00BE, wireless Gen-2 dome dongle 0x00BF): verified up to 45000 DPI,
    // 8000 Hz HyperPolling, Focus Pro 45K tracking (Smart + Asymmetric readback), rotation, Dynamic Sensitivity,
    // all 8 buttons (Normal + Hypershift) and the Gen-2 dome dongle's 3-LED indicators (07/15..07/95).
    DeviceSpec {
        asym_readback: true,
        ..spec!(0x00BE, "Razer DeathAdder V4 Pro (wired)", wireless false, tid 0x1f, HyperPolling, RATES_HYPERPOLLING, dpi 45000, sensor true, pairing false, verified true)
    },
    DeviceSpec {
        asym_readback: true,
        dongle: DongleKind::Gen2Dome,
        ..spec!(0x00BF, "Razer DeathAdder V4 Pro (wireless)", wireless true, tid 0x1f, HyperPolling, RATES_HYPERPOLLING, dpi 45000, sensor true, pairing false, verified true)
    },
    spec!(0x00B6, "Razer DeathAdder V3 Pro (wired)", wireless false, tid 0x1f, Classic, RATES_CLASSIC, dpi 30000, sensor false, pairing false, verified false, acceleration false, rotation false),
    spec!(0x00B7, "Razer DeathAdder V3 Pro (wireless)", wireless true, tid 0x1f, Classic, RATES_CLASSIC, dpi 35000, sensor false, pairing false, verified false),
    spec!(0x00B2, "Razer DeathAdder V3", wireless false, tid 0x1f, HyperPolling, RATES_HYPERPOLLING, dpi 30000, sensor false, pairing false, verified false),
    spec!(0x00A5, "Razer Viper V2 Pro (wired)", wireless false, tid 0x1f, Classic, RATES_CLASSIC, dpi 30000, sensor false, pairing false, verified false),
    spec!(0x00A6, "Razer Viper V2 Pro (wireless)", wireless true, tid 0x1f, Classic, RATES_CLASSIC, dpi 30000, sensor false, pairing false, verified false),
    spec!(0x00B8, "Razer Viper V3 HyperSpeed", wireless true, tid 0x1f, Classic, RATES_CLASSIC, dpi 30000, sensor false, pairing false, verified false),
    spec!(0x009E, "Razer Viper Mini SE (wired)", wireless false, tid 0x1f, HyperPolling, RATES_HYPERPOLLING_WIRED, dpi 30000, sensor false, pairing false, verified false),
    spec!(0x009F, "Razer Viper Mini SE (wireless)", wireless true, tid 0x1f, HyperPolling, RATES_HYPERPOLLING, dpi 30000, sensor false, pairing true, verified false),
    spec!(0x0091, "Razer Viper 8KHz", wireless false, tid 0xff, HyperPolling, RATES_VIPER_8K, dpi 20000, sensor false, pairing false, verified false),
    spec!(0x00AF, "Razer Cobra Pro (wired)", wireless false, tid 0x1f, Classic, RATES_CLASSIC, dpi 30000, sensor false, pairing false, verified false),
    spec!(0x00B0, "Razer Cobra Pro (wireless)", wireless true, tid 0x1f, Classic, RATES_CLASSIC, dpi 30000, sensor false, pairing false, verified false),
    // The same mouse over its cable (product id from OpenRazer, no probe yet): the same config file.
    DeviceSpec {
        config: Some("pro-click-v2-vertical"),
        ..spec!(0x00C7, "Razer Pro Click V2 Vertical (wired)", wireless false, tid 0x1f, Classic, RATES_CLASSIC, dpi 30000, sensor false, pairing false, verified false, acceleration false, rotation false)
    },
    // Verified by its owner over the HyperSpeed dongle (issue 4); its own config file.
    DeviceSpec {
        config: Some("pro-click-v2-vertical"),
        ..spec!(0x00C8, "Razer Pro Click V2 Vertical (wireless)", wireless true, tid 0x1f, Classic, RATES_CLASSIC, dpi 30000, sensor false, pairing false, verified true, acceleration false, rotation false)
    },
    // Wired only: no battery, so no idle sleep or low-battery mode either.
    DeviceSpec {
        battery: false,
        lighting: &[("logo", 0x04), ("wheel", 0x01)],
        config: Some("basilisk-v2"),
        ..spec!(0x0085, "Razer Basilisk V2", wireless false, tid 0x1f, Classic, RATES_CLASSIC, dpi 20000, sensor false, pairing false, verified true, acceleration false, rotation false, buttons BASILISK_V2_BUTTONS)
    },
    // From its owner's probe over the cable (issue 5): no battery, classic polling, angle and
    // Dynamic Sensitivity refused, macro memory, five onboard profiles. That owner confirmed DPI, polling and button writes on
    // both layers; macros, profile switching and the wheel tilts were not tried.
    // Its LEDs (logo, wheel, underglow) are not listed: nobody has probed their commands.
    DeviceSpec {
        battery: false,
        config: Some("basilisk-v3"),
        ..spec!(0x0099, "Razer Basilisk V3", wireless false, tid 0x1f, Classic, RATES_CLASSIC, dpi 26000, sensor false, pairing false, verified true, acceleration false, rotation false, buttons BASILISK_V3_BUTTONS)
    },
    spec!(0x00AA, "Razer Basilisk V3 Pro (wired)", wireless false, tid 0x1f, Classic, RATES_CLASSIC, dpi 30000, sensor false, pairing false, verified false),
    spec!(0x00AB, "Razer Basilisk V3 Pro (wireless)", wireless true, tid 0x1f, Classic, RATES_CLASSIC, dpi 30000, sensor false, pairing false, verified false),
];

impl DeviceSpec {
    /// The table row for a product id.
    pub fn find(product_id: u16) -> Option<&'static DeviceSpec> {
        DEVICES.iter().find(|d| d.product_id == product_id)
    }

    /// The table row, or the generic fallback for a Razer mouse that is not listed.
    pub fn lookup(product_id: u16) -> DeviceSpec {
        let spec = DeviceSpec::find(product_id).cloned().unwrap_or_else(|| DeviceSpec::unknown(product_id));
        // Debug builds only: FREENAPSE_TREAT_UNLISTED=1 opens the mouse as one that is not in
        // the table (the generic fallback row), to try that path without owning such a mouse.
        #[cfg(all(debug_assertions, not(test)))]
        if std::env::var_os("FREENAPSE_TREAT_UNLISTED").is_some() {
            return DeviceSpec::unknown(product_id);
        }
        // Debug builds only: FREENAPSE_TREAT_UNVERIFIED=1 opens a verified mouse as an
        // unverified one, to try the read-only path and the Devices page's switch on it.
        #[cfg(all(debug_assertions, not(test)))]
        if std::env::var_os("FREENAPSE_TREAT_UNVERIFIED").is_some() {
            return DeviceSpec { verified: false, ..spec };
        }
        spec
    }

    /// A Razer mouse that is not in the table: readable, writes refused, the
    /// most common transaction id and the classic polling command.
    pub fn unknown(product_id: u16) -> DeviceSpec {
        DeviceSpec {
            product_id,
            name: Cow::Owned(format!("Razer mouse 0x{product_id:04x}")),
            wireless: false,
            transaction_id: TID_MOUSE,
            polling: PollingStyle::Classic,
            polling_rates: &RATES_CLASSIC,
            dpi_max: 30000,
            sensor_tracking: false,
            asym_readback: false,
            supports_acceleration: true,
            supports_rotation: true,
            battery: true,
            lighting: &[],
            config: None,
            pairing: false,
            dongle: DongleKind::None,
            verified: false,
            buttons: BUTTONS,
        }
    }

    /// True for right-handed ergonomic shells (DeathAdder, Basilisk, Pro Click).
    pub fn is_ergonomic(&self) -> bool {
        self.is_placeholder() || self.name.contains("DeathAdder") || self.name.contains("Basilisk") || self.name.contains("Pro Click")
    }

    /// No particular mouse: what the config file is checked against, and what
    /// the settings window shows, until a mouse is open. It accepts whatever
    /// some row of the table accepts (every polling rate, [`DPI_MAX_ANY`]) and
    /// hides nothing; the open mouse's own row then narrows that down.
    pub fn any_mouse() -> &'static DeviceSpec {
        &ANY_MOUSE
    }

    /// True for the [`DeviceSpec::any_mouse`] placeholder (no mouse was opened yet).
    pub fn is_placeholder(&self) -> bool {
        self.product_id == 0
    }

    /// True when the product id is in the table (verified or not).
    pub fn is_known(&self) -> bool {
        DeviceSpec::find(self.product_id).is_some()
    }

    pub fn dpi_ok(&self, dpi: u16) -> bool {
        (DPI_MIN..=self.dpi_max).contains(&dpi)
    }

    /// "125/250/500/1000/2000/4000/8000", for error messages.
    pub fn rates_text(&self) -> String {
        self.polling_rates.iter().map(u16::to_string).collect::<Vec<_>>().join("/")
    }

    /// Why a write to this mouse is refused, or None when it may go ahead.
    pub fn write_refusal(&self, allow_unverified: bool) -> Option<String> {
        if self.verified || allow_unverified {
            return None;
        }
        let why = if self.is_known() {
            format!("the {} is in freenapse's device table but nobody has confirmed writes on it yet", self.name)
        } else {
            format!("the {} (product id 0x{:04x}) is not in freenapse's device table", self.name, self.product_id)
        };
        Some(format!(
            "{why}, so it is read-only: run `freenapse probe` and open an issue at \
             https://github.com/MatthewSaintBull/freenapse/issues with its output. \
             To write to it anyway, at your own risk, switch on \"Allow writes to this mouse\" on the Devices page \
             of the window, or set allow_unverified = [0x{:04x}] under [device] in the config file",
            self.product_id
        ))
    }
}

/// The highest DPI maximum of any row in the table.
pub const DPI_MAX_ANY: u16 = {
    let mut max = 0;
    let mut i = 0;
    while i < DEVICES.len() {
        if DEVICES[i].dpi_max > max {
            max = DEVICES[i].dpi_max;
        }
        i += 1;
    }
    max
};

/// See [`DeviceSpec::any_mouse`]. Product id 0 is no Razer product.
static ANY_MOUSE: DeviceSpec = DeviceSpec {
    product_id: 0,
    name: Cow::Borrowed("Razer mouse"),
    wireless: true,
    transaction_id: TID_MOUSE,
    polling: PollingStyle::HyperPolling,
    polling_rates: &RATES_HYPERPOLLING,
    dpi_max: DPI_MAX_ANY,
    sensor_tracking: true,
    asym_readback: true,
    supports_acceleration: true,
    supports_rotation: true,
    battery: true,
    lighting: &[],
    config: None,
    pairing: true,
    dongle: DongleKind::Gen2Dome,
    verified: true,
    buttons: BUTTONS,
};

impl Default for DeviceSpec {
    fn default() -> Self {
        DeviceSpec::any_mouse().clone()
    }
}

/// Refuse a write (command id with bit 7 clear) to a mouse nobody has verified.
pub fn guard_write(spec: &DeviceSpec, allow_unverified: bool, req: &Report) -> Result<()> {
    if req.command_id & 0x80 != 0 {
        return Ok(());
    }
    match spec.write_refusal(allow_unverified) {
        Some(why) => Err(HidError::Unverified(why)),
        None => Ok(()),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Battery {
    pub percent: u8,
    pub charging: bool,
    /// A mouse without a battery, powered by its cable (percent reads 100).
    pub wired: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DpiStages {
    /// 1-based index of the active stage.
    pub active: u8,
    pub stages: Vec<(u16, u16)>,
}

/// What the sensor currently reports: calibration in use, lift mode, height code and optional asymmetric levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SensorStatus {
    pub use_calibration: bool,
    pub mode: u8,
    pub height: u8,
    /// (lift-off 2..=26, landing 1..=25) when `0b/85` reports them (Focus Pro 45K Gen-2 on DeathAdder V4 Pro).
    pub asym_levels: Option<(u8, u8)>,
}

impl SensorStatus {
    pub fn describe(&self) -> String {
        let m = match (self.mode, self.height) {
            (0, _) => "未设置".to_string(),
            (1, h) => format!("智能追踪 (Smart Tracking), {} mm", h + 1),
            (2, h) => format!("通用迟滞模式, 高度 {h}"),
            (3, _) => "预设抬起截断模式".into(),
            (4, _) => match self.asym_levels {
                Some((lift, land)) => format!("非对称抬起截断 (抬起 {lift} / 着陆 {land})"),
                None => "非对称抬起截断".into(),
            },
            (5, _) => "手动校准模式".into(),
            (6, _) => "手动非对称校准模式".into(),
            (m, h) => format!("模式 {m} 高度 {h}"),
        };
        format!("{m}{}", if self.use_calibration && self.mode != 4 { " (校准数据启用)" } else { "" })
    }
}

/// Validate `r.data_size` (`min_len ..= 80`) and return the declared payload slice.
pub fn parse_payload<'a>(r: &'a Report, min_len: u8, what: &'static str) -> Result<&'a [u8]> {
    let n = r.data_size as usize;
    if n > r.arguments.len() || r.data_size < min_len {
        return Err(HidError::Invalid(format!("{what}: unexpected payload size {} (expected >= {min_len})", r.data_size)));
    }
    Ok(&r.arguments[..n])
}

/// Decode a Gen-2 dome dongle `07/95` response into `[left, center, right]`, rejecting unknown slot codes.
pub fn parse_dongle_leds(r: &Report) -> Result<[u8; 3]> {
    let a = parse_payload(r, 3, "dongle LEDs")?;
    for &b in &a[..3] {
        if dongle_led_slot_name_checked(b).is_none() {
            return Err(HidError::Invalid(format!("unknown dongle LED slot code 0x{b:02x}")));
        }
    }
    Ok([a[0], a[1], a[2]])
}

/// Decode a Gen-2 dome dongle `07/96` response into `0..=4`, rejecting out-of-range values.
pub fn parse_dongle_brightness(r: &Report) -> Result<u8> {
    let a = parse_payload(r, 1, "dongle LED brightness")?;
    if a[0] > 4 {
        return Err(HidError::Invalid(format!("unexpected dongle LED brightness {}", a[0])));
    }
    Ok(a[0])
}

/// Decode a Focus Pro 45K Gen-2 `0b/85` response (`[00, 04, 02, lift-1, land-1, ...]`) into 1-based `(lift, land)`.
pub fn parse_asym_config(r: &Report) -> Result<(u8, u8)> {
    let a = parse_payload(r, 5, "sensor config")?;
    if a[..2] != sensor::SENSOR_HDR || a[2] < 2 {
        return Err(HidError::Invalid(format!("unexpected sensor config header {:02x?}", &a[..3])));
    }
    let lo = a[3].saturating_add(1);
    let ld = a[4].saturating_add(1);
    if !(sensor::ASYM_LIFT_MIN..=sensor::ASYM_LIFT_MAX).contains(&lo) || !(sensor::ASYM_LAND_MIN..=sensor::ASYM_LAND_MAX).contains(&ld) || ld >= lo {
        return Err(HidError::Invalid(format!("sensor asymmetric levels {lo}/{ld} out of valid range")));
    }
    Ok((lo, ld))
}

/// Pre-write check: true only when every field in `plan` is proven already equal on the mouse.
/// On mice without `asym_readback` (Viper V3 Pro), asymmetric levels cannot be read back, so an
/// asymmetric plan must return `false` to ensure changes like `13/12 -> 20/4` are never skipped.
pub fn sensor_already_matches(s: &SensorStatus, plan: &SensorPlan, asym_readback: bool) -> bool {
    if s.mode != plan.lift.0 || s.height != plan.lift.1 || s.use_calibration != plan.state {
        return false;
    }
    if let Some([lo, ld]) = plan.config {
        return asym_readback && s.asym_levels == Some((lo + 1, ld + 1));
    }
    true
}

/// Post-write check: verifies all readable fields of `plan` after `apply_tracking`.
pub fn sensor_post_write_ok(s: &SensorStatus, plan: &SensorPlan, asym_readback: bool) -> bool {
    if s.mode != plan.lift.0 || s.height != plan.lift.1 || s.use_calibration != plan.state {
        return false;
    }
    if asym_readback {
        if let Some([lo, ld]) = plan.config {
            return s.asym_levels == Some((lo + 1, ld + 1));
        }
    }
    true
}

/// A device the dongle is paired with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PairedDevice {
    pub product_id: u16,
    /// Link state byte as the dongle reports it (1 = connected on this mouse).
    pub status: u8,
}

/// One onboard button assignment (7 bytes: category, length, data[5]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ButtonFunction(pub [u8; 7]);

impl ButtonFunction {
    pub const DISABLED: ButtonFunction = ButtonFunction([0x00, 0, 0, 0, 0, 0, 0]);
    pub fn mouse_button(code: u8) -> Self {
        ButtonFunction([0x01, 1, code, 0, 0, 0, 0])
    }
    pub fn key(modifiers: u8, hid_key: u8) -> Self {
        ButtonFunction([0x02, 2, modifiers, hid_key, 0, 0, 0])
    }
    pub fn consumer(usage: u16) -> Self {
        let b = usage.to_be_bytes();
        ButtonFunction([0x0a, 2, b[0], b[1], 0, 0, 0])
    }
    /// 0x01 = next stage, 0x02 = previous (both wrap), 0x03 + level = jump to a stage,
    /// 0x06 = cycle through the stages (what the DPI button does out of the box).
    pub fn dpi_cycle_up() -> Self {
        ButtonFunction([0x06, 1, 0x01, 0, 0, 0, 0])
    }
    pub fn dpi_cycle_down() -> Self {
        ButtonFunction([0x06, 1, 0x02, 0, 0, 0, 0])
    }
    pub fn dpi_stage(level: u8) -> Self {
        ButtonFunction([0x06, 2, 0x03, level, 0, 0, 0])
    }
    pub fn dpi_cycle() -> Self {
        ButtonFunction([0x06, 1, 0x06, 0, 0, 0, 0])
    }
    /// 0x01 next, 0x02 previous, 0x03 + id, 0x04 cycle up, 0x05 cycle down.
    /// Sensitivity clutch: this DPI while the button is held.
    pub fn clutch(x: u16, y: u16) -> Self {
        let (a, b) = (x.to_be_bytes(), y.to_be_bytes());
        ButtonFunction([0x06, 5, 0x05, a[0], a[1], b[0], b[1]])
    }
    /// The clutch DPI (x, y), when this is a clutch.
    pub fn clutch_dpi(&self) -> Option<(u16, u16)> {
        let d = &self.0;
        (d[0] == 0x06 && d[2] == 0x05).then(|| (u16::from_be_bytes([d[3], d[4]]), u16::from_be_bytes([d[5], d[6]])))
    }
    /// Profile switching: 0x01 next, 0x02 previous, 0x04 cycle up, 0x05 cycle down.
    pub fn profile_op(op: u8) -> Self {
        ButtonFunction([0x07, 1, op, 0, 0, 0, 0])
    }
    pub fn profile_next() -> Self {
        ButtonFunction([0x07, 1, 0x01, 0, 0, 0, 0])
    }
    pub fn hypershift() -> Self {
        ButtonFunction([0x0c, 1, 0x01, 0, 0, 0, 0])
    }
    /// Synapse's "Double Click": two left clicks per press.
    pub fn double_click() -> Self {
        ButtonFunction([0x0b, 1, 0x01, 0, 0, 0, 0])
    }
    /// Repeat a mouse button while held, `per_second` times a second.
    pub fn turbo_button(code: u8, per_second: u8) -> Self {
        let d = turbo_delay(per_second).to_be_bytes();
        ButtonFunction([0x0e, 3, code, d[0], d[1], 0, 0])
    }
    /// Repeat a key while held.
    pub fn turbo_key(modifiers: u8, hid_key: u8, per_second: u8) -> Self {
        let d = turbo_delay(per_second).to_be_bytes();
        ButtonFunction([0x0d, 4, modifiers, hid_key, d[0], d[1], 0])
    }
    /// System keys: [`POWER_SLEEP`], [`POWER_OFF`], [`POWER_WAKE`].
    pub fn power_key(code: u8) -> Self {
        ButtonFunction([0x09, 1, code, 0, 0, 0, 0])
    }

    /// Repeat rate of a turbo function, in presses per second.
    pub fn turbo_rate(&self) -> Option<u8> {
        let d = &self.0;
        let delay = match d[0] {
            0x0e => u16::from_be_bytes([d[3], d[4]]),
            0x0d => u16::from_be_bytes([d[4], d[5]]),
            _ => return None,
        };
        Some(((1000.0 / delay.max(1) as f32).round() as u8).clamp(TURBO_MIN, TURBO_MAX))
    }

    pub fn describe(&self) -> String {
        let d = &self.0;
        match d[0] {
            0x00 => "disabled".into(),
            0x01 => format!("mouse button {}", mouse_button_name(d[2])),
            0x0b => "double-click".into(),
            0x0e => format!("turbo {} every {} ms", mouse_button_name(d[2]), u16::from_be_bytes([d[3], d[4]])),
            0x02 => format!("key 0x{:02x} (mods 0x{:02x})", d[3], d[2]),
            0x0d => format!("turbo key 0x{:02x} (mods 0x{:02x}) every {} ms", d[3], d[2], u16::from_be_bytes([d[4], d[5]])),
            0x09 => match d[2] {
                POWER_OFF => "system power".into(),
                POWER_SLEEP => "system sleep".into(),
                POWER_WAKE => "system wake".into(),
                x => format!("system key 0x{x:02x}"),
            },
            0x0a => format!("media key 0x{:04x}", u16::from_be_bytes([d[2], d[3]])),
            0x06 => match d[2] {
                0x01 => "DPI cycle up".into(),
                0x02 => "DPI cycle down".into(),
                0x03 => format!("DPI stage {}", d[3]),
                0x06 => "DPI cycle".into(),
                0x05 => format!("sensitivity clutch {}x{} DPI", u16::from_be_bytes([d[3], d[4]]), u16::from_be_bytes([d[5], d[6]])),
                x => format!("DPI op 0x{x:02x}"),
            },
            0x07 => match d[2] {
                0x01 => "profile next".into(),
                0x02 => "profile previous".into(),
                0x03 => format!("profile {}", d[3]),
                0x04 => "profile cycle up".into(),
                0x05 => "profile cycle down".into(),
                x => format!("profile op 0x{x:02x}"),
            },
            0x0c => "hypershift".into(),
            0x12 => "scroll mode".into(),
            x => format!("unknown 0x{x:02x} {:02x?}", &d[1..]),
        }
    }

    /// Append every Windows virtual key (`VK_*`) this onboard assignment can hold down
    /// (mouse buttons, keyboard modifiers and the main key usage), deduplicated.
    pub fn push_virtual_keys(&self, out: &mut Vec<u16>) {
        let mut add = |vk: u16| {
            if !out.contains(&vk) {
                out.push(vk);
            }
        };
        let d = &self.0;
        match d[0] {
            0x01 | 0x0e => match d[2] {
                0x01 => add(0x01), // VK_LBUTTON
                0x02 => add(0x02), // VK_RBUTTON
                0x03 => add(0x04), // VK_MBUTTON
                0x04 => add(0x05), // VK_XBUTTON1
                0x05 => add(0x06), // VK_XBUTTON2
                _ => {}
            },
            0x0b => add(0x01), // double-click emits left button
            0x02 | 0x0d => {
                let mods = d[2];
                if mods & 0x01 != 0 {
                    add(0x11); // VK_CONTROL
                    add(0xa2); // VK_LCONTROL
                }
                if mods & 0x02 != 0 {
                    add(0x10); // VK_SHIFT
                    add(0xa0); // VK_LSHIFT
                }
                if mods & 0x04 != 0 {
                    add(0x12); // VK_MENU
                    add(0xa4); // VK_LMENU
                }
                if mods & 0x08 != 0 {
                    add(0x5b); // VK_LWIN
                }
                if mods & 0x10 != 0 {
                    add(0x11); // VK_CONTROL
                    add(0xa3); // VK_RCONTROL
                }
                if mods & 0x20 != 0 {
                    add(0x10); // VK_SHIFT
                    add(0xa1); // VK_RSHIFT
                }
                if mods & 0x40 != 0 {
                    add(0x12); // VK_MENU
                    add(0xa5); // VK_RMENU
                }
                if mods & 0x80 != 0 {
                    add(0x5c); // VK_RWIN
                }
                if let Some(vk) = usage_vk(d[3]) {
                    add(vk);
                }
            }
            _ => {}
        }
    }
}

/// Keyboard HID usage of a Windows virtual key (US layout for the punctuation keys).
pub fn vk_usage(vk: u16) -> Option<u8> {
    Some(match vk {
        0x41..=0x5a => 0x04 + (vk - 0x41) as u8, // A..Z
        0x31..=0x39 => 0x1e + (vk - 0x31) as u8, // 1..9
        0x30 => 0x27,                            // 0
        0x0d => 0x28,                            // Enter
        0x1b => 0x29,                            // Esc
        0x08 => 0x2a,                            // Backspace
        0x09 => 0x2b,                            // Tab
        0x20 => 0x2c,                            // Space
        0xbd => 0x2d,                            // -
        0xbb => 0x2e,                            // =
        0xdb => 0x2f,                            // [
        0xdd => 0x30,                            // ]
        0xdc => 0x31,                            // backslash
        0xba => 0x33,                            // ;
        0xde => 0x34,                            // '
        0xc0 => 0x35,                            // `
        0xbc => 0x36,                            // ,
        0xbe => 0x37,                            // .
        0xbf => 0x38,                            // /
        0x14 => 0x39,                            // Caps lock
        0x70..=0x7b => 0x3a + (vk - 0x70) as u8, // F1..F12
        0x7c..=0x87 => 0x68 + (vk - 0x7c) as u8, // F13..F24
        0x2c => 0x46,                            // Print screen
        0x91 => 0x47,                            // Scroll lock
        0x13 => 0x48,                            // Pause
        0x2d => 0x49,                            // Insert
        0x24 => 0x4a,                            // Home
        0x21 => 0x4b,                            // Page up
        0x2e => 0x4c,                            // Delete
        0x23 => 0x4d,                            // End
        0x22 => 0x4e,                            // Page down
        0x27 => 0x4f,                            // Right
        0x25 => 0x50,                            // Left
        0x28 => 0x51,                            // Down
        0x26 => 0x52,                            // Up
        0x90 => 0x53,                            // Num lock
        0x6f => 0x54,                            // keypad /
        0x6a => 0x55,                            // keypad *
        0x6d => 0x56,                            // keypad -
        0x6b => 0x57,                            // keypad +
        0x61..=0x69 => 0x59 + (vk - 0x61) as u8, // keypad 1..9
        0x60 => 0x62,                            // keypad 0
        0x6e => 0x63,                            // keypad .
        0xa2 => 0xe0,                            // left ctrl
        0xa0 => 0xe1,                            // left shift
        0xa4 => 0xe2,                            // left alt
        0x5b => 0xe3,                            // left win
        0xa3 => 0xe4,                            // right ctrl
        0xa1 => 0xe5,                            // right shift
        0xa5 => 0xe6,                            // right alt
        0x5c => 0xe7,                            // right win
        _ => return None,
    })
}

/// Inverse of [`vk_usage`].
pub fn usage_vk(usage: u8) -> Option<u16> {
    (0x08..=0xfeu16).find(|vk| vk_usage(*vk) == Some(usage))
}

/// Decode a `02/8c` button-function response, verifying payload size and the `(profile, button)` header.
/// (`arguments[2]` is not checked because DeathAdder V4 Pro firmware uses it as a button flag rather than echoing `hypershift`.)
pub fn parse_button_function(r: &Report, profile: u8, button: u8) -> Result<ButtonFunction> {
    let a = parse_payload(r, 10, "button function")?;
    if a[..2] != [profile, button] {
        return Err(HidError::NoResponse);
    }
    let mut f = [0u8; 7];
    f.copy_from_slice(&a[3..10]);
    Ok(ButtonFunction(f))
}

/// System key codes of the power-key function (Synapse: SystemPower, SystemSleep, SystemWake).
pub const POWER_OFF: u8 = 0x81;
pub const POWER_SLEEP: u8 = 0x82;
pub const POWER_WAKE: u8 = 0x83;

/// Turbo repeat rate range, presses per second.
pub const TURBO_MIN: u8 = 1;
pub const TURBO_MAX: u8 = 50;

/// Interval in ms between repeats, as Synapse computes it (`1000 / keysPerSecond`).
pub fn turbo_delay(per_second: u8) -> u16 {
    1000 / per_second.clamp(TURBO_MIN, TURBO_MAX) as u16
}

pub fn mouse_button_name(code: u8) -> &'static str {
    match code {
        0x01 => "left",
        0x02 => "right",
        0x03 => "middle",
        0x04 => "back",
        0x05 => "forward",
        0x09 => "scroll up",
        0x0a => "scroll down",
        _ => "?",
    }
}

/// The clicks a button function can emit (class 0x01), by config name: the
/// same codes on every Razer mouse.
pub const CLICK_CODES: [(&str, u8); 7] =
    [("left", 0x01), ("right", 0x02), ("middle", 0x03), ("back", 0x04), ("forward", 0x05), ("scroll-up", 0x09), ("scroll-down", 0x0a)];

pub fn button_code(name: &str) -> Option<u8> {
    CLICK_CODES.iter().find(|(n, _)| *n == name).map(|(_, c)| *c)
}

/// One physical button of a mouse: its config name, the code the
/// button-mapping command addresses, and its function out of the box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhysicalButton {
    pub name: &'static str,
    pub code: u8,
    pub default: ButtonFunction,
}

const fn clicks(name: &'static str, code: u8) -> PhysicalButton {
    PhysicalButton { name, code, default: ButtonFunction([0x01, 1, code, 0, 0, 0, 0]) }
}

/// The buttons a Razer mouse has unless its row lists others: the five
/// clicks, the wheel, and the DPI button at 0x60 (under the Viper V3 Pro,
/// on top of the Pro Click V2 Vertical), which cycles the stages out of the box.
pub const BUTTONS: &[PhysicalButton] = &[
    clicks("left", 0x01),
    clicks("right", 0x02),
    clicks("middle", 0x03),
    clicks("back", 0x04),
    clicks("forward", 0x05),
    clicks("scroll-up", 0x09),
    clicks("scroll-down", 0x0a),
    PhysicalButton { name: "dpi", code: 0x60, default: ButtonFunction([0x06, 1, 0x06, 0, 0, 0, 0]) },
];

/// The Basilisk V2: the seven clicks above (no 0x60) plus DPI up and down behind the wheel,
/// the wheel tilts, the sensitivity clutch and the profile button underneath.
/// Defaults are what the mouse reports on a layer nobody has changed.
pub const BASILISK_V2_BUTTONS: &[PhysicalButton] = &[
    clicks("left", 0x01),
    clicks("right", 0x02),
    clicks("middle", 0x03),
    clicks("back", 0x04),
    clicks("forward", 0x05),
    clicks("scroll-up", 0x09),
    clicks("scroll-down", 0x0a),
    PhysicalButton { name: "dpi-up", code: 0x0b, default: ButtonFunction([0x06, 1, 0x01, 0, 0, 0, 0]) },
    PhysicalButton { name: "dpi-down", code: 0x0c, default: ButtonFunction([0x06, 1, 0x02, 0, 0, 0, 0]) },
    // Scroll up / down repeated every 20 ms while the wheel is tilted.
    PhysicalButton { name: "tilt-left", code: 0x34, default: ButtonFunction([0x0e, 3, 0x09, 0x00, 0x14, 0, 0]) },
    PhysicalButton { name: "tilt-right", code: 0x35, default: ButtonFunction([0x0e, 3, 0x0a, 0x00, 0x14, 0, 0]) },
    // 788 DPI while held.
    PhysicalButton { name: "clutch", code: 0x0f, default: ButtonFunction([0x06, 5, 0x05, 0x03, 0x14, 0x03, 0x14]) },
    PhysicalButton { name: "profile", code: 0x0e, default: ButtonFunction([0x07, 1, 0x04, 0, 0, 0, 0]) },
];

/// The Basilisk V3, from its owner's probe (issue 5): the seven clicks, the wheel tilts, the
/// scroll-mode button and the DPI button behind the wheel, the sensitivity clutch and the
/// profile button underneath. Defaults are the bytes that mouse reported.
pub const BASILISK_V3_BUTTONS: &[PhysicalButton] = &[
    clicks("left", 0x01),
    clicks("right", 0x02),
    clicks("middle", 0x03),
    clicks("back", 0x04),
    clicks("forward", 0x05),
    clicks("scroll-up", 0x09),
    clicks("scroll-down", 0x0a),
    // Wheel-left 0x68 / wheel-right 0x69 repeated every 20 ms while the wheel is tilted.
    PhysicalButton { name: "tilt-left", code: 0x34, default: ButtonFunction([0x0e, 1, 0x68, 0x00, 0x14, 0, 0]) },
    PhysicalButton { name: "tilt-right", code: 0x35, default: ButtonFunction([0x0e, 1, 0x69, 0x00, 0x14, 0, 0]) },
    // Switches the wheel between tactile and free spin.
    PhysicalButton { name: "scroll-mode", code: 0x6a, default: ButtonFunction([0x12, 1, 0x01, 0, 0, 0, 0]) },
    // The probe read no function here, and with none stored the button does nothing, so
    // "default" writes the stage cycle the button has out of the box (its owner, issue 5).
    PhysicalButton { name: "dpi", code: 0x60, default: ButtonFunction([0x06, 1, 0x06, 0, 0, 0, 0]) },
    // 400 DPI while held.
    PhysicalButton { name: "clutch", code: 0x0f, default: ButtonFunction([0x06, 5, 0x05, 0x01, 0x90, 0x01, 0x90]) },
    PhysicalButton { name: "profile", code: 0x0e, default: ButtonFunction([0x07, 1, 0x04, 0, 0, 0, 0]) },
];

const BUTTON_LISTS: [&[PhysicalButton]; 3] = [BUTTONS, BASILISK_V2_BUTTONS, BASILISK_V3_BUTTONS];

/// A physical button by config name, on any mouse in the table.
pub fn physical_button(name: &str) -> Option<&'static PhysicalButton> {
    BUTTON_LISTS.iter().flat_map(|l| l.iter()).find(|b| b.name == name)
}

/// The config name of a button code, on any mouse in the table (a name means
/// the same code everywhere, see the tests).
pub fn button_name(code: u8) -> Option<&'static str> {
    BUTTON_LISTS.iter().flat_map(|l| l.iter()).find(|b| b.code == code).map(|b| b.name)
}

pub struct Mouse {
    hid: HidDevice,
    spec: DeviceSpec,
    /// `[device] allow_unverified` of the config file names this mouse: writes to it go ahead.
    allow_unverified: bool,
    /// Onboard profile that button and acceleration commands address.
    profile: std::cell::Cell<u8>,
}

impl Mouse {
    /// Open the first Razer mouse that is in the device table, or failing that
    /// any Razer device with a 90-byte control report as [`DeviceSpec::unknown`].
    /// Whether writes to an unverified mouse are allowed comes from the config file.
    /// The mouse's config file becomes [`crate::config_path`].
    pub fn open() -> Result<Mouse> {
        let mut m = Mouse::open_with(false)?;
        m.allow_unverified = crate::config::Config::allow_unverified_at(&crate::config_path(), m.product_id());
        Ok(m)
    }

    /// Take the write permission from a config that changed while the mouse was open.
    pub fn set_write_access(&mut self, cfg: &crate::config::Config) {
        self.allow_unverified = cfg.device.allow_unverified.allows(self.product_id());
    }

    pub fn open_with(allow_unverified: bool) -> Result<Mouse> {
        #[allow(unused_mut)]
        let mut known: Vec<u16> = DEVICES.iter().map(|d| d.product_id).collect();
        // Debug builds only (see `DeviceSpec::lookup`): no listed id, so the mouse is found
        // the way an unlisted one is.
        #[cfg(all(debug_assertions, not(test)))]
        if std::env::var_os("FREENAPSE_TREAT_UNLISTED").is_some() {
            known.clear();
        }
        let mut candidates: Vec<Mouse> = HidDevice::open_controls(VENDOR_RAZER, &known).into_iter().map(|hid| Mouse::from_hid(hid, allow_unverified)).collect();
        let mouse = match candidates.len() {
            0 => {
                let hid = HidDevice::open_control_any(VENDOR_RAZER, |info| {
                    // Not a keyboard's control collection (usage page 1, usage 6).
                    info.feature_report_len as usize >= HID_BUF_LEN && !(info.usage_page == 1 && info.usage == 6)
                })?;
                Mouse::from_hid(hid, allow_unverified)
            }
            1 => candidates.remove(0),
            // Several listed interfaces: a dongle whose mouse is on its cable (or switched
            // off) sits next to the one that answers. Take the first that does; when none
            // does (mouse asleep), the first listed, as before.
            _ => match candidates.iter().position(|m| m.firmware().is_ok()) {
                Some(i) => candidates.swap_remove(i),
                None => candidates.remove(0),
            },
        };
        crate::set_mouse_config(mouse.spec.config);
        Ok(mouse)
    }

    fn from_hid(hid: HidDevice, allow_unverified: bool) -> Mouse {
        let spec = DeviceSpec::lookup(hid.info.product_id);
        Mouse { hid, spec, allow_unverified, profile: std::cell::Cell::new(1) }
    }

    /// What the table says about this mouse.
    pub fn spec(&self) -> &DeviceSpec {
        &self.spec
    }

    /// False when every write would be refused (unverified mouse, flag not set).
    pub fn writes_allowed(&self) -> bool {
        self.spec.write_refusal(self.allow_unverified).is_none()
    }

    pub fn product_id(&self) -> u16 {
        self.hid.info.product_id
    }

    pub fn vendor_id(&self) -> u16 {
        self.hid.info.vendor_id
    }

    /// The onboard profile this handle writes to and reads from (1 until [`Mouse::select_profile`]).
    pub fn profile(&self) -> u8 {
        self.profile.get()
    }

    pub fn is_wireless(&self) -> bool {
        self.spec.wireless
    }

    pub fn path(&self) -> &str {
        &self.hid.info.path
    }

    pub fn set_wait_ms(&mut self, ms: u64) {
        self.hid.wait = Duration::from_millis(ms);
    }

    /// Send a command. A request addressed to [`TID_MOUSE`] is re-addressed to
    /// this mouse's transaction id (a no-op on current firmware, which uses that one); writes to an
    /// unverified mouse are refused, see [`DeviceSpec::write_refusal`].
    pub fn raw(&self, req: &Report) -> Result<Report> {
        if req.transaction_id == TID_MOUSE && self.spec.transaction_id != TID_MOUSE {
            let mut r = req.clone();
            r.transaction_id = self.spec.transaction_id;
            return self.raw_as_is(&r);
        }
        self.raw_as_is(req)
    }

    /// Send a command with the transaction id it carries, no re-addressing.
    pub fn raw_as_is(&self, req: &Report) -> Result<Report> {
        guard_write(&self.spec, self.allow_unverified, req)?;
        self.hid.transact(req)
    }

    fn need_dongle(&self) -> Result<()> {
        if self.is_wireless() { Ok(()) } else { Err(HidError::Unsupported("only available through the wireless dongle")) }
    }

    fn need_pairing(&self) -> Result<()> {
        self.need_dongle()?;
        if self.spec.pairing { Ok(()) } else { Err(HidError::Unsupported("dongle pairing is not known to apply to this mouse")) }
    }

    // --- identity ---

    pub fn firmware(&self) -> Result<(u8, u8)> {
        let r = self.raw(&cmd::firmware_version(TID_MOUSE))?;
        Ok((r.arguments[0], r.arguments[1]))
    }

    pub fn serial(&self) -> Result<String> {
        let r = self.raw(&cmd::serial(TID_MOUSE))?;
        Ok(ascii(&r.arguments[..22]))
    }

    pub fn dongle_firmware(&self) -> Result<(u8, u8)> {
        self.need_dongle()?;
        let r = match self.spec.dongle {
            DongleKind::Gen2Dome => self.raw(&cmd::dongle_gen2_firmware())?,
            _ => match self.raw(&cmd::firmware_version(TID_DONGLE)) {
                Ok(r) => r,
                Err(_) => self.raw(&cmd::dongle_gen2_firmware())?,
            },
        };
        let a = parse_payload(&r, 2, "dongle firmware")?;
        Ok((a[0], a[1]))
    }

    pub fn dongle_serial(&self) -> Result<String> {
        self.need_dongle()?;
        let r = match self.spec.dongle {
            DongleKind::Gen2Dome => self.raw(&cmd::dongle_gen2_serial())?,
            _ => match self.raw(&cmd::serial(TID_DONGLE)) {
                Ok(r) => r,
                Err(_) => self.raw(&cmd::dongle_gen2_serial())?,
            },
        };
        let a = parse_payload(&r, 22, "dongle serial")?;
        Ok(ascii(&a[..22]))
    }

    pub fn device_mode(&self) -> Result<u8> {
        Ok(self.raw(&cmd::get_device_mode())?.arguments[0])
    }

    pub fn set_device_mode(&self, mode: u8) -> Result<()> {
        self.raw(&cmd::set_device_mode(mode)).map(drop)
    }

    // --- power ---

    /// Battery level and charging state. A mouse without a battery answers a
    /// firmware read instead, so this still tells an answering mouse from a
    /// missing one, and reports itself as wired.
    pub fn battery(&self) -> Result<Battery> {
        if !self.spec.battery {
            self.firmware()?;
            return Ok(Battery { percent: 100, charging: false, wired: true });
        }
        let level = self.raw(&cmd::get_battery_level())?;
        let charging = self.raw(&cmd::get_charging_status())?;
        Ok(Battery { percent: cmd::byte_to_percent(level.arguments[1]), charging: charging.arguments[1] != 0, wired: false })
    }

    /// Seconds of inactivity before the mouse sleeps (60..=900).
    pub fn idle_time(&self) -> Result<u16> {
        let r = self.raw(&cmd::get_idle_time())?;
        Ok(u16::from_be_bytes([r.arguments[0], r.arguments[1]]))
    }

    pub fn set_idle_time(&self, seconds: u16) -> Result<()> {
        self.raw(&cmd::set_idle_time(seconds)).map(drop)
    }

    /// Low battery warning threshold in percent.
    pub fn low_battery_threshold(&self) -> Result<u8> {
        Ok(cmd::byte_to_percent(self.raw(&cmd::get_low_battery_threshold())?.arguments[0]))
    }

    pub fn set_low_battery_threshold(&self, percent: u8) -> Result<()> {
        self.raw(&cmd::set_low_battery_threshold(percent)).map(drop)
    }

    // --- performance ---

    pub fn dpi(&self) -> Result<(u16, u16)> {
        let a = self.raw(&cmd::get_dpi_xy())?.arguments;
        Ok((u16::from_be_bytes([a[1], a[2]]), u16::from_be_bytes([a[3], a[4]])))
    }

    pub fn set_dpi(&self, x: u16, y: u16) -> Result<()> {
        let max = self.spec.dpi_max;
        self.raw(&cmd::set_dpi_xy(x.clamp(DPI_MIN, max), y.clamp(DPI_MIN, max))).map(drop)
    }

    pub fn dpi_stages(&self) -> Result<DpiStages> {
        let r = self.raw(&cmd::get_dpi_stages())?;
        let a = &r.arguments;
        let active = a[1];
        let count = (a[2] as usize).min(5);
        let mut stages = Vec::with_capacity(count);
        for i in 0..count {
            let o = 3 + i * 7;
            stages.push((u16::from_be_bytes([a[o + 1], a[o + 2]]), u16::from_be_bytes([a[o + 3], a[o + 4]])));
        }
        Ok(DpiStages { active, stages })
    }

    pub fn set_dpi_stages(&self, active: u8, stages: &[(u16, u16)]) -> Result<()> {
        let max = self.spec.dpi_max;
        let stages: Vec<(u16, u16)> = stages.iter().take(5).map(|(x, y)| (x.clamp(&DPI_MIN, &max).to_owned(), y.clamp(&DPI_MIN, &max).to_owned())).collect();
        let active = active.clamp(1, stages.len().max(1) as u8);
        self.raw(&cmd::set_dpi_stages(active, &stages))?;
        // Keep the live DPI in sync with the selected stage, like Synapse does.
        if let Some((x, y)) = stages.get(active as usize - 1) {
            self.set_dpi(*x, *y)?;
        }
        Ok(())
    }

    pub fn polling_rate(&self) -> Result<u16> {
        match self.spec.polling {
            PollingStyle::HyperPolling => {
                let a = self.raw(&cmd::get_polling_rate())?.arguments;
                cmd::polling_from_code(a[1]).or_else(|| cmd::polling_from_code(a[0])).ok_or(HidError::NoResponse)
            }
            PollingStyle::Classic => {
                let a = self.raw(&cmd::get_polling_rate_classic())?.arguments;
                Ok(cmd::polling_from_code_classic(a[0]).unwrap_or(0))
            }
        }
    }

    pub fn set_polling_rate(&self, hz: u16) -> Result<()> {
        if !self.spec.polling_rates.contains(&hz) {
            return Err(HidError::Invalid(format!("polling rate must be one of {} on the {}", self.spec.rates_text(), self.spec.name)));
        }
        match self.spec.polling {
            PollingStyle::HyperPolling => {
                self.raw(&cmd::set_polling_rate(hz, 0x00))?;
                self.raw(&cmd::set_polling_rate(hz, 0x01))?;
                Ok(())
            }
            PollingStyle::Classic => {
                let code = cmd::polling_code_classic(hz).ok_or(HidError::Unsupported("the classic polling command has no code for that rate"))?;
                self.raw(&cmd::set_polling_rate_classic(code)).map(drop)
            }
        }
    }

    // --- dongle ---

    pub fn dongle_led_mode(&self) -> Result<u8> {
        self.need_dongle()?;
        if self.spec.dongle != DongleKind::HyperPolling {
            return Err(HidError::Unsupported("single HyperPolling LED mode is not used on this dongle"));
        }
        Ok(self.raw(&cmd::get_dongle_led_mode())?.arguments[0])
    }

    pub fn set_dongle_led_mode(&self, mode: u8) -> Result<()> {
        self.need_dongle()?;
        if self.spec.dongle != DongleKind::HyperPolling {
            return Err(HidError::Unsupported("single HyperPolling LED mode is not used on this dongle"));
        }
        self.raw(&cmd::set_dongle_led_mode(mode)).map(drop)
    }

    /// 3-LED indicator slots (left, center, right) of the HyperSpeed Gen-2 dome dongle.
    pub fn dongle_leds(&self) -> Result<[u8; 3]> {
        self.need_dongle()?;
        if self.spec.dongle != DongleKind::Gen2Dome {
            return Err(HidError::Unsupported("3-LED dongle indicator is only available on the Gen-2 dome dongle"));
        }
        parse_dongle_leds(&self.raw(&cmd::get_dongle_leds())?)
    }

    pub fn set_dongle_leds(&self, leds: [u8; 3]) -> Result<()> {
        self.need_dongle()?;
        if self.spec.dongle != DongleKind::Gen2Dome {
            return Err(HidError::Unsupported("3-LED dongle indicator is only available on the Gen-2 dome dongle"));
        }
        for &b in &leds {
            if dongle_led_slot_name_checked(b).is_none() {
                return Err(HidError::Invalid(format!("unknown dongle LED slot code 0x{b:02x}")));
            }
        }
        self.raw(&cmd::set_dongle_leds(leds)).map(drop)
    }

    /// Write Gen-2 dome dongle LED slots only when they differ from what the dongle reports.
    pub fn sync_dongle_leds(&self, want: [u8; 3]) -> Result<bool> {
        let cur = self.dongle_leds()?;
        if cur == want {
            return Ok(false);
        }
        self.set_dongle_leds(want)?;
        Ok(true)
    }

    pub fn dongle_led_brightness(&self) -> Result<u8> {
        self.need_dongle()?;
        if self.spec.dongle != DongleKind::Gen2Dome {
            return Err(HidError::Unsupported("dongle LED brightness is only available on the Gen-2 dome dongle"));
        }
        parse_dongle_brightness(&self.raw(&cmd::get_dongle_led_brightness())?)
    }

    pub fn set_dongle_led_brightness(&self, level: u8) -> Result<()> {
        self.need_dongle()?;
        if self.spec.dongle != DongleKind::Gen2Dome {
            return Err(HidError::Unsupported("dongle LED brightness is only available on the Gen-2 dome dongle"));
        }
        if level > 4 {
            return Err(HidError::Invalid(format!("dongle LED brightness must be 0..=4 (got {level})")));
        }
        self.raw(&cmd::set_dongle_led_brightness(level)).map(drop)
    }

    /// Write Gen-2 dome dongle LED brightness only when it differs from what the dongle reports.
    pub fn sync_dongle_led_brightness(&self, want: u8) -> Result<bool> {
        let cur = self.dongle_led_brightness()?;
        if cur == want {
            return Ok(false);
        }
        self.set_dongle_led_brightness(want)?;
        Ok(true)
    }

    // --- sensor ---

    fn sensor_lock() -> std::sync::MutexGuard<'static, ()> {
        static SENSOR_SEQ: std::sync::Mutex<()> = std::sync::Mutex::new(());
        SENSOR_SEQ.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn sensor_unlocked(&self) -> Result<SensorStatus> {
        let lift_r = self.raw(&sensor::get_lift())?;
        let lift = parse_payload(&lift_r, 4, "sensor lift")?;
        if lift[..2] != sensor::SENSOR_HDR {
            return Err(HidError::Invalid(format!("unexpected sensor lift header {:02x?}", &lift[..2])));
        }
        let state_r = self.raw(&sensor::get_state())?;
        let state = parse_payload(&state_r, 3, "sensor state")?;
        if state[..2] != sensor::SENSOR_HDR {
            return Err(HidError::Invalid(format!("unexpected sensor state header {:02x?}", &state[..2])));
        }
        let use_calibration = state[2] != 0;
        let asym_levels = if self.spec.asym_readback && use_calibration && lift[2] == sensor::LIFT_MODE_ASYMMETRIC {
            Some(parse_asym_config(&self.raw(&sensor::get_config())?)?)
        } else {
            None
        };
        Ok(SensorStatus { use_calibration, mode: lift[2], height: lift[3], asym_levels })
    }

    pub fn sensor(&self) -> Result<SensorStatus> {
        let _seq = Self::sensor_lock();
        self.sensor_unlocked()
    }

    fn apply_tracking_unlocked(&self, plan: &SensorPlan) -> Result<()> {
        if !self.spec.sensor_tracking {
            return Err(HidError::Unsupported("the sensor tracking page is not known to apply to this mouse"));
        }
        // Firmware 1.11 answers "Failure" to a lift-setting write that repeats the
        // mode it already has, although it applies it, so the status of that one
        // write is not trusted: the result is read back at the end instead.
        let tolerant = |r: &crate::protocol::Report| match self.raw(r) {
            Err(crate::hid::HidError::Status(crate::protocol::Status::Failure)) => Ok(()),
            other => other.map(drop),
        };
        if plan.state {
            // Asymmetric cut-off: state on, lift mode 4, then the two level bytes.
            self.raw(&sensor::set_state(true))?;
            tolerant(&sensor::set_lift(plan.lift.0, plan.lift.1))?;
            if let Some(cfg) = &plan.config {
                self.raw(&sensor::set_config(cfg))?;
            }
        } else {
            // Smart Tracking: state off first; with the state on the firmware
            // drops a mode-1 write to height 0 instead of applying it.
            self.raw(&sensor::set_state(false))?;
            tolerant(&sensor::set_lift(plan.lift.0, plan.lift.1))?;
        }
        let s = self.sensor_unlocked()?;
        if !sensor_post_write_ok(&s, plan, self.spec.asym_readback) {
            return Err(crate::hid::HidError::Status(crate::protocol::Status::Failure));
        }
        Ok(())
    }

    /// Write a tracking plan in the order Synapse's engine for this mouse does.
    pub fn apply_tracking(&self, plan: &SensorPlan) -> Result<()> {
        let _seq = Self::sensor_lock();
        self.apply_tracking_unlocked(plan)
    }

    /// Read sensor tracking and write `plan` only when it differs, under one sequence lock.
    pub fn sync_tracking(&self, plan: &SensorPlan) -> Result<bool> {
        if !self.spec.sensor_tracking {
            return Err(HidError::Unsupported("the sensor tracking page is not known to apply to this mouse"));
        }
        let _seq = Self::sensor_lock();
        let cur = self.sensor_unlocked()?;
        if sensor_already_matches(&cur, plan, self.spec.asym_readback) {
            return Ok(false);
        }
        self.apply_tracking_unlocked(plan)?;
        Ok(true)
    }

    /// Sensor rotation: (enabled, degrees).
    pub fn rotation(&self) -> Result<(bool, i8)> {
        if !self.spec.supports_rotation {
            return Err(HidError::Unsupported("sensor rotation is not supported on this mouse"));
        }
        let a = self.raw(&sensor::get_angle())?.arguments;
        Ok((a[1] != 0, a[2] as i8))
    }

    pub fn set_rotation(&self, degrees: i8) -> Result<()> {
        if !self.spec.supports_rotation {
            return Err(HidError::Unsupported("sensor rotation is not supported on this mouse"));
        }
        self.raw(&sensor::set_angle(degrees != 0, degrees)).map(drop)
    }

    // --- lighting ---

    fn led(&self, zone: &str) -> Result<u8> {
        self.spec.lighting.iter().find(|(n, _)| *n == zone).map(|(_, l)| *l).ok_or(HidError::Unsupported("this mouse has no such lighting zone"))
    }

    /// A zone's effect; None when it runs one freenapse does not write (reactive, wave...).
    pub fn lighting(&self, zone: &str) -> Result<Option<lighting::Effect>> {
        let a = self.raw(&lighting::get_effect(self.led(zone)?))?.arguments;
        Ok(lighting::decode_effect(&a))
    }

    pub fn set_lighting(&self, zone: &str, e: lighting::Effect) -> Result<()> {
        self.raw(&lighting::set_effect(self.led(zone)?, e)).map(drop)
    }

    /// A zone's brightness, 0..=100 %.
    pub fn brightness(&self, zone: &str) -> Result<u8> {
        Ok(cmd::byte_to_percent(self.raw(&lighting::get_brightness(self.led(zone)?))?.arguments[2]))
    }

    pub fn set_brightness(&self, zone: &str, percent: u8) -> Result<()> {
        self.raw(&lighting::set_brightness(self.led(zone)?, cmd::percent_to_byte(percent.min(100)))).map(drop)
    }

    /// Raw reads of every sensor register we know about, for diagnostics.
    pub fn sensor_dump(&self) -> Vec<(&'static str, Result<Vec<u8>>)> {
        let regs: [(&str, Report); 4] = [
            ("state   0b/83", sensor::get_state()),
            ("lift    0b/8b", sensor::get_lift()),
            ("config  0b/85", sensor::get_config()),
            ("angle   0b/94", sensor::get_angle()),
        ];
        regs.into_iter()
            .map(|(name, req)| {
                let n = req.data_size as usize;
                (name, self.raw(&req).map(|r| r.arguments[..n].to_vec()))
            })
            .collect()
    }

    // --- dynamic sensitivity (onboard acceleration) ---

    /// (enabled, mode) stored in the mouse.
    pub fn acceleration(&self) -> Result<(bool, accel::Mode)> {
        if !self.spec.supports_acceleration {
            return Err(HidError::Unsupported("Dynamic Sensitivity is not supported on this mouse"));
        }
        let p = self.profile();
        let st = self.raw(&accel::get_state(p))?.arguments;
        let md = self.raw(&accel::get_mode(p))?.arguments;
        Ok((st[1] != 0, accel::Mode::from_byte(md[1]).unwrap_or(accel::Mode::Classic)))
    }

    /// Write the acceleration settings in Synapse's order: state, then (when
    /// on) the mode, then the custom curve in 32 packets of 8 samples.
    pub fn set_acceleration(&self, on: bool, mode: accel::Mode, curve: Option<&[f32; accel::POINTS]>) -> Result<()> {
        if !self.spec.supports_acceleration {
            return Err(HidError::Unsupported("Dynamic Sensitivity is not supported on this mouse"));
        }
        let p = self.profile();
        self.raw(&accel::set_state(p, on))?;
        if !on {
            return Ok(());
        }
        self.raw(&accel::set_mode(p, mode))?;
        if mode == accel::Mode::Custom {
            if let Some(c) = curve {
                for (i, chunk) in c.chunks(accel::CHUNK).enumerate() {
                    self.raw(&accel::set_curve_chunk(p, mode, (i * accel::CHUNK) as u8, chunk))?;
                    thread::sleep(Duration::from_millis(15));
                }
            }
        }
        Ok(())
    }

    /// Read `count` samples of the stored custom curve starting at `offset`.
    pub fn acceleration_curve_chunk(&self, offset: u8, count: u8) -> Result<Vec<f32>> {
        if !self.spec.supports_acceleration {
            return Err(HidError::Unsupported("Dynamic Sensitivity is not supported on this mouse"));
        }
        let a = self.raw(&accel::get_curve_chunk(self.profile(), accel::Mode::Custom, offset, count))?.arguments;
        let n = (a[4] as usize).min(accel::CHUNK);
        Ok((0..n).map(|i| f32::from_le_bytes([a[5 + i * 4], a[6 + i * 4], a[7 + i * 4], a[8 + i * 4]])).collect())
    }

    /// The whole stored custom curve (32 reads).
    pub fn acceleration_curve(&self) -> Result<[f32; accel::POINTS]> {
        let mut out = [0f32; accel::POINTS];
        for i in (0..accel::POINTS).step_by(accel::CHUNK) {
            let v = self.acceleration_curve_chunk(i as u8, accel::CHUNK as u8)?;
            out[i..i + v.len()].copy_from_slice(&v);
        }
        Ok(out)
    }

    // --- onboard profiles ---

    /// (existing profiles, maximum the mouse supports).
    pub fn profiles(&self) -> Result<(u8, u8)> {
        let count = self.raw(&cmd::get_profile_count())?.arguments[0];
        let max = self.raw(&cmd::get_max_profiles())?.arguments[0];
        Ok((count, max))
    }

    /// Ids of the profiles stored in the mouse.
    pub fn profile_list(&self) -> Result<Vec<u8>> {
        let a = self.raw(&cmd::get_profile_list())?.arguments;
        let n = (a[0] as usize).min(5);
        Ok(a[1..1 + n].to_vec())
    }

    /// The profile the mouse itself reports as active (a mouse that holds a single profile answers Failure).
    pub fn active_profile(&self) -> Result<u8> {
        Ok(self.raw(&cmd::get_active_profile())?.arguments[0])
    }

    /// Make `id` the profile this handle addresses and the mouse's active one:
    /// creates the slot when it is missing and the mouse has room, and skips
    /// the activation when the mouse holds that single profile already.
    pub fn select_profile(&self, id: u8) -> Result<()> {
        if !(1..=5).contains(&id) {
            return Err(HidError::Unsupported("onboard profile id must be 1..5"));
        }
        let (_, max) = self.profiles()?;
        let list = self.profile_list()?;
        if !list.contains(&id) {
            if id > max {
                return Err(HidError::Unsupported("the mouse does not have that many onboard profiles"));
            }
            self.raw(&cmd::create_profile(id))?;
        }
        if !(list.len() == 1 && list[0] == id) {
            self.raw(&cmd::set_active_profile(id))?;
        }
        self.profile.set(id);
        Ok(())
    }

    // --- buttons ---

    pub fn button(&self, profile: u8, button: u8, hypershift: bool) -> Result<ButtonFunction> {
        let r = self.raw(&cmd::get_button_function(profile, button, hypershift))?;
        parse_button_function(&r, profile, button)
    }

    pub fn set_button(&self, profile: u8, button: u8, hypershift: bool, f: ButtonFunction) -> Result<()> {
        self.raw(&cmd::set_button_function(profile, button, hypershift, &f.0)).map(drop)
    }

    // --- dongle pairing ---

    /// Devices the dongle is paired with.
    pub fn paired_devices(&self) -> Result<Vec<PairedDevice>> {
        self.need_pairing()?;
        let a = self.raw(&cmd::get_paired_devices(TID_DONGLE))?.arguments;
        let n = (a[0] as usize).min(8);
        // Empty slots are reported as product id 0xffff.
        Ok((0..n)
            .map(|i| PairedDevice { status: a[1 + 3 * i], product_id: u16::from_be_bytes([a[2 + 3 * i], a[3 + 3 * i]]) })
            .filter(|d| d.product_id != 0xffff && d.product_id != 0)
            .collect())
    }

    /// Put the dongle in pairing mode for a device with this product id. Returns the mode byte it answers.
    pub fn start_pairing(&self, product_id: u16) -> Result<u8> {
        self.need_pairing()?;
        Ok(self.raw(&cmd::set_pairing_mode(TID_DONGLE, cmd::PAIRING_START, product_id))?.arguments[0])
    }

    pub fn cancel_pairing(&self, product_id: u16) -> Result<()> {
        self.need_pairing()?;
        self.raw(&cmd::set_pairing_mode(TID_DONGLE, cmd::PAIRING_CANCEL, product_id)).map(drop)
    }

    /// Forget the pairing with the device that has this product id.
    pub fn unpair(&self, product_id: u16) -> Result<()> {
        self.need_pairing()?;
        self.raw(&cmd::unpair(TID_DONGLE, product_id)).map(drop)
    }

    pub fn auto_pairing(&self) -> Result<bool> {
        self.need_pairing()?;
        Ok(self.raw(&cmd::get_auto_pairing(TID_DONGLE))?.arguments[0] != 0)
    }

    pub fn set_auto_pairing(&self, on: bool) -> Result<()> {
        self.need_pairing()?;
        self.raw(&cmd::set_auto_pairing(TID_DONGLE, on)).map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_lookup() {
        let wired = DeviceSpec::lookup(0x00C0);
        assert_eq!(wired.name, "Razer Viper V3 Pro (wired)");
        assert!(!wired.wireless && !wired.pairing && wired.verified);
        assert_eq!((wired.polling, wired.polling_rates), (PollingStyle::Classic, &RATES_CLASSIC[..]));
        let wireless = DeviceSpec::lookup(0x00C1);
        assert_eq!(wireless.name, "Razer Viper V3 Pro (wireless)");
        assert!(wireless.wireless && wireless.pairing && wireless.sensor_tracking && wireless.verified);
        assert_eq!(
            (wireless.polling, wireless.polling_rates, wireless.dpi_max, wireless.transaction_id),
            (PollingStyle::HyperPolling, &RATES_HYPERPOLLING[..], 35000, TID_MOUSE)
        );
        assert_eq!(wireless.rates_text(), "125/250/500/1000/2000/4000/8000");
        assert!(wireless.dpi_ok(35000) && !wireless.dpi_ok(35001) && !wireless.dpi_ok(99));
        // Every row has a distinct product id and a non-empty rate list.
        for list in BUTTON_LISTS {
            for (i, b) in list.iter().enumerate() {
                assert!(list[..i].iter().all(|o| o.code != b.code && o.name != b.name), "duplicate button {}", b.name);
                // A name means the same button on every mouse that has it.
                assert_eq!(physical_button(b.name).map(|p| p.code), Some(b.code), "{}", b.name);
            }
        }
        assert_eq!(&BASILISK_V2_BUTTONS[..7], &BUTTONS[..7], "the art and the config expect the seven clicks first");
        assert_eq!(&BASILISK_V3_BUTTONS[..7], &BUTTONS[..7]);
        // The Basilisk V3 (issue 5): listed from a probe, writes confirmed by its owner.
        let v3 = DeviceSpec::find(0x0099).unwrap();
        assert!(v3.verified && !v3.wireless && !v3.battery && !v3.supports_acceleration && !v3.supports_rotation);
        assert_eq!((v3.dpi_max, v3.polling, v3.buttons.len()), (26000, PollingStyle::Classic, 13));
        assert_eq!(crate::config_file_name(v3.config), "config-basilisk-v3.toml");
        assert_eq!(button_name(0x6a), Some("scroll-mode"));
        assert_eq!(v3.buttons.iter().find(|b| b.name == "dpi").map(|b| b.default), Some(ButtonFunction::dpi_cycle()));
        assert_eq!(BUTTONS[7], PhysicalButton { name: "dpi", code: 0x60, default: ButtonFunction::dpi_cycle() });
        assert_eq!(button_name(0x60), Some("dpi"));
        assert_eq!(button_name(0x0b), Some("dpi-up"));
        assert_eq!(button_name(0x61), None);
        assert_eq!(ButtonFunction::dpi_cycle().describe(), "DPI cycle");
        // The Viper V3 Pro SE (issue 8): classic polling on both rows, both verified by its owner.
        for pid in [0x00DE, 0x00DF] {
            let se = DeviceSpec::find(pid).unwrap();
            assert!(!se.sensor_tracking && !se.pairing && se.battery && se.wireless == (pid == 0x00DF) && se.verified);
            assert_eq!((se.dpi_max, se.polling, se.polling_rates, se.buttons), (35000, PollingStyle::Classic, &RATES_CLASSIC[..], BUTTONS));
            assert_eq!(crate::config_file_name(se.config), "config.toml");
        }
        // A row that names no file keeps config.toml; the Basilisk V2 has its own file and no battery.
        let b = DeviceSpec::find(0x0085).unwrap();
        assert!(b.verified && !b.battery && b.lighting.len() == 2);
        assert_eq!(crate::config_file_name(b.config), "config-basilisk-v2.toml");
        assert_eq!(crate::config_file_name(DeviceSpec::find(0x00C8).unwrap().config), "config-pro-click-v2-vertical.toml");
        // Wired and wireless Pro Click are one mouse: one config file, the wired row still unverified.
        let pc_wired = DeviceSpec::find(0x00C7).unwrap();
        assert!(!pc_wired.wireless && !pc_wired.verified && pc_wired.battery);
        assert_eq!(pc_wired.config, DeviceSpec::find(0x00C8).unwrap().config);
        assert_eq!(crate::config_file_name(DeviceSpec::any_mouse().config), "config.toml");
        assert_eq!(crate::config_file_name(DeviceSpec::find(0x00C0).unwrap().config), "config.toml");
        assert_eq!(crate::config_file_name(DeviceSpec::find(0x00C1).unwrap().config), "config.toml");
        for (i, d) in DEVICES.iter().enumerate() {
            assert!(!d.polling_rates.is_empty(), "{}", d.name);
            assert!(DEVICES[..i].iter().all(|o| o.product_id != d.product_id), "duplicate 0x{:04x}", d.product_id);
        }
        // The author's Viper V3 Pro (wired and wireless) and Basilisk V2, the Pro Click V2 Vertical (issue 4),
        // the Basilisk V3 (issue 5), the Viper V3 Pro SE (issue 8), and the DeathAdder V4 Pro (wired and wireless).
        assert_eq!(DEVICES.iter().filter(|d| d.verified).count(), 9);
        let dav4_wired = DeviceSpec::find(0x00BE).unwrap();
        assert!(dav4_wired.verified && !dav4_wired.wireless && dav4_wired.sensor_tracking && dav4_wired.is_ergonomic());
        assert_eq!((dav4_wired.dpi_max, dav4_wired.dongle), (45000, DongleKind::None));
        let dav4_wireless = DeviceSpec::find(0x00BF).unwrap();
        assert!(dav4_wireless.verified && dav4_wireless.wireless && dav4_wireless.sensor_tracking && dav4_wireless.is_ergonomic());
        assert_eq!((dav4_wireless.dpi_max, dav4_wireless.dongle), (45000, DongleKind::Gen2Dome));
    }

    #[test]
    fn placeholder_accepts_what_any_row_accepts() {
        let any = DeviceSpec::any_mouse();
        assert_eq!(DeviceSpec::default(), *any);
        assert!(any.is_placeholder() && !any.is_known());
        assert_eq!(any.name, "Razer mouse");
        assert_eq!(any.buttons, BUTTONS);
        assert_eq!(DPI_MAX_ANY, DEVICES.iter().map(|d| d.dpi_max).max().unwrap());
        for d in DEVICES {
            assert!(!d.is_placeholder(), "{}", d.name);
            assert!(any.dpi_ok(d.dpi_max), "{}", d.name);
            assert!(d.polling_rates.iter().all(|hz| any.polling_rates.contains(hz)), "{}", d.name);
            // Nothing a row has is hidden before a mouse is open.
            assert!(any.sensor_tracking >= d.sensor_tracking && any.pairing >= d.pairing && any.battery >= d.battery, "{}", d.name);
            assert!(any.supports_acceleration >= d.supports_acceleration && any.supports_rotation >= d.supports_rotation, "{}", d.name);
        }
        assert!(!DeviceSpec::unknown(0x0123).is_placeholder());
    }

    #[test]
    fn spec_unknown() {
        let u = DeviceSpec::lookup(0x0123);
        assert_eq!(u, DeviceSpec::unknown(0x0123));
        assert_eq!(u.name, "Razer mouse 0x0123");
        assert!(!u.is_known() && !u.verified && !u.wireless && !u.pairing && !u.sensor_tracking);
        assert_eq!((u.polling, u.dpi_max, u.transaction_id), (PollingStyle::Classic, 30000, TID_MOUSE));
        assert!(DeviceSpec::lookup(0x00B7).is_known());
    }

    #[test]
    fn deathadder_wired_capabilities_are_conservative() {
        let d = DeviceSpec::lookup(0x00B6);
        assert!(!d.verified && !d.wireless);
        assert!(!d.sensor_tracking, "successful sensor reads do not establish safe writes");
        assert!(!d.supports_acceleration && !d.supports_rotation);
        assert_eq!((d.transaction_id, d.polling, d.polling_rates, d.dpi_max), (0x1f, PollingStyle::Classic, &RATES_CLASSIC[..], 30000));
        assert!(matches!(guard_write(&d, false, &cmd::set_polling_rate_classic(1)), Err(HidError::Unverified(_))));
        assert!(guard_write(&d, true, &cmd::set_polling_rate_classic(1)).is_ok());
        let wireless = DeviceSpec::lookup(0x00B7);
        assert!(!wireless.verified && wireless.supports_acceleration && wireless.supports_rotation);
        assert_eq!(wireless.dpi_max, 35000);
        assert!(!wireless.sensor_tracking);
        // Unlike a row whose getters answered (the Viper V3 Pro's).
        let answered = DeviceSpec::lookup(0x00C1);
        assert!(answered.supports_acceleration && answered.supports_rotation);
    }

    #[test]
    fn unknown_device_refuses_writes() {
        let unknown = DeviceSpec::unknown(0x0123);
        let read = cmd::get_dpi_xy();
        let write = cmd::set_dpi_xy(800, 800);
        assert!(guard_write(&unknown, false, &read).is_ok());
        match guard_write(&unknown, false, &write) {
            Err(HidError::Unverified(why)) => {
                assert!(why.contains("freenapse probe") && why.contains("0x0123") && why.contains("allow_unverified"), "{why}");
            }
            other => panic!("{other:?}"),
        }
        assert!(guard_write(&unknown, true, &write).is_ok());
        // A table row that nobody verified is refused too, with its name.
        let listed = DeviceSpec::lookup(0x00B7);
        let why = listed.write_refusal(false).unwrap();
        assert!(why.contains("DeathAdder V3 Pro") && why.contains("not confirmed") || why.contains("nobody has confirmed"), "{why}");
        assert!(guard_write(&listed, false, &write).is_err());
        // Every verified row writes freely.
        for d in DEVICES.iter().filter(|d| d.verified) {
            assert!(guard_write(d, false, &write).is_ok(), "{}", d.name);
        }
    }

    #[test]
    fn v4_pro_fw120_fixtures_and_validation() {
        // Model isolation: V4 Pro enables asym_readback; Viper V3 Pro keeps it off.
        assert!(DeviceSpec::lookup(0x00BE).asym_readback && DeviceSpec::lookup(0x00BF).asym_readback);
        assert!(!DeviceSpec::lookup(0x00C0).asym_readback && !DeviceSpec::lookup(0x00C1).asym_readback);

        // Captured on DeathAdder V4 Pro wireless (1532:00BF, fw 1.20) Gen-2 dome dongle:
        // factory default left=battery (0x02), center=connection (0x01), right=polling (0x03), brightness=4.
        let mut leds_ok = Report::new(TID_DONGLE, 0x07, 0x95, 0x03).with_args(&[0x02, 0x01, 0x03]);
        leds_ok.status = 0x02;
        assert_eq!(parse_dongle_leds(&leds_ok).unwrap(), [0x02, 0x01, 0x03]);
        assert_eq!(dongle_led_slot_name_checked(0x02), Some("battery"));
        assert_eq!(dongle_led_slot_name_checked(0x01), Some("connection"));
        assert_eq!(dongle_led_slot_name_checked(0x03), Some("polling"));
        assert_eq!(dongle_led_slot_name_checked(0x09), None);

        // Truncated data_size or unknown LED code must error rather than silently coerce to "off".
        let leds_short = Report::new(TID_DONGLE, 0x07, 0x95, 0x02).with_args(&[0x02, 0x01, 0x03]);
        assert!(parse_dongle_leds(&leds_short).is_err());
        let leds_bad = Report::new(TID_DONGLE, 0x07, 0x95, 0x03).with_args(&[0x02, 0x09, 0x03]);
        assert!(parse_dongle_leds(&leds_bad).is_err());

        let br_ok = Report::new(TID_DONGLE, 0x07, 0x96, 0x01).with_args(&[0x04]);
        assert_eq!(parse_dongle_brightness(&br_ok).unwrap(), 4);
        let br_bad = Report::new(TID_DONGLE, 0x07, 0x96, 0x01).with_args(&[0x05]);
        assert!(parse_dongle_brightness(&br_bad).is_err());

        // Captured on DeathAdder V4 Pro (1532:00BF, fw 1.20) 0b/85 responses for 13/12, 20/4, 26/25:
        for ((want_lo, want_ld), raw) in [
            ((13, 12), [0x00, 0x04, 0x02, 0x0c, 0x0b, 0, 0, 0, 0, 0, 0]),
            ((20, 4), [0x00, 0x04, 0x02, 0x13, 0x03, 0, 0, 0, 0, 0, 0]),
            ((26, 25), [0x00, 0x04, 0x02, 0x19, 0x18, 0, 0, 0, 0, 0, 0]),
        ] {
            let r = Report::new(TID_MOUSE, 0x0b, 0x85, 0x0b).with_args(&raw);
            assert_eq!(parse_asym_config(&r).unwrap(), (want_lo, want_ld));
        }

        // Synthetic short packet (data_size = 4, padding byte 0 at [4]) must NOT decode as 13/1.
        let short_cfg = Report::new(TID_MOUSE, 0x0b, 0x85, 0x04).with_args(&[0x00, 0x04, 0x02, 0x0c]);
        assert!(parse_asym_config(&short_cfg).is_err());

        // Pre-write & post-write checks: V4 Pro skips identical 13/12 and rejects readback 20/19;
        // Viper V3 Pro (asym_readback = false) never skips asymmetric pre-write so 13/12 -> 20/4 writes.
        let plan_13_12 = sensor::Tracking::Asymmetric { lift: 13, land: 12 }.plan().unwrap();
        let st_match = SensorStatus { use_calibration: true, mode: 4, height: 0, asym_levels: Some((13, 12)) };
        let st_wrong = SensorStatus { use_calibration: true, mode: 4, height: 0, asym_levels: Some((20, 19)) };
        let st_none = SensorStatus { use_calibration: true, mode: 4, height: 0, asym_levels: None };
        assert!(sensor_already_matches(&st_match, &plan_13_12, true));
        assert!(!sensor_already_matches(&st_wrong, &plan_13_12, true));
        assert!(!sensor_already_matches(&st_none, &plan_13_12, true));
        assert!(!sensor_already_matches(&st_none, &plan_13_12, false), "Viper must not skip unknown asym levels");
        assert!(sensor_post_write_ok(&st_match, &plan_13_12, true));
        assert!(!sensor_post_write_ok(&st_wrong, &plan_13_12, true));
        assert!(!sensor_post_write_ok(&st_none, &plan_13_12, true));
        assert!(sensor_post_write_ok(&st_none, &plan_13_12, false));

        // Button readback validation & virtual key extraction for gesture protection:
        let mut btn_ok = cmd::get_button_function(1, 0x05, false);
        btn_ok.status = 0x02;
        btn_ok.arguments[2..10].copy_from_slice(&[0x01, 0x02, 0x02, 0x08, 0x07, 0, 0, 0]);
        let parsed = parse_button_function(&btn_ok, 1, 0x05).unwrap();
        assert_eq!(parsed, ButtonFunction::key(0x08, 0x07));
        assert!(parse_button_function(&btn_ok, 1, 0x04).is_err(), "wrong button code must fail");
        let short_btn = Report::new(TID_MOUSE, 0x02, 0x8c, 0x05).with_args(&[1, 5, 0, 2, 2]);
        assert!(parse_button_function(&short_btn, 1, 0x05).is_err(), "truncated payload must fail");

        let mut vks = Vec::new();
        parsed.push_virtual_keys(&mut vks);
        assert_eq!(vks, vec![0x5b, 0x44], "Win+D extracts VK_LWIN and 'D'");
        ButtonFunction::consumer(0x00e2).push_virtual_keys(&mut vks);
        assert_eq!(vks, vec![0x5b, 0x44], "media:mute adds no keyboard/mouse VKs");
        ButtonFunction::mouse_button(0x05).push_virtual_keys(&mut vks);
        assert_eq!(vks, vec![0x5b, 0x44, 0x06], "mouse:forward adds VK_XBUTTON2");

        for vk in [0x41u16, 0x30, 0x0d, 0x20, 0x7c, 0x87, 0x25, 0xa2, 0xa0] {
            let u = vk_usage(vk).unwrap();
            assert_eq!(usage_vk(u), Some(vk), "vk {vk:#x}");
        }
        assert_eq!(vk_usage(0x10), None);
    }
}

fn ascii(bytes: &[u8]) -> String {
    bytes.iter().take_while(|b| **b != 0).map(|b| if b.is_ascii_graphic() { *b as char } else { '?' }).collect()
}
