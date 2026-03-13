use seher::{BrowserDetector, CookieReader};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let detector = BrowserDetector::new();
    let browsers = detector.detect_browsers();

    for browser in &browsers {
        if !browser.is_chromium_based() {
            continue;
        }
        for prof in detector.list_profiles(*browser) {
            if let Ok(cookies) = CookieReader::read_cookies(&prof, "chatgpt.com") {
                let has_session = cookies
                    .iter()
                    .any(|c| c.name.starts_with("__Secure-next-auth.session-token"));

                if !has_session {
                    continue;
                }

                println!(
                    "Using {} - {} ({} cookies)",
                    browser.name(),
                    prof.name,
                    cookies.len(),
                );

                match seher::codex::CodexClient::fetch_usage(&cookies).await {
                    Ok(usage) => {
                        println!("\nSuccess! Codex usage:");
                        println!("  plan_type: {}", usage.plan_type);
                        println!("  rate_limit limited: {}", usage.rate_limit.is_limited());
                        println!(
                            "  rate_limit reset_time: {:?}",
                            usage.rate_limit.next_reset_time()
                        );
                        println!(
                            "  code_review limited: {}",
                            usage.code_review_rate_limit.is_limited()
                        );
                        return;
                    }
                    Err(e) => {
                        println!("\nFailed to fetch Codex usage: {e}");
                    }
                }
            }
        }
    }

    println!("No chatgpt.com session with __Secure-next-auth.session-token found");
}
