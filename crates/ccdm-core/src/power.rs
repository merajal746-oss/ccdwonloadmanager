//! Host power control (cf. XDM shutdown-on-completion).
//!
//! Only ever invoked on explicit user request (`--shutdown` / GUI toggle).

/// Command (program + args) that powers the machine off.
pub fn shutdown_command(delay_secs: u64) -> (String, Vec<String>) {
    if cfg!(windows) {
        (
            "shutdown".to_string(),
            vec![
                "/s".to_string(),
                "/t".to_string(),
                delay_secs.to_string(),
                "/c".to_string(),
                "ccdwonloadmanager: queue finished".to_string(),
            ],
        )
    } else if cfg!(target_os = "macos") {
        (
            "osascript".to_string(),
            vec![
                "-e".to_string(),
                "tell app \"System Events\" to shut down".to_string(),
            ],
        )
    } else {
        let minutes = delay_secs.div_ceil(60);
        (
            "shutdown".to_string(),
            vec!["-h".to_string(), format!("+{}", minutes.max(1))],
        )
    }
}

/// Power the machine off (best effort — missing privileges/tools error).
pub fn shutdown_host(delay_secs: u64) -> std::io::Result<()> {
    let (program, args) = shutdown_command(delay_secs);
    let status = std::process::Command::new(&program).args(&args).status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "{program} exited with {status}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_shape() {
        let (program, args) = shutdown_command(60);
        assert!(!program.is_empty());
        assert!(!args.is_empty());
    }
}
