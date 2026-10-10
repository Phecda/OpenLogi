//! Arming for gesture capture: which of one device's controls a session
//! diverts, the firmware state that records, and how it is re-armed and handed
//! back.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use hidpp::{channel::HidppChannel, device::Device};
use openlogi_core::binding::ButtonId;
use tracing::{debug, warn};

use super::{CaptureSpec, CapturedInput};
use crate::reprog_controls::{self, ReprogControlsV4};
use crate::session::capture::open_device;
use crate::session::capture_restore::{
    ArmedReporting, CaptureError, CaptureSessionFailure, PendingCaptureRestore, ReprogRestore,
    divert_change,
};
use crate::session::restore::rollback_start;
use crate::thumbwheel::{self, Thumbwheel, ThumbwheelInfo, WheelDirection, WheelResolution};
use crate::{ChannelRegistry, SharedChannel};

/// The set of controls a session has diverted, kept so they can be handed back
/// to the firmware on teardown.
#[derive(Default)]
pub(super) struct ArmedControls {
    /// `0x1b04` accessor, present when the device exposes it.
    pub(super) reprog: Option<ReprogControlsV4>,
    /// The device control table retained for in-place spec changes.
    controls: Vec<reprog_controls::CtrlIdInfo>,
    /// Effective diversion mode for each control currently owned by capture.
    modes: BTreeMap<u16, ControlMode>,
    /// The gesture-source CIDs diverted with raw-XY reporting: the
    /// `spec.divert_gesture_sources` members the device exposes.
    pub(super) gesture_cids: Vec<u16>,
    /// Raw-XY-capable additional CIDs diverted as gesture sources (side
    /// buttons on supported desktops and a gesture-mode DPI/ModeShift button).
    pub(super) gesture_button_cids: Vec<(u16, ButtonId)>,
    /// DPI/ModeShift CIDs diverted as plain buttons when gesture mode is off.
    pub(super) dpi_cids: Vec<u16>,
    /// Standard-button CIDs diverted per the session's [`CaptureSpec`], with
    /// the [`ButtonId`] each dispatches as.
    pub(super) button_cids: Vec<(u16, ButtonId)>,
    /// Original reporting state for every diverted `0x1b04` control.
    reporting: Vec<ArmedReporting>,
    /// `0x2150` accessor and the information read while diverting it, present
    /// when the thumb wheel is diverted.
    pub(super) thumb: Option<ArmedThumbwheel>,
}

pub(super) struct ArmedThumbwheel {
    pub(super) wheel: Thumbwheel,
    info: Option<ThumbwheelInfo>,
    pub(super) diverted: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ControlMode {
    Plain,
    RawXy,
}

impl ControlMode {
    const fn raw_xy(self) -> bool {
        matches!(self, Self::RawXy)
    }
}

impl ArmedThumbwheel {
    pub(super) fn resolution(&self) -> WheelResolution {
        self.info
            .map_or(WheelResolution::UNKNOWN, |info| info.resolution)
    }

    fn direction(&self) -> WheelDirection {
        if self.info.is_some_and(|info| !info.positive_is_forward()) {
            WheelDirection::Inverted
        } else {
            WheelDirection::Default
        }
    }
}

impl ArmedControls {
    /// Build the one-time polarity fact learned while arming the thumb wheel.
    pub(super) fn thumbwheel_direction(&self) -> Option<CapturedInput> {
        let thumb = self.thumb.as_ref()?;
        if !thumb.diverted {
            return None;
        }
        let positive_is_forward = thumb.info.map(ThumbwheelInfo::positive_is_forward)?;
        Some(CapturedInput::ThumbwheelDirection {
            positive_is_forward,
        })
    }

    /// Convert all armed firmware state into the one capability that can
    /// release it. Consuming `self` prevents a session and a restore retry from
    /// both claiming ownership at once.
    pub(super) fn into_pending(self, retired: &SharedChannel) -> Option<PendingCaptureRestore> {
        let Self {
            reprog,
            reporting,
            thumb,
            ..
        } = self;
        let reprog =
            reprog.and_then(|controls| ReprogRestore::new(controls.feature_index(), reporting));
        PendingCaptureRestore::new(
            retired,
            reprog,
            thumb
                .as_ref()
                .filter(|thumb| thumb.diverted)
                .map(|thumb| thumb.wheel.feature_index()),
        )
    }

    /// Reapply volatile diversion after a wireless reconnect broadcast.
    pub(super) async fn rearm(&self) {
        if let Some(rc) = self.reprog.as_ref() {
            for (&cid, &mode) in &self.modes {
                let Some(reporting) = self.reporting.iter().find(|entry| entry.cid == cid) else {
                    continue;
                };
                let change = divert_change(reporting.original, mode.raw_xy());
                if let Err(error) = rc.set_cid_reporting_full(reporting.cid, change).await {
                    warn!(
                        cid = format_args!("{:#06x}", reporting.cid),
                        ?error,
                        "re-divert after wake failed"
                    );
                }
            }
        }
        if let Some(thumb) = self.thumb.as_ref()
            && thumb.diverted
            && let Err(error) = thumb.wheel.divert(thumb.direction()).await
        {
            warn!(?error, "thumb-wheel re-divert after wake failed");
        }
    }

    /// Apply a new specification without replacing the HID++ channel. The
    /// desired mode is derived from the retained control table, so raw-XY
    /// always wins over plain diversion for a control requested by both.
    pub(super) async fn reconfigure(&mut self, spec: &CaptureSpec) -> Result<(), CaptureError> {
        let result = self.reconfigure_inner(spec).await;
        self.rebuild_lists(spec);
        result
    }

    async fn reconfigure_inner(&mut self, spec: &CaptureSpec) -> Result<(), CaptureError> {
        let desired = desired_modes(spec, &self.controls);
        if let Some(rc) = self.reprog.clone() {
            let mut cids: BTreeSet<u16> = self.modes.keys().copied().collect();
            cids.extend(self.reporting.iter().map(|entry| entry.cid));
            cids.extend(desired.keys().copied());
            for cid in cids {
                let current = self.modes.get(&cid).copied();
                let wanted = desired.get(&cid).copied();
                let uncertain_ownership = wanted.is_none()
                    && current.is_none()
                    && self.reporting.iter().any(|entry| entry.cid == cid);
                if current == wanted && !uncertain_ownership {
                    continue;
                }
                if let Some(mode) = wanted {
                    let original =
                        if let Some(entry) = self.reporting.iter().find(|entry| entry.cid == cid) {
                            entry.original
                        } else {
                            let original = rc.get_cid_reporting(cid).await?;
                            self.reporting.push(ArmedReporting { cid, original });
                            original
                        };
                    rc.set_cid_reporting_full(cid, divert_change(original, mode.raw_xy()))
                        .await?;
                    self.modes.insert(cid, mode);
                } else {
                    let Some(entry) = self
                        .reporting
                        .iter()
                        .find(|entry| entry.cid == cid)
                        .copied()
                    else {
                        continue;
                    };
                    rc.set_cid_reporting_full(
                        cid,
                        crate::session::capture_restore::undivert_change(entry.original),
                    )
                    .await?;
                    self.modes.remove(&cid);
                    self.reporting.retain(|entry| entry.cid != cid);
                }
            }
        }

        if let Some(thumb) = self.thumb.as_mut() {
            if spec.capture_thumbwheel && !thumb.diverted {
                thumb.diverted = true;
                thumb.wheel.divert(thumb.direction()).await?;
            } else if !spec.capture_thumbwheel && thumb.diverted {
                thumb.wheel.undivert().await?;
                thumb.diverted = false;
            }
        }
        self.rebuild_lists(spec);
        Ok(())
    }

    fn rebuild_lists(&mut self, spec: &CaptureSpec) {
        self.gesture_cids.clear();
        self.gesture_button_cids.clear();
        self.dpi_cids.clear();
        self.button_cids.clear();
        for &cid in &spec.divert_gesture_sources {
            if self.modes.get(&cid) == Some(&ControlMode::RawXy) {
                self.gesture_cids.push(cid);
            }
        }
        for &(cid, button) in &spec.divert_gesture_buttons {
            if self.modes.get(&cid) == Some(&ControlMode::RawXy)
                && !self.gesture_cids.contains(&cid)
            {
                self.gesture_button_cids.push((cid, button));
            }
        }
        for &cid in &reprog_controls::DPI_MODE_SHIFT_CIDS {
            if self.modes.get(&cid) == Some(&ControlMode::Plain) {
                self.dpi_cids.push(cid);
            }
        }
        for &(cid, button) in &spec.divert_buttons {
            if self.modes.get(&cid) == Some(&ControlMode::Plain) && !self.dpi_cids.contains(&cid) {
                self.button_cids.push((cid, button));
            }
        }
    }
}

/// Resolve features off the device's root and divert the controls `spec`
/// selects: the gesture sources (raw-XY), DPI/ModeShift buttons and rebindable
/// standard buttons over `0x1b04`, and the thumb wheel over `0x2150`. The
/// root-feature lookup mirrors `write::open_feature`,
/// since hidpp 0.2's registry doesn't carry the features OpenLogi reimplements.
///
/// A failure mid-way tries to hand every possibly-diverted control back to the
/// firmware. If compensation is incomplete, the returned failure carries an
/// opaque restore capability for the manager to retain and retry.
pub(super) async fn arm_controls(
    shared: &SharedChannel,
    spec: &CaptureSpec,
    registry: &ChannelRegistry,
) -> Result<ArmedControls, CaptureSessionFailure> {
    let device = open_device(shared).await?;
    let chan = shared.channel();
    let slot = shared.device_index();
    let mut armed = ArmedControls::default();
    if let Err(error) = arm_controls_into(&device, chan, slot, spec, &mut armed).await {
        let pending = armed.into_pending(shared);
        return Err(rollback_start(error, pending, registry).await);
    }
    if armed.gesture_cids.is_empty()
        && armed.gesture_button_cids.is_empty()
        && armed.dpi_cids.is_empty()
        && armed.button_cids.is_empty()
        && armed.thumb.is_none()
    {
        debug!(slot, "no capturable controls — idle session");
    }
    Ok(armed)
}

/// The fallible arming steps of [`arm_controls`], recording ownership before
/// each write. A transport failure cannot prove whether firmware applied that
/// write, so rollback deliberately includes the uncertain current control.
pub(super) async fn arm_controls_into(
    device: &Device,
    chan: &Arc<HidppChannel>,
    slot: u8,
    spec: &CaptureSpec,
    armed: &mut ArmedControls,
) -> Result<(), CaptureError> {
    if let Some(info) = device
        .root()
        .get_feature(reprog_controls::FEATURE_ID)
        .await?
    {
        let rc = ReprogControlsV4::new(Arc::clone(chan), slot, info.index);
        let controls = enumerate_controls(&rc).await?;
        // Register an accessor before the first divert, so a failure on any
        // divert (including the first) can become a restore capability.
        armed.reprog = Some(rc.clone());
        armed.controls.clone_from(&controls);
        apply_reprog_spec(&rc, spec, armed).await?;
    }

    if let Some(info) = device.root().get_feature(thumbwheel::FEATURE_ID).await? {
        let tw = Thumbwheel::new(Arc::clone(chan), slot, info.index);
        let wheel_info = match tw.get_info().await {
            Ok(twinfo) => Some(twinfo),
            Err(e) => {
                warn!(error = ?e, "thumb wheel getInfo failed");
                None
            }
        };
        // Divert whenever capture was requested: rotation rebinds and the
        // sensitivity multiplier need the diverted event stream even on wheels
        // that report no single-tap capability (e.g. MX Master 4) — lacking the
        // tap only means a bound click can never fire.
        if wheel_info.is_some_and(|info| !info.supports_single_tap) {
            debug!("thumb wheel reports no single tap — click not capturable");
        }
        // Store ownership before the write: a transport error cannot prove
        // whether firmware applied diversion, so rollback must cover it too.
        armed.thumb = Some(ArmedThumbwheel {
            wheel: tw,
            info: wheel_info,
            diverted: false,
        });
        if spec.capture_thumbwheel
            && let Some(thumb) = armed.thumb.as_mut()
        {
            thumb.diverted = true;
            thumb.wheel.divert(thumb.direction()).await?;
        }
    }
    Ok(())
}

fn desired_modes(
    spec: &CaptureSpec,
    controls: &[reprog_controls::CtrlIdInfo],
) -> BTreeMap<u16, ControlMode> {
    let mut modes = BTreeMap::new();
    for &cid in &spec.divert_gesture_sources {
        if controls
            .iter()
            .any(|control| control.cid == cid && control.supports_raw_xy())
        {
            modes.insert(cid, ControlMode::RawXy);
        }
    }
    for &(cid, _) in &spec.divert_gesture_buttons {
        if controls.iter().any(|control| {
            control.cid == cid && control.is_divertable() && control.supports_raw_xy()
        }) {
            modes.insert(cid, ControlMode::RawXy);
        }
    }
    for &cid in &reprog_controls::DPI_MODE_SHIFT_CIDS {
        let gesture_requested = spec
            .divert_gesture_buttons
            .iter()
            .any(|&(gesture_cid, button)| gesture_cid == cid && button == ButtonId::DpiToggle);
        if !gesture_requested
            && controls
                .iter()
                .any(|control| control.cid == cid && control.is_divertable())
        {
            modes.entry(cid).or_insert(ControlMode::Plain);
        }
    }
    for &(cid, _) in &spec.divert_buttons {
        if modes.get(&cid) != Some(&ControlMode::RawXy)
            && controls
                .iter()
                .any(|control| control.cid == cid && control.is_divertable())
        {
            modes.entry(cid).or_insert(ControlMode::Plain);
        }
    }
    modes
}

async fn apply_reprog_spec(
    rc: &ReprogControlsV4,
    spec: &CaptureSpec,
    armed: &mut ArmedControls,
) -> Result<(), CaptureError> {
    for (cid, mode) in desired_modes(spec, &armed.controls) {
        let original = rc.get_cid_reporting(cid).await?;
        armed.reporting.push(ArmedReporting { cid, original });
        rc.set_cid_reporting_full(cid, divert_change(original, mode.raw_xy()))
            .await?;
        armed.modes.insert(cid, mode);
    }
    armed.rebuild_lists(spec);
    Ok(())
}

/// Read the device's full reprogrammable-control table in one pass, so we can
/// test several CIDs without rescanning per control.
pub(crate) async fn enumerate_controls(
    rc: &ReprogControlsV4,
) -> Result<Vec<reprog_controls::CtrlIdInfo>, CaptureError> {
    let count = rc.get_count().await?;
    let mut controls = Vec::with_capacity(usize::from(count));
    for index in 0..count {
        controls.push(rc.get_ctrl_id_info(index).await?);
    }
    Ok(controls)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn control(cid: u16, raw_xy: bool) -> reprog_controls::CtrlIdInfo {
        reprog_controls::CtrlIdInfo {
            cid,
            task_id: 0,
            flags: (1 << 5) | if raw_xy { 1 << 8 } else { 0 },
        }
    }

    #[test]
    fn raw_xy_wins_when_a_control_is_requested_by_both_specs() {
        let cid = 0x00c4;
        let spec = CaptureSpec {
            divert_gesture_buttons: vec![(cid, ButtonId::DpiToggle)],
            divert_buttons: vec![(cid, ButtonId::DpiToggle)],
            ..CaptureSpec::default()
        };

        assert_eq!(
            desired_modes(&spec, &[control(cid, true)]),
            BTreeMap::from([(cid, ControlMode::RawXy)])
        );
    }

    #[test]
    fn rebuilt_runtime_lists_follow_native_plain_and_raw_modes() {
        let raw_cid = 0x00c4;
        let plain_cid = 0x0052;
        let spec = CaptureSpec {
            divert_gesture_buttons: vec![(raw_cid, ButtonId::DpiToggle)],
            divert_buttons: vec![(plain_cid, ButtonId::MiddleClick)],
            ..CaptureSpec::default()
        };
        let mut armed = ArmedControls {
            controls: vec![control(raw_cid, true), control(plain_cid, false)],
            modes: BTreeMap::from([
                (raw_cid, ControlMode::RawXy),
                (plain_cid, ControlMode::Plain),
            ]),
            ..ArmedControls::default()
        };

        armed.rebuild_lists(&spec);

        assert_eq!(
            armed.gesture_button_cids,
            vec![(raw_cid, ButtonId::DpiToggle)]
        );
        assert_eq!(armed.button_cids, vec![(plain_cid, ButtonId::MiddleClick)]);
        assert!(armed.gesture_cids.is_empty());
        assert!(armed.dpi_cids.is_empty());
    }
}
