// SPDX-License-Identifier: GPL-2.0-or-later

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use oauth2::basic::BasicClient;
use oauth2::{
    AuthUrl, ClientId, DeviceAuthorizationUrl, RefreshToken, Scope,
    StandardDeviceAuthorizationResponse, TokenResponse, TokenUrl,
};
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::config::{AccountConfig, AuthProfile, MICROSOFT_GRAPH_RESOURCE};
use crate::secrets::{AccountSecrets, SecretStore};

#[derive(Clone)]
pub struct TokenManager {
    account: AccountConfig,
    store: SecretStore,
    secrets: Arc<Mutex<AccountSecrets>>,
    http: reqwest::Client,
}

impl TokenManager {
    pub fn from_store(account: &AccountConfig, store: &SecretStore) -> Result<Self> {
        let secrets = store.load(&account.name)?;
        Ok(Self::new(account.clone(), store.clone(), secrets))
    }

    /// Persist the current secrets without blocking the async runtime; the
    /// keyring backend performs synchronous D-Bus I/O.
    async fn persist(&self, secrets: &AccountSecrets) -> Result<()> {
        let store = self.store.clone();
        let name = self.account.name.clone();
        let secrets = secrets.clone();
        tokio::task::spawn_blocking(move || store.save(&name, &secrets))
            .await
            .context("secret store task failed")?
    }

    pub fn new(account: AccountConfig, store: SecretStore, secrets: AccountSecrets) -> Self {
        Self {
            account,
            store,
            secrets: Arc::new(Mutex::new(secrets)),
            http: secure_http_client(),
        }
    }

    pub async fn access_token(&self) -> Result<String> {
        let mut secrets = self.secrets.lock().await;
        let usable = secrets
            .access_token_expires_at
            .is_some_and(|expires| expires > Utc::now().timestamp() + 90);
        if usable && let Some(token) = &secrets.access_token {
            return Ok(token.clone());
        }

        match self.account.auth_profile {
            AuthProfile::Goa => {
                let account = self.account.goa_account.as_deref().unwrap_or_default();
                let (token, expires_in) = goa_access_token(account).await?;
                secrets.access_token = Some(token.clone());
                secrets.access_token_expires_at = Some(Utc::now().timestamp() + expires_in);
                // Nothing worth persisting: GOA holds the refresh token.
                return Ok(token);
            }
            AuthProfile::MicrosoftOffice => {
                let response =
                    office_refresh_token(&self.account, &self.http, secrets.refresh_token.as_str())
                        .await
                        .context(
                            "Microsoft rejected the refresh token; run `graphmail-bridge login`",
                        )?;
                secrets.access_token = Some(response.access_token);
                secrets.access_token_expires_at =
                    Some(Utc::now().timestamp() + response.expires_in.unwrap_or(3600) as i64);
                if let Some(refresh_token) = response.refresh_token {
                    secrets.refresh_token = refresh_token;
                }
            }
            AuthProfile::CustomEntra => {
                let client = oauth_client(&self.account)?;
                let refresh_token = RefreshToken::new(secrets.refresh_token.clone());
                let response = client
                    .exchange_refresh_token(&refresh_token)
                    .request_async(&self.http)
                    .await
                    .context(
                        "Microsoft rejected the refresh token; run `graphmail-bridge login`",
                    )?;
                secrets.access_token = Some(response.access_token().secret().to_owned());
                secrets.access_token_expires_at = Some(
                    Utc::now().timestamp()
                        + response
                            .expires_in()
                            .map(|duration| i64::try_from(duration.as_secs()).unwrap_or(3600))
                            .unwrap_or(3600),
                );
                if let Some(refresh) = response.refresh_token() {
                    secrets.refresh_token = refresh.secret().to_owned();
                }
            }
        }
        self.persist(&secrets).await?;
        Ok(secrets.access_token.clone().expect("access token just set"))
    }

    /// Force a refresh-token exchange even when the cached access token is
    /// still usable. This is intended for explicit diagnostics.
    pub async fn refresh_access_token(&self) -> Result<String> {
        {
            let mut secrets = self.secrets.lock().await;
            secrets.access_token_expires_at = None;
        }
        self.access_token().await
    }

    pub async fn bridge_password_matches(&self, candidate: &str) -> bool {
        use subtle::ConstantTimeEq;
        let secrets = self.secrets.lock().await;
        secrets
            .bridge_password
            .as_bytes()
            .ct_eq(candidate.as_bytes())
            .into()
    }
}

/// What a device-code login asks the user to do: open the page and type
/// the code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceCode {
    pub verification_url: String,
    pub user_code: String,
}

/// Device-code login for a terminal: the code is printed, and the page
/// opened in the browser.
pub async fn device_login(
    account: &AccountConfig,
    store: &SecretStore,
    bridge_password: String,
) -> Result<AccountSecrets> {
    device_login_with(account, store, bridge_password, &|code: &DeviceCode| {
        println!(
            "\nOpen {} and enter code {}.\n",
            code.verification_url, code.user_code
        );
        let _ = open::that(&code.verification_url);
    })
    .await
}

/// Device-code login that hands the code to `on_code`, for a program that
/// shows it its own way. A GOA account has no login of its own: its tokens
/// come from GNOME Online Accounts.
pub async fn device_login_with(
    account: &AccountConfig,
    store: &SecretStore,
    bridge_password: String,
    on_code: &(dyn Fn(&DeviceCode) + Send + Sync),
) -> Result<AccountSecrets> {
    let secrets = match account.auth_profile {
        AuthProfile::MicrosoftOffice => {
            office_device_login(account, bridge_password, on_code).await?
        }
        AuthProfile::CustomEntra => custom_device_login(account, bridge_password, on_code).await?,
        AuthProfile::Goa => AccountSecrets {
            bridge_password,
            refresh_token: String::new(),
            access_token: None,
            access_token_expires_at: None,
        },
    };
    store.save(&account.name, &secrets)?;
    Ok(secrets)
}

/// A GOA account's current access token and its lifetime in seconds,
/// through GOA's D-Bus API (GOA refreshes it itself).
async fn goa_access_token(goa_account: &str) -> Result<(String, i64)> {
    if !goa_account
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        bail!("invalid GNOME Online Accounts id {goa_account:?}");
    }
    let output = tokio::process::Command::new("gdbus")
        .args([
            "call",
            "--session",
            "--dest",
            "org.gnome.OnlineAccounts",
            "--object-path",
            &format!("/org/gnome/OnlineAccounts/Accounts/{goa_account}"),
            "--method",
            "org.gnome.OnlineAccounts.OAuth2Based.GetAccessToken",
        ])
        .output()
        .await
        .context("could not run gdbus to ask GNOME Online Accounts for a token")?;
    if !output.status.success() {
        bail!(
            "GNOME Online Accounts gave no token: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    parse_goa_token(&String::from_utf8_lossy(&output.stdout))
        .context("could not read the token GNOME Online Accounts returned")
}

/// `('eyJ0…', 3599)`, the shape gdbus prints the (si) reply in.
fn parse_goa_token(reply: &str) -> Option<(String, i64)> {
    let reply = reply.trim().strip_prefix("(")?.strip_suffix(")")?;
    let (token, expires) = reply.rsplit_once(',')?;
    let token = token.trim().strip_prefix('\'')?.strip_suffix('\'')?;
    let expires = expires.trim().parse().ok()?;
    (!token.is_empty()).then(|| (token.to_owned(), expires))
}

async fn custom_device_login(
    account: &AccountConfig,
    bridge_password: String,
    on_code: &(dyn Fn(&DeviceCode) + Send + Sync),
) -> Result<AccountSecrets> {
    let client = oauth_client(account)?;
    let mut request = client.exchange_device_code();
    for scope in &account.scopes {
        request = request.add_scope(Scope::new(scope.clone()));
    }
    let details: StandardDeviceAuthorizationResponse = request
        .request_async(&secure_http_client())
        .await
        .context("could not start Microsoft device authorization")?;

    on_code(&DeviceCode {
        verification_url: details
            .verification_uri_complete()
            .map(|uri| uri.secret().to_owned())
            .unwrap_or_else(|| details.verification_uri().to_string()),
        user_code: details.user_code().secret().to_owned(),
    });

    let response = client
        .exchange_device_access_token(&details)
        .request_async(&secure_http_client(), tokio::time::sleep, None)
        .await
        .context("Microsoft device authorization failed")?;
    let Some(refresh_token) = response.refresh_token() else {
        bail!("Microsoft did not return a refresh token; verify offline_access is allowed");
    };
    Ok(AccountSecrets {
        bridge_password,
        refresh_token: refresh_token.secret().to_owned(),
        access_token: Some(response.access_token().secret().to_owned()),
        access_token_expires_at: Some(
            Utc::now().timestamp()
                + response
                    .expires_in()
                    .map(|duration| i64::try_from(duration.as_secs()).unwrap_or(3600))
                    .unwrap_or(3600),
        ),
    })
}

#[derive(Debug, Deserialize)]
struct OfficeDeviceAuthorization {
    device_code: String,
    user_code: String,
    #[serde(alias = "verification_uri")]
    verification_url: String,
    #[serde(deserialize_with = "deserialize_u64")]
    expires_in: u64,
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    interval: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct OfficeTokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    expires_in: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct OfficeOAuthError {
    error: String,
    error_description: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum NumberOrString {
    Number(u64),
    String(String),
}

impl NumberOrString {
    fn into_u64<E: serde::de::Error>(self) -> std::result::Result<u64, E> {
        match self {
            Self::Number(value) => Ok(value),
            Self::String(value) => value.parse().map_err(E::custom),
        }
    }
}

fn deserialize_u64<'de, D>(deserializer: D) -> std::result::Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    NumberOrString::deserialize(deserializer)?.into_u64()
}

fn deserialize_optional_u64<'de, D>(deserializer: D) -> std::result::Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<NumberOrString>::deserialize(deserializer)?
        .map(NumberOrString::into_u64)
        .transpose()
}

async fn office_device_login(
    account: &AccountConfig,
    bridge_password: String,
    on_code: &(dyn Fn(&DeviceCode) + Send + Sync),
) -> Result<AccountSecrets> {
    let http = secure_http_client();
    let authority = oauth_authority(account);
    let response = http
        .post(format!("{authority}/devicecode"))
        .form(&[
            ("client_id", account.client_id.as_str()),
            ("resource", MICROSOFT_GRAPH_RESOURCE),
        ])
        .send()
        .await
        .context("could not start Microsoft Office device authorization")?;
    let response = response
        .error_for_status()
        .context("Microsoft rejected the Office device authorization request")?;
    let details: OfficeDeviceAuthorization = response
        .json()
        .await
        .context("could not parse Microsoft Office device authorization")?;

    on_code(&DeviceCode {
        verification_url: details.verification_url.clone(),
        user_code: details.user_code.clone(),
    });

    let deadline = Instant::now() + Duration::from_secs(details.expires_in);
    let mut interval = Duration::from_secs(details.interval.unwrap_or(5).max(1));
    let token_url = format!("{authority}/token");
    loop {
        if Instant::now() >= deadline {
            bail!("Microsoft Office device authorization expired; run login again");
        }
        tokio::time::sleep(interval).await;
        let response = http
            .post(&token_url)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", account.client_id.as_str()),
                ("code", details.device_code.as_str()),
                ("resource", MICROSOFT_GRAPH_RESOURCE),
            ])
            .send()
            .await
            .context("Microsoft Office device-token request failed")?;
        if response.status().is_success() {
            let token: OfficeTokenResponse = response
                .json()
                .await
                .context("could not parse Microsoft Office token response")?;
            let Some(refresh_token) = token.refresh_token else {
                bail!("Microsoft did not return a refresh token");
            };
            return Ok(AccountSecrets {
                bridge_password,
                refresh_token,
                access_token: Some(token.access_token),
                access_token_expires_at: Some(
                    Utc::now().timestamp() + token.expires_in.unwrap_or(3600) as i64,
                ),
            });
        }

        let error: OfficeOAuthError = response
            .json()
            .await
            .context("could not parse Microsoft Office authorization error")?;
        match error.error.as_str() {
            "authorization_pending" => {}
            "slow_down" => interval += Duration::from_secs(5),
            "authorization_declined" | "access_denied" => {
                bail!("Microsoft Office authorization was declined")
            }
            "expired_token" | "code_expired" => {
                bail!("Microsoft Office device authorization expired; run login again")
            }
            _ => bail!(
                "Microsoft Office authorization failed: {}: {}",
                error.error,
                error.error_description.as_deref().unwrap_or("no details")
            ),
        }
    }
}

async fn office_refresh_token(
    account: &AccountConfig,
    http: &reqwest::Client,
    refresh_token: &str,
) -> Result<OfficeTokenResponse> {
    let response = http
        .post(format!("{}/token", oauth_authority(account)))
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", account.client_id.as_str()),
            ("resource", MICROSOFT_GRAPH_RESOURCE),
        ])
        .send()
        .await
        .context("Microsoft Office token refresh request failed")?;
    if response.status().is_success() {
        return response
            .json()
            .await
            .context("could not parse Microsoft Office refresh response");
    }
    let error: OfficeOAuthError = response
        .json()
        .await
        .context("could not parse Microsoft Office refresh error")?;
    bail!(
        "Microsoft Office token refresh failed: {}: {}",
        error.error,
        error.error_description.as_deref().unwrap_or("no details")
    )
}

fn oauth_client(
    account: &AccountConfig,
) -> Result<
    BasicClient<
        oauth2::EndpointSet,
        oauth2::EndpointSet,
        oauth2::EndpointNotSet,
        oauth2::EndpointNotSet,
        oauth2::EndpointSet,
    >,
> {
    let authority = oauth_authority(account);
    Ok(BasicClient::new(ClientId::new(account.client_id.clone()))
        .set_auth_uri(AuthUrl::new(format!("{authority}/authorize"))?)
        .set_token_uri(TokenUrl::new(format!("{authority}/token"))?)
        .set_device_authorization_url(DeviceAuthorizationUrl::new(format!(
            "{authority}/devicecode"
        ))?))
}

fn oauth_authority(account: &AccountConfig) -> String {
    let version = match account.auth_profile {
        AuthProfile::MicrosoftOffice => "oauth2",
        AuthProfile::CustomEntra | AuthProfile::Goa => "oauth2/v2.0",
    };
    format!(
        "https://login.microsoftonline.com/{}/{version}",
        account.tenant
    )
}

fn secure_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(20))
        .timeout(std::time::Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("TLS HTTP client construction should succeed")
}

/// The delegated permissions (`scp` claim) an access token carries. Graph
/// tokens are JWTs; the claim is read without verifying the signature,
/// which only Graph itself needs to do.
pub fn token_scopes(access_token: &str) -> Vec<String> {
    use base64::Engine;
    let Some(payload) = access_token.split('.').nth(1) else {
        return Vec::new();
    };
    let Ok(bytes) =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload.trim_end_matches('='))
    else {
        return Vec::new();
    };
    #[derive(Deserialize)]
    struct Claims {
        #[serde(default)]
        scp: String,
    }
    serde_json::from_slice::<Claims>(&bytes)
        .map(|claims| claims.scp.split_whitespace().map(str::to_owned).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_goa_token_reply() {
        assert_eq!(
            parse_goa_token("('eyJ0.abc', 3599)\n"),
            Some(("eyJ0.abc".to_owned(), 3599))
        );
        assert_eq!(parse_goa_token("('', 10)"), None);
        assert_eq!(parse_goa_token("Error: no such account"), None);
    }
    use crate::config::{MICROSOFT_OFFICE_CLIENT_ID, default_scopes};

    fn account(auth_profile: AuthProfile) -> AccountConfig {
        AccountConfig {
            name: "work".into(),
            email: "me@example.com".into(),
            auth_profile,
            tenant: "common".into(),
            client_id: MICROSOFT_OFFICE_CLIENT_ID.into(),
            scopes: default_scopes(),
            goa_account: None,
        }
    }

    #[test]
    fn reads_scopes_from_the_token_payload() {
        use base64::Engine;
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"scp":"Mail.Read Calendars.ReadWrite"}"#);
        assert_eq!(
            token_scopes(&format!("header.{payload}.signature")),
            ["Mail.Read", "Calendars.ReadWrite"]
        );
        assert!(token_scopes("opaque").is_empty());
    }

    #[test]
    fn office_profile_uses_v1_authority() {
        assert_eq!(
            oauth_authority(&account(AuthProfile::MicrosoftOffice)),
            "https://login.microsoftonline.com/common/oauth2"
        );
    }

    #[test]
    fn custom_profile_uses_v2_authority() {
        assert_eq!(
            oauth_authority(&account(AuthProfile::CustomEntra)),
            "https://login.microsoftonline.com/common/oauth2/v2.0"
        );
    }

    #[test]
    fn parses_microsoft_v1_quoted_lifetimes() {
        let device: OfficeDeviceAuthorization = serde_json::from_str(
            r#"{
                "device_code": "device",
                "user_code": "ABCD-EFGH",
                "verification_url": "https://login.microsoft.com/device",
                "expires_in": "900",
                "interval": "5"
            }"#,
        )
        .unwrap();
        assert_eq!(device.expires_in, 900);
        assert_eq!(device.interval, Some(5));

        let token: OfficeTokenResponse = serde_json::from_str(
            r#"{
                "access_token": "access",
                "refresh_token": "refresh",
                "expires_in": "3599"
            }"#,
        )
        .unwrap();
        assert_eq!(token.expires_in, Some(3599));
    }
}
