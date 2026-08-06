//! Fail-closed startup UI for an unreadable `config.toml`.

use gpui::{
    AppContext as _, AsyncApp, Bounds, Context, FocusHandle, InteractiveElement as _, IntoElement,
    ParentElement as _, PromptButton, PromptLevel, Render, Size, Styled as _, Subscription, Window,
    WindowBounds, WindowHandle, WindowOptions, div, px, rgb,
};
use gpui_component::{ActiveTheme as _, Icon, IconName, Root, v_flex};
use openlogi_core::config::{Config, ConfigError};
use tracing::{error, info, warn};

use crate::theme::{self, Typography as _};

struct ConfigRecoveryView {
    focus_handle: FocusHandle,
    detail: gpui::SharedString,
    #[allow(dead_code, reason = "held to keep the appearance observer alive")]
    appearance_obs: Option<Subscription>,
}

impl ConfigRecoveryView {
    fn new(detail: gpui::SharedString, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);
        Self {
            focus_handle,
            detail,
            appearance_obs: None,
        }
    }
}

impl Render for ConfigRecoveryView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let pal = theme::palette(cx);
        v_flex()
            .size_full()
            .bg(pal.bg)
            .text_color(pal.text_primary)
            .track_focus(&self.focus_handle)
            .items_center()
            .justify_center()
            .gap_4()
            .p_8()
            .child(
                Icon::new(IconName::TriangleAlert)
                    .size_8()
                    .text_color(rgb(theme::STATUS_CONNECTING)),
            )
            .child(div().text_title().child(tr!("Configuration Error")))
            .child(
                div()
                    .max_w(px(520.))
                    .text_body()
                    .text_center()
                    .text_color(pal.text_muted)
                    .child(self.detail.clone()),
            )
    }
}

/// Ask how to handle an unreadable config. Returns defaults only after the
/// existing file has been backed up successfully; every other path closes
/// without modifying it.
pub async fn resolve(error: ConfigError, cx: &mut AsyncApp) -> Option<Config> {
    warn!(error = %error, "could not load config.toml; waiting for recovery choice");
    let detail = tr!(
        "OpenLogi couldn't load config.toml. No changes have been made. Back up the current file and use default settings, or close the app without modifying it.\n\n%{error}",
        error => error_chain(&error)
    );
    let title = tr!("Configuration Error");
    let answers = [
        PromptButton::cancel(tr!("Close Without Changes")),
        PromptButton::ok(tr!("Back Up and Use Defaults")),
    ];

    let (window, answer) = match open(title, detail, &answers, cx).await {
        Ok(opened) => opened,
        Err(open_error) => {
            error!(error = %open_error, "could not open config recovery window");
            return None;
        }
    };

    // Close, Escape, and a canceled platform prompt all take the safe path.
    if answer != Some(1) {
        return None;
    }

    match cx
        .background_spawn(async { Config::backup_and_reset() })
        .await
    {
        Ok(backup_path) => {
            info!(path = %backup_path.display(), "backed up invalid config and wrote defaults");
            close(&window, cx);
            Some(Config::default())
        }
        Err(recovery_error) => {
            error!(error = %recovery_error, "config recovery failed; app will close");
            let failure_title = tr!("Recovery Failed");
            let failure_detail = tr!(
                "OpenLogi couldn't complete configuration recovery. It will close without starting.\n\n%{error}",
                error => error_chain(&recovery_error)
            );
            let close_answer = [PromptButton::cancel(tr!("Close"))];
            if let Ok(receiver) = window.update(cx, |_, platform_window, cx| {
                platform_window.prompt(
                    PromptLevel::Critical,
                    failure_title.as_ref(),
                    Some(failure_detail.as_ref()),
                    &close_answer,
                    cx,
                )
            }) {
                let _ = receiver.await;
            }
            None
        }
    }
}

async fn open(
    title: gpui::SharedString,
    detail: gpui::SharedString,
    answers: &[PromptButton],
    cx: &mut AsyncApp,
) -> anyhow::Result<(WindowHandle<Root>, Option<usize>)> {
    let size = Size::new(px(600.), px(300.));
    let options = cx.update(|cx| WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size, cx))),
        window_min_size: Some(size),
        app_id: Some("openlogi".to_string()),
        titlebar: Some(crate::windows::titlebar_options(title.clone())),
        ..WindowOptions::default()
    });
    let view_detail = detail.clone();
    let window = cx.open_window(options, move |platform_window, cx| {
        theme::apply_from_settings(Some(platform_window), cx);
        let view = cx.new(|cx| ConfigRecoveryView::new(view_detail, platform_window, cx));
        let appearance_obs = platform_window.observe_window_appearance(|platform_window, cx| {
            theme::apply_from_settings(Some(platform_window), cx);
        });
        view.update(cx, |view, _| view.appearance_obs = Some(appearance_obs));
        cx.new(|cx| Root::new(view, platform_window, cx).bg(cx.theme().background))
    })?;
    let answer = window.update(cx, |_, platform_window, cx| {
        platform_window.activate_window();
        platform_window.prompt(
            PromptLevel::Critical,
            title.as_ref(),
            Some(detail.as_ref()),
            answers,
            cx,
        )
    })?;
    cx.update(|cx| cx.activate(true));
    Ok((window, answer.await.ok()))
}

fn close(window: &WindowHandle<Root>, cx: &mut AsyncApp) {
    let _ = window.update(cx, |_, platform_window, _| {
        platform_window.remove_window();
    });
}

fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
}
