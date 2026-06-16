use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use log::{error, info, trace, warn};
use notify::Watcher;
use rfd::FileDialog;
use sem86_core::hw::Ev;
use sem86_core::hw::vga::{ModeSet, VideoMemory};
use sem86_video::{ModeSetReceiver, MouseState, VideoRenderer};
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::error::ExternalError;
use winit::event::*;
use winit::event_loop::ControlFlow;
use winit::keyboard::{Key, KeyCode, NamedKey, PhysicalKey};
use winit::window::{CursorGrabMode, Window, WindowAttributes};

pub struct App<'a> {
    start_unfocussed: bool,
    sender: Sender<Ev>,
    modeset: Option<Receiver<ModeSet>>,
    running: Arc<AtomicBool>,
    video_memory: VideoMemory,
    active_window: Option<ActiveWindow<'a>>,
    mouse_state: MouseState,
    surface_configured: bool,
    shader_changed_receiver: Receiver<Result<notify::Event, notify::Error>>,
    cursor_grabbed: bool,
    alt_down: bool,
    shift_down: bool,
    use_logical_keys: bool,
}

struct ActiveWindow<'a> {
    renderer: VideoRenderer<'a>,
    window: Arc<Window>,
}

impl<'a> App<'a> {
    pub fn new(
        start_unfocussed: bool, sender: Sender<Ev>, modeset: Option<Receiver<ModeSet>>, running: Arc<AtomicBool>,
        video_memory: VideoMemory,
    ) -> Self {
        let (tx, shader_changed_receiver) = channel();
        let mut watcher = notify::recommended_watcher(tx).unwrap();
        watcher
            .watch(Path::new("sem86-video/src/shader.wgsl"), notify::RecursiveMode::NonRecursive)
            .unwrap();

        Self {
            start_unfocussed,
            sender,
            modeset,
            running,
            video_memory,
            active_window: None,
            mouse_state: MouseState::default(),
            surface_configured: false,
            shader_changed_receiver,
            cursor_grabbed: false,
            alt_down: false,
            shift_down: false,
            use_logical_keys: true,
        }
    }

    pub fn capture(&mut self) -> (u32, u32, Vec<u8>) {
        let active = self.active_window.as_mut().unwrap();
        let size = active.window.inner_size();
        active.renderer.capture(size)
    }
}

impl ApplicationHandler<()> for App<'_> {
    fn resumed(&mut self, event_loop: &winit::event_loop::ActiveEventLoop) {
        info!("Creating window...");
        let window = Arc::new(
            event_loop
                .create_window(
                    WindowAttributes::default()
                        .with_title("VGA Output")
                        .with_inner_size(PhysicalSize::new(1024, 768))
                        .with_active(!self.start_unfocussed),
                )
                .unwrap(),
        );

        let size = window.inner_size();
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            ..Default::default()
        });

        let surface = instance.create_surface(window.clone()).unwrap();

        window.set_visible(true);
        window.request_redraw();

        let renderer = pollster::block_on(VideoRenderer::new(
            surface,
            instance,
            size,
            ModeSetReceiver::new(self.modeset.take().unwrap()),
            self.video_memory.clone(),
        ));
        self.active_window = Some(ActiveWindow {
            renderer,
            window,
        })
    }

    fn window_event(
        &mut self, event_loop: &winit::event_loop::ActiveEventLoop, _window_id: winit::window::WindowId,
        event: winit::event::WindowEvent,
    ) {
        if !self.running.load(Ordering::SeqCst) {
            event_loop.exit();
            return
        }

        while let Ok(ev) = self.shader_changed_receiver.try_recv() {
            match ev {
                Ok(ev) => {
                    if ev.kind.is_create() || ev.kind.is_modify() {
                        info!("Reloading shader");

                        pollster::block_on(
                            self.active_window
                                .as_mut()
                                .unwrap()
                                .renderer
                                .reload_pipeline(&std::fs::read_to_string("emui/src/shader.wgsl").unwrap()),
                        );
                    }
                },
                Err(e) => error!("{e}"),
            }
        }

        if !matches!(event, WindowEvent::RedrawRequested) {
            trace!("Event: {event:#?}");
        }

        let active = self.active_window.as_mut().unwrap();
        match event {
            WindowEvent::MouseInput {
                button: MouseButton::Right,
                state: ElementState::Released,
                ..
            } if !self.cursor_grabbed => {
                if let Some(new_fdd_image) = FileDialog::new().add_filter("Images", &["img"]).pick_file() {
                    self.sender.send(Ev::InsertFdd(new_fdd_image)).unwrap();
                }
            },
            WindowEvent::CursorMoved {
                ..
            } => {
                if self.cursor_grabbed {
                    let size = active.window.inner_size();
                    let middle = PhysicalPosition::new(size.width as f64 / 2., size.height as f64 / 2.);
                    active.window.set_cursor_position(middle).ok();
                }
            },
            WindowEvent::MouseWheel {
                delta, ..
            } => {
                let z_delta = match delta {
                    MouseScrollDelta::LineDelta(_, delta) => delta,
                    MouseScrollDelta::PixelDelta(physical_position) => physical_position.y as f32 / 64.0,
                };

                self.sender
                    .send(Ev::MouseMove(self.mouse_state.scroll_event(z_delta)))
                    .unwrap();
            },
            WindowEvent::MouseInput {
                state,
                button,
                ..
            } => {
                if button == MouseButton::Middle && state == ElementState::Released {
                    self.toggle_cursor_grab();
                } else if self.cursor_grabbed {
                    let val = match state {
                        ElementState::Pressed => true,
                        ElementState::Released => false,
                    };

                    let new = match button {
                        MouseButton::Left => Some(MouseState {
                            left_pressed: val,
                            ..self.mouse_state
                        }),
                        MouseButton::Right => Some(MouseState {
                            right_pressed: val,
                            ..self.mouse_state
                        }),
                        _ => None,
                    };

                    if let Some(new) = new {
                        self.sender
                            .send(Ev::MouseMove(
                                self.mouse_state.event_from_delta(new, PhysicalPosition::new(0.0, 0.0)),
                            ))
                            .unwrap();
                    }
                }
            },
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        state,
                        physical_key,
                        logical_key,
                        ..
                    },
                ..
            } => {
                if physical_key == PhysicalKey::Code(KeyCode::AltLeft) {
                    self.alt_down = state == ElementState::Pressed;
                }
                if physical_key == PhysicalKey::Code(KeyCode::ShiftLeft) {
                    self.shift_down = state == ElementState::Pressed;
                }

                info!("Key: {physical_key:?} / {logical_key:?}");
                match physical_key {
                    PhysicalKey::Code(key_code) => {
                        if key_code == KeyCode::KeyG && state == ElementState::Pressed && self.alt_down && self.shift_down {
                            self.toggle_cursor_grab();
                            return;
                        }

                        let p = match state {
                            ElementState::Pressed => 0,
                            ElementState::Released => 0x80,
                        };
                        let scancodes = match key_code {
                            KeyCode::Escape => &[0x01 + p] as &[_],
                            KeyCode::Digit1 => &[0x02 + p],
                            KeyCode::Digit2 => &[0x03 + p],
                            KeyCode::Digit3 => &[0x04 + p],
                            KeyCode::Digit4 => &[0x05 + p],
                            KeyCode::Digit5 => &[0x06 + p],
                            KeyCode::Digit6 => &[0x07 + p],
                            KeyCode::Digit7 => &[0x08 + p],
                            KeyCode::Digit8 => &[0x09 + p],
                            KeyCode::Digit9 => &[0x0A + p],
                            KeyCode::Digit0 => &[0x0B + p],
                            KeyCode::Minus => &[0x0C + p],
                            KeyCode::Equal => &[0x0D + p],
                            KeyCode::Backspace => &[0x0e + p],
                            KeyCode::BracketLeft => &[0x1a + p],
                            KeyCode::BracketRight => &[0x1b + p],
                            KeyCode::Enter => &[0x1c + p],
                            KeyCode::ControlLeft => &[0x1d + p],
                            KeyCode::Quote => &[0x28 + p],
                            KeyCode::Backquote => &[0x29 + p],
                            KeyCode::ShiftLeft => &[0x2a + p],
                            KeyCode::Backslash | KeyCode::IntlBackslash => &[0x2b + p],
                            KeyCode::Comma => &[0x33 + p],
                            KeyCode::Period => &[0x34 + p],
                            KeyCode::Slash => &[0x35 + p],
                            KeyCode::ShiftRight => &[0x36 + p],

                            // Swap ALT left and right to be able to input ALT without affecting the window.
                            KeyCode::AltRight => &[0x38 + p],
                            KeyCode::AltLeft => &[0xe0, 0x53 + p],
                            KeyCode::CapsLock => &[0x40 + p],
                            KeyCode::Space => &[0x39 + p],
                            KeyCode::Tab => &[0x0f + p],
                            KeyCode::NumLock => &[0x45 + p],
                            KeyCode::Numpad0 => &[0x52 + p],
                            KeyCode::Numpad1 => &[0x4f + p],
                            KeyCode::Numpad2 => &[0x50 + p],
                            KeyCode::Numpad3 => &[0x51 + p],
                            KeyCode::Numpad4 => &[0x4b + p],
                            KeyCode::Numpad5 => &[0x4c + p],
                            KeyCode::Numpad6 => &[0x4d + p],
                            KeyCode::Numpad7 => &[0x47 + p],
                            KeyCode::Numpad8 => &[0x48 + p],
                            KeyCode::Numpad9 => &[0x49 + p],
                            KeyCode::NumpadAdd => &[0x4e + p],
                            KeyCode::NumpadDecimal => &[0x53 + p],
                            KeyCode::NumpadMultiply => &[0x37 + p],
                            KeyCode::NumpadSubtract => &[0x4a + p],
                            KeyCode::ScrollLock => &[0x46 + p],
                            KeyCode::F1 => &[0x3b + p],
                            KeyCode::F2 => &[0x3c + p],
                            KeyCode::F3 => &[0x3d + p],
                            KeyCode::F4 => &[0x3e + p],
                            KeyCode::F5 => &[0x3f + p],
                            KeyCode::F6 => &[0x40 + p],
                            KeyCode::F7 => &[0x41 + p],
                            KeyCode::F8 => &[0x42 + p],
                            KeyCode::F9 => &[0x43 + p],
                            KeyCode::F10 => &[0x44 + p],
                            KeyCode::F11 => &[0x57 + p],
                            KeyCode::F12 => &[0x58 + p],
                            KeyCode::ContextMenu => &[0xe0, 0x5d + p],
                            KeyCode::ControlRight => &[0xe0, 0x1d + p],
                            KeyCode::SuperLeft => &[0xe0, 0x5b + p],
                            KeyCode::SuperRight => &[0xe0, 0x5c + p],
                            KeyCode::Delete => &[0xe0, 0x53 + p],
                            KeyCode::End => &[0xe0, 0x4f + p],
                            KeyCode::Home => &[0xe0, 0x47 + p],
                            KeyCode::Insert => &[0xe0, 0x52 + p],
                            KeyCode::PageDown => &[0xe0, 0x51 + p],
                            KeyCode::PageUp => &[0xe0, 0x49 + p],
                            KeyCode::ArrowDown => &[0xe0, 0x50 + p],
                            KeyCode::ArrowLeft => &[0xe0, 0x4b + p],
                            KeyCode::ArrowRight => &[0xe0, 0x4d + p],
                            KeyCode::ArrowUp => &[0xe0, 0x48 + p],
                            _ => {
                                // Use logical keys for letters to allow rebindings
                                if self.use_logical_keys {
                                    match logical_key {
                                        Key::Named(NamedKey::Escape) => &[0x01 + p] as &[_],
                                        Key::Character(s) if s.as_str().eq_ignore_ascii_case("Q") => &[0x10 + p],
                                        Key::Character(s) if s.as_str().eq_ignore_ascii_case("W") => &[0x11 + p],
                                        Key::Character(s) if s.as_str().eq_ignore_ascii_case("E") => &[0x12 + p],
                                        Key::Character(s) if s.as_str().eq_ignore_ascii_case("R") => &[0x13 + p],
                                        Key::Character(s) if s.as_str().eq_ignore_ascii_case("T") => &[0x14 + p],
                                        Key::Character(s) if s.as_str().eq_ignore_ascii_case("Y") => &[0x15 + p],
                                        Key::Character(s) if s.as_str().eq_ignore_ascii_case("U") => &[0x16 + p],
                                        Key::Character(s) if s.as_str().eq_ignore_ascii_case("I") => &[0x17 + p],
                                        Key::Character(s) if s.as_str().eq_ignore_ascii_case("O") => &[0x18 + p],
                                        Key::Character(s) if s.as_str().eq_ignore_ascii_case("P") => &[0x19 + p],
                                        Key::Character(s) if s.as_str().eq_ignore_ascii_case("A") => &[0x1e + p],
                                        Key::Character(s) if s.as_str().eq_ignore_ascii_case("S") => &[0x1f + p],
                                        Key::Character(s) if s.as_str().eq_ignore_ascii_case("D") => &[0x20 + p],
                                        Key::Character(s) if s.as_str().eq_ignore_ascii_case("F") => &[0x21 + p],
                                        Key::Character(s) if s.as_str().eq_ignore_ascii_case("G") => &[0x22 + p],
                                        Key::Character(s) if s.as_str().eq_ignore_ascii_case("H") => &[0x23 + p],
                                        Key::Character(s) if s.as_str().eq_ignore_ascii_case("J") => &[0x24 + p],
                                        Key::Character(s) if s.as_str().eq_ignore_ascii_case("K") => &[0x25 + p],
                                        Key::Character(s) if s.as_str().eq_ignore_ascii_case("L") => &[0x26 + p],
                                        Key::Character(s) if s.as_str().eq_ignore_ascii_case("Z") => &[0x2c + p],
                                        Key::Character(s) if s.as_str().eq_ignore_ascii_case("X") => &[0x2d + p],
                                        Key::Character(s) if s.as_str().eq_ignore_ascii_case("C") => &[0x2e + p],
                                        Key::Character(s) if s.as_str().eq_ignore_ascii_case("V") => &[0x2f + p],
                                        Key::Character(s) if s.as_str().eq_ignore_ascii_case("B") => &[0x30 + p],
                                        Key::Character(s) if s.as_str().eq_ignore_ascii_case("N") => &[0x31 + p],
                                        Key::Character(s) if s.as_str().eq_ignore_ascii_case("M") => &[0x32 + p],
                                        Key::Character(s)
                                            if s.as_str().eq_ignore_ascii_case(";") || s.as_str().eq_ignore_ascii_case(":") =>
                                        {
                                            &[0x27 + p]
                                        },
                                        key => {
                                            info!("Unmapped key: {key:?}");
                                            &[]
                                        },
                                    }
                                } else {
                                    match key_code {
                                        KeyCode::KeyQ => &[0x10 + p] as &[_],
                                        KeyCode::KeyW => &[0x11 + p],
                                        KeyCode::KeyE => &[0x12 + p],
                                        KeyCode::KeyR => &[0x13 + p],
                                        KeyCode::KeyT => &[0x14 + p],
                                        KeyCode::KeyY => &[0x15 + p],
                                        KeyCode::KeyU => &[0x16 + p],
                                        KeyCode::KeyI => &[0x17 + p],
                                        KeyCode::KeyO => &[0x18 + p],
                                        KeyCode::KeyP => &[0x19 + p],
                                        KeyCode::KeyA => &[0x1e + p],
                                        KeyCode::KeyS => &[0x1f + p],
                                        KeyCode::KeyD => &[0x20 + p],
                                        KeyCode::KeyF => &[0x21 + p],
                                        KeyCode::KeyG => &[0x22 + p],
                                        KeyCode::KeyH => &[0x23 + p],
                                        KeyCode::KeyJ => &[0x24 + p],
                                        KeyCode::KeyK => &[0x25 + p],
                                        KeyCode::KeyL => &[0x26 + p],
                                        KeyCode::KeyZ => &[0x2c + p],
                                        KeyCode::KeyX => &[0x2d + p],
                                        KeyCode::KeyC => &[0x2e + p],
                                        KeyCode::KeyV => &[0x2f + p],
                                        KeyCode::KeyB => &[0x30 + p],
                                        KeyCode::KeyN => &[0x31 + p],
                                        KeyCode::KeyM => &[0x32 + p],
                                        KeyCode::Semicolon => &[0x27 + p],
                                        key => {
                                            info!("Unmapped key: {key:?}");

                                            &[]
                                        },
                                    }
                                }
                            },
                        };

                        for &scancode in scancodes {
                            self.sender.send(Ev::ScanCode(scancode)).unwrap()
                        }
                    },
                    PhysicalKey::Unidentified(native_key_code) => error!("unidentified scancode: {native_key_code:?}"),
                }
            },
            WindowEvent::CloseRequested => {
                println!("Terminating window event loop");
                event_loop.exit();
            },
            WindowEvent::Resized(physical_size) => {
                self.surface_configured = true;
                active.renderer.resize(physical_size);
            },
            WindowEvent::RedrawRequested => {
                if !self.surface_configured {
                    return;
                }

                active.renderer.update();
                match active.renderer.render(|| active.window.pre_present_notify()) {
                    Ok(_) => {
                        event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(30)));
                    },
                    // Reconfigure the surface if it's lost or outdated
                    Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                        active.renderer.resize(active.window.inner_size());
                        active.window.request_redraw();
                    },
                    // The system is out of memory, we should probably quit
                    Err(wgpu::SurfaceError::OutOfMemory) => {
                        error!("OutOfMemory");
                        event_loop.exit();
                    },

                    // This happens when the a frame takes too long to present
                    Err(wgpu::SurfaceError::Timeout) => {
                        warn!("Surface timeout");
                        active.window.request_redraw();
                    },

                    Err(wgpu::SurfaceError::Other) => {
                        warn!("Unknown error");
                    },
                }
            },
            _ => {},
        }
    }

    fn about_to_wait(&mut self, _event_loop: &winit::event_loop::ActiveEventLoop) {
        if let Some(active) = self.active_window.as_mut() {
            active.window.request_redraw();
        }
    }

    fn device_event(&mut self, _event_loop: &winit::event_loop::ActiveEventLoop, _device_id: DeviceId, event: DeviceEvent) {
        if let DeviceEvent::MouseMotion {
            delta: (dx, dy),
        } = event
            && self.cursor_grabbed
        {
            self.sender
                .send(Ev::MouseMove(self.mouse_state.event_from_delta(
                    self.mouse_state,
                    PhysicalPosition {
                        x: dx,
                        y: dy,
                    },
                )))
                .unwrap();
        }
    }
}

impl<'a> App<'a> {
    fn toggle_cursor_grab(&mut self) {
        let active = self.active_window.as_mut().unwrap();
        self.cursor_grabbed = !self.cursor_grabbed;
        active.window.set_cursor_visible(!self.cursor_grabbed);
        let e = active.window.set_cursor_grab(if self.cursor_grabbed {
            CursorGrabMode::Locked
        } else {
            CursorGrabMode::None
        });

        if let Err(ExternalError::NotSupported(_)) = e {
            match active.window.set_cursor_grab(if self.cursor_grabbed {
                CursorGrabMode::Confined
            } else {
                CursorGrabMode::None
            }) {
                Ok(_) => (),
                Err(e) => println!("unable to confine or lock cursor: {e}"),
            }
        }
    }
}
