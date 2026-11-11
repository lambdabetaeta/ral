use crate::wire::SignInStep;
use exarch::provider::oauth;

impl From<oauth::LoginPhase> for SignInStep {
    fn from(phase: oauth::LoginPhase) -> Self {
        match phase {
            oauth::LoginPhase::AwaitingBrowser { url } => Self {
                say: "Finish signing in, in your browser.  If no window opened, \
                      follow this link:"
                    .to_string(),
                link: Some(url),
            },
            oauth::LoginPhase::AwaitingDevice {
                user_code,
                url,
                expires_in,
            } => Self {
                say: format!(
                    "Open the link and enter this code: {user_code} (it expires in {expires_in})."
                ),
                link: Some(url),
            },
            oauth::LoginPhase::ExchangingCode => Self {
                say: "Signing you in…".to_string(),
                link: None,
            },
        }
    }
}
