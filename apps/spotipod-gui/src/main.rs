//! SpotiPod window: the iPod Classic page (ui/ipod.html) in a webview, driven by the backend.

mod backend;

use std::time::Duration;

use wry::http::{Response, header::CONTENT_TYPE};
use std::fs;

use backend::Cmd;
use tao::{
    dpi::LogicalSize,
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder},
    window::WindowBuilder,
};
use wry::WebViewBuilder;

const PAGE: &str = include_str!("../ui/ipod.html");
/// Waits this long for a clean stop (cache save, Connect unregister) on close.
const QUIT_TIMEOUT: Duration = Duration::from_secs(3);

fn main() -> wry::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let event_loop = EventLoopBuilder::<String>::with_user_event().build();
    let window = std::sync::Arc::new(
        WindowBuilder::new()
            .with_title("SpotiPod")
            .with_inner_size(LogicalSize::new(460.0, 750.0))
            .with_resizable(false)
            .with_decorations(false)
            .with_transparent(true)
            .build(&event_loop)
            .expect("window")
    );

    let proxy = event_loop.create_proxy();
    let (tx, done) = backend::spawn(Box::new(move |msg| _ = proxy.send_event(msg)));

    let ipc_tx = tx.clone();

    let window_for_ipc = window.clone();

    let builder = WebViewBuilder::new()
        .with_html(PAGE)
        .with_background_color((0, 0, 0, 0))

        .with_custom_protocol("asset".into(), move |_, request| {
            let path = request.uri().path().trim_start_matches('/');
            let (body, mime) = match fs::read(path) {
                Ok(bytes) => {
                    let ext = path.split('.').last().unwrap_or("");
                    let mime = match ext {
                        "svg" => "image/svg+xml",
                        "png" => "image/png",
                        "jpg" | "jpeg" => "image/jpeg",
                        "mp3" => "audio/mpeg",
                        _ => "application/octet-stream",
                    };
                    (bytes, mime)
                }
                Err(e) => {
                    log::warn!("Asset not found {}: {}", path, e);
                    (Vec::new(), "text/plain")
                }
            };

            Response::builder()
                .header(CONTENT_TYPE, mime)
                .body(body.into())
                .unwrap()
        })
        .with_ipc_handler(move |req| {
            if req.body().contains("drag_window") {
                _ = window_for_ipc.drag_window();
                return;
            }
            match serde_json::from_str::<Cmd>(req.body()) {
                Ok(cmd) => _ = ipc_tx.send(cmd),
                Err(e) => log::warn!("bad ui command {}: {e}", req.body()),
            }
        });

    #[cfg(not(target_os = "linux"))]
    let webview = builder.build(&window)?;
    #[cfg(target_os = "linux")]
    let webview = {
        use tao::platform::unix::WindowExtUnix;
        use wry::WebViewBuilderExtUnix;
        builder.build_gtk(window.default_vbox().expect("gtk vbox"))?
    };

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(msg) => {
                let js = format!("window.ipod && ipod.receive({msg})");
                if let Err(e) = webview.evaluate_script(&js) {
                    log::warn!("ui update failed: {e}");
                }

                if msg.contains("drag_window") {
                    _ = window.drag_window();
                }
                let js = format!("window.ipod && ipod.receive({msg})");
                if let Err(e) = webview.evaluate_script(&js) {
                    log::warn!("ui update failed: {e}");
                }
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                window.set_visible(false);
                _ = tx.send(Cmd::Quit);
                _ = done.recv_timeout(QUIT_TIMEOUT);
                *control_flow = ControlFlow::Exit;
            }
            _ => {}
        }
    })
}
