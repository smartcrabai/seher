use std::io::Write;

pub struct Logger {
    pub quiet: bool,
}

impl Logger {
    #[must_use]
    pub fn new(quiet: bool) -> Self {
        Self { quiet }
    }

    pub fn info(&self, msg: &str) {
        if !self.quiet {
            let stderr = std::io::stderr();
            let mut e = stderr.lock();
            let _ = writeln!(e, "{msg}");
        }
    }

    pub fn warn(&self, msg: &str) {
        let stderr = std::io::stderr();
        let mut e = stderr.lock();
        let _ = writeln!(e, "{msg}");
    }
}
