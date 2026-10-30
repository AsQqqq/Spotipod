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
use wry::PageLoadEvent;

const PAGE: &str = include_str!("../ui/ipod.html");
/// Waits this long for a clean stop (cache save, Connect unregister) on close.
const QUIT_TIMEOUT: Duration = Duration::from_secs(3);

fn main() -> wry::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let event_loop = EventLoopBuilder::<String>::with_user_event().build();
    let window = std::sync::Arc::new(
        WindowBuilder::new()
            .with_title("SpotiPod")
            .with_inner_size(LogicalSize::new(415.0, 692.0))
            .with_min_inner_size(LogicalSize::new(320.0, 500.0))
            .with_max_inner_size(LogicalSize::new(600.0, 900.0))
            .with_resizable(true)
            .with_decorations(false)
            .with_transparent(true)
            .build(&event_loop)
            .expect("window")
    );

    let proxy = event_loop.create_proxy();

    let quit_proxy = event_loop.create_proxy();

    let proxy_for_backend = proxy.clone();
    let (tx, done) = backend::spawn(Box::new(move |msg| _ = proxy_for_backend.send_event(msg)));

    let ipc_tx = tx.clone();
    let ipc_tx_for_loop = ipc_tx.clone();

    let window_for_ipc = window.clone();

    let proxy_for_ipc = proxy.clone();

    let builder = WebViewBuilder::new()
        .with_html(PAGE)
        .with_background_color((0, 0, 0, 0))
        .with_on_page_load_handler(move |event, _url| {
            if matches!(event, PageLoadEvent::Finished) {
                let _ = proxy_for_ipc.send_event("__page_loaded__".to_string());
            }
        })

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
            let body = req.body();

            if req.body().contains("drag_window") {
                _ = window_for_ipc.drag_window();
                return;
            }
            if body.contains("minimize") {
                _ = window_for_ipc.set_minimized(true);
                return;
            }
            if body.contains("quit") {
                _ = quit_proxy.send_event("__quit__".to_string());
                return;
            }

            // if body.contains("resize_to") {
            //     #[derive(serde::Deserialize, Debug)]
            //     struct ResizeTarget { width: f64, height: f64 }
                
            //     if let Ok(data) = serde_json::from_str::<ResizeTarget>(body) {
            //         let scale = window_for_ipc.scale_factor();
                    
            //         let target_w = (data.width * scale).clamp(320.0 * scale, 800.0 * scale) as u32;
            //         let target_h = (data.height * scale).clamp(500.0 * scale, 1000.0 * scale) as u32;
                    
            //         _ = window_for_ipc.set_inner_size(tao::dpi::PhysicalSize::new(target_w, target_h));
            //         window_for_ipc.request_redraw();
            //     }
            //     return;
            // }

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
                if msg == "__page_loaded__" {
                    let _ = ipc_tx_for_loop.send(Cmd::Ready);
                    return;
                }
                let js = format!("window.ipod && ipod.receive({msg})");
                if let Err(e) = webview.evaluate_script(&js) {
                    log::warn!("ui update failed: {e}");
                } else {
                    log::info!("eval ok");
                }

                if msg.contains("drag_window") {
                    _ = window.drag_window();
                }

                if msg == "__quit__" {
                    window.set_visible(false);
                    _ = tx.send(Cmd::Quit);
                    _ = done.recv_timeout(QUIT_TIMEOUT);
                    *control_flow = ControlFlow::Exit;
                    return;
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
