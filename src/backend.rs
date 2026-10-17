//! Small platform boundary shared by the terminal model and both UI backends.
//! Linux keeps the existing egui/OpenGL path; macOS and Windows use GPUI.
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(target_os = "linux")]
pub type Color = eframe::egui::Color32;
#[cfg(not(target_os = "linux"))]
pub type Color = gpui::Rgba;

#[cfg(target_os = "linux")]
pub type PointerButton = eframe::egui::PointerButton;
#[cfg(not(target_os = "linux"))]
pub type PointerButton = u8;

pub fn rgb(value: u32) -> Color {
    #[cfg(target_os = "linux")]
    {
        Color::from_rgb((value >> 16) as u8, (value >> 8) as u8, value as u8)
    }
    #[cfg(not(target_os = "linux"))]
    {
        gpui::rgb(value)
    }
}

pub struct Repaint {
    dirty: AtomicBool,
    #[cfg(target_os = "linux")]
    context: std::sync::OnceLock<eframe::egui::Context>,
}
impl Default for Repaint {
    fn default() -> Self {
        Self::new()
    }
}
impl Repaint {
    pub fn new() -> Self {
        Self {
            dirty: AtomicBool::new(true),
            #[cfg(target_os = "linux")]
            context: std::sync::OnceLock::new(),
        }
    }
    pub fn store(&self, dirty: bool, ordering: Ordering) {
        self.dirty.store(dirty, ordering);
        #[cfg(target_os = "linux")]
        if dirty && let Some(context) = self.context.get() {
            context.request_repaint();
        }
    }
    pub fn swap(&self, dirty: bool, ordering: Ordering) -> bool {
        self.dirty.swap(dirty, ordering)
    }
    #[cfg(target_os = "linux")]
    pub fn get_or_init(
        &self,
        create: impl FnOnce() -> eframe::egui::Context,
    ) -> &eframe::egui::Context {
        let context = self.context.get_or_init(create);
        if self.dirty.swap(false, Ordering::AcqRel) {
            context.request_repaint();
        }
        context
    }
}
