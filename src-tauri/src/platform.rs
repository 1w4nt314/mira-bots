//! Per-platform app setup in one place (plan7 A.1).

/// `std::env::consts::OS`: "windows" | "macos" | "linux".
pub fn name() -> &'static str {
    std::env::consts::OS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_is_the_compile_target_os() {
        #[cfg(windows)]
        assert_eq!(name(), "windows");
        #[cfg(target_os = "macos")]
        assert_eq!(name(), "macos");
        #[cfg(target_os = "linux")]
        assert_eq!(name(), "linux");
    }
}
