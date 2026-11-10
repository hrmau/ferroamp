//! Puts a canvas on screen by handing the small art-resolution image straight
//! to Core Animation, which scales it on the GPU with nearest-neighbour
//! filtering. The CPU never touches the many times larger scaled-up image.

use std::ffi::c_void;
use std::ptr::NonNull;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, NSObject};
use objc2::msg_send;
use objc2_core_foundation::{CFRetained, CGPoint, CGRect};
use objc2_core_graphics::{
    CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGDataProvider, CGImage, CGImageAlphaInfo,
    CGImageByteOrderInfo, CGImageComponentInfo, CGImagePixelFormatInfo, kCGColorSpaceSRGB,
};
use objc2_quartz_core::{
    CALayer, CATransaction, kCAFilterLinear, kCAFilterNearest, kCAGravityResize,
};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::Window;

use super::gfx::Canvas;

pub struct Presenter {
    root: Retained<CALayer>,
    layer: Retained<CALayer>,
    color_space: CFRetained<CGColorSpace>,
    bounds: CGRect,
}

unsafe extern "C-unwind" fn free_pixels(_info: *mut c_void, data: NonNull<c_void>, size: usize) {
    let pixels = std::ptr::slice_from_raw_parts_mut(data.cast::<u32>().as_ptr(), size / 4);
    // SAFETY: this is the exact pointer and length from `Box::into_raw` below.
    drop(unsafe { Box::from_raw(pixels) });
}

impl Presenter {
    /// `smooth` blends pixels when scaling, for visuals. Otherwise edges stay
    /// crisp, for pixel art.
    pub fn new(window: &Window, smooth: bool) -> Option<Self> {
        let RawWindowHandle::AppKit(handle) = window.window_handle().ok()?.as_raw() else {
            return None;
        };
        // SAFETY: winit's handle holds a valid NSView for the window's lifetime
        // and these are standard NSView messages on the main thread.
        let root: Retained<CALayer> = unsafe {
            let view: &NSObject = handle.ns_view.cast().as_ref();
            let _: () = msg_send![view, setWantsLayer: Bool::YES];
            let layer: Option<Retained<CALayer>> = msg_send![view, layer];
            layer?
        };
        let layer = CALayer::new();
        root.addSublayer(&layer);
        layer.setAnchorPoint(CGPoint::new(0.0, 0.0));
        layer.setGeometryFlipped(true);
        layer.setOpaque(true);
        // SAFETY: framework-provided constants.
        unsafe {
            layer.setContentsGravity(kCAGravityResize);
            layer.setMagnificationFilter(if smooth {
                kCAFilterLinear
            } else {
                kCAFilterNearest
            });
        }
        // SAFETY: framework-provided constant, valid colour space name.
        let color_space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }))
            .or_else(CGColorSpace::new_device_rgb)?;
        Some(Self {
            root,
            layer,
            color_space,
            bounds: CGRect::default(),
        })
    }

    pub fn present(&mut self, canvas: &Canvas) {
        let pixels: Box<[u32]> = canvas.px.clone().into_boxed_slice();
        let len = pixels.len() * 4;
        let data = Box::into_raw(pixels).cast::<c_void>();
        // SAFETY: `data` is valid for `len` bytes until `free_pixels` runs.
        let Some(provider) = (unsafe {
            CGDataProvider::with_data(std::ptr::null_mut(), data, len, Some(free_pixels))
        }) else {
            return;
        };
        let info = CGBitmapInfo(
            CGImageAlphaInfo::NoneSkipFirst.0
                | CGImageComponentInfo::Integer.0
                | CGImageByteOrderInfo::Order32Little.0
                | CGImagePixelFormatInfo::Packed.0,
        );
        // SAFETY: width, height and stride describe the provider's buffer.
        let image = unsafe {
            CGImage::new(
                canvas.w,
                canvas.h,
                8,
                32,
                canvas.w * 4,
                Some(&self.color_space),
                info,
                Some(&provider),
                std::ptr::null(),
                false,
                CGColorRenderingIntent::RenderingIntentDefault,
            )
        };
        let Some(image) = image else {
            return;
        };

        CATransaction::begin();
        CATransaction::setDisableActions(true);
        let bounds = self.root.bounds();
        if bounds != self.bounds {
            self.bounds = bounds;
            self.layer.setFrame(bounds);
        }
        let contents: &AnyObject = image.as_ref();
        // SAFETY: a CGImage is a valid value for a layer's contents.
        unsafe { self.layer.setContents(Some(contents)) };
        CATransaction::commit();
    }
}
