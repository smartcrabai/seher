//! Live check of the authoritative OpenCode Go usage via the opencode.ai
//! web dashboard. Reads the `opencode.ai` session cookie from local browsers.
//!
//!   cargo run -p seher-sdk --example test_opencode_go

use seher::{BrowserDetector, CookieReader};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let detector = BrowserDetector::new();
    let browsers = detector.detect_browsers();

    for browser in &browsers {
        for prof in detector.list_profiles(*browser) {
            let Ok(cookies) = CookieReader::read_cookies(&prof, "opencode.ai") else {
                continue;
            };
            let has_auth = cookies
                .iter()
                .any(|c| c.name == "auth" || c.name == "__Host-auth");
            if !has_auth {
                continue;
            }
            println!(
                "Using {} - {} ({} opencode.ai cookies)",
                browser.name(),
                prof.name,
                cookies.len(),
            );
            match seher::opencode_go::fetch_remote_usage(&cookies).await {
                Ok(usage) => {
                    println!("\nSuccess! OpenCode Go usage (hosted dashboard):");
                    for w in &usage.windows {
                        println!(
                            "  {:<8} {:>6.1}%  reset_in_sec={:?}  limited={}",
                            w.name,
                            w.used_percent,
                            w.reset_in_sec,
                            w.is_limited()
                        );
                    }
                    println!("  => limited: {}", usage.is_limited());
                    return;
                }
                Err(e) => {
                    println!("\nFailed to fetch OpenCode Go usage: {e}");
                }
            }
        }
    }

    println!("No opencode.ai session cookie (auth / __Host-auth) found in any browser");
}
