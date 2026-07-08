// Platform-specific code. Each OS gets its own submodule enabled via cfg.
// M5: computer use per platform.

// Stub module — M0
// All submodules are currently empty. Implementations land in M5.

// Include the active platform's module via cfg.
#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "windows")]
pub mod windows;

#[cfg(target_os = "linux")]
pub mod linux;
