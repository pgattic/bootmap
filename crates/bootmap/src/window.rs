//! Windowed viewer (winit + softbuffer): replay recordings or watch live.
//!
//! Mouse: hover a unit to highlight its neighbors and show details, drag a
//! unit to move it (it rejoins the physics on release), drag the background
//! to pan, scroll to zoom at the cursor.

use crate::live::{Scene, Source};
use crate::GraphArgs;
use anyhow::Result;
use std::num::NonZeroU32;
use std::rc::Rc;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{CursorIcon, Window, WindowId};

enum Drag {
    None,
    /// Dragging the background: last cursor position.
    Pan(f32, f32),
    /// Dragging a node (the scene tracks which).
    Node,
}

struct App {
    scene: Scene,
    window: Option<Rc<Window>>,
    surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
    pixmap: Option<tiny_skia::Pixmap>,
    cursor: (f32, f32),
    drag: Drag,
    last_frame: std::time::Instant,
}

pub fn run(source: Source, graph: GraphArgs, look: bootmap_core::config::Look) -> Result<()> {
    let el = EventLoop::new()?;
    el.set_control_flow(ControlFlow::Poll);
    // The viewer is for looking at the graph: the whole of it, even in prune
    // mode (where a finished boot would leave only the center). `p` prunes.
    let mut scene = Scene::new(source, &graph, look);
    scene.renderer.opts.prune = false;
    let mut app = App {
        scene,
        window: None,
        surface: None,
        pixmap: None,
        cursor: (0.0, 0.0),
        drag: Drag::None,
        last_frame: std::time::Instant::now(),
    };
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
        self.surface =
            Some(softbuffer::Surface::new(&ctx, window.clone()).expect("softbuffer surface"));
        self.window = Some(window);
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, ev: WindowEvent) {
        match ev {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        logical_key,
                        state: ElementState::Pressed,
                        ..
                    },
                ..
            } => self.key(el, logical_key),
            WindowEvent::CursorMoved { position, .. } => {
                let (x, y) = (position.x as f32, position.y as f32);
                self.cursor_moved(x, y);
            }
            WindowEvent::CursorLeft { .. } => {
                if matches!(self.drag, Drag::None) {
                    self.scene.hover = None;
                }
            }
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } => match state {
                ElementState::Pressed => self.press(),
                ElementState::Released => self.release(),
            },
            WindowEvent::MouseWheel { delta, .. } => {
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y,
                    MouseScrollDelta::PixelDelta(p) => p.y as f32 / 60.0,
                };
                let (w, h) = self.size();
                self.scene.auto_fit = false;
                self.scene
                    .cam
                    .zoom_at(w, h, self.cursor.0, self.cursor.1, 1.15f32.powf(lines));
            }
            WindowEvent::RedrawRequested => self.draw(),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _el: &ActiveEventLoop) {
        // Cap at ~60 fps: sleep only for what is left of the frame budget.
        let budget = std::time::Duration::from_micros(16_600);
        std::thread::sleep(budget.saturating_sub(self.last_frame.elapsed()));
        self.last_frame = std::time::Instant::now();
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }
}

impl App {
    fn size(&self) -> (f32, f32) {
        self.pixmap
            .as_ref()
            .map(|p| (p.width() as f32, p.height() as f32))
            .unwrap_or((1.0, 1.0))
    }

    fn key(&mut self, el: &ActiveEventLoop, key: Key) {
        let s = &mut self.scene;
        match key {
            Key::Named(NamedKey::Escape) => el.exit(),
            Key::Named(NamedKey::Space) => s.source.toggle_pause(),
            Key::Named(NamedKey::ArrowLeft) => s.source.seek(-0.25),
            Key::Named(NamedKey::ArrowRight) => s.source.seek(0.25),
            Key::Character(c) => match c.as_str() {
                "q" => el.exit(),
                "r" => s.source.restart(),
                "l" => s.renderer.opts.labels = !s.renderer.opts.labels,
                "e" => s.renderer.opts.extra_edges = !s.renderer.opts.extra_edges,
                "[" => s.adjust_radial(-0.1),
                "]" => s.adjust_radial(0.1),
                "f" | "0" => s.auto_fit = true,
                "p" => s.renderer.opts.prune = !s.renderer.opts.prune,
                _ => {}
            },
            _ => {}
        }
    }

    fn cursor_moved(&mut self, x: f32, y: f32) {
        let (w, h) = self.size();
        self.cursor = (x, y);
        match self.drag {
            Drag::Pan(lx, ly) => {
                let cam = &mut self.scene.cam;
                cam.cx -= (x - lx) / cam.scale;
                cam.cy -= (y - ly) / cam.scale;
                self.drag = Drag::Pan(x, y);
            }
            Drag::Node => {
                let world = self.scene.cam.to_world(w, h, x, y);
                self.scene.drag_to(world);
            }
            Drag::None => {
                let hit = self.scene.pick(w, h, x, y);
                self.scene.hover = hit.map(|i| self.scene.node_name(i));
                if let Some(win) = &self.window {
                    win.set_cursor(if hit.is_some() {
                        CursorIcon::Pointer
                    } else {
                        CursorIcon::Default
                    });
                }
            }
        }
    }

    fn press(&mut self) {
        let (w, h) = self.size();
        let (x, y) = self.cursor;
        match self.scene.pick(w, h, x, y) {
            Some(i) => {
                self.scene.begin_drag(i);
                self.scene.auto_fit = false;
                self.drag = Drag::Node;
            }
            None => {
                self.scene.auto_fit = false;
                self.drag = Drag::Pan(x, y);
                if let Some(win) = &self.window {
                    win.set_cursor(CursorIcon::Grabbing);
                }
            }
        }
    }

    fn release(&mut self) {
        if matches!(self.drag, Drag::Node) {
            self.scene.end_drag();
        }
        self.drag = Drag::None;
        if let Some(win) = &self.window {
            win.set_cursor(CursorIcon::Default);
        }
    }

    fn draw(&mut self) {
        let (Some(window), Some(surface)) = (&self.window, &mut self.surface) else {
            return;
        };
        let size = window.inner_size();
        // The desktop's scale for this window (follows it across monitors).
        self.scene.renderer.ui_scale = window.scale_factor() as f32;
        let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else {
            return;
        };
        if surface.resize(w, h).is_err() {
            return;
        }
        let pm = match &mut self.pixmap {
            Some(p) if p.width() == w.get() && p.height() == h.get() => p,
            slot => slot.insert(tiny_skia::Pixmap::new(w.get(), h.get()).expect("pixmap")),
        };
        self.scene.frame(pm);
        let Ok(mut buf) = surface.buffer_mut() else {
            return;
        };
        for (dst, px) in buf.iter_mut().zip(pm.data().as_chunks::<4>().0) {
            // Background is opaque, so premultiplied == straight here.
            *dst = (px[0] as u32) << 16 | (px[1] as u32) << 8 | px[2] as u32;
        }
        let _ = buf.present();
    }
}
