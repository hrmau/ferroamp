//! Text the bitmap font has no letters for, such as Cyrillic or Japanese. The
//! system draws it, at the same low resolution as everything else, with its
//! own choice of font for each script. Shaping and right-to-left order come
//! for free because whole runs are drawn at once.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ptr::NonNull;
use std::rc::Rc;

use objc2_core_foundation::{CFAttributedString, CFBoolean, CFDictionary, CFRetained, CFString, CFType};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGBitmapContextGetData, CGColorSpace, CGContext, CGImageAlphaInfo,
};
use objc2_core_text::{
    CTFont, CTFontUIFontType, CTLine, kCTFontAttributeName, kCTForegroundColorFromContextAttributeName,
};

/// Point size, chosen so that a full-height character fills a playlist row.
const SIZE: f64 = 9.0;
/// Rows of pixels above and below the baseline.
const ABOVE: usize = 9;
const BELOW: usize = 3;
/// Rows above the top of a bitmap-font letter, whose baseline is 7 rows down.
pub const OVERHANG: i32 = ABOVE as i32 - 7;
/// Thin strokes at this size come out faint, so coverage is pushed up a bit.
const CONTRAST: f32 = 0.7;
/// Enough for every title on screen many times over.
const CACHE_LIMIT: usize = 256;

/// A line of text as coverage values, one byte per pixel, top row first.
#[derive(Debug)]
pub struct Run {
    pub width: i32,
    pub height: i32,
    pub alpha: Vec<u8>,
}

thread_local! {
    static CACHE: RefCell<HashMap<String, Rc<Run>>> = RefCell::new(HashMap::new());
}

/// The rendered form of `text`, from the cache when possible.
pub fn run(text: &str) -> Rc<Run> {
    CACHE.with_borrow_mut(|cache| {
        if let Some(found) = cache.get(text) {
            return found.clone();
        }
        if cache.len() >= CACHE_LIMIT {
            cache.clear();
        }
        let made = Rc::new(render(text).unwrap_or_else(|| blank(text)));
        cache.insert(text.to_owned(), made.clone());
        made
    })
}

fn blank(text: &str) -> Run {
    let width = text.chars().count() as i32 * super::font::ADVANCE;
    Run {
        width,
        height: (ABOVE + BELOW) as i32,
        alpha: vec![0; width as usize * (ABOVE + BELOW)],
    }
}

fn line_for(text: &str) -> Option<CFRetained<CTLine>> {
    let string = CFString::from_str(text);
    // SAFETY: these are valid constants and every pointer passed is checked by
    // the types. The dictionary is copied by the attributed string.
    unsafe {
        let font = CTFont::new_ui_font_for_language(CTFontUIFontType::System, SIZE, None)?;
        let yes = CFBoolean::new(true);
        let font_value: &CFType = font.as_ref();
        let yes_value: &CFType = yes.as_ref();
        let attributes = CFDictionary::<CFString, CFType>::from_slices(
            &[kCTFontAttributeName, kCTForegroundColorFromContextAttributeName],
            &[font_value, yes_value],
        );
        let attributed = CFAttributedString::new(None, Some(&string), Some(attributes.as_ref()))?;
        Some(CTLine::with_attributed_string(&attributed))
    }
}

fn render(text: &str) -> Option<Run> {
    let line = line_for(text)?;
    // SAFETY: null is allowed for all three out-parameters.
    let advance = unsafe { line.typographic_bounds(std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) };
    let width = (advance.ceil() as usize).clamp(1, 4096);
    let height = ABOVE + BELOW;
    let gray = CGColorSpace::new_device_gray()?;
    // SAFETY: with a null data pointer the context allocates and owns the
    // buffer, one byte per pixel, and the gray space matches that layout.
    let context = unsafe {
        CGBitmapContextCreate(
            std::ptr::null_mut(),
            width,
            height,
            8,
            width,
            Some(&gray),
            CGImageAlphaInfo::None.0,
        )
    }?;
    CGContext::set_gray_fill_color(Some(&context), 1.0, 1.0);
    CGContext::set_should_antialias(Some(&context), true);
    CGContext::set_should_smooth_fonts(Some(&context), false);
    CGContext::set_should_subpixel_position_fonts(Some(&context), false);
    CGContext::set_should_subpixel_quantize_fonts(Some(&context), false);
    CGContext::set_text_position(Some(&context), 0.0, BELOW as f64);
    // SAFETY: the context and line are both alive for the call.
    unsafe { line.draw(&context) };
    let data = NonNull::new(CGBitmapContextGetData(Some(&context)).cast::<u8>())?;
    // SAFETY: the context's buffer is `width * height` bytes and stays alive
    // until `context` drops, after this copy.
    let pixels = unsafe { std::slice::from_raw_parts(data.as_ptr(), width * height) };
    let alpha = pixels
        .iter()
        .map(|&a| ((a as f32 / 255.0).powf(CONTRAST) * 255.0) as u8)
        .collect();
    Some(Run {
        width: width as i32,
        height: height as i32,
        alpha,
    })
}
