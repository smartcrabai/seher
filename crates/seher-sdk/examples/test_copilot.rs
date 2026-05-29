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
            if let Ok(cookies) = CookieReader::read_cookies(&prof, "github.com") {
                let has_user_session = cookies
                    .iter()
                    .any(|c| c.name == "user_session" || c.name == "__Host-user_session_same_site");

                if !has_user_session {
                    continue;
                }

                let dotcom_user = cookies
                    .iter()
                    .find(|c| c.name == "dotcom_user")
                    .map_or("unknown", |c| c.value.as_str());
                println!(
                    "Using {} - {} ({} cookies, user={})",
                    browser.name(),
                    prof.name,
                    cookies.len(),
                    dotcom_user
                );

                match seher::copilot::CopilotClient::fetch_quota(&cookies).await {
                    Ok(quota) => {
                        println!("\nSuccess! Copilot quota:");
                        println!("  chat_utilization: {:.1}%", quota.chat_utilization);
                        println!("  premium_utilization: {:.1}%", quota.premium_utilization);
                        println!("  reset_time: {:?}", quota.reset_time);
                        println!("  is_limited: {}", quota.is_limited());
                        return;
                    }
                    Err(e) => {
                        println!("\nFailed to fetch Copilot quota: {e}");
                    }
                }
            }
        }
    }

    println!("No github.com session with user_session cookie found");
}
