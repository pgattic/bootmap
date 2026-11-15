//! Windowed viewer (winit + softbuffer): replay recordings or watch live.

use crate::live::{Scene, Source};
use crate::GraphArgs;
use anyhow::Result;
use std::num::NonZeroU32;
use std::rc::Rc;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

struct App {
    scene: Scene,
    window: Option<Rc<Window>>,
    surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
    pixmap: Option<tiny_skia::Pixmap>,
}

pub fn run(source: Source, graph: GraphArgs) -> Result<()> {
    let el = EventLoop::new()?;
    el.set_control_flow(ControlFlow::Poll);
    let mut app = App { scene: Scene::new(source, &graph), window: None, surface: None, pixmap: None };
    el.run_app(&mut app)?;
    Ok(())
}

impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("bootmap")
            .with_inner_size(winit::dpi::LogicalSize::new(1400.0, 900.0));
        let window = Rc::new(el.create_window(attrs).expect("create window"));
        let ctx = softbuffer::Context::new(window.clone()).expect("softbuffer context");
        self.surface = Some(softbuffer::Surface::new(&ctx, window.clone()).expect("softbuffer surface"));
        self.window = Some(window);
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, ev: WindowEvent) {
        match ev {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::KeyboardInput { event: KeyEvent { logical_key, state: ElementState::Pressed, .. }, .. } => {
                match logical_key {
                    Key::Named(NamedKey::Escape) => el.exit(),
                    Key::Named(NamedKey::Space) => self.scene.source.toggle_pause(),
                    Key::Named(NamedKey::ArrowLeft) => self.scene.source.seek(-0.25),
                    Key::Named(NamedKey::ArrowRight) => self.scene.source.seek(0.25),
                    Key::Character(c) if c == "q" => el.exit(),
                    Key::Character(c) if c == "r" => self.scene.source.restart(),
                    Key::Character(c) if c == "l" => {
                        self.scene.renderer.opts.labels = !self.scene.renderer.opts.labels
                    }
                    Key::Character(c) if c == "e" => {
                        self.scene.renderer.opts.extra_edges = !self.scene.renderer.opts.extra_edges
                    }
                    _ => {}
                }
            }
            WindowEvent::RedrawRequested => self.draw(),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _el: &ActiveEventLoop) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
        // ~60 fps without spinning a core.
        std::thread::sleep(std::time::Duration::from_millis(14));
    }
}

impl App {
    fn draw(&mut self) {
        let (Some(window), Some(surface)) = (&self.window, &mut self.surface) else { return };
        let size = window.inner_size();
        let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else { return };
        if surface.resize(w, h).is_err() {
            return;
        }
        let pm = match &mut self.pixmap {
            Some(p) if p.width() == w.get() && p.height() == h.get() => p,
            slot => slot.insert(tiny_skia::Pixmap::new(w.get(), h.get()).expect("pixmap")),
        };
        self.scene.frame(pm);
        let Ok(mut buf) = surface.buffer_mut() else { return };
        for (dst, px) in buf.iter_mut().zip(pm.data().chunks_exact(4)) {
            // Background is opaque, so premultiplied == straight here.
            *dst = (px[0] as u32) << 16 | (px[1] as u32) << 8 | px[2] as u32;
        }
        let _ = buf.present();
    }
}
