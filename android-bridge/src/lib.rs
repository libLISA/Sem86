use std::fs::File;
use std::io::{Cursor, Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Instant;

use android_logger::{Config, FilterBuilder};
use ffi::AndroidApp;
use liblisa::arch::CpuState;
use log::info;
use lz4_flex::frame::FrameDecoder;
use sem86_arch::mem::{Mem32, Shm};
use sem86_core::arch::intel386::{GpReg, State};
use sem86_core::decoder::PackedInstrSem;
use sem86_core::emulator::EmulatorContext;
use sem86_core::hw::storage::DiskData;
use sem86_core::hw::vga::VideoMemory;
use sem86_core::hw::{Ev, Hw};
use sem86_core::time::EmulatorClock;
use sem86_video::{ModeSetReceiver, MouseState, VideoRenderer};
use wgpu::Instance;
use winit::dpi::{PhysicalPosition, PhysicalSize};

use crate::ffi::{AssetReader, create_surface};

mod ffi;
mod keycodes;

pub struct RunningEmulator {
    emulator_thread: Option<JoinHandle<()>>,
    video_memory: VideoMemory,
    receiver: Option<ModeSetReceiver>,
    renderer: Option<VideoRenderer<'static>>,
    event_sender: std::sync::mpsc::Sender<sem86_core::hw::Ev>,
    mouse_state: MouseState,
    snapshot_writer: Arc<Mutex<Option<Box<dyn Write + Send + Sync>>>>,
}

static ASSET_READER: OnceLock<AssetReader> = OnceLock::new();
static EMULATORS: Mutex<Vec<RunningEmulator>> = Mutex::new(Vec::new());

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub extern "C" fn JNI_OnLoad(vm: jni::JavaVM, res: *mut std::os::raw::c_void) -> jni::sys::jint {
    let env = vm.get_env().unwrap();
    let vm = vm.get_java_vm_pointer() as *mut std::os::raw::c_void;
    unsafe {
        ndk_context::initialize_android_context(vm, res);
    }
    jni::JNIVersion::V6.into()
}

// Required because we depend on `android-activity`
#[unsafe(no_mangle)]
pub extern "C" fn android_main(_app: AndroidApp) {
    unreachable!()
}

#[unsafe(no_mangle)]
pub extern "C" fn Java_nl_liblisa_sem86_RustInterop_test() -> u64 {
    0x1112_3456_78ab_cdef
}

#[unsafe(no_mangle)]
pub extern "C" fn Java_nl_liblisa_sem86_RustInterop_init(_env: jni::JNIEnv, _class: jni::objects::JClass) {
    std::panic::set_hook(Box::new(|info| {
        log::error!("{}", info);
    }));

    android_logger::init_once(
        Config::default()
            .with_tag("android-bridge")
            .with_max_level(log::LevelFilter::Trace)
            .with_filter(
                FilterBuilder::new()
                    .filter_level(log::LevelFilter::Error)
                    .filter_module("android_bridge", log::LevelFilter::Trace)
                    .filter_module("sem86_core::hw::sound", log::LevelFilter::Warn)
                    .filter_module("sem86_core::hw::net", log::LevelFilter::Trace)
                    .filter_module("sem86_core::emulator::perf", log::LevelFilter::Info)
                    .filter_module("sem86_core::emulator", log::LevelFilter::Off)
                    // .filter_module("sem86_core::codegen::mm", log::LevelFilter::Trace)
                    .filter_module("sem86_video", log::LevelFilter::Debug)
                    .build(),
            ),
    );
}

#[unsafe(no_mangle)]
pub extern "C" fn Java_nl_liblisa_sem86_RustInterop_setAssetManager(
    env: jni::JNIEnv, _class: jni::objects::JClass, asset_manager: jni::objects::JObject,
) {
    ASSET_READER.get_or_init(|| AssetReader::new(env, asset_manager));
}

#[unsafe(no_mangle)]
pub extern "C" fn Java_nl_liblisa_sem86_RustInterop_resizeSurface(
    _env: jni::JNIEnv, _class: jni::objects::JClass, emulator_index: u64, width: u32, height: u32,
) {
    let mut emulators = EMULATORS.lock().unwrap();
    let renderer = emulators[emulator_index as usize].renderer.as_mut().unwrap();
    renderer.resize(PhysicalSize::new(width, height));
}

#[unsafe(no_mangle)]
pub extern "C" fn Java_nl_liblisa_sem86_RustInterop_dropSurface(
    _env: jni::JNIEnv, _class: jni::objects::JClass, emulator_index: u64,
) {
    let mut emulators = EMULATORS.lock().unwrap();
    let emulator = &mut emulators[emulator_index as usize];
    let renderer = emulator.renderer.take().unwrap();

    // TODO: This doesn't persist the current modeset. This breaks rendering when we connect a new surface.
    let receiver = renderer.into_inner();

    emulator.receiver = Some(receiver);
}

#[unsafe(no_mangle)]
pub extern "C" fn Java_nl_liblisa_sem86_RustInterop_mouseMove(
    _env: jni::JNIEnv, _class: jni::objects::JClass, emulator_index: u64, dx: f64, dy: f64,
) {
    let mut emulators = EMULATORS.lock().unwrap();
    let emulator = &mut emulators[emulator_index as usize];

    let ev = emulator
        .mouse_state
        .event_from_delta(emulator.mouse_state, PhysicalPosition::new(dx, dy));

    info!("Sending mouse movement: {ev:?}");
    emulator.event_sender.send(Ev::MouseMove(ev)).unwrap();
}

#[unsafe(no_mangle)]
pub extern "C" fn Java_nl_liblisa_sem86_RustInterop_mouseScroll(
    _env: jni::JNIEnv, _class: jni::objects::JClass, emulator_index: u64, dz: f64,
) {
    let mut emulators = EMULATORS.lock().unwrap();
    let emulator = &mut emulators[emulator_index as usize];

    let ev = emulator.mouse_state.scroll_event(dz as f32);

    info!("Sending mouse movement: {ev:?}");
    emulator.event_sender.send(Ev::MouseMove(ev)).unwrap();
}

#[unsafe(no_mangle)]
pub extern "C" fn Java_nl_liblisa_sem86_RustInterop_keyboardInput(
    _env: jni::JNIEnv, _class: jni::objects::JClass, emulator_index: u64, is_down: bool, keycode: u32,
) {
    let mut emulators = EMULATORS.lock().unwrap();
    let emulator = &mut emulators[emulator_index as usize];

    let p = if is_down { 0 } else { 0x80 };
    let scancodes = match keycode {
        keycodes::KEYCODE_0 => &[0x0B + p] as &[_],
        keycodes::KEYCODE_1 => &[0x02 + p],
        keycodes::KEYCODE_2 => &[0x03 + p],
        keycodes::KEYCODE_3 => &[0x04 + p],
        keycodes::KEYCODE_4 => &[0x05 + p],
        keycodes::KEYCODE_5 => &[0x06 + p],
        keycodes::KEYCODE_6 => &[0x07 + p],
        keycodes::KEYCODE_7 => &[0x08 + p],
        keycodes::KEYCODE_8 => &[0x09 + p],
        keycodes::KEYCODE_9 => &[0x0A + p],
        keycodes::KEYCODE_APOSTROPHE => &[],
        keycodes::KEYCODE_AT => &[],
        keycodes::KEYCODE_BACKSLASH => &[],
        keycodes::KEYCODE_BOOKMARK => &[],
        keycodes::KEYCODE_BREAK => &[],
        keycodes::KEYCODE_CAPS_LOCK => &[],
        keycodes::KEYCODE_COMMA => &[],
        keycodes::KEYCODE_CTRL_LEFT => &[],
        keycodes::KEYCODE_CTRL_RIGHT => &[],
        // "DEL" is actually the backspace key
        keycodes::KEYCODE_DEL => &[0x0e + p],
        keycodes::KEYCODE_ENTER => &[0x1c + p],
        keycodes::KEYCODE_DPAD_DOWN => &[0xe0, 0x50 + p],
        keycodes::KEYCODE_DPAD_LEFT => &[0xe0, 0x4b + p],
        keycodes::KEYCODE_DPAD_RIGHT => &[0xe0, 0x4d + p],
        keycodes::KEYCODE_DPAD_UP => &[0xe0, 0x48 + p],
        keycodes::KEYCODE_EQUALS => &[0x0D + p],
        keycodes::KEYCODE_ESCAPE => &[0x01 + p],
        keycodes::KEYCODE_EXPLORER => &[],
        keycodes::KEYCODE_F1 => &[0x3b + p],
        keycodes::KEYCODE_F2 => &[0x3c + p],
        keycodes::KEYCODE_F3 => &[0x3d + p],
        keycodes::KEYCODE_F4 => &[0x3e + p],
        keycodes::KEYCODE_F5 => &[0x3f + p],
        keycodes::KEYCODE_F6 => &[0x40 + p],
        keycodes::KEYCODE_F7 => &[0x41 + p],
        keycodes::KEYCODE_F8 => &[0x42 + p],
        keycodes::KEYCODE_F9 => &[0x43 + p],
        keycodes::KEYCODE_F10 => &[0x44 + p],
        keycodes::KEYCODE_F11 => &[0x57 + p],
        keycodes::KEYCODE_F12 => &[0x58 + p],
        keycodes::KEYCODE_GRAVE => &[],
        keycodes::KEYCODE_HOME => &[],
        keycodes::KEYCODE_INSERT => &[],
        keycodes::KEYCODE_LEFT_BRACKET => &[],
        keycodes::KEYCODE_LOCK => &[],
        keycodes::KEYCODE_MINUS => &[0xC + p],
        keycodes::KEYCODE_NUM => &[],
        keycodes::KEYCODE_NUM_LOCK => &[],
        keycodes::KEYCODE_NUMPAD_0 => &[0x52 + p],
        keycodes::KEYCODE_NUMPAD_1 => &[0x4f + p],
        keycodes::KEYCODE_NUMPAD_2 => &[0x50 + p],
        keycodes::KEYCODE_NUMPAD_3 => &[0x51 + p],
        keycodes::KEYCODE_NUMPAD_4 => &[0x4b + p],
        keycodes::KEYCODE_NUMPAD_5 => &[0x4c + p],
        keycodes::KEYCODE_NUMPAD_6 => &[0x4d + p],
        keycodes::KEYCODE_NUMPAD_7 => &[0x47 + p],
        keycodes::KEYCODE_NUMPAD_8 => &[0x48 + p],
        keycodes::KEYCODE_NUMPAD_9 => &[0x49 + p],
        keycodes::KEYCODE_NUMPAD_ADD => &[0x4e + p],
        keycodes::KEYCODE_NUMPAD_COMMA => &[0x53 + p],
        keycodes::KEYCODE_NUMPAD_DIVIDE => &[],
        keycodes::KEYCODE_NUMPAD_DOT => &[],
        keycodes::KEYCODE_NUMPAD_ENTER => &[],
        keycodes::KEYCODE_NUMPAD_EQUALS => &[],
        keycodes::KEYCODE_NUMPAD_LEFT_PAREN => &[],
        keycodes::KEYCODE_NUMPAD_MULTIPLY => &[],
        keycodes::KEYCODE_NUMPAD_RIGHT_PAREN => &[],
        keycodes::KEYCODE_NUMPAD_SUBTRACT => &[],
        keycodes::KEYCODE_PAGE_DOWN => &[],
        keycodes::KEYCODE_PAGE_UP => &[],
        keycodes::KEYCODE_PERIOD => &[0x34 + p],
        keycodes::KEYCODE_PLUS => &[],
        keycodes::KEYCODE_POUND => &[],
        keycodes::KEYCODE_RIGHT_BRACKET => &[],
        keycodes::KEYCODE_SCROLL_LOCK => &[],
        keycodes::KEYCODE_SEMICOLON => &[],
        keycodes::KEYCODE_SHIFT_LEFT => &[0x2a + p],
        keycodes::KEYCODE_SHIFT_RIGHT => &[0x36 + p],
        keycodes::KEYCODE_SLASH => &[0x35 + p],
        keycodes::KEYCODE_SPACE => &[0x39 + p],
        keycodes::KEYCODE_STAR => &[],
        keycodes::KEYCODE_TAB => &[],
        keycodes::KEYCODE_Q => &[0x10 + p],
        keycodes::KEYCODE_W => &[0x11 + p],
        keycodes::KEYCODE_E => &[0x12 + p],
        keycodes::KEYCODE_R => &[0x13 + p],
        keycodes::KEYCODE_T => &[0x14 + p],
        keycodes::KEYCODE_Y => &[0x15 + p],
        keycodes::KEYCODE_U => &[0x16 + p],
        keycodes::KEYCODE_I => &[0x17 + p],
        keycodes::KEYCODE_O => &[0x18 + p],
        keycodes::KEYCODE_P => &[0x19 + p],
        keycodes::KEYCODE_A => &[0x1e + p],
        keycodes::KEYCODE_S => &[0x1f + p],
        keycodes::KEYCODE_D => &[0x20 + p],
        keycodes::KEYCODE_F => &[0x21 + p],
        keycodes::KEYCODE_G => &[0x22 + p],
        keycodes::KEYCODE_H => &[0x23 + p],
        keycodes::KEYCODE_J => &[0x24 + p],
        keycodes::KEYCODE_K => &[0x25 + p],
        keycodes::KEYCODE_L => &[0x26 + p],
        keycodes::KEYCODE_Z => &[0x2c + p],
        keycodes::KEYCODE_X => &[0x2d + p],
        keycodes::KEYCODE_C => &[0x2e + p],
        keycodes::KEYCODE_V => &[0x2f + p],
        keycodes::KEYCODE_B => &[0x30 + p],
        keycodes::KEYCODE_N => &[0x31 + p],
        keycodes::KEYCODE_M => &[0x32 + p],
        _ => &[],
    };

    for &code in scancodes {
        emulator.event_sender.send(Ev::ScanCode(code)).unwrap();
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn Java_nl_liblisa_sem86_RustInterop_mouseButtonState(
    _env: jni::JNIEnv, _class: jni::objects::JClass, emulator_index: u64, left: bool, right: bool,
) {
    let mut emulators = EMULATORS.lock().unwrap();
    let emulator = &mut emulators[emulator_index as usize];

    let ev = emulator.mouse_state.event_from_delta(
        MouseState {
            left_pressed: left,
            right_pressed: right,
        },
        PhysicalPosition::new(0.0, 0.0),
    );

    info!("Sending mouse click: {ev:?}");
    emulator.event_sender.send(Ev::MouseMove(ev)).unwrap();
}

#[unsafe(no_mangle)]
pub extern "C" fn Java_nl_liblisa_sem86_RustInterop_connectSurface(
    env: jni::JNIEnv, _class: jni::objects::JClass, emulator_index: u64, surface: jni::objects::JObject,
) {
    info!("Creating instance...");

    let instance = Instance::new(&wgpu::InstanceDescriptor {
        backends: wgpu::Backends::PRIMARY,
        ..Default::default()
    });

    let surface = create_surface(&env, surface, &instance);
    let size: wgpu::Extent3d = wgpu::Extent3d {
        width: 1024, // or dynamically query native_window.width()
        height: 768, // or dynamically query native_window.height()
        depth_or_array_layers: 1,
    };

    let mut emulators = EMULATORS.lock().unwrap();
    let emulator = &mut emulators[emulator_index as usize];
    let size = PhysicalSize::new(size.width, size.height);
    let mut renderer = pollster::block_on(VideoRenderer::new(
        surface,
        instance,
        size,
        emulator.receiver.take().unwrap(),
        emulator.video_memory.clone(),
    ));

    renderer.resize(PhysicalSize::new(1024, 768));
    emulator.renderer = Some(renderer);
}

#[unsafe(no_mangle)]
pub extern "C" fn Java_nl_liblisa_sem86_RustInterop_render(_env: jni::JNIEnv, _class: jni::objects::JClass, emulator_index: u64) {
    let mut emulators = EMULATORS.lock().unwrap();
    let renderer = emulators[emulator_index as usize].renderer.as_mut().unwrap();

    renderer.update();
    renderer.render(|| ()).unwrap();
}

#[unsafe(no_mangle)]
pub extern "C" fn Java_nl_liblisa_sem86_RustInterop_stopEmulation(
    _env: jni::JNIEnv, _class: jni::objects::JClass, emulator_index: u64, fd: i32,
) {
    let mut emulators = EMULATORS.lock().unwrap();
    let emulator = &mut emulators[emulator_index as usize];
    if fd != -1 {
        *emulator.snapshot_writer.lock().unwrap() = Some(Box::new(unsafe { File::from_raw_fd(fd) }) as _);
    }

    emulator.event_sender.send(Ev::Stop).unwrap();
    emulator.emulator_thread.take().unwrap().join().unwrap();
}

#[unsafe(no_mangle)]
pub extern "C" fn Java_nl_liblisa_sem86_RustInterop_startEmulation(
    _env: jni::JNIEnv, _class: jni::objects::JClass, ide_fd_0_0: i32, ide_fd_1_0: i32, ide_1_0_is_cd: bool, memory_size_mb: i32,
    resume_from_fd: i32,
) -> u64 {
    let assets = ASSET_READER.get().unwrap();

    let (event_sender, recv) = channel();
    let memory = memory_size_mb;
    let disks = Vec::new();
    let physical_memory = Arc::new(Shm::new("physical_memory", (memory as usize) << 20));
    let memory = Arc::new(Mem32::new(physical_memory.clone()));
    let (cga_mode_sender, cga_mode_recv) = channel();
    let vgabios_data = assets.read_to_vec("bios/VGABIOS-lgpl-git");
    let vgabios = Arc::new(Shm::new("vgabios", (vgabios_data.len() + 0xfff) & !0xfff));
    let (video_memory_sender, video_memory_recv) = channel();
    let snapshot_writer = Arc::new(Mutex::new(None));

    let ide_fd_0_0 = if ide_fd_0_0 != -1 {
        Some(unsafe { OwnedFd::from_raw_fd(ide_fd_0_0) })
    } else {
        None
    };
    let ide_fd_1_0 = if ide_fd_1_0 != -1 {
        Some(unsafe { OwnedFd::from_raw_fd(ide_fd_1_0) })
    } else {
        None
    };

    let emulator_thread = {
        let snapshot_writer = snapshot_writer.clone();
        std::thread::Builder::new()
            .name("emulator-thread".into())
            // Require a big stack size for all stack-allocated structures
            .stack_size(128 << 20)
            .spawn(move || {
                log::info!("Reading BIOS...");
                let bochs_bios = assets.read_to_vec("bios/BIOS-bochs-latest");

                let semantics_data = assets.read_to_vec("x86.semantics");

                log::info!("BIOS is {} bytes, VGABIOS is {} bytes", bochs_bios.len(), vgabios_data.len());

                let f = Cursor::new(semantics_data);
                let f = FrameDecoder::new(f);

                log::info!("Loading semantics...");
                let start = Instant::now();
                let instr_semantics: PackedInstrSem = pot::from_reader(f).unwrap();
                let instr_semantics = Arc::new(instr_semantics);

                info!("Loading semantics took {}ms", start.elapsed().as_millis());

                info!("Creating physical memory...");
                let v = physical_memory.view();

                // Mimic Bochs' behavior.
                for addr in 0xc0000..0xe0000 {
                    v.write_byte(addr, 0xff);
                }

                vgabios.view().write_slice(0, &vgabios_data);

                info!("Loaded VGABIOS of {} bytes", vgabios_data.len());

                info!("Writing ROMs...");
                {
                    let v = physical_memory.view();

                    let data = bochs_bios;
                    let addr = 0x100000 - data.len() as u64;

                    log::info!("Placing BIOS at 0x{addr:X}");
                    v.write_slice(addr as u32, &data);
                }

                let entry = 0xffff0000u32;
                let cs = (entry >> 16) as u16;
                let ip = entry as u16;
                let mut state = State::default();
                state.set_gpreg(GpReg::Ip, ip as u64);
                state.set_gpreg(GpReg::Cs, cs as u64);
                state.set_gpreg(GpReg::CsBase, (cs as u64) * 16);

                generativity::make_guard!(guard);
                info!("Setting up emulator..");
                let mut emu = EmulatorContext::new(
                    &memory,
                    instr_semantics,
                    state,
                    |intr| {
                        let mut hw = Hw::new(
                            memory.clone(),
                            disks,
                            cga_mode_sender,
                            recv,
                            vgabios.clone(),
                            intr,
                            EmulatorClock::new_asynchronous(),
                        );
                        video_memory_sender.send(hw.video_memory()).unwrap();

                        info!("Loading disks...");

                        if let Some(fd) = ide_fd_0_0 {
                            info!("Loading IDE0:0");
                            let f = File::from(fd);
                            hw.set_disk(0, 0, Some(DiskData::from_file(f)));
                        }

                        if let Some(fd) = ide_fd_1_0 {
                            info!("Loading IDE1:0");
                            let f = File::from(fd);
                            hw.set_disk(1, 0, Some(DiskData::from_file(f).with_is_cd(ide_1_0_is_cd)));
                        }

                        hw
                    },
                    guard,
                );

                if resume_from_fd != -1 {
                    info!("Decompressing snapshot...");
                    let start = Instant::now();
                    let f = unsafe { File::from_raw_fd(resume_from_fd) };
                    let mut f = lz4_flex::frame::FrameDecoder::new(f);
                    let mut data = Vec::new();
                    f.read_to_end(&mut data).unwrap();

                    info!("Deserializing snapshot...");
                    let snapshot = pot::from_slice(&data).unwrap();
                    drop(data);

                    info!("Restoring snapshot...");
                    emu.restore(snapshot);

                    info!("Snapshot restored in {:.2}s", start.elapsed().as_secs_f32());
                }

                info!("Running...");
                emu.run(None);

                if let Some(writer) = snapshot_writer.lock().unwrap().take() {
                    let snapshot = emu.snapshot();

                    let data = pot::to_vec(&snapshot).unwrap();

                    info!("Compressing and writing snapshot to disk...");
                    let start = Instant::now();
                    let mut f = lz4_flex::frame::FrameEncoder::new(writer).auto_finish();
                    f.write_all(&data).unwrap();

                    info!("Compression took {:.2}s", start.elapsed().as_secs_f64());
                }
            })
            .unwrap()
    };
    let mut emulators = EMULATORS.lock().unwrap();
    let index = emulators.len() as u64;
    emulators.push(RunningEmulator {
        emulator_thread: Some(emulator_thread),
        video_memory: video_memory_recv.recv().unwrap(),
        event_sender,
        mouse_state: MouseState::default(),
        receiver: Some(ModeSetReceiver::new(cga_mode_recv)),
        renderer: None,
        snapshot_writer,
    });

    index
}
