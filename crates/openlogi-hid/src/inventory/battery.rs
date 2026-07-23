use std::sync::Arc;

use futures_lite::StreamExt as _;
use hidpp::{
    channel::HidppChannel,
    feature::{
        CreatableFeature, EmittingFeature,
        battery_status::{
            BatteryEvent as StatusBatteryEvent, BatteryInfo as StatusBatteryInfo,
            BatteryStatusFeature,
        },
        unified_battery::{
            BatteryEvent as UnifiedBatteryEvent, BatteryInfo as UnifiedBatteryInfo,
            UnifiedBatteryFeature,
        },
    },
};
use openlogi_core::device::{BatteryInfo, BatteryLevel};
use tokio::sync::mpsc;
use tracing::debug;

use crate::mappings::{map_battery_level, map_battery_status};
use crate::route::DeviceRoute;

/// A battery broadcast associated with the route that emitted it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatteryUpdate {
    /// Device whose battery state changed.
    pub route: DeviceRoute,
    /// Fresh battery information carried by the HID++ broadcast.
    pub battery: BatteryInfo,
}

#[derive(Clone, Copy)]
pub(super) struct BatteryEventContext<'a> {
    route: Option<&'a DeviceRoute>,
    events: Option<&'a mpsc::UnboundedSender<BatteryUpdate>>,
}

impl<'a> BatteryEventContext<'a> {
    pub(super) const fn new(
        route: Option<&'a DeviceRoute>,
        events: Option<&'a mpsc::UnboundedSender<BatteryUpdate>>,
    ) -> Self {
        Self { route, events }
    }

    #[cfg(test)]
    const fn none() -> Self {
        Self {
            route: None,
            events: None,
        }
    }
}

/// Battery feature selected by a successful full probe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BatteryEndpoint {
    /// HID++ `UnifiedBattery` (`0x1004`).
    Unified { percentage: bool },
    /// Legacy HID++ `BatteryStatus` (`0x1000`).
    Status { percentage: bool },
}

#[derive(Clone)]
pub(super) struct BatteryHandle {
    endpoint: BatteryEndpoint,
    feature: Arc<BatteryFeature>,
}

enum BatteryFeature {
    Unified(UnifiedBatteryFeature),
    Status(BatteryStatusFeature),
}

impl BatteryHandle {
    fn unified(
        feature: UnifiedBatteryFeature,
        percentage: bool,
        route: Option<&DeviceRoute>,
        events: Option<&mpsc::UnboundedSender<BatteryUpdate>>,
    ) -> Self {
        spawn_event_forwarder(feature.listen(), route, events, move |event| match event {
            UnifiedBatteryEvent::InfoUpdate(info) => Some(map_unified_battery(info, percentage)),
            _ => None,
        });
        Self {
            endpoint: BatteryEndpoint::Unified { percentage },
            feature: Arc::new(BatteryFeature::Unified(feature)),
        }
    }

    fn status(
        feature: BatteryStatusFeature,
        percentage: bool,
        route: Option<&DeviceRoute>,
        events: Option<&mpsc::UnboundedSender<BatteryUpdate>>,
    ) -> Self {
        spawn_event_forwarder(feature.listen(), route, events, move |event| match event {
            StatusBatteryEvent::InfoUpdate(info) => Some(map_status_battery(info, percentage)),
            _ => None,
        });
        Self {
            endpoint: BatteryEndpoint::Status { percentage },
            feature: Arc::new(BatteryFeature::Status(feature)),
        }
    }

    #[cfg(test)]
    fn endpoint(&self) -> BatteryEndpoint {
        self.endpoint
    }
}

fn spawn_event_forwarder<T: Send + 'static>(
    receiver: impl futures_lite::Stream<Item = T> + Send + 'static,
    route: Option<&DeviceRoute>,
    events: Option<&mpsc::UnboundedSender<BatteryUpdate>>,
    map: impl Fn(T) -> Option<BatteryInfo> + Send + 'static,
) {
    let (Some(route), Some(events)) = (route.cloned(), events.cloned()) else {
        return;
    };
    tokio::spawn(async move {
        futures_lite::pin!(receiver);
        while let Some(event) = receiver.next().await {
            let Some(battery) = map(event) else {
                continue;
            };
            if events
                .send(BatteryUpdate {
                    route: route.clone(),
                    battery,
                })
                .is_err()
            {
                break;
            }
        }
    });
}

/// Runtime indices of battery features found in the enumerated feature table.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct BatteryFeatureIndices {
    unified: Option<u8>,
    status: Option<u8>,
}

/// Read just the battery through the endpoint selected by the full probe. This
/// is exactly one round-trip, with no `Device::new` ping, capability read, or
/// feature-table walk. `None` means the device did not answer or returned an
/// unsupported payload.
pub(super) async fn read_battery(handle: &BatteryHandle) -> Option<BatteryInfo> {
    match (&*handle.feature, handle.endpoint) {
        (BatteryFeature::Unified(feature), BatteryEndpoint::Unified { percentage, .. }) => feature
            .get_battery_info()
            .await
            .ok()
            .map(|info| map_unified_battery(info, percentage)),
        (BatteryFeature::Status(feature), BatteryEndpoint::Status { percentage, .. }) => feature
            .get_battery_info()
            .await
            .ok()
            .map(|info| map_status_battery(info, percentage)),
        _ => None,
    }
}

fn map_unified_battery(info: UnifiedBatteryInfo, percentage: bool) -> BatteryInfo {
    map_unified_battery_fields(
        info.charging_percentage,
        info.level,
        info.status,
        percentage,
    )
}

fn map_unified_battery_fields(
    charging_percentage: u8,
    level: hidpp::feature::unified_battery::BatteryLevel,
    status: hidpp::feature::unified_battery::BatteryStatus,
    percentage: bool,
) -> BatteryInfo {
    BatteryInfo {
        percentage: percentage
            .then_some(charging_percentage)
            .filter(|value| *value <= 100),
        level: map_battery_level(level),
        status: map_battery_status(status),
    }
}

fn map_status_battery(info: StatusBatteryInfo, percentage: bool) -> BatteryInfo {
    map_status_battery_fields(
        info.discharge_level.map(std::num::NonZeroU8::get),
        info.status,
        percentage,
    )
}

fn map_status_battery_fields(
    discharge_level: Option<u8>,
    status: hidpp::feature::unified_battery::BatteryStatus,
    percentage: bool,
) -> BatteryInfo {
    BatteryInfo {
        percentage: discharge_level.filter(|_| percentage),
        level: map_status_level(discharge_level),
        status: map_battery_status(status),
    }
}

fn map_status_level(level: Option<u8>) -> BatteryLevel {
    match level {
        Some(1..=10) => BatteryLevel::Critical,
        Some(11..=29) => BatteryLevel::Low,
        Some(30..=80) => BatteryLevel::Good,
        Some(81..=100) => BatteryLevel::Full,
        Some(_) | None => BatteryLevel::Unknown,
    }
}

/// Runtime indices of the battery features in an enumerated feature-ID table.
/// The table is 1-based because index 0 is the implicit root feature omitted by
/// enumeration.
pub(super) fn battery_feature_indices(ids: impl IntoIterator<Item = u16>) -> BatteryFeatureIndices {
    let mut indices = BatteryFeatureIndices::default();
    for (position, id) in ids.into_iter().enumerate() {
        let Some(feature_index) = u8::try_from(position + 1).ok() else {
            break;
        };
        match id {
            UnifiedBatteryFeature::ID if indices.unified.is_none() => {
                indices.unified = Some(feature_index);
            }
            BatteryStatusFeature::ID if indices.status.is_none() => {
                indices.status = Some(feature_index);
            }
            _ => {}
        }
    }
    indices
}

/// Probe the available battery protocols and select the endpoint used by later
/// inventory ticks. `0x1004` wins when its status read succeeds; a failed
/// status read falls back to `0x1000`. Capability failures are conservative:
/// the status is still used, but without exposing a percentage.
pub(super) async fn probe_battery(
    channel: &Arc<HidppChannel>,
    slot: u8,
    indices: BatteryFeatureIndices,
    event_context: BatteryEventContext<'_>,
) -> (Option<BatteryInfo>, Option<BatteryHandle>) {
    let mut failed_unified = None;
    if let Some(feature_index) = indices.unified {
        let feature = UnifiedBatteryFeature::new(Arc::clone(channel), slot, feature_index);
        let percentage = match feature.get_battery_capabilities().await {
            Ok(capabilities) => capabilities.percentage,
            Err(error) => {
                debug!(slot, error = ?error, "UnifiedBattery capability read failed");
                false
            }
        };
        match feature.get_battery_info().await {
            Ok(info) => {
                let battery = map_unified_battery(info, percentage);
                let handle = BatteryHandle::unified(
                    feature,
                    percentage,
                    event_context.route,
                    event_context.events,
                );
                return (Some(battery), Some(handle));
            }
            Err(error) => {
                debug!(slot, error = ?error, "UnifiedBattery status read failed");
                failed_unified = Some(BatteryHandle::unified(
                    feature,
                    percentage,
                    event_context.route,
                    event_context.events,
                ));
            }
        }
    }

    if let Some(feature_index) = indices.status {
        let feature = BatteryStatusFeature::new(Arc::clone(channel), slot, feature_index);
        let percentage = match feature.get_battery_capabilities().await {
            Ok(capabilities) => capabilities.reports_percentage(),
            Err(error) => {
                debug!(slot, error = ?error, "BatteryStatus capability read failed");
                false
            }
        };
        let battery = match feature.get_battery_info().await {
            Ok(info) => Some(map_status_battery(info, percentage)),
            Err(error) => {
                debug!(slot, error = ?error, "BatteryStatus status read failed");
                None
            }
        };
        let handle = BatteryHandle::status(
            feature,
            percentage,
            event_context.route,
            event_context.events,
        );
        return (battery, Some(handle));
    }

    (None, failed_unified)
}

#[cfg(test)]
mod tests;
