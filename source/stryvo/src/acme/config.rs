use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// ACME configuration for automatic SSL certificate management
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct AcmeConfig {
    /// Whether ACME automatic SSL is enabled
    #[serde(default)]
    pub enabled: bool,
    
    /// ACME directory URL (Let's Encrypt production by default)
    #[serde(default = "AcmeConfig::default_directory_url")]
    pub directory_url: String,
    
    /// Email address for ACME account registration
    pub email: Option<String>,
    
    /// Directory to store ACME certificates and keys
    #[serde(default = "AcmeConfig::default_cert_dir")]
    pub cert_dir: PathBuf,
    
    /// Directory to store ACME account information
    #[serde(default = "AcmeConfig::default_account_dir")]
    pub account_dir: PathBuf,
    
    /// Certificate renewal threshold in days (renew when cert expires within this many days)
    #[serde(default = "AcmeConfig::default_renewal_days")]
    pub renewal_days: u32,
    
    /// Challenge type to use for domain validation
    #[serde(default = "AcmeConfig::default_challenge_type")]
    pub challenge_type: ChallengeType,
    
    /// HTTP-01 challenge configuration
    #[serde(default)]
    pub http_challenge: HttpChallengeConfig,
    
    /// DNS-01 challenge configuration (for wildcard certificates)
    pub dns_challenge: Option<DnsChallengeConfig>,
    
    /// Key type for generated certificates
    #[serde(default = "AcmeConfig::default_key_type")]
    pub key_type: KeyType,
    
    /// RSA key size (only used when key_type is RSA)
    #[serde(default = "AcmeConfig::default_rsa_key_size")]
    pub rsa_key_size: u32,
}

/// Challenge type for ACME domain validation
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChallengeType {
    /// HTTP-01 challenge (serves challenge files via HTTP)
    Http01,
    /// DNS-01 challenge (creates DNS TXT records)
    Dns01,
}

/// HTTP-01 challenge configuration
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct HttpChallengeConfig {
    /// Port to serve HTTP-01 challenges on
    #[serde(default = "HttpChallengeConfig::default_port")]
    pub port: u16,
    
    /// Bind address for HTTP-01 challenge server
    #[serde(default = "HttpChallengeConfig::default_bind_addr")]
    pub bind_addr: String,
}

/// DNS-01 challenge configuration
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct DnsChallengeConfig {
    /// DNS provider (cloudflare, route53, etc.)
    pub provider: String,
    
    /// DNS provider specific configuration
    pub config: std::collections::HashMap<String, String>,
}

/// Key type for certificate generation
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum KeyType {
    /// RSA key
    Rsa,
    /// ECDSA P-256 key
    EcdsaP256,
    /// ECDSA P-384 key
    EcdsaP384,
}

impl Default for AcmeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            directory_url: Self::default_directory_url(),
            email: None,
            cert_dir: Self::default_cert_dir(),
            account_dir: Self::default_account_dir(),
            renewal_days: Self::default_renewal_days(),
            challenge_type: Self::default_challenge_type(),
            http_challenge: HttpChallengeConfig::default(),
            dns_challenge: None,
            key_type: Self::default_key_type(),
            rsa_key_size: Self::default_rsa_key_size(),
        }
    }
}

impl AcmeConfig {
    fn default_directory_url() -> String {
        "https://acme-v02.api.letsencrypt.org/directory".to_string()
    }
    
    fn default_cert_dir() -> PathBuf {
        PathBuf::from("./certs")
    }
    
    fn default_account_dir() -> PathBuf {
        PathBuf::from("./acme-accounts")
    }
    
    fn default_renewal_days() -> u32 {
        30
    }
    
    fn default_challenge_type() -> ChallengeType {
        ChallengeType::Http01
    }
    
    fn default_key_type() -> KeyType {
        KeyType::EcdsaP256
    }
    
    fn default_rsa_key_size() -> u32 {
        2048
    }
}

impl Default for HttpChallengeConfig {
    fn default() -> Self {
        Self {
            port: Self::default_port(),
            bind_addr: Self::default_bind_addr(),
        }
    }
}

impl HttpChallengeConfig {
    fn default_port() -> u16 {
        80
    }
    
    fn default_bind_addr() -> String {
        "0.0.0.0".to_string()
    }
}

impl Default for ChallengeType {
    fn default() -> Self {
        ChallengeType::Http01
    }
}

impl Default for KeyType {
    fn default() -> Self {
        KeyType::EcdsaP256
    }
}