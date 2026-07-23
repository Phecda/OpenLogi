use std::{collections::VecDeque, error::Error, io, sync::Arc};

use hidpp::channel::{HidppChannel, RawHidChannel};
use hidpp::feature::{
    CreatableFeature as _,
    battery_status::BatteryStatusFeature,
    unified_battery::{BatteryLevel as HidppBatteryLevel, BatteryStatus, UnifiedBatteryFeature},
};
use openlogi_core::device::{BatteryLevel, BatteryStatus as CoreBatteryStatus};

use crate::DeviceRoute;

use super::{
    BatteryEndpoint, BatteryEventContext, BatteryFeatureIndices, BatteryHandle,
    battery_feature_indices, map_status_battery_fields, map_status_level,
    map_unified_battery_fields, probe_battery, read_battery,
};

enum ScriptedResponse {
    Payload([u8; 3]),
    Error(u8),
}

struct ScriptedRawHidChannel {
    incoming_tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    incoming_rx: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>>,
    responses: tokio::sync::Mutex<VecDeque<ScriptedResponse>>,
    writes: Arc<tokio::sync::Mutex<Vec<Vec<u8>>>>,
}

impl ScriptedRawHidChannel {
    fn new(
        responses: impl IntoIterator<Item = ScriptedResponse>,
    ) -> (Self, Arc<tokio::sync::Mutex<Vec<Vec<u8>>>>) {
        let (incoming_tx, incoming_rx) = tokio::sync::mpsc::unbounded_channel();
        let writes = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        (
            Self {
                incoming_tx,
                incoming_rx: tokio::sync::Mutex::new(incoming_rx),
                responses: tokio::sync::Mutex::new(responses.into_iter().collect()),
                writes: Arc::clone(&writes),
            },
            writes,
        )
    }
}

#[hidpp::async_trait]
impl RawHidChannel for ScriptedRawHidChannel {
    fn vendor_id(&self) -> u16 {
        0x046d
    }

    fn product_id(&self) -> u16 {
        0xc52b
    }

    async fn write_report(&self, src: &[u8]) -> Result<usize, Box<dyn Error + Sync + Send>> {
        self.writes.lock().await.push(src.to_vec());
        let Some(scripted) = self.responses.lock().await.pop_front() else {
            return Err(mock_error());
        };
        let response = match scripted {
            ScriptedResponse::Payload(payload) => {
                let mut response = src.to_vec();
                response[4..7].copy_from_slice(&payload);
                response
            }
            ScriptedResponse::Error(code) => {
                vec![src[0], src[1], 0xff, src[2], src[3], code, 0]
            }
        };
        self.incoming_tx.send(response).map_err(|_| mock_error())?;
        Ok(src.len())
    }

    async fn read_report(&self, buf: &mut [u8]) -> Result<usize, Box<dyn Error + Sync + Send>> {
        let Some(report) = self.incoming_rx.lock().await.recv().await else {
            return Err(mock_error());
        };
        let len = report.len().min(buf.len());
        buf[..len].copy_from_slice(&report[..len]);
        Ok(len)
    }

    fn supports_short_long_hidpp(&self) -> Option<(bool, bool)> {
        Some((true, true))
    }

    async fn get_report_descriptor(
        &self,
        _buf: &mut [u8],
    ) -> Result<usize, Box<dyn Error + Sync + Send>> {
        unreachable!("scripted channel declares HID++ support")
    }
}

fn mock_error() -> Box<dyn Error + Sync + Send> {
    Box::new(io::Error::other("scripted channel exhausted"))
}

async fn scripted_channel(
    responses: impl IntoIterator<Item = ScriptedResponse>,
) -> (
    Arc<hidpp::channel::HidppChannel>,
    Arc<tokio::sync::Mutex<Vec<Vec<u8>>>>,
) {
    let (channel, writes, _) = scripted_channel_with_input(responses).await;
    (channel, writes)
}

async fn scripted_channel_with_input(
    responses: impl IntoIterator<Item = ScriptedResponse>,
) -> (
    Arc<hidpp::channel::HidppChannel>,
    Arc<tokio::sync::Mutex<Vec<Vec<u8>>>>,
    tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
) {
    let (raw, writes) = ScriptedRawHidChannel::new(responses);
    let incoming = raw.incoming_tx.clone();
    let channel = HidppChannel::from_raw_channel(raw).await;
    let Ok(channel) = channel else {
        panic!("scripted channel must support HID++");
    };
    (Arc::new(channel), writes, incoming)
}

#[test]
fn battery_indices_are_one_based_in_the_enumerated_table() {
    // `enumerate_features` omits the root feature (index 0), so the first
    // enumerated entry sits at runtime index 1.
    let table = [BatteryStatusFeature::ID, UnifiedBatteryFeature::ID, 0x2201];
    let indices = battery_feature_indices(table);
    assert_eq!(indices.status, Some(1));
    assert_eq!(indices.unified, Some(2));
    assert_eq!(
        battery_feature_indices([UnifiedBatteryFeature::ID]).unified,
        Some(1),
        "first entry maps to index 1, not 0"
    );
}

#[test]
fn no_battery_feature_means_no_indices() {
    assert_eq!(
        battery_feature_indices([0x0001, 0x2201, 0x1b04]),
        BatteryFeatureIndices::default()
    );
    assert_eq!(
        battery_feature_indices([]),
        BatteryFeatureIndices::default()
    );
}

#[test]
fn status_levels_use_the_documented_coarse_buckets() {
    for (raw, expected) in [
        (None, BatteryLevel::Unknown),
        (Some(1), BatteryLevel::Critical),
        (Some(10), BatteryLevel::Critical),
        (Some(11), BatteryLevel::Low),
        (Some(29), BatteryLevel::Low),
        (Some(30), BatteryLevel::Good),
        (Some(80), BatteryLevel::Good),
        (Some(81), BatteryLevel::Full),
        (Some(100), BatteryLevel::Full),
    ] {
        assert_eq!(map_status_level(raw), expected, "raw level {raw:?}");
    }
}

#[test]
fn status_percentage_is_only_exposed_when_capability_is_trusted() {
    assert_eq!(
        map_status_battery_fields(Some(28), BatteryStatus::Discharging, false).percentage,
        None
    );
    assert_eq!(
        map_status_battery_fields(Some(28), BatteryStatus::Discharging, true).percentage,
        Some(28)
    );
    assert_eq!(
        map_status_battery_fields(Some(28), BatteryStatus::Discharging, false).level,
        BatteryLevel::Low
    );
}

#[test]
fn unified_percentage_respects_capability_and_valid_range() {
    assert_eq!(
        map_unified_battery_fields(
            80,
            HidppBatteryLevel::Good,
            BatteryStatus::Discharging,
            false,
        )
        .percentage,
        None
    );
    assert_eq!(
        map_unified_battery_fields(
            80,
            HidppBatteryLevel::Good,
            BatteryStatus::Discharging,
            true,
        )
        .percentage,
        Some(80)
    );
    assert_eq!(
        map_unified_battery_fields(
            101,
            HidppBatteryLevel::Good,
            BatteryStatus::Discharging,
            true,
        )
        .percentage,
        None
    );
}

#[tokio::test]
async fn probe_prefers_unified_battery_when_its_status_is_readable() {
    let (channel, writes) = scripted_channel([
        ScriptedResponse::Payload([0x0f, 0x02, 0]),
        ScriptedResponse::Payload([
            80,
            u8::from(HidppBatteryLevel::Good),
            u8::from(BatteryStatus::Discharging),
        ]),
    ])
    .await;

    let (battery, endpoint) = probe_battery(
        &channel,
        1,
        BatteryFeatureIndices {
            unified: Some(2),
            status: Some(1),
        },
        BatteryEventContext::none(),
    )
    .await;

    assert_eq!(battery.and_then(|info| info.percentage), Some(80));
    assert_eq!(
        endpoint.map(|handle| handle.endpoint()),
        Some(BatteryEndpoint::Unified { percentage: true })
    );
    let writes = writes.lock().await;
    assert_eq!(writes.len(), 2, "legacy fallback must not be queried");
    assert!(writes.iter().all(|report| report[2] == 2));
}

#[tokio::test]
async fn probe_falls_back_to_coarse_status_when_unified_status_fails() {
    let (channel, writes) = scripted_channel([
        ScriptedResponse::Payload([0x0f, 0x02, 0]),
        ScriptedResponse::Payload([80, 0xff, 0]),
        ScriptedResponse::Payload([4, 0, 0]),
        ScriptedResponse::Payload([28, 10, 0]),
    ])
    .await;

    let (battery, endpoint) = probe_battery(
        &channel,
        1,
        BatteryFeatureIndices {
            unified: Some(2),
            status: Some(1),
        },
        BatteryEventContext::none(),
    )
    .await;

    let Some(battery) = battery else {
        panic!("legacy battery response should be retained");
    };
    assert_eq!(battery.percentage, None);
    assert_eq!(battery.level, BatteryLevel::Low);
    assert_eq!(
        endpoint.map(|handle| handle.endpoint()),
        Some(BatteryEndpoint::Status { percentage: false })
    );
    let requests = writes
        .lock()
        .await
        .iter()
        .map(|report| (report[2], report[3] >> 4))
        .collect::<Vec<_>>();
    assert_eq!(requests, [(2, 0), (2, 1), (1, 1), (1, 0)]);
}

#[tokio::test]
async fn capability_failure_keeps_unified_reading_coarse() {
    let (channel, _) = scripted_channel([
        ScriptedResponse::Error(1),
        ScriptedResponse::Payload([
            80,
            u8::from(HidppBatteryLevel::Good),
            u8::from(BatteryStatus::Discharging),
        ]),
    ])
    .await;

    let (battery, endpoint) = probe_battery(
        &channel,
        1,
        BatteryFeatureIndices {
            unified: Some(2),
            status: None,
        },
        BatteryEventContext::none(),
    )
    .await;

    assert_eq!(battery.and_then(|info| info.percentage), None);
    assert_eq!(
        endpoint.map(|handle| handle.endpoint()),
        Some(BatteryEndpoint::Unified { percentage: false })
    );
}

#[tokio::test]
async fn cached_status_refresh_sends_only_one_request() {
    let (channel, writes) = scripted_channel([ScriptedResponse::Payload([28, 10, 0])]).await;
    let feature = BatteryStatusFeature::new(Arc::clone(&channel), 1, 1);
    let handle = BatteryHandle::status(feature, false, None, None);

    let battery = read_battery(&handle).await;

    assert_eq!(battery.map(|info| info.level), Some(BatteryLevel::Low));
    let writes = writes.lock().await;
    assert_eq!(writes.len(), 1);
    assert_eq!((writes[0][2], writes[0][3] >> 4), (1, 0));
}

#[tokio::test]
async fn status_broadcast_is_forwarded_with_its_device_route() {
    let (channel, _, incoming) = scripted_channel_with_input([]).await;
    let (events, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let route = DeviceRoute::Unifying {
        receiver_uid: "receiver-a".to_string(),
        slot: 1,
    };
    let feature = BatteryStatusFeature::new(Arc::clone(&channel), 1, 1);
    let _handle = BatteryHandle::status(feature, false, Some(&route), Some(&events));

    let mut report = vec![0u8; 20];
    report[..7].copy_from_slice(&[0x11, 1, 1, 0, 28, 10, 1]);
    assert!(incoming.send(report).is_ok(), "raw channel stays open");

    let Ok(Some(update)) =
        tokio::time::timeout(std::time::Duration::from_secs(1), event_rx.recv()).await
    else {
        panic!("battery broadcast should be forwarded");
    };
    assert_eq!(update.route, route);
    assert_eq!(update.battery.percentage, None);
    assert_eq!(update.battery.level, BatteryLevel::Low);
    assert_eq!(update.battery.status, CoreBatteryStatus::Charging);
}

#[tokio::test]
async fn unified_broadcast_preserves_a_trusted_percentage() {
    let (channel, _, incoming) = scripted_channel_with_input([]).await;
    let (events, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let route = DeviceRoute::Bolt {
        receiver_uid: "receiver-b".to_string(),
        slot: 1,
    };
    let feature = UnifiedBatteryFeature::new(Arc::clone(&channel), 1, 2);
    let _handle = BatteryHandle::unified(feature, true, Some(&route), Some(&events));

    let mut report = vec![0u8; 20];
    report[..7].copy_from_slice(&[
        0x11,
        1,
        2,
        0,
        80,
        u8::from(HidppBatteryLevel::Good),
        u8::from(BatteryStatus::Discharging),
    ]);
    assert!(incoming.send(report).is_ok(), "raw channel stays open");

    let Ok(Some(update)) =
        tokio::time::timeout(std::time::Duration::from_secs(1), event_rx.recv()).await
    else {
        panic!("battery broadcast should be forwarded");
    };
    assert_eq!(update.route, route);
    assert_eq!(update.battery.percentage, Some(80));
    assert_eq!(update.battery.level, BatteryLevel::Good);
    assert_eq!(update.battery.status, CoreBatteryStatus::Discharging);
}
