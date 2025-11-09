use chrono::{DateTime, Utc};
use std::path::PathBuf;
use thiserror::Error;

/// Information about a certificate managed by ACME
#[derive(Debug, Clone)]
pub struct CertificateInfo {
    /// Domain names covered by this certificate
    pub domains: Vec<String>,
    /// Path to the certificate file
    pub cert_path: PathBuf,
    /// Path to the private key file
    pub key_path: PathBuf,
    /// Certificate expiration date
    pub expires_at: DateTime<Utc>,
    /// Whether this certificate is currently valid
    pub is_valid: bool,
}

/// Errors that can occur during certificate operations
#[derive(Error, Debug)]
pub enum CertificateError {
    #[error("Certificate file not found: {path}")]
    CertificateNotFound { path: PathBuf },
    
    #[error("Private key file not found: {path}")]
    PrivateKeyNotFound { path: PathBuf },
    
    #[error("Failed to parse certificate: {reason}")]
    ParseError { reason: String },
    
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    
    #[error("OpenSSL error: {0}")]
    OpenSsl(#[from] openssl::error::ErrorStack),
}

impl CertificateInfo {
    /// Parse OpenSSL ASN.1 time into UTC DateTime in a robust way
    fn parse_asn1_time_to_utc(asn1: &openssl::asn1::Asn1TimeRef) -> DateTime<Utc> {
        // OpenSSL typically formats as "Feb  6 07:36:56 2026 GMT"
        let s = asn1.to_string();
        let trimmed = s.trim();
        let without_tz = trimmed.strip_suffix(" GMT").unwrap_or(trimmed);

        // First try parsing without timezone (assume UTC)
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(without_tz, "%b %e %H:%M:%S %Y")
            .or_else(|_| chrono::NaiveDateTime::parse_from_str(without_tz, "%b %d %H:%M:%S %Y"))
        {
            return chrono::DateTime::from_naive_utc_and_offset(naive, Utc);
        }

        // Fallback: try with timezone token
        chrono::DateTime::parse_from_str(trimmed, "%b %e %H:%M:%S %Y %Z")
            .or_else(|_| chrono::DateTime::parse_from_str(trimmed, "%b %d %H:%M:%S %Y %Z"))
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now())
    }

    /// Load certificate information from files
    pub fn from_files(cert_path: PathBuf, key_path: PathBuf) -> Result<Self, CertificateError> {
        use openssl::x509::X509;
        use std::fs;
        
        // Check if files exist
        if !cert_path.exists() {
            return Err(CertificateError::CertificateNotFound { path: cert_path });
        }
        
        if !key_path.exists() {
            return Err(CertificateError::PrivateKeyNotFound { path: key_path });
        }
        
        // Read and parse certificate
        let cert_pem = fs::read_to_string(&cert_path)?;
        let cert = X509::from_pem(cert_pem.as_bytes())
            .map_err(|e| CertificateError::ParseError { 
                reason: format!("Failed to parse certificate PEM: {}", e) 
            })?;
        
        // Extract certificate information

        
        let not_after = cert.not_after();
        let expires_at = Self::parse_asn1_time_to_utc(not_after);

        let not_before = cert.not_before();
        let valid_from = Self::parse_asn1_time_to_utc(not_before);
        
        let now = Utc::now();
        let is_valid = now >= valid_from && now <= expires_at;
        
        // Extract domains from certificate
        let mut domains = Vec::new();
        
        // Get common name
        if let Some(subject_name) = cert.subject_name().entries().find(|entry| {
            entry.object().nid() == openssl::nid::Nid::COMMONNAME
        }) {
            if let Ok(cn) = subject_name.data().as_utf8() {
                domains.push(cn.to_string());
            }
        }
        
        // Get Subject Alternative Names
        if let Some(san_extension) = cert.subject_alt_names() {
            for san in san_extension {
                if let Some(dns_name) = san.dnsname() {
                    let domain = dns_name.to_string();
                    if !domains.contains(&domain) {
                        domains.push(domain);
                    }
                }
            }
        }
        
        Ok(CertificateInfo {
            domains,
            cert_path,
            key_path,
            expires_at,
            is_valid,
        })
    }
    
    /// Check if this certificate covers the given domain
    pub fn covers_domain(&self, domain: &str) -> bool {
        self.domains.iter().any(|cert_domain| {
            // Exact match
            if cert_domain == domain {
                return true;
            }
            
            // Wildcard match
            if cert_domain.starts_with("*.") {
                let wildcard_domain = &cert_domain[2..];
                if domain.ends_with(wildcard_domain) {
                    // Ensure it's a proper single-level subdomain match (e.g., x.example.com)
                    // The part before wildcard_domain must end with a dot, and the label before that dot
                    // must not contain any additional dots.
                    let prefix = &domain[..domain.len() - wildcard_domain.len()];
                    if prefix.ends_with('.') {
                        let label = &prefix[..prefix.len() - 1];
                        return !label.is_empty() && !label.contains('.');
                    }
                }
            }
            
            false
        })
    }
    
    /// Check if the certificate needs renewal
    pub fn needs_renewal(&self, renewal_days: u32) -> bool {
        let renewal_threshold = Utc::now() + chrono::Duration::days(renewal_days as i64);
        self.expires_at <= renewal_threshold || !self.is_valid
    }
    
    /// Get the primary domain (first domain in the list)
    pub fn primary_domain(&self) -> Option<&String> {
        self.domains.first()
    }
    
    /// Get days until expiration
    pub fn days_until_expiration(&self) -> i64 {
        (self.expires_at - Utc::now()).num_days()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_covers_domain_exact_match() {
        let cert_info = CertificateInfo {
            domains: vec!["example.com".to_string(), "www.example.com".to_string()],
            cert_path: PathBuf::from("cert.pem"),
            key_path: PathBuf::from("key.pem"),
            expires_at: Utc::now() + chrono::Duration::days(30),
            is_valid: true,
        };
        
        assert!(cert_info.covers_domain("example.com"));
        assert!(cert_info.covers_domain("www.example.com"));
        assert!(!cert_info.covers_domain("api.example.com"));
    }
    
    #[test]
    fn test_covers_domain_wildcard() {
        let cert_info = CertificateInfo {
            domains: vec!["*.example.com".to_string()],
            cert_path: PathBuf::from("cert.pem"),
            key_path: PathBuf::from("key.pem"),
            expires_at: Utc::now() + chrono::Duration::days(30),
            is_valid: true,
        };
        
        assert!(cert_info.covers_domain("api.example.com"));
        assert!(cert_info.covers_domain("www.example.com"));
        assert!(!cert_info.covers_domain("example.com")); // Wildcard doesn't cover root domain
        assert!(!cert_info.covers_domain("sub.api.example.com")); // Wildcard doesn't cover subdomains
    }
    
    #[test]
    fn test_needs_renewal() {
        let cert_info = CertificateInfo {
            domains: vec!["example.com".to_string()],
            cert_path: PathBuf::from("cert.pem"),
            key_path: PathBuf::from("key.pem"),
            expires_at: Utc::now() + chrono::Duration::days(15), // Expires in 15 days
            is_valid: true,
        };
        
        assert!(cert_info.needs_renewal(30)); // Should renew (15 < 30)
        assert!(!cert_info.needs_renewal(10)); // Should not renew (15 > 10)
    }
}