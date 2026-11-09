//! Process launcher: start the target application unmodified and
//! track its lifetime. The child inherits stdio so the user sees its
//! output; PixelFlow never writes to the child's stdin.

use std::process::{Child, Command, Stdio};

/// A launched target application.
pub struct Launcher {
    child: Child,
}

impl Launcher {
    /// Spawn the target with its arguments. The child is a plain
    /// process: no environment changes, no wrapping, no injection.
    ///
    /// # Errors
    ///
    /// Returns the spawn error when the program cannot start.
    pub fn spawn(program: &str, args: &[String]) -> Result<Launcher, String> {
        let child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .spawn()
            .map_err(|e| format!("failed to launch {program}: {e}"))?;
        Ok(Launcher { child })
    }

    /// True when the child has exited.
    #[must_use]
    pub fn exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }

    /// Kill the child if it is still running. A child that already
    /// exited is left alone; a kill error is reported.
    ///
    /// # Errors
    ///
    /// Returns the kill error when the child is running and cannot
    /// be killed.
    pub fn kill(&mut self) -> Result<(), String> {
        if self.exited() {
            return Ok(());
        }
        self.child
            .kill()
            .map_err(|e| format!("failed to stop child: {e}"))?;
        match self.child.wait() {
            Ok(_) => Ok(()),
            Err(e) => Err(format!("failed to wait for child: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_missing_program_is_an_error() {
        let result = Launcher::spawn("pixelflow-definitely-not-a-program", &[]);
        assert!(result.is_err(), "expected a spawn error");
    }

    #[test]
    fn true_exits_immediately() -> Result<(), String> {
        let mut launcher = Launcher::spawn("/bin/true", &[])?;
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(launcher.exited(), "/bin/true has exited");
        assert!(launcher.kill().is_ok(), "killing an exited child succeeds");
        Ok(())
    }
}
