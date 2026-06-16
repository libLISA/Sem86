use std::process::abort;

use raw_window_handle::{
    AndroidNdkWindowHandle, DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawWindowHandle, WindowHandle,
};
use wgpu::Instance;

#[cfg(target_os = "android")]
pub type AndroidApp = android_activity::AndroidApp;

#[cfg(not(target_os = "android"))]
pub type AndroidApp = u8;

pub struct AssetReader {
    #[cfg(target_os = "android")]
    manager: ndk::asset::AssetManager,
}

#[allow(unused)]
impl AssetReader {
    pub fn new(env: jni::JNIEnv<'_>, asset_manager: jni::objects::JObject<'_>) -> Self {
        Self {
            #[cfg(target_os = "android")]
            manager: {
                let mgr_ptr = unsafe { ndk_sys::AAssetManager_fromJava(env.get_native_interface(), asset_manager.into_raw()) };
                assert!(!mgr_ptr.is_null(), "AAssetManager_fromJava returned null");
                let mut am = unsafe { ndk::asset::AssetManager::from_ptr(std::ptr::NonNull::new_unchecked(mgr_ptr)) };
                am
            },
        }
    }

    pub fn read_to_vec(&self, asset: &str) -> Vec<u8> {
        #[cfg(target_os = "android")]
        {
            let Some(mut bochs_bios_asset) = self.manager.open(std::ffi::CString::new(asset).as_deref().unwrap()) else {
                log::error!("Missing asset: {asset}");
                panic!("Missing asset: {asset}");
            };

            bochs_bios_asset.buffer().expect("failed to read asset").to_vec()
        }

        #[cfg(not(target_os = "android"))]
        unimplemented!()
    }
}

#[allow(unused)]
pub fn create_surface<'gpu>(
    env: &jni::JNIEnv<'_>, surface: jni::objects::JObject<'_>, instance: &Instance,
) -> wgpu::Surface<'gpu> {
    #[cfg(target_os = "android")]
    {
        use ndk::asset::AssetManager;
        use ndk::native_window::NativeWindow;
        use ndk::surface_texture::SurfaceTexture;
        let native_window = unsafe { NativeWindow::from_surface(env.get_raw(), surface.into_raw()) }.unwrap();

        log::info!("Native Window: {native_window:?}");

        // SAFETY: We have a valid ANativeWindow
        let raw_handle = AndroidNdkWindowHandle::new(
            std::ptr::NonNull::new(unsafe { native_window.ptr().as_mut() } as *mut _ as *mut _).unwrap(),
        );
        let dummy_window = DummyWindow {
            handle: raw_handle,
        };

        let surface = unsafe { instance.create_surface(dummy_window) }.unwrap();
        surface
    }

    #[cfg(not(target_os = "android"))]
    unimplemented!()
}

#[allow(unused)]
struct DummyWindow {
    handle: AndroidNdkWindowHandle,
}

unsafe impl Send for DummyWindow {}
unsafe impl Sync for DummyWindow {}

impl HasWindowHandle for DummyWindow {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        Ok(unsafe { WindowHandle::borrow_raw(RawWindowHandle::AndroidNdk(self.handle)) })
    }
}

impl HasDisplayHandle for DummyWindow {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        Ok(DisplayHandle::android())
    }
}

// Called if a pure virtual function is called
#[unsafe(no_mangle)]
unsafe extern "C" fn __cxa_pure_virtual() -> ! {
    abort() // abort immediately
}

// Called by the C++ runtime when terminate() is invoked
#[unsafe(no_mangle)]
unsafe extern "C" fn __cxa_terminate() -> ! {
    abort() // abort immediately
}
