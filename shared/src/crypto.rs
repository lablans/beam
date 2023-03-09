use axum::{Json, http::Request, body::Body, async_trait};

use once_cell::sync::OnceCell;
use static_init::dynamic;
use tokio::{sync::{RwLock, mpsc, oneshot}};
use tracing::{debug, warn, info, error};
use std::{path::{Path, PathBuf}, error::Error, time::{SystemTime, Duration}, collections::HashMap, sync::Arc, fs::read_to_string, borrow::BorrowMut};
use rsa::{PublicKey, RsaPrivateKey, RsaPublicKey, PublicKeyParts, PaddingScheme, pkcs1::DecodeRsaPublicKey, pkcs8::DecodePublicKey};
use sha2::{Sha256, Digest};
use openssl::{x509::X509, string::OpensslString, asn1::{Asn1Time, Asn1TimeRef}, error::ErrorStack, rand::rand_bytes};

use crate::{errors::{SamplyBeamError, CertificateInvalidReason}, MsgTaskRequest, EncryptedMsgTaskRequest, config, beam_id::{ProxyId, BeamId, AppOrProxyId}, config_shared::ConfigCrypto, crypto};

type Serial = String;

pub(crate) struct ProxyCertInfo {
    pub(crate) proxy_name: String,
    pub(crate) valid_since: String,
    pub(crate) valid_until: String,
    pub(crate) common_name: String,
    pub(crate) serial: String
}

impl TryFrom<&X509> for ProxyCertInfo {
    type Error = SamplyBeamError;

    fn try_from(cert: &X509) -> Result<Self, Self::Error> {
        // let remaining = Asn1Time::days_from_now(0)?.diff(cert.not_after())?;
        let common_name = cert.subject_name().entries()
            .find(|c| c.object().nid() == openssl::nid::Nid::COMMONNAME)
            .ok_or(SamplyBeamError::CertificateError(CertificateInvalidReason::NoCommonName))?
            .data().as_utf8()?.to_string();

        const SERIALERR: SamplyBeamError = SamplyBeamError::CertificateError(CertificateInvalidReason::WrongSerial);

        let certinfo = ProxyCertInfo {
            proxy_name: common_name
                .split('.')
                .next().ok_or(SamplyBeamError::CertificateError(CertificateInvalidReason::InvalidCommonName))?
                .into(),
            common_name,
            valid_since: cert.not_before().to_string(),
            valid_until: cert.not_after().to_string(),
            serial: cert.serial_number()
                .to_bn()
                .map_err(|e| SERIALERR)?
                .to_hex_str()
                .map_err(|e| SERIALERR)?
                .to_string()
        };
        Ok(certinfo)
    }
}

#[derive(Clone)]
enum CertificateCacheEntry {
    Valid(X509),
    Invalid(CertificateInvalidReason)
}

pub(crate) struct CertificateCache{
    serial_to_x509: HashMap<Serial, CertificateCacheEntry>,
    cn_to_serial: HashMap<ProxyId, Vec<Serial>>,
    update_trigger: mpsc::Sender<oneshot::Sender<Result<(),SamplyBeamError>>>,
    root_cert: Option<X509>, // Might not be available at initialization time
    im_cert: Option<X509> // Might not be available at initialization time
}

#[async_trait]
pub trait GetCerts: Sync + Send {
    fn new() -> Result<Self, SamplyBeamError> where Self: Sized;
    async fn certificate_list(&self) -> Result<Vec<String>,SamplyBeamError>;
    async fn certificate_by_serial_as_pem(&self, serial: &str) -> Result<String,SamplyBeamError>;
    async fn im_certificate_as_pem(&self) -> Result<String,SamplyBeamError>;
}

impl CertificateCache {
    pub fn new(update_trigger: mpsc::Sender<oneshot::Sender<Result<(),SamplyBeamError>>>) -> Result<CertificateCache,SamplyBeamError> {
        Ok(Self{
            serial_to_x509: HashMap::new(),
            cn_to_serial: HashMap::new(),
            update_trigger,
            root_cert: None,
            im_cert: None
        })
    }

    /// Searches cache for a certificate with the given ClientId. If not found, updates cache from central vault. If then still not found, return None
    pub async fn get_all_certs_by_cname(cname: &ProxyId) -> Vec<X509> { // TODO: What if multiple certs are found?
        let mut result = Vec::new();
        Self::update_certificates().await.unwrap_or_else(|e| { // requires write lock.
            warn!("Updating certificates failed: {}",e);
            ()
        });
        debug!("Getting cert(s) with cname {}", cname);
        { // TODO: Do smart caching: Return reference to existing certificate that exists only once in memory.
            let cache = CERT_CACHE.read().await;
            if let Some(serials) = cache.cn_to_serial.get(cname){
                debug!("Considering {} certificates with matching CN: {:?}", serials.len(), serials);
                for serial in serials {
                    debug!("Fetching certificate with serial {}", serial);
                    let x509 = cache.serial_to_x509.get(serial);
                    if let Some(x509) = x509 {
                        if let CertificateCacheEntry::Valid(x509) = x509 {
                            if ! x509_date_valid(x509).unwrap_or(true) {
                                let Ok(info) = crypto::ProxyCertInfo::try_from(x509) else {
                                    warn!("Found invalid x509 certificate -- even unable to parse it.");
                                    continue;
                                };
                                warn!("Found x509 certificate with invalid date: CN={}, serial={}", info.common_name, info.serial);
                            } else {
                                debug!("Certificate with serial {} successfully retrieved.", serial);
                                result.push(x509.clone());
                            }
                        }
                    }
                }
            };
        } // Drop Read Locks
        if result.is_empty() {
            warn!("Did not find certificate for cname {}, even after update.", cname);
        } else {
            debug!("Found {} certificate(s) for cname {}.", result.len(), cname);
        }
        result
    }

    /// Searches cache for a certificate with the given Serial. If not found, updates cache from central vault. If then still not found, return None
    pub async fn get_by_serial(serial: &str) -> Option<X509> {
        { // TODO: Do smart caching: Return reference to existing certificate that exists only once in memory.
            let cache = CERT_CACHE.read().await;
            let cert = cache.serial_to_x509.get(serial);
            match cert { // why is this not done in the second try?
                Some(CertificateCacheEntry::Valid(certificate)) => {
                    if x509_date_valid(&certificate).unwrap_or(false) { 
                        return Some(certificate.clone());
                    }
                },
                Some(CertificateCacheEntry::Invalid(reason)) => {
                    debug!("Ignoring invalid certificate {serial}: {reason}");
                    return None;
                },
                None => ()
            }
        }
        Self::update_certificates().await.unwrap_or_else(|e| { // requires write lock.
            warn!("Updating certificates failed: {}",e);
            ()
        });
        let cache = CERT_CACHE.read().await;
        let cert = cache.serial_to_x509.get(serial);

        return if let Some(CertificateCacheEntry::Valid(certificate)) = cert {
            Some(certificate.clone())
        } else {
            None
        }
    }

    /// Manually update cache from fetching all certs from the central vault
    async fn update_certificates() -> Result<(),SamplyBeamError> {
        debug!("Triggering certificate update ...");
        let (tx, rx) = oneshot::channel::<Result<(),SamplyBeamError>>();
        CERT_CACHE.read().await.update_trigger.send(tx).await
            .expect("Internal Error: Certificate Store Updater is not listening for requests.");
        match rx.await {
            Ok(result) => {
                debug!("Certificate update successfully completed.");
                result
            },
            Err(e) => Err(SamplyBeamError::InternalSynchronizationError(e.to_string()))
        }
    }

    async fn update_certificates_mut(&mut self) -> Result<usize,SamplyBeamError> {
        info!("Updating certificates ...");
        let certificate_list = CERT_GETTER.get().unwrap().certificate_list().await?;
        let new_certificate_serials: Vec<&String> = {
            certificate_list.iter()
                .filter(|serial| {
                    let cached = self.serial_to_x509.get(*serial);
                    if let Some(CertificateCacheEntry::Invalid(reason)) = cached {
                        warn!("Skipping known-to-be-broken certificate {serial}: {reason}");
                        false
                    } else {
                        true
                    }
                })
                .collect()
        };
        debug!("Received {} certificates ({} of which were new).", certificate_list.len(), new_certificate_serials.len());

        let mut new_count = 0;
        //TODO Check for validity
        for serial in new_certificate_serials {
            debug!("Checking certificate with serial {serial}");

            let certificate = CERT_GETTER.get().unwrap().certificate_by_serial_as_pem(serial).await;
            if let Err(e) = certificate {
                warn!("Could not retrieve certificate for serial {serial}: {}", e);
                continue;
            }
            let certificate = certificate.unwrap();
            let opensslcert = X509::from_pem(certificate.as_bytes())?;
            let commonnames: Vec<ProxyId> = 
                opensslcert.subject_name()
                .entries()
                .map(|x| x.data().as_utf8().unwrap()) // TODO: Remove unwrap, e.g. by supplying empty _or-string
                .collect::<Vec<OpensslString>>()
                .iter()
                .map(|x| {
                    ProxyId::new(&x.to_string())
                        .expect(&format!("Internal error: Vault returned certificate with invalid common name: {}", x))
                })
                .collect();

            let err = {
                if commonnames.is_empty() {
                    Some(CertificateInvalidReason::NoCommonName)
                } else if let Err(e) = verify_cert(&opensslcert, &self.im_cert.as_ref().expect("No intermediate CA cert found")) {
                    Some(e)
                } else {
                    None
                }
            };
            if let Some(err) = err {
                warn!("Certificate with serial {} invalid: {}.", serial, err);
                self.serial_to_x509.insert(serial.clone(), CertificateCacheEntry::Invalid(err));
            } else {
                let cn = commonnames.first()
                    .expect("Internal error: common names empty; this should not happen");
                self.serial_to_x509.insert(serial.clone(), CertificateCacheEntry::Valid(opensslcert));
                match self.cn_to_serial.get_mut(cn) {
                    Some(serials) => serials.push(serial.clone()),
                    None => {
                        let new = vec![serial.clone()];
                        self.cn_to_serial.insert(cn.clone(), new);
                    }
                };
                debug!("Added certificate {} for cname {}", serial, cn);
                new_count+=1;
            }
        }
    Ok(new_count)

    }

/*
    /// Returns all ClientIds and associated certificates currently in cache
    pub async fn get_all_cnames_and_certs() -> Vec<(ProxyId,X509)> {
        let cache = &CERT_CACHE.read().await.serial_to_x509;
        let alias = &CERT_CACHE.read().await.cn_to_serial;
        let mut result = Vec::new();
        if alias.is_empty() {
            return result;
        }
        for (cname,serials) in alias.iter() {
            for serial in serials {
                if let Some(cert) = cache.get(serial) {
                    result.push((cname.clone(), cert.clone()));
                } else {
                    warn!("Unable to find certificate for serial {}.", serial);
                }
            }
        }
        result
    }*/

    /// Sets the root certificate, which is usually not available at static time. Must be called before certificate validation
    pub fn set_root_cert(&mut self, root_certificate: &X509) {
        self.root_cert = Some(root_certificate.clone());
    }

    pub async fn set_im_cert(&mut self) {
        self.im_cert = Some(X509::from_pem(&get_im_cert().await.unwrap().as_bytes()).unwrap()); // TODO Unwrap
        let _ = verify_cert(&self.im_cert.as_ref().expect("No IM certificate provided"), &self.root_cert.as_ref().expect("No root certificate set!"))
            .expect(&format!("The intermediate certificate is invalid. Please send this info to the central beam admin for debugging:\n---BEGIN DEBUG---\n{}\nroot\n{}\n---END DEBUG---", 
                             String::from_utf8(self.im_cert.as_ref().unwrap().to_text().unwrap_or("Cannot convert IM certificate to text".into())).unwrap_or("Invalid characters in IM certificate".to_string()),
                             String::from_utf8(self.root_cert.as_ref().unwrap().to_text().unwrap_or("Cannot convert root certificate to text".into())).unwrap_or("Invalid characters in root certificate".to_string())));
        
    }
}

/// Wrapper for initializing the CA chain. Must be called *after* config initialization 
pub async fn init_ca_chain() {
    CERT_CACHE.write().await.set_root_cert(&config::CONFIG_SHARED.root_cert);
    CERT_CACHE.write().await.set_im_cert().await;
}

static CERT_GETTER: OnceCell<Box<dyn GetCerts>> = OnceCell::new();

pub fn init_cert_getter<G: GetCerts + 'static>(getter: G) {
    let res = CERT_GETTER.set(Box::new(getter));
    if res.is_err() {
        panic!("Internal error: Tried to initialize cert_getter twice");
    }
}

pub async fn get_serial_list() -> Result<Vec<String>, SamplyBeamError> {
    CERT_GETTER.get().unwrap().certificate_list().await
}

pub async fn get_im_cert() -> Result<String, SamplyBeamError> {
    CERT_GETTER.get().unwrap().im_certificate_as_pem().await
}

#[dynamic(lazy)]
pub(crate) static CERT_CACHE: Arc<RwLock<CertificateCache>> = {
    let (tx, mut rx) = mpsc::channel::<oneshot::Sender<Result<(),SamplyBeamError>>>(1);
    let cc = Arc::new(RwLock::new(CertificateCache::new(tx).unwrap()));
    let cc2 = cc.clone();
    tokio::task::spawn(async move {
        while let Some(_sender) = rx.recv().await {
            let mut locked_cache = cc2.write().await;
            let result = locked_cache.update_certificates_mut().await;
            match result {
                Err(e) => {
                    warn!("Unable to inform requesting thread that CertificateCache has been updated. Maybe it stopped? Reason: {e}");
                },
                Ok(count) => {
                    info!("Added {count} new certificates.");
                }
            }
        }
    });
    cc
};

async fn get_cert_by_serial(serial: &str) -> Option<X509>{
    match CertificateCache::get_by_serial(serial).await {
        Some(x) => Some(x.clone()),
        None => None
    }
}

async fn get_all_certs_by_cname(cname: &ProxyId) -> Vec<X509>{
    CertificateCache::get_all_certs_by_cname(cname).await
}

#[derive(Debug, Clone)]
pub struct CryptoPublicPortion {
    pub beam_id: ProxyId,
    pub cert: X509,
    pub pubkey: String,
}

pub async fn get_all_certs_and_clients_by_cname_as_pemstr(cname: &ProxyId) -> Option<Vec<CryptoPublicPortion>> {
    get_all_certs_by_cname(cname).await
        .iter()
        .map(|c| extract_x509(c))
        .collect()
}

pub async fn get_cert_and_client_by_serial_as_pemstr(serial: &str) -> Option<CryptoPublicPortion> {
    let cert = get_cert_by_serial(serial).await;
    if let Some(x) = cert {
        extract_x509(&x)
    } else {
        None
    }
}

pub async fn get_newest_certs_for_cnames_as_pemstr(cnames: Vec<ProxyId>) -> Option<Vec<CryptoPublicPortion>> {
    let mut result: Vec<CryptoPublicPortion> = Vec::new(); // No fancy map/iter, bc of async
    for id in cnames {
        let certs = get_all_certs_and_clients_by_cname_as_pemstr(&id).await;
        if let Some(certificates) = certs {
            if let Some(best_candidate) = get_best_other_certificate(&certificates) {
                result.push(best_candidate);
            }
        };
    }
    (!result.is_empty()).then_some(result)
}

fn extract_x509(cert: &X509) -> Option<CryptoPublicPortion> {
    // Public key
    let pubkey = cert.public_key();
    if pubkey.is_err() {
        error!(?pubkey);
        return None;
    }
    let pubkey = pubkey.unwrap().public_key_to_pem();
    if pubkey.is_err() {
        error!(?pubkey);
        return None;
    }
    let pubkey = std::str::from_utf8(&pubkey.unwrap()).unwrap().to_string();

    let cn = cert.subject_name().entries().next();
    if cn.is_none() {
        return None;
    }
    let verified_sender = cn
        .and_then(|s| Some(s.data()))
        .and_then(|s| match s.as_utf8() {
            Ok(s) => Some(s),
            Err(_) => None
        })
        .and_then(|s| Some(s.to_string()));
    let verified_sender = match verified_sender {
        None => { return None; },
        Some(x) => {
            match ProxyId::new(&x) {
                Ok(x) => x,
                Err(_) => { return None; }
            }
        }
    };
    let cert = cert
        .to_pem()
        .ok()?;
    let cert = X509::from_pem(&cert).ok()?;
    Some(CryptoPublicPortion {
        beam_id: verified_sender,
        cert,
        pubkey,
    })
}

/// Verify whether the certificate is signed by root_ca_cert and the dates are valid
pub fn verify_cert(certificate: &X509, root_ca_cert: &X509) -> Result<(),CertificateInvalidReason> {
    let client_ok = certificate
        .verify(
            root_ca_cert.public_key()
                .map_err(|e| CertificateInvalidReason::InvalidPublicKey)?
        .as_ref())
            .map_err(|e| CertificateInvalidReason::Other(e.to_string()))?;
    let date_ok = x509_date_valid(&certificate)
        .map_err(|_err| CertificateInvalidReason::InvalidDate)?;

    match (client_ok, date_ok) {
        (true, true) => Ok(()), // TODO: Check if actually constant time
        (true, false) => Err(CertificateInvalidReason::InvalidDate),
        (false, _) => Err(CertificateInvalidReason::InvalidPublicKey),
    }
}

pub(crate) fn hash(data: &[u8]) -> Result<[u8; 32],SamplyBeamError> {
    let mut hasher = Sha256::new();
    hasher.update(&data);
    let digest = hasher.finalize();
    let digest: [u8; 32] = digest[0..32].try_into().unwrap();
    Ok(digest)
}

pub fn get_own_privkey() -> &'static RsaPrivateKey {
    &config::CONFIG_SHARED_CRYPTO.get().unwrap().privkey_rsa
}
/* Utility Functions */

/// Extracts the pem-encoded public key from a x509 certificate
fn x509_cert_to_x509_public_key(cert: &X509) -> Result<Vec<u8>, SamplyBeamError> {
    match cert.public_key() {
        Ok(key) => key.public_key_to_pem().or_else(|_| Err(SamplyBeamError::SignEncryptError("Invalid public key in x509 certificate.".into()))),
        Err(_) => Err(SamplyBeamError::SignEncryptError("Unable to extract public key from certificate.".into()))
    }
}

/// Converts the x509 pem-encoded public key to the rsa public key
pub fn x509_public_key_to_rsa_pub_key(cert_key: &Vec<u8>) -> Result<RsaPublicKey, SamplyBeamError> {
    let rsa_key = 
        RsaPublicKey::from_pkcs1_pem(
            std::str::from_utf8(cert_key)
            .or_else(|e| Err(SamplyBeamError::SignEncryptError(format!("Invalid character in certificate public key because {}",e))))?)
        .or_else(|e| Err(SamplyBeamError::SignEncryptError(format!("Can not extract public rsa key from certificate because {}",e))));
    rsa_key
}

/// Convenience function to extract a rsa public key from a x509 certificate. Calls x509_cert_to_x509_public_key and x509_public_key_to_rsa_pub_key internally.
pub fn x509_cert_to_rsa_pub_key(cert: &X509) -> Result<RsaPublicKey, SamplyBeamError> {
    x509_public_key_to_rsa_pub_key(&x509_cert_to_x509_public_key(cert)?)
}

/// Converts the asn.1 time (e.g., from a certificate exiration date) to rust's SystemTime. From https://github.com/sfackler/rust-openssl/issues/1157#issuecomment-1016737160
pub fn asn1_time_to_system_time(time: &Asn1TimeRef) -> Result<SystemTime, ErrorStack> {
    let unix_time = Asn1Time::from_unix(0)?.diff(time)?;
    Ok(SystemTime::UNIX_EPOCH + Duration::from_secs(unix_time.days as u64 * 86400 + unix_time.secs as u64))
}

/// Checks if SystemTime::now() is between the not_before and the not_after dates of a x509 certificate
pub fn x509_date_valid(cert: &X509) -> Result<bool, ErrorStack> {
    let expirydate = asn1_time_to_system_time(cert.not_after())?;
    let startdate = asn1_time_to_system_time(cert.not_before())?;
    let now = SystemTime::now();
    return Ok(expirydate > now && now > startdate);
}

pub fn load_certificates_from_file(ca_file: PathBuf) -> Result<X509,SamplyBeamError> {
    let file = ca_file.as_path();
    let content = std::fs::read(file)
        .map_err(|e| SamplyBeamError::ConfigurationFailed(format!("Can not load certificate {}: {}",file.to_string_lossy(), e)))?;
    let cert = X509::from_pem(&content)
        .map_err(|e| SamplyBeamError::ConfigurationFailed( format!("Unable to read certificate from file {}: {}", file.to_string_lossy(), e)));
    cert
}

pub fn load_certificates_from_dir(ca_dir: Option<PathBuf>) -> Result<Vec<X509>, std::io::Error> {
    let mut result = Vec::new();
    if let Some(ca_dir) = ca_dir {
        for file in ca_dir.read_dir()? { //.map_err(|e| SamplyBeamError::ConfigurationFailed(format!("Unable to read from TLS CA directory {}: {}", ca_dir.to_string_lossy(), e)))
            let path = file?.path();
            let content = std::fs::read(&path)?;
            let cert = X509::from_pem(&content);
            if let Err(e) = cert {
                warn!("Unable to read certificate from file {}: {}", path.to_string_lossy(), e);
                continue;
            }
            result.push(cert.unwrap());
        }
    }
    Ok(result)
}

/// Checks whether or not a x509 certificate matches a private key by comparing the (public) modulus
pub fn is_cert_from_privkey(cert: &X509, key: &RsaPrivateKey) -> Result<bool,ErrorStack>{
    let cert_rsa = cert.public_key()?.rsa()?;
    let cert_mod = cert_rsa.n();
    let key_mod = key.n();
    let key_mod_bignum = openssl::bn::BigNum::from_slice(&key_mod.to_bytes_be())?;
    let is_equal = cert_mod.ucmp(&key_mod_bignum) == std::cmp::Ordering::Equal;
    if ! is_equal {
        match ProxyCertInfo::try_from(cert) {
            Ok(x) => {
                warn!("CA error: Found certificate (serial {}) that does not match private key.", x.serial);
            },
            Err(_) => {
                warn!("CA error: Found a certificate that does not match private key; I cannot even parse it: {:?}", cert);
            }
        };
    }
    return Ok(is_equal);
}

/// Selecs the best fitting certificate from a vector of certs according to:
/// 1) Does it match the private key?
/// 2) Is the current date in the valid date range?
/// 3) Select the newest of the remaining
pub(crate) fn get_best_own_certificate(publics: impl Into<Vec<CryptoPublicPortion>>, private_rsa: &RsaPrivateKey) -> Option<CryptoPublicPortion> {
    let mut publics = publics.into();
    debug!("get_best_certificate(): Considering {} certificates: {:?}", publics.len(), publics);
    publics.retain(|c| is_cert_from_privkey(&c.cert,private_rsa).unwrap_or(false)); // retain certs matching the private cert
    debug!("get_best_certificate(): {} certificates match our private key.", publics.len());
    publics.retain(|c| x509_date_valid(&c.cert).unwrap_or(false)); // retain certs with valid dates
    debug!("get_best_certificate(): After sorting, {} certificates remaining.", publics.len());
    publics.sort_by(|a,b| a.cert.not_before().compare(b.cert.not_before()).expect("Unable to select newest certificate").reverse()); // sort by newest
    publics.first().cloned() // If empty vec --> return None
}

/// Selecs the best fitting certificate from a vector of certs according to:
/// 1) Is the current date in the valid date range?
/// 2) Select the newest of the remaining
pub fn get_best_other_certificate(publics: &Vec<CryptoPublicPortion>) -> Option<CryptoPublicPortion> {
    let mut publics = publics.to_owned();
    publics.retain(|c| x509_date_valid(&c.cert).unwrap_or(false)); // retain certs with valid dates
    publics.sort_by(|a,b| a.cert.not_before().compare(b.cert.not_before()).expect("Unable to select newest certificate").reverse()); // sort by newest
    publics.first().cloned() // If empty vec --> return None
}

pub async fn get_proxy_public_keys(receivers: impl IntoIterator<Item = &AppOrProxyId>) -> Result<Vec<RsaPublicKey>,SamplyBeamError> {
    let proxy_receivers: Vec<ProxyId> = receivers
        .into_iter()
        .map(|app_or_proxy| match app_or_proxy {
            AppOrProxyId::ProxyId(id) => id.to_owned(),
            AppOrProxyId::AppId(id) => id.proxy_id()
        }).collect();
    let receivers_crypto_bundle = crypto::get_newest_certs_for_cnames_as_pemstr(proxy_receivers).await;
    let receivers_keys = match receivers_crypto_bundle {
        Some(vec) => Ok(vec.iter().map(|crypt_publ| rsa::RsaPublicKey::from_public_key_pem(&crypt_publ.pubkey).expect("Cannot collect recipients' public keys")).collect::<Vec<rsa::RsaPublicKey>>()), // TODO Expect
        None => Err(SamplyBeamError::SignEncryptError("Cannot gather encryption keys.".into()))
    }?;
    Ok(receivers_keys)
}