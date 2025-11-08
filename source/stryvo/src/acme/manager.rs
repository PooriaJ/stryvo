use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{RwLock, broadcast};
use tokio::time::Duration;
use chrono::TimeZone;
use thiserror::Error;
use tracing::{info, warn, error, debug};

use super::config::{AcmeConfig, ChallengeType, KeyType};
use super::certificate::{CertificateInfo, CertificateError};
use super::challenge::{HttpChallengeServer, ChallengeError};

/// Events emitted when certificates change, allowing TLS to hot-reload
#[derive(Debug, Clone)]
pub enum CertEvent {
    Installed { domain: String, cert_path: PathBuf, key_path: PathBuf },
    Renewed { domain: String, cert_path: PathBuf, key_path: PathBuf },
}

/// ACME certificate manager
#[derive(Debug)]
pub struct AcmeManager {
    /// ACME configuration
    config: AcmeConfig,
    /// Currently managed certificates
    certificates: Arc<RwLock<HashMap<String, CertificateInfo>>>,
    /// HTTP challenge server (if using HTTP-01 challenges)
    challenge_server: Option<HttpChallengeServer>,
    /// Background renewal task handle
    renewal_task: Option<tokio::task::JoinHandle<()>>,
    /// Domains pending initial certificate acquisition
    pub(crate) pending_domains: Arc<RwLock<Vec<String>>>,
    /// Broadcast channel for certificate change notifications
    cert_event_tx: broadcast::Sender<CertEvent>,
}

/// Errors that can occur during ACME operations
#[derive(Error, Debug)]
pub enum AcmeError {
    #[error("ACME is not enabled")]
    NotEnabled,
    
    #[error("Email address is required for ACME account registration")]
    EmailRequired,
    
    #[error("Certificate error: {0}")]
    Certificate(#[from] CertificateError),
    
    #[error("Challenge error: {0}")]
    Challenge(#[from] ChallengeError),
    
    #[error("ACME client error: {reason}")]
    AcmeClient { reason: String },
    
    #[error("Domain validation failed for {domain}: {reason}")]
    DomainValidation { domain: String, reason: String },
    
    #[error("Certificate generation failed: {reason}")]
    CertificateGeneration { reason: String },
    
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    
    #[error("Directory creation failed: {path}")]
    DirectoryCreation { path: PathBuf },
}

impl AcmeManager {
    /// Create a new ACME manager
    pub fn new(config: AcmeConfig) -> Self {
        let challenge_server = if config.enabled && config.challenge_type == ChallengeType::Http01 {
            Some(HttpChallengeServer::new(
                config.http_challenge.bind_addr.clone(),
                config.http_challenge.port,
            ))
        } else {
            None
        };
        let (cert_event_tx, _rx) = broadcast::channel(100);
        
        Self {
            config,
            certificates: Arc::new(RwLock::new(HashMap::new())),
            challenge_server,
            renewal_task: None,
            pending_domains: Arc::new(RwLock::new(Vec::new())),
            cert_event_tx,
        }
    }
    
    /// Add domains that need initial certificate acquisition
    pub async fn add_pending_domains(&self, domains: Vec<String>) {
        let mut pending = self.pending_domains.write().await;
        pending.extend(domains);
        info!("Added {} domains for initial ACME certificate acquisition", pending.len());
    }
    
    /// Initialize the ACME manager
    pub async fn initialize(&mut self) -> Result<(), AcmeError> {
        if !self.config.enabled {
            return Ok(());
        }
        
        info!("Initializing ACME manager");
        
        // Create necessary directories
        self.create_directories().await?;
        
        // Note: HTTP challenge server is not started here anymore
        // Challenges are handled directly by the VHost service in request_filter
        // This avoids port conflicts and integrates better with the main service
        
        // Load existing certificates
        self.load_existing_certificates().await?;
        
        // Start renewal task
        self.start_renewal_task().await;
        
        info!("ACME manager initialized successfully");
        Ok(())
    }

    /// Subscribe to certificate change events
    pub fn subscribe(&self) -> broadcast::Receiver<CertEvent> {
        self.cert_event_tx.subscribe()
    }
    
    /// Get or create a certificate for the given domains
    pub async fn get_certificate(&self, domains: &[String]) -> Result<CertificateInfo, AcmeError> {
        if !self.config.enabled {
            return Err(AcmeError::NotEnabled);
        }
        
        let primary_domain = domains.first()
            .ok_or_else(|| AcmeError::AcmeClient { 
                reason: "At least one domain is required".to_string() 
            })?;
        
        // Check if we already have a valid certificate
        let mut is_renewal = false;
        {
            let certificates = self.certificates.read().await;
            // Find any certificate that covers this domain (not only keyed by primary domain)
            if let Some(cert_info) = certificates.values().find(|c| c.covers_domain(primary_domain)) {
                let days = cert_info.days_until_expiration();
                let threshold = self.config.renewal_days as i64;
                if !cert_info.needs_renewal(self.config.renewal_days) {
                    info!(
                        "Using cached certificate for '{}' (expires in {} days, threshold {} days) — skipping ACME claim",
                        primary_domain, days, threshold
                    );
                    return Ok(cert_info.clone());
                } else {
                    info!(
                        "Cached certificate for '{}' is nearing expiration ({} days left, threshold {} days) or invalid — proceeding to renewal",
                        primary_domain, days, threshold
                    );
                    is_renewal = true;
                }
            } else {
                info!(
                    "No cached certificate found covering '{}' — initiating ACME claim",
                    primary_domain
                );
            }
        }
        
        // Need to obtain or renew certificate
        info!("Obtaining certificate for domains: {:?}", domains);
        let cert_info = self.obtain_certificate(domains).await?;
        
        // Store the certificate info
        {
            let mut certificates = self.certificates.write().await;
            certificates.insert(primary_domain.clone(), cert_info.clone());
        }
        // Broadcast install/renew event for hot-reload
        let _ = if is_renewal {
            self.cert_event_tx.send(CertEvent::Renewed {
                domain: primary_domain.clone(),
                cert_path: cert_info.cert_path.clone(),
                key_path: cert_info.key_path.clone(),
            })
        } else {
            self.cert_event_tx.send(CertEvent::Installed {
                domain: primary_domain.clone(),
                cert_path: cert_info.cert_path.clone(),
                key_path: cert_info.key_path.clone(),
            })
        };
        
        Ok(cert_info)
    }
    
    /// Get certificate info for a domain
    pub async fn get_certificate_info(&self, domain: &str) -> Option<CertificateInfo> {
        let certificates = self.certificates.read().await;
        certificates.values()
            .find(|cert| cert.covers_domain(domain))
            .cloned()
    }
    
    /// List all managed certificates
    pub async fn list_certificates(&self) -> Vec<CertificateInfo> {
        let certificates = self.certificates.read().await;
        certificates.values().cloned().collect()
    }
    
    /// Get challenge response for a given token (used by HTTP-01 challenge handling)
    pub async fn get_challenge_response(&self, token: &str) -> Option<String> {
        if let Some(ref server) = self.challenge_server {
            let challenges = server.challenges.read().await;
            challenges.get(token).cloned()
        } else {
            None
        }
    }
    
    /// Create necessary directories
    async fn create_directories(&self) -> Result<(), AcmeError> {
        use tokio::fs;
        
        for dir in [&self.config.cert_dir, &self.config.account_dir] {
            if !dir.exists() {
                fs::create_dir_all(dir).await
                    .map_err(|_| AcmeError::DirectoryCreation { path: dir.clone() })?;
                info!("Created directory: {:?}", dir);
            }
        }
        
        Ok(())
    }
    
    /// Load existing certificates from disk
    async fn load_existing_certificates(&self) -> Result<(), AcmeError> {
        use tokio::fs;
        
        let mut entries = fs::read_dir(&self.config.cert_dir).await?;
        let mut certificates = self.certificates.write().await;
        
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("crt") {
                let cert_path = path.clone();
                let key_path = path.with_extension("key");
                
                match CertificateInfo::from_files(cert_path.clone(), key_path.clone()) {
                    Ok(cert_info) => {
                        if let Some(primary_domain) = cert_info.primary_domain() {
                            info!(
                                "Loaded existing certificate for '{}' (expires in {} days, valid: {})",
                                primary_domain,
                                cert_info.days_until_expiration(),
                                cert_info.is_valid
                            );
                            certificates.insert(primary_domain.clone(), cert_info);
                        }
                    }
                    Err(e) => {
                        warn!("Failed to load certificate from {:?}: {} - removing invalid files", path, e);
                        // Remove invalid certificate files
                        let _ = fs::remove_file(&cert_path).await;
                        let _ = fs::remove_file(&key_path).await;
                        debug!("Removed invalid certificate files");
                    }
                }
            }
        }
        
        Ok(())
    }
    
    /// Start the background renewal task (daily midnight checks only)
    async fn start_renewal_task(&mut self) {
        let certificates = Arc::clone(&self.certificates);
        let pending_domains = Arc::clone(&self.pending_domains);
        let config = self.config.clone();
        let event_tx = self.cert_event_tx.clone();
        // Share the same challenge store with the spawned manager to serve HTTP-01 via VHost
        let challenge_store = self.challenge_server.as_ref().map(|s| Arc::clone(&s.challenges));

        let handle = tokio::spawn(async move {
            // Align first run to local midnight to avoid per-request overhead
            #[allow(deprecated)]
            let now = chrono::Local::now();
            let today = now.date_naive();
            let next_day = today.succ_opt().unwrap_or(today + chrono::Days::new(1));
            let next_midnight_naive = next_day.and_hms_opt(0, 0, 0).unwrap();
            let next_midnight = chrono::Local.from_local_datetime(&next_midnight_naive).unwrap();
            let secs_until_midnight = (next_midnight - now).num_seconds().max(0);

            info!(
                "ACME renewal task scheduled: sleeping {}s until local midnight",
                secs_until_midnight
            );
            tokio::time::sleep(Duration::from_secs(secs_until_midnight as u64)).await;

            loop {
                info!("Running daily ACME certificate check at local midnight");

                // Log current certificate statuses for audit
                {
                    let certs_read = certificates.read().await;
                    for (primary, cert) in certs_read.iter() {
                        debug!(
                            "Certificate status: '{}' expires in {} days (valid: {}, threshold: {} days)",
                            primary,
                            cert.days_until_expiration(),
                            cert.is_valid,
                            config.renewal_days
                        );
                    }
                }

                // Process any pending domains: acquire only if missing or under threshold
                let domains_to_acquire = {
                    let mut pending = pending_domains.write().await;
                    pending.drain(..).collect::<Vec<_>>()
                };

                for domain in domains_to_acquire {
                    if domain == "default" {
                        continue;
                    }

                    let existing = {
                        let certs_read = certificates.read().await;
                        certs_read.get(&domain).cloned()
                    };

                    let should_acquire = match existing {
                        None => {
                            info!("No existing certificate for '{}' — acquiring", domain);
                            true
                        }
                        Some(info) => {
                            let days_left = info.days_until_expiration();
                            if info.needs_renewal(config.renewal_days) {
                                info!(
                                    "Certificate for '{}' near expiration ({} days < {} days) — acquiring",
                                    domain, days_left, config.renewal_days
                                );
                                true
                            } else {
                                info!(
                                    "Certificate for '{}' is sufficiently valid ({} days ≥ {} days) — skipping",
                                    domain, days_left, config.renewal_days
                                );
                                false
                            }
                        }
                    };

                    if should_acquire {
                        // Create a manager that shares the same challenge store so VHost can serve tokens
                        let mut manager = AcmeManager::new(config.clone());
                        if let Some(store) = &challenge_store {
                            if let Some(ref mut server) = manager.challenge_server {
                                server.challenges = Arc::clone(store);
                            }
                        }
                        if let Err(e) = manager.create_directories().await {
                            error!("Failed to create directories for {}: {}", domain, e);
                            continue;
                        }
                        if let Err(e) = manager.load_existing_certificates().await {
                            warn!(
                                "Failed to load existing certificates before acquisition for {}: {}",
                                domain, e
                            );
                        }
                        match manager.get_certificate(&[domain.clone()]).await {
                            Ok(cert_info) => {
                                let mut certs_write = certificates.write().await;
                                certs_write.insert(domain.clone(), cert_info.clone());
                                info!(
                                    "Installed certificate for {} (expires in {} days)",
                                    domain,
                                    cert_info.days_until_expiration()
                                );
                                // Notify listeners for hot-reload
                                let _ = event_tx.send(CertEvent::Installed {
                                    domain: domain.clone(),
                                    cert_path: cert_info.cert_path.clone(),
                                    key_path: cert_info.key_path.clone(),
                                });
                            }
                            Err(e) => {
                                error!(
                                    "Failed to acquire initial/renewed certificate for {}: {}",
                                    domain, e
                                );
                            }
                        }
                    }
                }

                // Renew certificates already managed if under threshold
                let certs_to_renew = {
                    let certs_read = certificates.read().await;
                    certs_read
                        .iter()
                        .filter(|(_, cert)| cert.needs_renewal(config.renewal_days))
                        .map(|(domain, cert)| (domain.clone(), cert.domains.clone()))
                        .collect::<Vec<_>>()
                };

                for (primary_domain, domains) in certs_to_renew {
                    info!("Renewing certificate for {}", primary_domain);
                    let mut manager = AcmeManager::new(config.clone());
                    if let Some(store) = &challenge_store {
                        if let Some(ref mut server) = manager.challenge_server {
                            server.challenges = Arc::clone(store);
                        }
                    }
                    if let Err(e) = manager.create_directories().await {
                        error!(
                            "Failed to create directories for {}: {}",
                            primary_domain, e
                        );
                        continue;
                    }
                    match manager.obtain_certificate(&domains).await {
                        Ok(new_cert) => {
                            let mut certs_write = certificates.write().await;
                            // Clone paths before moving cert into map
                            let cert_path = new_cert.cert_path.clone();
                            let key_path = new_cert.key_path.clone();
                            certs_write.insert(primary_domain.clone(), new_cert);
                            info!("Successfully renewed certificate for {}", primary_domain);
                            // Notify listeners for hot-reload
                            let _ = event_tx.send(CertEvent::Renewed {
                                domain: primary_domain.clone(),
                                cert_path,
                                key_path,
                            });
                        }
                        Err(e) => {
                            error!(
                                "Failed to renew certificate for {}: {}",
                                primary_domain, e
                            );
                        }
                    }
                }

                // Sleep exactly 24 hours to hit next midnight consistently
                tokio::time::sleep(Duration::from_secs(24 * 60 * 60)).await;
            }
        });

        self.renewal_task = Some(handle);
    }

    /// Run immediate acquisition for pending domains after a delay, once listeners are expected up
    pub async fn run_startup_acquisition_after(&self, delay: Duration) {
        info!(
            "ACME startup: waiting {}s before immediate acquisition for pending domains",
            delay.as_secs()
        );
        tokio::time::sleep(delay).await;

        // Drain pending domains populated at startup (missing certs only)
        let domains_to_acquire = {
            let mut pending = self.pending_domains.write().await;
            pending.drain(..).collect::<Vec<_>>()
        };

        for domain in domains_to_acquire {
            if domain == "default" {
                continue;
            }

            let existing = {
                let certs_read = self.certificates.read().await;
                certs_read.get(&domain).cloned()
            };

            match existing {
                None => {
                    info!(
                        "Startup: no existing certificate for '{}' — acquiring immediately",
                        domain
                    );
                    match self.get_certificate(&[domain.clone()]).await {
                        Ok(info) => {
                            info!(
                                "Startup: installed certificate for {} (expires in {} days)",
                                domain,
                                info.days_until_expiration()
                            );
                        }
                        Err(e) => {
                            error!(
                                "Startup: failed to acquire certificate for {}: {}",
                                domain, e
                            );
                        }
                    }
                }
                Some(info) => {
                    let days_left = info.days_until_expiration();
                    if info.needs_renewal(self.config.renewal_days) {
                        info!(
                            "Startup: certificate for '{}' near expiration ({} days < {} days) — deferring to midnight",
                            domain, days_left, self.config.renewal_days
                        );
                    } else {
                        info!(
                            "Startup: certificate for '{}' is sufficiently valid ({} days ≥ {} days) — skipping",
                            domain, days_left, self.config.renewal_days
                        );
                    }
                }
            }
        }
    }
    
    /// Obtain a new certificate from ACME provider
    async fn obtain_certificate(&self, domains: &[String]) -> Result<CertificateInfo, AcmeError> {
        use acme_lib::{create_p256_key, create_rsa_key, Directory, DirectoryUrl};
        use acme_lib::persist::FilePersist;
        use std::fs;
        
        let primary_domain = &domains[0];
        info!("Starting ACME certificate acquisition for domains: {:?}", domains);
        
        // Create ACME directory
        let directory_url = match self.config.directory_url.as_str() {
            "https://acme-v02.api.letsencrypt.org/directory" => DirectoryUrl::LetsEncrypt,
            "https://acme-staging-v02.api.letsencrypt.org/directory" => DirectoryUrl::LetsEncryptStaging,
            url => DirectoryUrl::Other(url),
        };
        
        info!("Using ACME directory: {}", self.config.directory_url);
        
        // Create file persistence for ACME data
        let persist = FilePersist::new(&self.config.account_dir);
        let dir = Directory::from_url(persist, directory_url)
            .map_err(|e| AcmeError::AcmeClient { reason: format!("Failed to create ACME directory: {}", e) })?;
        
        // Create or load account
        let email = self.config.email.as_ref()
            .ok_or(AcmeError::EmailRequired)?;
        
        info!("Creating/loading ACME account for email: {}", email);
        let account = dir.account(email)
            .map_err(|e| AcmeError::AcmeClient { reason: format!("Failed to create/load ACME account: {}", e) })?;
        
        // Create order for domains
        let alt_names: Vec<&str> = domains.iter().skip(1).map(|s| s.as_str()).collect();
        info!("Creating ACME order for primary domain '{}' with {} alternative names", primary_domain, alt_names.len());
        
        let order = account.new_order(primary_domain, &alt_names)
            .map_err(|e| AcmeError::AcmeClient { reason: format!("Failed to create ACME order: {}", e) })?;
        
        // Get authorizations and complete challenges
        info!("Processing domain authorizations and challenges");
        // Track which domains/tokens we've already validated to avoid re-validating in a loop
        let mut validated_domains: HashSet<String> = HashSet::new();
        let mut handled_tokens: HashSet<String> = HashSet::new();
        let mut attempts: u32 = 0;

        let order_csr = loop {
            attempts += 1;
            if let Some(order_csr) = order.confirm_validations() {
                info!("All domain validations confirmed, proceeding to certificate generation");
                break order_csr;
            }
            
            let auths = order.authorizations()
                .map_err(|e| AcmeError::AcmeClient { reason: format!("Failed to get authorizations: {}", e) })?;
            
            for auth in auths {
                let domain = auth.api_auth().identifier.value.clone();
                
                match self.config.challenge_type {
                    ChallengeType::Http01 => {
                        let challenge = auth.http_challenge();
                        let token = challenge.http_token().to_string();
                        let key_auth = challenge.http_proof();

                        // Skip re-validation if we've already handled this token in a prior loop
                        if handled_tokens.contains(&token) {
                            debug!(
                                "Challenge token already handled for domain '{}'; skipping re-validate",
                                domain
                            );
                            continue;
                        }
                        
                        // First time handling this challenge for the domain
                        info!("Validating domain: {}", domain);
                        
                        info!("Setting up HTTP-01 challenge for domain '{}' with token: {}", domain, token);
                        
                        // Add challenge to server (challenges are handled by VHost service)
                        if let Some(ref server) = self.challenge_server {
                            info!("Storing challenge - token: {}, key_auth length: {}", token, key_auth.len());
                            info!("Challenge key_auth value: {}", key_auth);
                            server.add_challenge(token.clone(), key_auth.clone()).await;
                            info!("Challenge token added to challenge store - will be served by VHost service");
                            
                            // Verify it was stored correctly
                            let challenges = server.challenges.read().await;
                            if let Some(stored) = challenges.get(&token) {
                                info!("Verified challenge stored correctly - length: {}", stored.len());
                                if stored != &key_auth {
                                    error!("Challenge mismatch! Stored value differs from original");
                                }
                            } else {
                                error!("Challenge was not stored!");
                            }
                        } else {
                            return Err(AcmeError::AcmeClient { 
                                reason: "HTTP challenge server not available".to_string() 
                            });
                        }
                        
                        // Validate challenge
                        info!("Requesting ACME provider to validate domain '{}'", domain);
                        challenge.validate(5000)
                            .map_err(|e| AcmeError::DomainValidation { 
                                domain: domain.clone(), 
                                reason: format!("Domain validation failed: {}", e)
                            })?;
                        
                        info!("Domain '{}' validated successfully", domain);
                        validated_domains.insert(domain.clone());
                        handled_tokens.insert(token.clone());
                        
                        // Remove challenge from server
                        if let Some(ref server) = self.challenge_server {
                            server.remove_challenge(&token).await;
                            info!("Challenge token removed from challenge store");
                        }
                    }
                    ChallengeType::Dns01 => {
                        return Err(AcmeError::AcmeClient { 
                            reason: "DNS-01 challenges not yet implemented".to_string() 
                        });
                    }
                }
            }

            // If we've already submitted all validations, avoid hammering the challenge API; wait briefly
            if validated_domains.len() >= domains.len() {
                debug!(
                    "All challenges submitted ({}); waiting for order confirmation",
                    validated_domains.len()
                );
                tokio::time::sleep(Duration::from_millis(2000)).await;
            }

            // Safety guard to avoid endless loops in case of unexpected API behavior
            if attempts > 120 { // ~60 seconds max with 500ms sleeps
                return Err(AcmeError::AcmeClient { 
                    reason: "Timed out waiting for ACME order validations to confirm".to_string() 
                });
            }
        };
        
        // Generate certificate key
        info!("Generating certificate private key");
        let cert_key = match self.config.key_type {
            KeyType::Rsa => create_rsa_key(self.config.rsa_key_size),
            KeyType::EcdsaP256 => create_p256_key(),
            KeyType::EcdsaP384 => {
                return Err(AcmeError::CertificateGeneration { 
                    reason: "ECDSA P-384 keys not supported by acme-lib".to_string() 
                });
            }
        };
        
        // Finalize order and get certificate
        info!("Finalizing ACME order and obtaining certificate");
        
        // Keep the cert_key for later use
        let cert_key_clone = cert_key.clone();
        
        let cert_order = order_csr.finalize_pkey(cert_key, 5000)
            .map_err(|e| AcmeError::CertificateGeneration { 
                reason: format!("Failed to finalize certificate order: {}", e)
            })?;
        
        info!("ACME certificate successfully obtained for domain: {}", primary_domain);

        // Save certificate to our cert directory
        let cert_path = self.config.cert_dir.join(format!("{}.crt", primary_domain));
        let key_path = self.config.cert_dir.join(format!("{}.key", primary_domain));

        info!("Saving certificate to: {:?}", cert_path);
        info!("Saving private key to: {:?}", key_path);

        // Use OpenSSL to convert the private key to PEM
        use openssl::x509::X509;
        use openssl::nid::Nid;

        // Convert the cert_key_clone (PKey<Private>) to PEM format
        let key_pem = cert_key_clone.private_key_to_pem_pkcs8()
            .map_err(|e| AcmeError::CertificateGeneration { 
                reason: format!("Failed to convert private key to PEM: {}", e)
            })?;

        // Save the private key
        fs::write(&key_path, &key_pem)
            .map_err(|e| AcmeError::Io(e))?;

        info!("Private key saved successfully ({} bytes)", key_pem.len());

        // Download and persist the certificate using acme-lib
        info!("Downloading certificate and saving to ACME persistence store");
        let _downloaded_cert = cert_order
            .download_and_save_cert()
            .map_err(|e| AcmeError::CertificateGeneration { 
                reason: format!("Failed to download certificate: {}", e) 
            })?;
        info!("Certificate downloaded and saved to persistence store");

        // Attempt to find and assemble a fullchain PEM from the persistence directory
        let account_dir = &self.config.account_dir;
        info!("Searching for certificate files in: {:?}", account_dir);

        let mut parsed_certs: Vec<X509> = Vec::new();

        if let Ok(dir_iter) = fs::read_dir(account_dir) {
            for entry in dir_iter.flatten() {
                let path = entry.path();
                if path.is_file() {
                    if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                        info!("Found file in account dir: {}", name);
                    }
                    // Try DER first
                    if let Ok(bytes) = fs::read(&path) {
                        if let Ok(cert) = X509::from_der(&bytes) {
                            parsed_certs.push(cert);
                            continue;
                        }
                        // Try PEM
                        if let Ok(cert) = X509::from_pem(&bytes) {
                            parsed_certs.push(cert);
                            continue;
                        }
                    }
                }
            }
        }

        if parsed_certs.is_empty() {
            error!("Could not find certificate files in acme-lib persistence directory");
            return Err(AcmeError::CertificateGeneration { 
                reason: "Certificate was obtained from Let's Encrypt but acme-lib didn't save it to expected location".to_string() 
            });
        }

        // Identify the leaf certificate matching our domains
        let mut leaf_cert_index: Option<usize> = None;
        for (idx, cert) in parsed_certs.iter().enumerate() {
            // Check Common Name
            let cn = cert.subject_name()
                .entries_by_nid(Nid::COMMONNAME)
                .next()
                .and_then(|e| e.data().as_utf8().ok())
                .map(|s| s.to_string());
            if let Some(cn) = cn.as_ref() {
                if domains.iter().any(|d| d == cn) {
                    leaf_cert_index = Some(idx);
                    break;
                }
            }
            // Check SAN DNS names
            if let Some(san) = cert.subject_alt_names() {
                let mut matches = false;
                for name in san.iter() {
                    if let Some(dns) = name.dnsname() {
                        if domains.iter().any(|d| d == dns) {
                            matches = true;
                            break;
                        }
                    }
                }
                if matches {
                    leaf_cert_index = Some(idx);
                    break;
                }
            }
        }

        // Build fullchain: leaf first, then the rest
        let mut fullchain_pem: Vec<u8> = Vec::new();
        if let Some(leaf_idx) = leaf_cert_index {
            if let Ok(pem) = parsed_certs[leaf_idx].to_pem() {
                fullchain_pem.extend_from_slice(&pem);
            }
            for (idx, cert) in parsed_certs.iter().enumerate() {
                if idx == leaf_idx { continue; }
                if let Ok(pem) = cert.to_pem() {
                    fullchain_pem.extend_from_slice(&pem);
                }
            }
        } else {
            // Fallback: just concatenate all certs
            for cert in &parsed_certs {
                if let Ok(pem) = cert.to_pem() {
                    fullchain_pem.extend_from_slice(&pem);
                }
            }
        }

        // Write the fullchain to cert_path
        fs::write(&cert_path, &fullchain_pem)
            .map_err(|e| AcmeError::Io(e))?;
        info!("Certificate chain saved successfully ({} bytes)", fullchain_pem.len());

        // Build CertificateInfo from files to get accurate metadata
        match CertificateInfo::from_files(cert_path.clone(), key_path.clone()) {
            Ok(info) => Ok(info),
            Err(e) => {
                error!("Failed to parse saved certificate: {}", e);
                Err(AcmeError::Certificate(e))
            }
        }
    }
}

impl Drop for AcmeManager {
    fn drop(&mut self) {
        if let Some(handle) = self.renewal_task.take() {
            handle.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    
    fn create_test_config() -> (AcmeConfig, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let config = AcmeConfig {
            enabled: true,
            directory_url: "https://acme-staging-v02.api.letsencrypt.org/directory".to_string(),
            email: Some("test@example.com".to_string()),
            cert_dir: temp_dir.path().join("certs"),
            account_dir: temp_dir.path().join("accounts"),
            renewal_days: 30,
            challenge_type: ChallengeType::Http01,
            http_challenge: Default::default(),
            dns_challenge: None,
            key_type: KeyType::EcdsaP256,
            rsa_key_size: 2048,
        };
        (config, temp_dir)
    }
    
    #[tokio::test]
    async fn test_acme_manager_creation() {
        let (config, _temp_dir) = create_test_config();
        let manager = AcmeManager::new(config);
        
        assert!(manager.challenge_server.is_some());
        assert!(manager.renewal_task.is_none());
    }
    
    #[tokio::test]
    async fn test_acme_manager_disabled() {
        let mut config = AcmeConfig::default();
        config.enabled = false;
        
        let manager = AcmeManager::new(config);
        let result = manager.get_certificate(&["example.com".to_string()]).await;
        
        assert!(matches!(result, Err(AcmeError::NotEnabled)));
    }
    
    #[tokio::test]
    async fn test_directory_creation() {
        let (config, _temp_dir) = create_test_config();
        let manager = AcmeManager::new(config);
        
        let result = manager.create_directories().await;
        assert!(result.is_ok());
        
        assert!(manager.config.cert_dir.exists());
        assert!(manager.config.account_dir.exists());
    }
}