#![allow(unused_imports)]

use beam_id::{BeamId, AppId, AppOrProxyId};
use crypto_jwt::extract_jwt;
use errors::SamplyBeamError;
use serde_json::{Value, json};
use sha2::Sha256;
use static_init::dynamic;
use tracing::debug;
//use aes_gcm::{NewAead, aead::Aead, Aes256Gcm};
use chacha20poly1305::{aead::{AeadCore, KeyInit, OsRng, Aead}, XChaCha20Poly1305, XNonce};
use itertools::Itertools;
use rsa::{RsaPrivateKey, RsaPublicKey, PaddingScheme, PublicKey};

use std::{time::{Duration, Instant, SystemTime}, ops::Deref, fmt::Display};

use rand::Rng;
use serde::{Deserialize, Serialize, de::{Visitor, DeserializeOwned}};
use std::{collections::HashMap, str::FromStr};
use uuid::Uuid;

pub type MsgId = MyUuid;
pub type MsgType = String;
pub type TaskResponse = String;

mod traits;
pub mod logger;
pub mod crypto;
pub mod crypto_jwt;
pub mod errors;

pub mod config;
pub mod config_shared;
// #[cfg(feature = "config-for-broker")]
pub mod config_broker;
// #[cfg(feature = "config-for-proxy")]
pub mod config_proxy;

pub mod middleware;
pub mod http_proxy;
pub mod beam_id;

pub mod examples;

#[derive(Debug,Serialize,Deserialize,Clone,Copy,PartialEq,Eq,Hash)]
pub struct MyUuid(Uuid);
impl MyUuid {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}
impl Default for MyUuid {
    fn default() -> Self {
        Self::new()
    }
}
impl Deref for MyUuid {
    type Target = Uuid;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl From<Uuid> for MyUuid {
    fn from(uuid: Uuid) -> Self {
        MyUuid(uuid)
    }
}
impl TryFrom<&str> for MyUuid {
    type Error = uuid::Error;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        let parsed = Uuid::from_str(value)?;
        Ok(Self(parsed))
    }
}

impl Display for MyUuid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(PartialEq, Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "lowercase", tag = "status", content = "body")]
pub enum WorkStatus {
    Claimed,
    TempFailed(TaskResponse),
    PermFailed(TaskResponse),
    Succeeded(TaskResponse),
}

impl Display for WorkStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let str = match self {
            WorkStatus::Claimed => String::from("Claimed"),
            WorkStatus::TempFailed(e) => format!("Temporary failure: {e}"),
            WorkStatus::PermFailed(e) => format!("Permanent failure: {e}"),
            WorkStatus::Succeeded(e) => format!("Success: {e}"),
        };
        f.write_str(&str)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum FailureStrategy {
    Discard,
    Retry {
        backoff_millisecs: usize,
        max_tries: usize,
    }, // backoff for Duration and try max. times
}

#[derive(Serialize, Deserialize, Debug)]
pub struct HowLongToBlock {
    pub wait_time: Option<Duration>,
    pub wait_count: Option<u16>,
}

#[derive(Clone,Debug,Serialize,Deserialize, PartialEq)]
pub struct MsgSigned<M: Msg> {
    pub msg: M,
    pub sig: String
}

impl<M: Msg> MsgSigned<M> {
    pub async fn verify(&self) -> Result<(), SamplyBeamError> {
        // Signature valid?
        let (proxy_public_info, _, content) 
            = extract_jwt(&self.sig).await?;

        // Message content matches token?
        let val = serde_json::to_value(&self.msg)
            .expect("Internal error: Unable to interpret already parsed message to JSON Value.");
        if content.custom != val {
            return Err(SamplyBeamError::RequestValidationFailed("content.custom did not match parsed message.".to_string()));
        }

        // From field matches CN in certificate?
        if ! self.get_from().can_be_signed_by(&proxy_public_info.beam_id) {
            return Err(SamplyBeamError::RequestValidationFailed(format!("{} is not allowed to sign for {}", &proxy_public_info.beam_id, self.get_from())));
        }
        debug!("Message has been verified succesfully.");
        Ok(())
    }
}

#[dynamic]
pub static EMPTY_VEC_APPORPROXYID: Vec<AppOrProxyId> = Vec::new();

#[derive(Serialize,Deserialize,Debug)]
pub struct MsgEmpty {
    pub from: AppOrProxyId,
}

impl Msg for MsgEmpty {
    fn get_from(&self) -> &AppOrProxyId {
        &self.from
    }

    fn get_to(&self) -> &Vec<AppOrProxyId> {
        &EMPTY_VEC_APPORPROXYID
    }

    fn get_metadata(&self) -> &Value {
        &json!(null)
    }
}

trait EncMsg<M>: Msg + Serialize where M: Msg + DeserializeOwned{
    /// Dectypts an encrypted message. Caution: can panic.
    fn decrypt(&self, my_id: &AppOrProxyId, my_priv_key: &RsaPrivateKey) -> Result<M,SamplyBeamError> {
        // JSON parsing
        let binding = serde_json::to_value(self)
            .map_err(|e|SamplyBeamError::SignEncryptError(format!("Decryption error: Cannot deserialize message because {}",e)))?;
        let mut encrypted_json = binding.as_object()
            .ok_or(SamplyBeamError::SignEncryptError("Decryption error: Cannot deserialize message".into()))?
            .to_owned();
        let encrypted_field = &mut encrypted_json
            .remove("encrypted")
            .ok_or(SamplyBeamError::SignEncryptError("Decryption error: No encrypted payload found".into()))?;
        let encrypted = encrypted_field.as_str()
            .ok_or(SamplyBeamError::SignEncryptError("Decryption error: Encrypted payload not readable".into()))?
            .as_bytes();
        let to_array_index: usize = encrypted_json.get("to").ok_or(SamplyBeamError::SignEncryptError("Decryption error: 'to' field not readable".into()))?
            .as_array().ok_or(SamplyBeamError::SignEncryptError("Decryption error: Cannot get adressee array".into()))?
            .iter()
            .position(|entry| entry.as_str().expect("Decryption error: Cannot parse 'to' entries") == my_id.to_string())// TODO remove expect!
            .ok_or(SamplyBeamError::SignEncryptError("Decryption error: This client cannot be found in 'to' list".into()))?;
        let encrypted_decryption_keys = &mut encrypted_json.remove("encryption_keys")
            .ok_or(SamplyBeamError::SignEncryptError("Decryption error: Cannot read 'encryption_keys' field".into()))?;
        let encrypted_decryption_key = encrypted_decryption_keys
            .as_array()
            .ok_or(SamplyBeamError::SignEncryptError("Decryption error: Cannot read 'encrypted_keys' array".into()))?
            [to_array_index].as_str()
            .ok_or(SamplyBeamError::SignEncryptError("Decryption error: Encryption key is not readable".into()))?;
        // Cryptographic Operations
        let cipher_engine = XChaCha20Poly1305::new_from_slice(&my_priv_key.decrypt(rsa::PaddingScheme::new_oaep::<sha2::Sha256>(), &encrypted_decryption_key.as_bytes())?)
            .map_err(|e| SamplyBeamError::SignEncryptError(format!("Decryption error: Cannot initialize stream cipher because {}",e)))?;
        let nonce: XNonce = XNonce::clone_from_slice(&encrypted[0..24]);
        let ciphertext = &encrypted[24..];
        let plaintext = cipher_engine.decrypt(&nonce, ciphertext.as_ref())
            .map_err(|e|SamplyBeamError::SignEncryptError(format!("Decryption error: Cannot decrypt payload because {}",e)))?;
        //JSON Reassembling
        let mut decrypted_json = encrypted_json; // The "encrypted" field was removed earlier
        let decrypted_elements = serde_json::to_value(plaintext)
            .map_err(|e|SamplyBeamError::SignEncryptError(format!("Decryption error: Decrypted plaintext invalid because {}",e)))?;
        let decrypted_elements = decrypted_elements.as_object()
            .ok_or(SamplyBeamError::SignEncryptError("Decryption error: Decrypted plaintext invalid".into()))?;
        for (key, value) in decrypted_elements.to_owned() {
            _ = decrypted_json.insert(key, value).ok_or(SamplyBeamError::SignEncryptError("Decryption error: Cannot reassemble decrypted task".into()))?;
        }
        let result: M = serde_json::from_value(serde_json::Value::from(decrypted_json)).or(Err(SamplyBeamError::SignEncryptError("Decryption error: Cannot deserialize message".into())))?;
        Ok(result)
    }
}

trait DecMsg<M>: Msg + Serialize where M: Msg + DeserializeOwned {
    fn encrypt(&self, fields_to_encrypt: &Vec<&str>, reciever_public_keys: &Vec<RsaPublicKey>) -> Result<M, SamplyBeamError> {
        let mut rng = rand::thread_rng();
        let symmetric_key = XChaCha20Poly1305::generate_key(&mut rng);
        let nonce = XChaCha20Poly1305::generate_nonce(&mut rng);

        let binding = serde_json::to_value(&self)
            .or_else(|e|Err(SamplyBeamError::SignEncryptError(format!("Cannot deserialize message: {}",e))))?;
        let cleartext_json = binding
            .as_object()
            .ok_or(SamplyBeamError::SignEncryptError("Cannot deserialize message".into()))?.to_owned();
        
        
        let (encrypted_keys,err): (Vec<_>,Vec<_>) = reciever_public_keys.iter()
            .map(|key| key.encrypt(&mut rng, PaddingScheme::new_oaep::<Sha256>(), symmetric_key.as_slice()))
            .partition_result();
        if !err.is_empty() {
            return Err(SamplyBeamError::SignEncryptError("Encryption error: Cannot encrypt symmetric key".into()));
        }
        
        let mut json_to_encrypt = cleartext_json.clone();
        json_to_encrypt.retain(|k,_| fields_to_encrypt.contains(&k.as_str()));
        let mut encrypted_json = cleartext_json;
        for f in fields_to_encrypt {
            _ = encrypted_json.remove(*f);
        }

        encrypted_json.insert(String::from("encryption_keys"), serde_json::Value::from(encrypted_keys));

        let cipher = XChaCha20Poly1305::new(&symmetric_key);
        let plain_value = serde_json::Value::from(json_to_encrypt);
        let plaintext = plain_value.as_str().ok_or(SamplyBeamError::SignEncryptError("Encryption error: Cannot encrypt data".into()))?.as_bytes();
        let ciphertext = cipher.encrypt(&nonce, plaintext.as_ref()).or(Err(SamplyBeamError::SignEncryptError("Encryption error: Can not encrypt data.".into())))?;
        
        encrypted_json.insert(String::from("encrypted"), serde_json::Value::from(ciphertext));

        let result: M = serde_json::from_value(serde_json::Value::from(encrypted_json)).or(Err(SamplyBeamError::SignEncryptError("Encryption error: Cannot deserialize message".into())))?;


        Ok(result)
    }

}

pub trait Msg: Serialize {
    fn get_from(&self) -> &AppOrProxyId;
    fn get_to(&self) -> &Vec<AppOrProxyId>;
    fn get_metadata(&self) -> &Value;
}

pub trait MsgWithBody : Msg{
    // fn get_body(&self) -> &str;
}
impl MsgWithBody for MsgTaskRequest {
    // fn get_body(&self) -> &str {
    //     &self.body
    // }
}
impl MsgWithBody for MsgTaskResult {
    // fn get_body(&self) -> &str {
    //     self.get_body()
    // }
}

impl<M: Msg> Msg for MsgSigned<M> {
    fn get_from(&self) -> &AppOrProxyId {
        self.msg.get_from()
    }

    fn get_to(&self) -> &Vec<AppOrProxyId> {
        self.msg.get_to()
    }

    fn get_metadata(&self) -> &Value {
        self.msg.get_metadata()
    }
}

impl Msg for MsgTaskRequest {
    fn get_from(&self) -> &AppOrProxyId {
        &self.from
    }

    fn get_to(&self) -> &Vec<AppOrProxyId> {
        &self.to
    }

    fn get_metadata(&self) -> &Value {
        &self.metadata
    }
}

impl Msg for MsgTaskResult {
    fn get_from(&self) -> &AppOrProxyId {
        &self.from
    }

    fn get_to(&self) -> &Vec<AppOrProxyId> {
        &self.to
    }

    fn get_metadata(&self) -> &Value {
        &self.metadata
    }
}

// impl From<MsgSigned<MsgTaskRequest>> for MsgTaskRequest {
//     fn from(x: MsgSigned<MsgTaskRequest>) -> Self {
//         x.msg
//     }
// }

// impl From<MsgSigned<MsgTaskResult>> for MsgTaskResult {
//     fn from(x: MsgSigned<MsgTaskResult>) -> Self {
//         x.msg
//     }
// }

mod serialize_time {
    use std::{time::{SystemTime, UNIX_EPOCH, Duration}};

    use serde::{self, Deserialize, Deserializer, Serializer};
    use tracing::{warn, debug, error};


    pub fn serialize<S>(time: &SystemTime, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let ttl = match time.duration_since(SystemTime::now()) {
            Ok(v) => v,
            Err(e) => {
                error!("Internal Error: Tried to serialize a task which should have expired and expunged from memory {} seconds ago. Will return TTL=0. Cause: {}", e.duration().as_secs(), e);
                Duration::ZERO
            },
        };
        s.serialize_u64(ttl.as_secs())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<SystemTime, D::Error>
    where
        D: Deserializer<'de>,
    {
        let ttl: u64 = u64::deserialize(deserializer)?;
        let expire = SystemTime::now() + Duration::from_secs(ttl);
        debug!("Deserialized u64 {} to time {:?}", ttl, expire);
        Ok(
            expire
        )
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct MsgTaskRequest {
    pub id: MsgId,
    pub from: AppOrProxyId,
    pub to: Vec<AppOrProxyId>,
    pub body: String,
    #[serde(with="serialize_time", rename="ttl")]
    pub expire: SystemTime,
    pub failure_strategy: FailureStrategy,
    #[serde(skip)]
    pub results: HashMap<AppOrProxyId,MsgSigned<MsgTaskResult>>,
    pub metadata: Value,
}
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct EncryptedMsgTaskRequest {
    pub id: MsgId,
    pub from: AppOrProxyId,
    pub to: Vec<AppOrProxyId>,
    //auth
    pub body: Option<String>,
    // pub expire: Instant,
    pub failure_strategy: Option<FailureStrategy>,
    pub encrypted: String,
    pub encryption_keys: Vec<Option<String>>,
    #[serde(skip)]
    pub results: HashMap<AppOrProxyId,MsgTaskResult>,
}

//TODO: Implement EncMsg and DecMsg for all message types
//impl<MsgTaskRequest> EncMsg<MsgTaskRequest> for EncryptedMsgTaskRequest{}
//impl<EncryptedMsgTaskRequest> DecMsg<EncryptedMsgTaskRequest> for MsgTaskRequest{}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct MsgTaskResult {
    pub from: AppOrProxyId,
    pub to: Vec<AppOrProxyId>,
    pub task: MsgId,
    #[serde(flatten)]
    pub status: WorkStatus,
    pub metadata: Value,
}

pub trait HasWaitId<I: PartialEq> {
    fn wait_id(&self) -> I;
}

impl HasWaitId<MsgId> for MsgTaskRequest {
    fn wait_id(&self) -> MsgId {
        self.id
    }
}

impl HasWaitId<String> for MsgTaskResult {
    fn wait_id(&self) -> String {
        format!("{},{}", self.task, self.from)
    }
}

impl<M> HasWaitId<MsgId> for MsgSigned<M> where M: HasWaitId<MsgId> + Msg {
    fn wait_id(&self) -> MsgId {
        self.msg.wait_id()
    }
}

impl<M> HasWaitId<String> for MsgSigned<M> where M: HasWaitId<String> + Msg {
    fn wait_id(&self) -> String {
        self.msg.wait_id()
    }
}

impl MsgTaskRequest {
    pub fn id(&self) -> &MsgId {
        &self.id
    }

    pub fn new(
        from: AppOrProxyId,
        to: Vec<AppOrProxyId>,
        body: String,
        failure_strategy: FailureStrategy,
        metadata: serde_json::Value
    ) -> Self {
        MsgTaskRequest {
            id: MsgId::new(),
            from,
            to,
            body,
            failure_strategy,
            results: HashMap::new(),
            metadata,
            expire: SystemTime::now() + Duration::from_secs(3600)
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MsgPing {
    id: MsgId,
    from: AppOrProxyId,
    to: Vec<AppOrProxyId>,
    nonce: [u8; 16],
    metadata: Value
}

impl MsgPing {
    pub fn new(from: AppOrProxyId, to: AppOrProxyId) -> Self {
        let mut nonce = [0;16];
        openssl::rand::rand_bytes(&mut nonce)
            .expect("Critical Error: Failed to generate random byte array.");
        MsgPing { id: MsgId::new(), from, to: vec![to], nonce, metadata: json!(null) }
    }
}

impl Msg for MsgPing {
    fn get_from(&self) -> &AppOrProxyId {
        &self.from
    }

    fn get_to(&self) -> &Vec<AppOrProxyId> {
        &self.to
    }

    fn get_metadata(&self) -> &Value {
        &self.metadata
    }
}

pub fn try_read<T>(map: &HashMap<String, String>, key: &str) -> Option<T>
where
    T: FromStr,
{
    map.get(key).and_then(|value| match value.parse() {
        Ok(v) => Some(v),
        Err(_) => None,
    })
}