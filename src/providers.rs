//! Provider registry: preset IMAP/SMTP endpoints for well-known mailbox
//! providers, plus a fully custom mode for any other IMAP server.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Transport security for one endpoint.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Security {
    /// Implicit TLS from the first byte (IMAPS 993 / SMTPS 465).
    #[default]
    Ssl,
    /// Plaintext connect, upgrade with STARTTLS.
    StartTls,
    /// No TLS. Use only with a trusted, isolated local transport.
    Plain,
}

impl fmt::Display for Security {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Ssl => "ssl",
            Self::StartTls => "starttls",
            Self::Plain => "plain",
        })
    }
}

impl std::str::FromStr for Security {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "ssl" | "tls" => Ok(Self::Ssl),
            "starttls" => Ok(Self::StartTls),
            "plain" | "none" => Ok(Self::Plain),
            other => Err(format!(
                "unsupported security mode: {other}; expected ssl, starttls, or plain"
            )),
        }
    }
}

/// One network endpoint (IMAP or SMTP).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub security: Security,
}

impl Endpoint {
    pub fn new(host: impl Into<String>, port: u16, security: Security) -> Self {
        Self {
            host: host.into(),
            port,
            security,
        }
    }

    pub fn label(&self) -> String {
        format!("{}:{} ({})", self.host, self.port, self.security)
    }
}

/// A known mailbox provider preset.
#[derive(Clone, Debug)]
pub struct Provider {
    /// Stable registry id, e.g. `qq`, stored in login.json.
    pub id: &'static str,
    /// Human-readable name for the login wizard.
    pub display_name: &'static str,
    /// Address domains that auto-select this provider.
    pub domains: &'static [&'static str],
    pub imap_host: &'static str,
    pub imap_port: u16,
    pub imap_security: Security,
    pub smtp_host: &'static str,
    pub smtp_port: u16,
    pub smtp_security: Security,
    /// What the provider calls the secret ("authorization code", "app password"…).
    pub secret_label: &'static str,
    /// One-line login hint shown in the wizard.
    pub secret_hint: &'static str,
    /// NetEase servers reject clients that skip the IMAP `ID` handshake.
    pub needs_imap_id: bool,
}

impl Provider {
    pub fn imap(&self) -> Endpoint {
        Endpoint::new(self.imap_host, self.imap_port, self.imap_security)
    }

    pub fn smtp(&self) -> Endpoint {
        Endpoint::new(self.smtp_host, self.smtp_port, self.smtp_security)
    }
}

pub const CUSTOM_PROVIDER_ID: &str = "custom";

pub const PROVIDERS: &[Provider] = &[
    Provider {
        id: "qq",
        display_name: "QQ Mail",
        domains: &["qq.com", "foxmail.com"],
        imap_host: "imap.qq.com",
        imap_port: 993,
        imap_security: Security::Ssl,
        smtp_host: "smtp.qq.com",
        smtp_port: 465,
        smtp_security: Security::Ssl,
        secret_label: "Authorization code",
        secret_hint: "QQ 邮箱 -> 设置 -> 账户 -> 开启 IMAP/SMTP 服务，用生成的授权码（不是 QQ 密码）。",
        needs_imap_id: false,
    },
    Provider {
        id: "gmail",
        display_name: "Gmail",
        domains: &["gmail.com", "googlemail.com"],
        imap_host: "imap.gmail.com",
        imap_port: 993,
        imap_security: Security::Ssl,
        smtp_host: "smtp.gmail.com",
        smtp_port: 465,
        smtp_security: Security::Ssl,
        secret_label: "App password",
        secret_hint: "Enable IMAP in Gmail settings, then create a 16-char App Password (Account -> Security -> App passwords).",
        needs_imap_id: false,
    },
    Provider {
        id: "outlook",
        display_name: "Outlook / Hotmail",
        domains: &["outlook.com", "hotmail.com", "live.com", "msn.com"],
        imap_host: "outlook.office365.com",
        imap_port: 993,
        imap_security: Security::Ssl,
        smtp_host: "smtp-mail.outlook.com",
        smtp_port: 587,
        smtp_security: Security::StartTls,
        secret_label: "App password",
        secret_hint: "Microsoft account -> Security -> app passwords (basic auth must be allowed).",
        needs_imap_id: false,
    },
    Provider {
        id: "163",
        display_name: "NetEase 163",
        domains: &["163.com"],
        imap_host: "imap.163.com",
        imap_port: 993,
        imap_security: Security::Ssl,
        smtp_host: "smtp.163.com",
        smtp_port: 465,
        smtp_security: Security::Ssl,
        secret_label: "Authorization code",
        secret_hint: "163 Mail settings -> POP3/SMTP/IMAP -> enable IMAP, use the authorization code.",
        needs_imap_id: true,
    },
    Provider {
        id: "126",
        display_name: "NetEase 126",
        domains: &["126.com"],
        imap_host: "imap.126.com",
        imap_port: 993,
        imap_security: Security::Ssl,
        smtp_host: "smtp.126.com",
        smtp_port: 465,
        smtp_security: Security::Ssl,
        secret_label: "Authorization code",
        secret_hint: "126 Mail settings -> POP3/SMTP/IMAP -> enable IMAP, use the authorization code.",
        needs_imap_id: true,
    },
    Provider {
        id: "icloud",
        display_name: "iCloud Mail",
        domains: &["icloud.com", "me.com", "mac.com"],
        imap_host: "imap.mail.me.com",
        imap_port: 993,
        imap_security: Security::Ssl,
        smtp_host: "smtp.mail.me.com",
        smtp_port: 587,
        smtp_security: Security::StartTls,
        secret_label: "App-specific password",
        secret_hint: "appleid.apple.com -> Sign-In and Security -> App-Specific Passwords.",
        needs_imap_id: false,
    },
    Provider {
        id: "yahoo",
        display_name: "Yahoo Mail",
        domains: &["yahoo.com", "ymail.com"],
        imap_host: "imap.mail.yahoo.com",
        imap_port: 993,
        imap_security: Security::Ssl,
        smtp_host: "smtp.mail.yahoo.com",
        smtp_port: 465,
        smtp_security: Security::Ssl,
        secret_label: "App password",
        secret_hint: "Yahoo Account Security -> Generate app password.",
        needs_imap_id: false,
    },
];

/// Look a provider up by registry id (`qq`, `gmail`, …).
pub fn provider_by_id(id: &str) -> Option<&'static Provider> {
    let id = id.to_ascii_lowercase();
    // Historical aliases from the QQ/Gmail-only era.
    let id = match id.as_str() {
        "google" => "gmail",
        other => other,
    };
    PROVIDERS.iter().find(|provider| provider.id == id)
}

/// Detect the provider from an email address domain.
pub fn provider_for_address(address: &str) -> Option<&'static Provider> {
    let domain = address.trim().rsplit('@').next()?.to_ascii_lowercase();
    PROVIDERS
        .iter()
        .find(|provider| provider.domains.contains(&domain.as_str()))
}
