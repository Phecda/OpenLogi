//! Implements the legacy `BatteryStatus` feature (ID `0x1000`).

use std::{num::NonZeroU8, sync::Arc};

use crate::{
    channel::{HidppChannel, MessageListenerGuard},
    event::EventEmitter,
    feature::{
        CreatableFeature, EmittingFeature, Feature, FeatureEndpoint, event_payload,
        unified_battery::BatteryStatus,
    },
    protocol::v20::Hidpp20Error,
};

bitflags::bitflags! {
    /// Capabilities reported by `getBatteryCapability`.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    #[cfg_attr(feature = "serde", derive(serde::Serialize))]
    pub struct BatteryCapabilityFlags: u8 {
        /// The device asks the host not to show an on-screen battery indicator.
        const DISABLE_OSD = 1 << 0;
        /// The discharge level has enough resolution to be treated as mileage.
        const MILEAGE = 1 << 1;
        /// The device battery is rechargeable.
        const RECHARGEABLE = 1 << 2;
    }
}

/// Implements the legacy `BatteryStatus` / `0x1000` feature.
pub struct BatteryStatusFeature {
    endpoint: FeatureEndpoint,
    emitter: Arc<EventEmitter<BatteryEvent>>,
    _msg_listener: MessageListenerGuard,
}

impl CreatableFeature for BatteryStatusFeature {
    const ID: u16 = 0x1000;
    const STARTING_VERSION: u8 = 0;

    fn new(chan: Arc<HidppChannel>, device_index: u8, feature_index: u8) -> Self {
        let emitter = Arc::new(EventEmitter::new());
        let listener = chan.add_msg_listener_guarded({
            let emitter = Arc::clone(&emitter);
            move |raw, matched| {
                let Some((function, payload)) =
                    event_payload(raw, matched, device_index, feature_index)
                else {
                    return;
                };
                if let Some(event) = decode_event(function.to_lo(), &payload) {
                    emitter.emit(event);
                }
            }
        });
        Self {
            endpoint: FeatureEndpoint::new(chan, device_index, feature_index),
            emitter,
            _msg_listener: listener,
        }
    }
}

impl Feature for BatteryStatusFeature {}

impl EmittingFeature<BatteryEvent> for BatteryStatusFeature {
    fn listen(&self) -> async_channel::Receiver<BatteryEvent> {
        self.emitter.create_receiver()
    }
}

impl BatteryStatusFeature {
    /// Retrieves the number of discharge levels and the feature flags.
    pub async fn get_battery_capabilities(&self) -> Result<BatteryCapabilities, Hidpp20Error> {
        let payload = self.endpoint.call(1, [0; 3]).await?.extend_payload();
        Ok(decode_capabilities(&payload))
    }

    /// Retrieves the current discharge level and charging state.
    pub async fn get_battery_info(&self) -> Result<BatteryInfo, Hidpp20Error> {
        let payload = self.endpoint.call(0, [0; 3]).await?.extend_payload();
        decode_battery_info(&payload)
    }
}

/// Static capabilities of a `0x1000` battery implementation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[non_exhaustive]
pub struct BatteryCapabilities {
    /// Number of distinct discharge levels the device can report.
    pub level_count: u8,
    /// Battery capability flags, retaining unknown future bits.
    pub flags: BatteryCapabilityFlags,
}

impl BatteryCapabilities {
    /// Whether the reported discharge level has percentage-like mileage
    /// resolution rather than a small set of coarse thresholds.
    #[must_use]
    pub fn reports_percentage(self) -> bool {
        self.level_count >= 10 && self.flags.contains(BatteryCapabilityFlags::MILEAGE)
    }
}

/// Current battery information reported by `getBatteryLevelStatus`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[non_exhaustive]
pub struct BatteryInfo {
    /// Current discharge level in percentage units, or `None` when unavailable.
    pub discharge_level: Option<NonZeroU8>,
    /// Next lower discharge threshold, or `None` when unavailable.
    pub next_level: Option<NonZeroU8>,
    /// Current charging status.
    pub status: BatteryStatus,
}

/// Event emitted when a `0x1000` device reports changed battery information.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[non_exhaustive]
pub enum BatteryEvent {
    /// The current discharge level or charging state changed.
    ///
    /// This broadcast is always enabled by the device.
    InfoUpdate(BatteryInfo),
}

fn decode_capabilities(payload: &[u8]) -> BatteryCapabilities {
    BatteryCapabilities {
        level_count: payload[0],
        flags: BatteryCapabilityFlags::from_bits_retain(payload[1]),
    }
}

fn decode_battery_info(payload: &[u8]) -> Result<BatteryInfo, Hidpp20Error> {
    let discharge_level = decode_level(payload[0])?;
    let next_level = decode_level(payload[1])?;
    let status =
        BatteryStatus::try_from(payload[2]).map_err(|_| Hidpp20Error::UnsupportedResponse)?;
    Ok(BatteryInfo {
        discharge_level,
        next_level,
        status,
    })
}

fn decode_event(function: u8, payload: &[u8]) -> Option<BatteryEvent> {
    if function != 0 {
        return None;
    }
    decode_battery_info(payload)
        .ok()
        .map(BatteryEvent::InfoUpdate)
}

fn decode_level(raw: u8) -> Result<Option<NonZeroU8>, Hidpp20Error> {
    match raw {
        0 => Ok(None),
        1..=100 => Ok(NonZeroU8::new(raw)),
        _ => Err(Hidpp20Error::UnsupportedResponse),
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU8;

    use super::{
        BatteryCapabilities, BatteryCapabilityFlags, BatteryEvent, decode_battery_info,
        decode_capabilities, decode_event,
    };
    use crate::feature::unified_battery::BatteryStatus;

    #[test]
    fn decodes_capabilities_and_retains_unknown_flags() {
        let capabilities = decode_capabilities(&[
            10,
            BatteryCapabilityFlags::MILEAGE.bits()
                | BatteryCapabilityFlags::RECHARGEABLE.bits()
                | 0x80,
        ]);

        assert_eq!(capabilities.level_count, 10);
        assert!(capabilities.reports_percentage());
        assert!(
            capabilities
                .flags
                .contains(BatteryCapabilityFlags::RECHARGEABLE)
        );
        assert_eq!(capabilities.flags.bits() & 0x80, 0x80);
    }

    #[test]
    fn mileage_with_too_few_levels_is_still_coarse() {
        let capabilities = BatteryCapabilities {
            level_count: 4,
            flags: BatteryCapabilityFlags::MILEAGE,
        };
        assert!(!capabilities.reports_percentage());
    }

    #[test]
    fn decodes_levels_and_status() {
        let info = decode_battery_info(&[80, 32, 0]).unwrap();
        assert_eq!(info.discharge_level, NonZeroU8::new(80));
        assert_eq!(info.next_level, NonZeroU8::new(32));
        assert_eq!(info.status, BatteryStatus::Discharging);
    }

    #[test]
    fn zero_levels_are_unavailable() {
        let info = decode_battery_info(&[0, 0, 1]).unwrap();
        assert_eq!(info.discharge_level, None);
        assert_eq!(info.next_level, None);
        assert_eq!(info.status, BatteryStatus::Charging);
    }

    #[test]
    fn rejects_levels_above_one_hundred() {
        assert!(matches!(
            decode_battery_info(&[101, 0, 0]),
            Err(crate::protocol::v20::Hidpp20Error::UnsupportedResponse)
        ));
        assert!(matches!(
            decode_battery_info(&[80, 101, 0]),
            Err(crate::protocol::v20::Hidpp20Error::UnsupportedResponse)
        ));
    }

    #[test]
    fn rejects_unknown_status() {
        assert!(matches!(
            decode_battery_info(&[80, 32, 8]),
            Err(crate::protocol::v20::Hidpp20Error::UnsupportedResponse)
        ));
    }

    #[test]
    fn decodes_battery_status_broadcast() {
        let event = decode_event(0, &[80, 32, 0]);
        assert!(matches!(
            event,
            Some(BatteryEvent::InfoUpdate(info))
                if info.discharge_level == NonZeroU8::new(80)
                    && info.next_level == NonZeroU8::new(32)
                    && info.status == BatteryStatus::Discharging
        ));
    }

    #[test]
    fn ignores_unknown_or_invalid_broadcasts() {
        assert!(decode_event(1, &[80, 32, 0]).is_none());
        assert!(decode_event(0, &[101, 32, 0]).is_none());
        assert!(decode_event(0, &[80, 32, 8]).is_none());
    }
}
