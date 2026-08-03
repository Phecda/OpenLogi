use super::*;
use std::error::Error;
use std::sync::atomic::{AtomicBool, Ordering};

use hidpp::channel::RawHidChannel;

struct PoolRawChannel {
    connected: Arc<AtomicBool>,
}

#[hidpp::async_trait]
impl RawHidChannel for PoolRawChannel {
    fn vendor_id(&self) -> u16 {
        LOGITECH_VID
    }

    fn product_id(&self) -> u16 {
        0xb023
    }

    async fn write_report(&self, src: &[u8]) -> Result<usize, Box<dyn Error + Send + Sync>> {
        Ok(src.len())
    }

    async fn read_report(&self, _buf: &mut [u8]) -> Result<usize, Box<dyn Error + Send + Sync>> {
        std::future::pending().await
    }

    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }

    fn supports_short_long_hidpp(&self) -> Option<(bool, bool)> {
        Some((false, true))
    }

    async fn get_report_descriptor(
        &self,
        _buf: &mut [u8],
    ) -> Result<usize, Box<dyn Error + Send + Sync>> {
        unreachable!("mock declares HID++ support")
    }
}

async fn pool_channel(connected: Arc<AtomicBool>) -> Arc<HidppChannel> {
    Arc::new(
        HidppChannel::from_raw_channel(PoolRawChannel { connected })
            .await
            .unwrap_or_else(|e| panic!("mock HID++ channel should open: {e}")),
    )
}

#[tokio::test]
async fn channel_cache_reuses_one_live_channel_per_node() {
    let channel = pool_channel(Arc::new(AtomicBool::new(true))).await;
    let mut cache = HidppChannelCache::default();
    cache.remember(7, &channel);

    let reused = cache
        .live(&7)
        .unwrap_or_else(|| panic!("live channel should remain reusable"));

    assert!(Arc::ptr_eq(&channel, &reused));
}

#[tokio::test]
async fn channel_cache_rejects_a_disconnected_channel() {
    let connected = Arc::new(AtomicBool::new(true));
    let channel = pool_channel(Arc::clone(&connected)).await;
    let mut cache = HidppChannelCache::default();
    cache.remember(7, &channel);

    connected.store(false, Ordering::Release);

    assert!(cache.live(&7).is_none());
}

#[tokio::test]
async fn channel_cache_rejects_an_invalidated_channel() {
    let channel = pool_channel(Arc::new(AtomicBool::new(true))).await;
    let mut cache = HidppChannelCache::default();
    cache.remember(7, &channel);

    channel.invalidate();

    assert!(cache.live(&7).is_none());
}

#[tokio::test]
async fn channel_cache_does_not_keep_an_unowned_channel_alive() {
    let channel = pool_channel(Arc::new(AtomicBool::new(true))).await;
    let mut cache = HidppChannelCache::default();
    cache.remember(7, &channel);

    drop(channel);

    assert!(cache.live(&7).is_none());
    assert!(cache.channels.is_empty());
}

#[test]
fn matches_usb_ble_and_keyboard_hidpp_collections() {
    assert!(is_hidpp_long_collection(0xff00, 0x0002)); // USB / receiver / BT-classic
    assert!(is_hidpp_long_collection(0xff43, 0x0202)); // BLE-direct (Lift, Signature)
    assert!(is_hidpp_long_collection(0xff43, 0x0602)); // wired G-series keyboard (G513)
    assert!(!is_hidpp_long_collection(0x0001, 0x0002)); // generic-desktop mouse
    assert!(!is_hidpp_long_collection(0xff43, 0x0002)); // page right, usage wrong
}

#[test]
fn only_ble_collection_is_long_only() {
    assert!(is_long_only_collection(0xff43, 0x0202)); // BLE-direct → short-unsupported
    assert!(!is_long_only_collection(0xff00, 0x0002)); // USB / receiver carries both reports
    assert!(!is_long_only_collection(0xff43, 0x0602)); // wired G-series keyboard carries both
    assert!(!is_long_only_collection(0x0001, 0x0002)); // not a HID++ collection at all
}

#[test]
fn short_and_long_collections_of_one_interface_share_a_grouping_key() {
    // Real Bolt receiver paths: the short (Col01) and long (Col02) HID++
    // collections of interface MI_02 must collapse to the same key.
    let short = normalize_collection_path(
        r"\\?\HID#VID_046D&PID_C548&MI_02&Col01#7&348660ac&0&0000#{4d1e55b2-f16f-11cf-88cb-001111000030}",
    );
    let long = normalize_collection_path(
        r"\\?\HID#VID_046D&PID_C548&MI_02&Col02#7&348660ac&0&0001#{4d1e55b2-f16f-11cf-88cb-001111000030}",
    );
    assert_eq!(short, long);
    assert_eq!(short, "vid_046d&pid_c548&mi_02#7&348660ac&0");
}

#[test]
fn distinct_interfaces_do_not_share_a_grouping_key() {
    // A different interface (MI_01) on the same receiver has its own instance
    // hash, so it must not pair with MI_02's HID++ collections.
    let mi01 = normalize_collection_path(
        r"\\?\HID#VID_046D&PID_C548&MI_01&Col02#7&1cc2d467&0&0001#{4d1e55b2-f16f-11cf-88cb-001111000030}",
    );
    let mi02 = normalize_collection_path(
        r"\\?\HID#VID_046D&PID_C548&MI_02&Col02#7&348660ac&0&0001#{4d1e55b2-f16f-11cf-88cb-001111000030}",
    );
    assert_ne!(mi01, mi02);
}

#[test]
fn distinct_physical_receivers_do_not_share_a_grouping_key() {
    // Two receivers plugged in at once (here two identical Bolt receivers,
    // same VID/PID/interface/collection) must not cross-pair: each physical
    // device has a distinct instance hash, which the key preserves. This is
    // the multi-receiver scenario the single-interface tests don't cover.
    let recv_a = normalize_collection_path(
        r"\\?\HID#VID_046D&PID_C548&MI_02&Col01#7&348660ac&0&0000#{4d1e55b2-f16f-11cf-88cb-001111000030}",
    );
    let recv_b = normalize_collection_path(
        r"\\?\HID#VID_046D&PID_C548&MI_02&Col01#7&9f1be20c&0&0000#{4d1e55b2-f16f-11cf-88cb-001111000030}",
    );
    assert_ne!(recv_a, recv_b);

    // A Bolt + a Unifying receiver (different PID) must also stay distinct.
    let bolt = normalize_collection_path(
        r"\\?\HID#VID_046D&PID_C548&MI_02&Col02#7&348660ac&0&0001#{4d1e55b2-f16f-11cf-88cb-001111000030}",
    );
    let unifying = normalize_collection_path(
        r"\\?\HID#VID_046D&PID_C52B&MI_02&Col02#7&1a2b3c4d&0&0001#{4d1e55b2-f16f-11cf-88cb-001111000030}",
    );
    assert_ne!(bolt, unifying);
}

// Sysfs path: child of Unifying receiver
const UNIFYING_CHILD: &str = "/sys/devices/pci0000:00/0000:00:14.0/usb3/3-5/3-5.4/3-5.4.3/\
     3-5.4.3:1.2/0003:046D:C52B.0009/0003:046D:4076.000A";
// Sysfs path: the Unifying receiver node itself (terminal component has C52B)
const UNIFYING_RECEIVER: &str = "/sys/devices/pci0000:00/0000:00:14.0/usb3/3-5/3-5.4/3-5.4.3/\
     3-5.4.3:1.2/0003:046D:C52B.0009";
// Sysfs path: child of Bolt receiver
const BOLT_CHILD: &str = "/sys/devices/pci0000:00/0000:00:14.0/usb3/3-5/\
     0003:046D:C548.0001/0003:046D:B037.0002";
// Sysfs path: unrelated non-Logitech device
const UNRELATED: &str = "/sys/devices/pci0000:00/0000:00:15.0/i2c-0/0018:06CB:CE67.0001";

#[test]
fn child_of_unifying_receiver_is_detected() {
    assert!(is_receiver_child_sysfs_path(UNIFYING_CHILD));
}

#[test]
fn unifying_receiver_itself_is_not_a_child() {
    assert!(!is_receiver_child_sysfs_path(UNIFYING_RECEIVER));
}

#[test]
fn child_of_bolt_receiver_is_detected() {
    assert!(is_receiver_child_sysfs_path(BOLT_CHILD));
}

#[test]
fn unrelated_device_is_not_a_child() {
    assert!(!is_receiver_child_sysfs_path(UNRELATED));
}
