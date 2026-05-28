use super::types::KiroUsageInfo;

pub struct KiroClient;

impl KiroClient {
    /// # Errors
    ///
    /// Returns an error if the kiro-cli subprocess fails or its output cannot be parsed.
    pub async fn fetch_usage() -> Result<KiroUsageInfo, Box<dyn std::error::Error>> {
        let output = tokio::task::spawn_blocking(|| {
            let output = std::process::Command::new("kiro-cli")
                .args(["chat", "--no-interactive", "/usage"])
                .output()?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err::<String, std::io::Error>(std::io::Error::other(format!(
                    "kiro-cli exited with {}: {stderr}",
                    output.status
                )));
            }
            String::from_utf8(output.stdout)
                .map_err(|e| std::io::Error::other(format!("invalid UTF-8 in kiro output: {e}")))
        })
        .await??;

        KiroUsageInfo::parse(&output)
    }
}
