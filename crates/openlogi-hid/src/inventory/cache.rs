use std::sync::Arc;

use hidpp::channel::HidppChannel;

use super::battery::{BatteryEventContext, BatteryHandle, read_battery};
use super::features::{ProbedFeatures, probe_features};

/// How many `enumerate` ticks a device's probe is reused before a fresh read.
/// The expensive part of a probe (the `enumerate_features` feature-table walk)
/// reads *immutable* data — model, capabilities, marketing type — so it never
/// needs re-reading for a known device; the periodic full probe is kept only as
/// a self-healing pass (e.g. a firmware update reshuffling the feature table).
/// The volatile battery does NOT ride this window: cache hits re-read it every
/// tick through the memoized feature index (see [`read_battery`]), so it stays
/// as fresh as it was before the cache existed (#153).
pub(super) const REFRESH_TICKS: u64 = 15;

/// Stable identity used to memoize a device's probe across `enumerate` ticks.
/// Keyed on the device's *own* identity (never its slot) so a re-paired or
/// moved device can't inherit another device's cached probe.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum CacheKey {
    /// Bolt: the unit id from the pairing register (cheap, read every tick).
    Bolt { unit_id: [u8; 4] },
    /// Unifying: keyed on the full receiver serial number + pairing slot.
    /// Using the complete serial (not just a prefix) avoids collisions between
    /// two receivers whose serials share a common prefix (e.g. "DA2699E1" and
    /// "DA2604F2" share "DA2").
    UnifyingSlot { receiver_uid: String, slot: u8 },
    /// Direct (Bluetooth/USB): the OS-assigned HID node id (macOS registry-entry
    /// id, Linux dev path, Windows interface path). Unique *per node*, so two
    /// units of the same model never collide, and stable while connected so the
    /// cache still hits across ticks.
    Direct(async_hid::DeviceId),
}

/// Enumeration ticks a device may be missing before its cache entry is evicted.
/// A small grace rides out a transient receiver timeout without dropping the
/// device's memoized data.
pub(super) const CACHE_MISS_GRACE: u8 = 3;

/// A memoized probe result plus the tick it was taken on.
#[derive(Clone)]
pub(super) struct Cached {
    pub(super) probe: ProbedFeatures,
    /// Long-lived battery feature selected by the full probe. It retains the
    /// broadcast listener and cache hits use it for one fallback status read.
    pub(super) battery_handle: Option<BatteryHandle>,
    pub(super) probed_tick: u64,
}

/// What a probed device contributes to the cache this tick. The key lets stale
/// entries be evicted; `Fresh` (a full probe) and `Update` (a cache hit whose
/// volatile battery was re-read) also carry the value to insert. `Unkeyed` is a
/// device we can't (or won't) cache — an all-zero unit id, or a rejected
/// non-peripheral — so its key is neither inserted nor kept alive.
pub(super) enum CacheOutcome {
    Fresh(CacheKey, Cached),
    Update(CacheKey, Cached),
    Seen(CacheKey),
    Unkeyed,
}

/// `Seen` when the device has a stable key, else `Unkeyed`.
pub(super) fn seen(id: Option<CacheKey>) -> CacheOutcome {
    id.map_or(CacheOutcome::Unkeyed, CacheOutcome::Seen)
}

/// Whether `cached` is stale enough that the device should be re-probed.
pub(super) fn is_stale(cached: &Cached, tick: u64) -> bool {
    tick.wrapping_sub(cached.probed_tick) >= REFRESH_TICKS
}

/// Decide a device's probe: reuse a fresh cache, or (online + miss/stale)
/// re-probe — but keep the last-known immutable data if the re-probe fails
/// rather than overwriting it with an empty default. An unprobed offline device
/// with no cache yields a default probe. Returns the probe plus its cache
/// contribution (only a *successful* probe is cached).
pub(super) async fn probe_or_reuse(
    channel: &Arc<HidppChannel>,
    index: u8,
    id: Option<CacheKey>,
    cached: Option<&Cached>,
    online: bool,
    tick: u64,
    battery_events: BatteryEventContext<'_>,
) -> (ProbedFeatures, CacheOutcome) {
    if online && cached.is_none_or(|c| is_stale(c, tick)) {
        let (mut fresh, battery_handle) = probe_features(channel, index, battery_events).await;
        // `capabilities` is `Some` exactly when the feature-table walk succeeded;
        // only then is the probe worth caching.
        if fresh.capabilities.is_some() {
            if let Some(c) = cached {
                backfill_identity(&mut fresh, &c.probe);
            }
            // The table walk succeeded and still exposes a battery endpoint,
            // so an empty reading is a transient status failure rather than a
            // removed feature. Preserve the last valid reading in that case.
            if fresh.battery.is_none() && battery_handle.is_some() {
                fresh.battery = cached.and_then(|entry| entry.probe.battery.clone());
            }
            // A first-sight probe whose identity reads failed is served but not
            // memoized: caching it would pin a wrong (all-zero unit or
            // serial-less) config key for `REFRESH_TICKS` (#482). The next tick
            // re-probes instead.
            if fresh.identity_incomplete && cached.is_none() {
                return (fresh, seen(id));
            }
            return match id {
                Some(key) => {
                    let value = Cached {
                        probe: fresh.clone(),
                        battery_handle,
                        probed_tick: tick,
                    };
                    (fresh, CacheOutcome::Fresh(key, value))
                }
                None => (fresh, CacheOutcome::Unkeyed),
            };
        }
        // Re-probe failed: don't cache the failure. Fall back to the last-known
        // data so a transient glitch doesn't drop the device or its battery.
        // No battery re-read either — the device just proved unresponsive.
        return match cached {
            Some(c) => (c.probe.clone(), seen(id)),
            None => (fresh, seen(id)),
        };
    }
    match cached {
        Some(c) => {
            // Cache hit: the immutable data is reused as-is, but the battery is
            // volatile (#153) — re-read just it through the memoized feature
            // index and fold the reading back into the cache. A failed read
            // (asleep, mid-host-switch) keeps the last-known value.
            if online
                && let Some(handle) = &c.battery_handle
                && let Some(key) = id.clone()
                && let Some(battery) = read_battery(handle).await
            {
                let mut entry = c.clone();
                entry.probe.battery = Some(battery);
                return (entry.probe.clone(), CacheOutcome::Update(key, entry));
            }
            (c.probe.clone(), seen(id))
        }
        None => (ProbedFeatures::default(), seen(id)),
    }
}

/// Carry immutable identity data the fresh probe failed to read forward from
/// the cached probe, so a transient `DeviceInformation` failure can't flip the
/// device's config key (#482). A probe whose identity reads all succeeded is
/// returned untouched.
pub(super) fn backfill_identity(fresh: &mut ProbedFeatures, cached: &ProbedFeatures) {
    if fresh.kind.is_none() {
        fresh.kind = cached.kind;
    }
    if !fresh.identity_incomplete {
        return;
    }
    match (fresh.model_info.as_mut(), cached.model_info.as_ref()) {
        (None, Some(previous)) => {
            fresh.model_info = Some(previous.clone());
            fresh.identity_incomplete = false;
        }
        (Some(now), Some(previous))
            if now.serial_number.is_none() && previous.serial_number.is_some() =>
        {
            now.serial_number.clone_from(&previous.serial_number);
            fresh.identity_incomplete = false;
        }
        _ => {}
    }
}
