//! iOS WKWebView safe-area & viewport normalization.
use std::ffi::c_void;

use objc2::{msg_send, rc::Retained, runtime::AnyObject};
use objc2_core_foundation::CGRect;
use objc2_ui_kit::{UIScrollView, UIScrollViewContentInsetAdjustmentBehavior, UIView, UIWindow};
use objc2_web_kit::WKWebViewConfiguration;
use tauri::Manager;

const EPSILON: f64 = 0.5;

fn rect_eq(a: CGRect, b: CGRect) -> bool {
    (a.origin.x - b.origin.x).abs() < EPSILON
        && (a.origin.y - b.origin.y).abs() < EPSILON
        && (a.size.width - b.size.width).abs() < EPSILON
        && (a.size.height - b.size.height).abs() < EPSILON
}

/// 读 `UIWindow.windowScene.coordinateSpace.bounds`，即窗口所在场景的真实可用尺寸。
///
/// Apple《Prepare your app for iPhone Duo》有两条硬要求：不要用 `UIScreen.mainScreen.bounds`
/// （内屏/折叠态下它报的是外屏尺寸），并且每次场景尺寸变化都要重新读。tao 0.37 建窗时正是
/// `initWithFrame:(UIScreen.mainScreen.bounds)` 再 `setWindowScene:`，绑定场景后不再重设 frame，
/// 于是内屏展开后窗口仍停在外屏尺寸，WKWebView 布局视口随之卡在旧宽度，
/// `ios-duo.css` 的 `min-width: 641px / 900px` 断点永远不触发，表现为「宽屏布局不生效」。
fn scene_frame(win: &UIWindow) -> Option<CGRect> {
    unsafe {
        let scene: *mut AnyObject = msg_send![win, windowScene];
        let scene = scene.as_ref()?;
        let space: *mut AnyObject = msg_send![scene, coordinateSpace];
        let space = space.as_ref()?;
        let rect: CGRect = msg_send![space, bounds];
        (rect.size.width > 1.0 && rect.size.height > 1.0).then_some(rect)
    }
}

fn window_frame(win: &UIWindow) -> CGRect {
    unsafe { msg_send![win, frame] }
}

fn set_window_frame(win: &UIWindow, rect: CGRect) {
    unsafe {
        let _: () = msg_send![win, setFrame: rect];
        let _: () = msg_send![win, layoutIfNeeded];
    }
}

/// 只读探测：窗口 / 容器 / WKWebView 的几何是否已经落后于所在场景的尺寸。
/// 跟随线程每帧先问这个，没变化就完全不碰任何 UIKit 对象，避免无谓的 layout 抖动。
fn needs_fit(inner: *mut c_void) -> bool {
    unsafe {
        let Some(view) = inner.cast::<UIView>().as_ref() else {
            return false;
        };
        let Some(win) = view.window() else {
            return false;
        };
        let target = scene_frame(&win).unwrap_or_else(|| win.bounds());
        if target.size.width <= 1.0 || target.size.height <= 1.0 {
            return false;
        }
        if !rect_eq(window_frame(&win), target) {
            return true;
        }
        if !rect_eq(view.frame(), target) {
            return true;
        }
        match view.superview() {
            Some(sup) => !rect_eq(sup.frame(), target),
            None => true,
        }
    }
}

/// iOS 14.5+：WKWebView 长按会弹出系统文本选择放大镜（loupe）。
/// 纯 CSS `-webkit-user-select:none` 挡不住它；唯一可靠开关是
/// `WKPreferences.isTextInteractionEnabled = false`。代价是它会连 `<input>/<textarea>`
/// 一起禁用，因此默认关闭，前端在输入框获得焦点时再调用 `set_text_interaction` 重新打开。
pub(crate) fn set_text_interaction_on_webview(webview: &tauri::Webview, enabled: bool) {
    let _ = webview.with_webview(move |platform| {
        set_text_interaction(platform.inner(), enabled);
    });
}

fn set_text_interaction(inner: *mut c_void, enabled: bool) {
    unsafe {
        // objc2_web_kit 的 `WKWebView` 类型只在 macOS 导出；iOS 上 WKWebView 是 UIView 子类，
        // 这里把指针当 UIView 用，直接发 `configuration` selector，配置/偏好再走 iOS 可用的类型。
        let Some(view) = inner.cast::<UIView>().as_ref() else {
            return;
        };
        // 链接长按预览与文本选择放大镜同源触发，一并关掉，避免长按快进误弹预览。
        let config: Retained<WKWebViewConfiguration> = msg_send![view, configuration];
        let prefs = config.preferences();
        prefs.setTextInteractionEnabled(enabled);
        let _: () = msg_send![view, setAllowsLinkPreview: false];
    }
}

pub(crate) fn apply(webview: &tauri::Webview, nudge_frame: bool) {
    let Ok(dir) = webview.app_handle().path().app_data_dir() else {
        return;
    };
    let error_dir = dir.clone();
    // Tauri dispatches this closure on the main thread while retaining the WKWebView.
    if let Err(error) = webview.with_webview(move |platform| match normalize(platform.inner(), nudge_frame) {
        Some(message) => {
            let _ = crate::logging::append_debug(&dir, &message);
        }
        None => {
            let _ = crate::logging::append_debug(&dir, "[ios-layout] webview unavailable");
        }
    }) {
        let _ = crate::logging::append_debug(&error_dir, &format!("[ios-layout] capture failed: {error}"));
    }
}

/// iPhone Duo 内屏展开/合拢、旋转、分屏后，窗口几何会落后于场景尺寸（见 [`scene_frame`]）。
/// 这里起一个常驻的轻量跟随：每 400ms 只在几何真的不一致时重排一次，不写日志、不改 UI 状态。
pub(crate) fn start_scene_follower(webview: &tauri::Webview) {
    static STARTED: std::sync::Once = std::sync::Once::new();
    let wv = (*webview).clone();
    STARTED.call_once(move || {
        tauri::async_runtime::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                let _ = wv.with_webview(|platform| {
                    let inner = platform.inner();
                    if needs_fit(inner) {
                        let _ = normalize(inner, false);
                    }
                });
            }
        });
    });
}

fn normalize(inner: *mut c_void, nudge_frame: bool) -> Option<String> {
    unsafe {
        // WKWebView subclasses UIView; use UIKit's typed geometry methods rather than
        // hand-declaring CGRect/UIEdgeInsets Objective-C return ABIs.
        let view = inner.cast::<UIView>().as_ref()?;
        let scroll: Retained<UIScrollView> = msg_send![view, scrollView];
        let parent = view.superview();
        let window = view.window();

        // 1. Disable UIKit automatic safe-area contentInset adjustment on WKWebView's scrollView.
        // Without this, UIKit applies top+bottom safeAreaInsets (47+34 = 81pt) before
        // `viewport-fit=cover` is committed, shrinking the initial layout viewport height to
        // 844 - 81 = 763pt and leaving an 81pt unpainted band at the bottom until rotation.
        let mut behavior = scroll.contentInsetAdjustmentBehavior();
        if behavior != UIScrollViewContentInsetAdjustmentBehavior::Never {
            behavior = UIScrollViewContentInsetAdjustmentBehavior::Never;
            scroll.setContentInsetAdjustmentBehavior(behavior);
        }
        scroll.setAutomaticallyAdjustsScrollIndicatorInsets(false);

        let mut zero_insets = scroll.contentInset();
        zero_insets.top = 0.0;
        zero_insets.left = 0.0;
        zero_insets.bottom = 0.0;
        zero_insets.right = 0.0;
        let cur_insets = scroll.contentInset();
        if cur_insets.top != 0.0
            || cur_insets.left != 0.0
            || cur_insets.bottom != 0.0
            || cur_insets.right != 0.0
        {
            scroll.setContentInset(zero_insets);
            scroll.setScrollIndicatorInsets(zero_insets);
        }

        let mut zero_offset = scroll.contentOffset();
        zero_offset.x = 0.0;
        zero_offset.y = 0.0;
        let cur_offset = scroll.contentOffset();
        if cur_offset.x != 0.0 || cur_offset.y != 0.0 {
            scroll.setContentOffset(zero_offset);
        }

        // 2. 先把 UIWindow 自身撑到它所在 UIWindowScene 的 coordinateSpace.bounds。
        // tao 用 `[UIScreen mainScreen].bounds` 建窗、绑场景后不再重设 frame，内屏展开时窗口
        // 尺寸不会自己跟；不修这一步，下面把 view 撑满 window 也只是撑满一个错尺寸的窗口，
        // WKWebView 的布局视口（css 像素宽度）就一直停在旧值。
        let scene_rect = window.as_ref().and_then(|win| scene_frame(win));
        if let (Some(win), Some(sr)) = (window.as_ref(), scene_rect) {
            if !rect_eq(window_frame(win), sr) {
                set_window_frame(win, sr);
            }
        }

        // 3. Sync container and WKWebView frames to the scene bounds (fallback: window bounds),
        // and nudge WKWebView's frame on page-load completion so WebKit's `_frameOrBoundsChanged`
        // recomputes unobscured layout viewport dimensions under `viewport-fit=cover` without
        // needing a physical rotation.
        let target_frame = scene_rect
            .or_else(|| window.as_ref().map(|win| win.bounds()))
            .unwrap_or_else(|| view.bounds());
        if target_frame.size.width > 0.0 && target_frame.size.height > 1.0 {
            if let Some(sup) = parent.as_ref() {
                let sf = sup.frame();
                if !rect_eq(sf, target_frame) {
                    sup.setFrame(target_frame);
                }
            }
            let vf = view.frame();
            if nudge_frame || !rect_eq(vf, target_frame) {
                let mut nudged = target_frame;
                nudged.size.height = target_frame.size.height - 1.0;
                view.setFrame(nudged);
                view.layoutIfNeeded();
                view.setFrame(target_frame);
            }
            scroll.adjustedContentInsetDidChange();
            view.setNeedsLayout();
            view.layoutIfNeeded();
        }

        let message = format!(
            "[ios-layout] scene.bounds={:?} window.frame={:?} webview.frame={:?} webview.bounds={:?} \
             webview.safe={:?} parent.frame_bounds_safe={:?} window.bounds_safe={:?} \
             scroll.frame={:?} scroll.bounds={:?} scroll.contentInset={:?} \
             scroll.adjustedContentInset={:?} scroll.adjustmentBehavior={} scroll.contentOffset={:?}",
            scene_rect,
            window.as_ref().map(|win| window_frame(win)),
            view.frame(),
            view.bounds(),
            view.safeAreaInsets(),
            parent.as_ref().map(|v| (v.frame(), v.bounds(), v.safeAreaInsets())),
            window.as_ref().map(|v| (v.bounds(), v.safeAreaInsets())),
            scroll.frame(),
            scroll.bounds(),
            scroll.contentInset(),
            scroll.adjustedContentInset(),
            scroll.contentInsetAdjustmentBehavior().0,
            scroll.contentOffset(),
        );
        Some(message)
    }
}
