use std::{
    ffi::{CStr, CString, c_char},
    ptr,
};

use fontconfig_sys as fc;

pub(crate) use gpui_wgpu::CosmicTextSystem;

/// The family `.SystemUIFont` names when fontconfig resolves no font at all.
const FALLBACK_UI_FONT_FAMILY: &str = "IBM Plex Sans";

/// Resolves the desktop's user interface font family, which `.SystemUIFont` names.
///
/// Fontconfig answers with the configured `system-ui` family, or with the `sans-serif`
/// family on configurations that predate the `system-ui` alias.
pub(crate) fn system_ui_font_family() -> String {
    ["system-ui", "sans-serif"]
        .into_iter()
        .find_map(fontconfig_family_match)
        .unwrap_or_else(|| FALLBACK_UI_FONT_FAMILY.to_string())
}

/// Returns the family of the font that fontconfig matches for `family`, applying the
/// configuration's substitutions as a font request from an application would.
fn fontconfig_family_match(family: &str) -> Option<String> {
    let family = CString::new(family).ok()?;
    // SAFETY: Every pattern is created and destroyed here, and the family string that
    // fontconfig returns is copied before the pattern that owns it is destroyed. A null
    // configuration selects fontconfig's current configuration, initializing it once.
    unsafe {
        let pattern = Pattern::new()?;
        if fc::FcPatternAddString(
            pattern.0,
            fc::constants::FC_FAMILY.as_ptr(),
            family.as_ptr().cast(),
        ) == 0
        {
            return None;
        }
        if fc::FcConfigSubstitute(ptr::null_mut(), pattern.0, fc::FcMatchPattern) == 0 {
            return None;
        }
        fc::FcDefaultSubstitute(pattern.0);

        let mut result = fc::FcResultNoMatch;
        let font = Pattern(fc::FcFontMatch(ptr::null_mut(), pattern.0, &mut result));
        if font.0.is_null() || result != fc::FcResultMatch {
            return None;
        }

        let mut matched_family = ptr::null_mut();
        if fc::FcPatternGetString(
            font.0,
            fc::constants::FC_FAMILY.as_ptr(),
            0,
            &mut matched_family,
        ) != fc::FcResultMatch
            || matched_family.is_null()
        {
            return None;
        }
        let matched_family = CStr::from_ptr(matched_family.cast::<c_char>())
            .to_str()
            .ok()?;
        (!matched_family.is_empty()).then(|| matched_family.to_owned())
    }
}

/// An owned fontconfig pattern.
struct Pattern(*mut fc::FcPattern);

impl Pattern {
    fn new() -> Option<Self> {
        // SAFETY: FcPatternCreate has no preconditions and returns null on failure.
        let pattern = unsafe { fc::FcPatternCreate() };
        (!pattern.is_null()).then_some(Self(pattern))
    }
}

impl Drop for Pattern {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: The pattern is owned by this value and destroyed once.
            unsafe { fc::FcPatternDestroy(self.0) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_a_named_ui_font_family() {
        let family = system_ui_font_family();

        assert!(!family.is_empty());
        assert_ne!(family, "system-ui");
        assert_ne!(family, "sans-serif");
    }
}
