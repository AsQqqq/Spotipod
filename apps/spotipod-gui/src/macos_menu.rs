//! macOS menu bar: app menu (About, Hide, Quit) and Edit menu for the webview.

use std::sync::{Mutex, OnceLock};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, Sel};
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAboutPanelOptionApplicationName, NSAboutPanelOptionApplicationVersion,
    NSAboutPanelOptionCredits, NSAboutPanelOptionVersion, NSApplication, NSColor,
    NSEventModifierFlags, NSFont, NSFontAttributeName, NSForegroundColorAttributeName, NSMenu,
    NSMenuItem, NSMutableParagraphStyle, NSParagraphStyleAttributeName, NSTextAlignment,
};
use objc2_foundation::{NSAttributedString, NSDictionary, NSString, ns_string};
use spotipod_core::settings::{AUTHOR, COMPANY, VERSION};
use tao::event_loop::EventLoopProxy;

const APP_NAME: &str = "SpotiPod";
const REPOSITORY: &str = "github.com/AsQqqq/SpotiPOD";
const COPYRIGHT: &str = "Copyright © 2026 trade-cloud. PolyForm Noncommercial 1.0.0.";
/// User event that runs the normal shutdown path in main.rs.
const QUIT_EVENT: &str = "__quit__";

static PROXY: OnceLock<Mutex<EventLoopProxy<String>>> = OnceLock::new();

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SpotiPodMenuTarget"]
    struct MenuTarget;

    impl MenuTarget {
        #[unsafe(method(showAbout:))]
        fn show_about(&self, _sender: Option<&AnyObject>) {
            show_about_panel(self.mtm());
        }

        #[unsafe(method(quit:))]
        fn quit(&self, _sender: Option<&AnyObject>) {
            if let Some(proxy) = PROXY.get() {
                _ = proxy.lock().unwrap().send_event(QUIT_EVENT.to_string());
            }
        }
    }
);

impl MenuTarget {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

/// Builds the main menu. Call on the main thread before the event loop runs.
pub fn install(proxy: EventLoopProxy<String>) {
    let Some(mtm) = MainThreadMarker::new() else {
        log::error!("could not build the menu outside the main thread");
        return;
    };
    _ = PROXY.set(Mutex::new(proxy));
    // Menu items hold their target weakly; keep it for the app's lifetime.
    let target: &'static MenuTarget = Box::leak(Box::new(MenuTarget::new(mtm)));

    let app_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str(APP_NAME));
    app_menu.addItem(&item(mtm, &format!("About {APP_NAME}"), Some(sel!(showAbout:)), "", Some(target)));
    app_menu.addItem(&NSMenuItem::separatorItem(mtm));
    app_menu.addItem(&item(mtm, &format!("Hide {APP_NAME}"), Some(sel!(hide:)), "h", None));
    let hide_others = item(mtm, "Hide Others", Some(sel!(hideOtherApplications:)), "h", None);
    hide_others.setKeyEquivalentModifierMask(
        NSEventModifierFlags::Command | NSEventModifierFlags::Option,
    );
    app_menu.addItem(&hide_others);
    app_menu.addItem(&item(mtm, "Show All", Some(sel!(unhideAllApplications:)), "", None));
    app_menu.addItem(&NSMenuItem::separatorItem(mtm));
    app_menu.addItem(&item(mtm, &format!("Quit {APP_NAME}"), Some(sel!(quit:)), "q", Some(target)));

    // Standard first-responder actions, so copy/paste work in the login page.
    let edit_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), ns_string!("Edit"));
    edit_menu.addItem(&item(mtm, "Undo", Some(sel!(undo:)), "z", None));
    edit_menu.addItem(&item(mtm, "Redo", Some(sel!(redo:)), "Z", None));
    edit_menu.addItem(&NSMenuItem::separatorItem(mtm));
    edit_menu.addItem(&item(mtm, "Cut", Some(sel!(cut:)), "x", None));
    edit_menu.addItem(&item(mtm, "Copy", Some(sel!(copy:)), "c", None));
    edit_menu.addItem(&item(mtm, "Paste", Some(sel!(paste:)), "v", None));
    edit_menu.addItem(&item(mtm, "Select All", Some(sel!(selectAll:)), "a", None));

    let menubar = NSMenu::new(mtm);
    for submenu in [&app_menu, &edit_menu] {
        let holder = NSMenuItem::new(mtm);
        holder.setSubmenu(Some(submenu));
        menubar.addItem(&holder);
    }
    NSApplication::sharedApplication(mtm).setMainMenu(Some(&menubar));
}

fn item(
    mtm: MainThreadMarker,
    title: &str,
    action: Option<Sel>,
    key: &str,
    target: Option<&MenuTarget>,
) -> Retained<NSMenuItem> {
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(title),
            action,
            &NSString::from_str(key),
        )
    };
    if let Some(target) = target {
        unsafe { item.setTarget(Some(target)) };
    }
    item
}

/// Standard About panel filled with the same data as Settings > About.
fn show_about_panel(mtm: MainThreadMarker) {
    let name = NSString::from_str(APP_NAME);
    let version = NSString::from_str(VERSION);
    let build = NSString::from_str("");
    let credits = credits();
    let copyright = NSString::from_str(COPYRIGHT);

    let keys: [&NSString; 5] = unsafe {
        [
            NSAboutPanelOptionApplicationName,
            NSAboutPanelOptionApplicationVersion,
            NSAboutPanelOptionVersion,
            NSAboutPanelOptionCredits,
            ns_string!("Copyright"),
        ]
    };
    let values: [&AnyObject; 5] = [&name, &version, &build, &credits, &copyright];
    let options = NSDictionary::from_slices(&keys, &values);

    let app = NSApplication::sharedApplication(mtm);
    unsafe { app.orderFrontStandardAboutPanelWithOptions(&options) };
}

fn credits() -> Retained<NSAttributedString> {
    let text = NSString::from_str(&format!(
        "Company: {COMPANY}\nAuthor: {AUTHOR}\n{REPOSITORY}"
    ));
    let font = NSFont::systemFontOfSize(NSFont::smallSystemFontSize());
    let color = NSColor::secondaryLabelColor();
    let paragraph = NSMutableParagraphStyle::new();
    paragraph.setAlignment(NSTextAlignment::Center);

    let keys = unsafe {
        [
            NSFontAttributeName,
            NSForegroundColorAttributeName,
            NSParagraphStyleAttributeName,
        ]
    };
    let values: [&AnyObject; 3] = [&font, &color, &paragraph];
    let attributes = NSDictionary::from_slices(&keys, &values);
    unsafe { NSAttributedString::new_with_attributes(&text, &attributes) }
}
