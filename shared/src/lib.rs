#![allow(unused_imports)]

use axum::async_trait;
use beam_id::{AppId, AppOrProxyId, BeamId, ProxyId};
use chacha20poly1305::{
    aead::{Aead, AeadCore, KeyInit, OsRng},
    XChaCha20Poly1305, XNonce,
};
use crypto_jwt::extract_jwt;
use errors::SamplyBeamError;
use itertools::Itertools;
use jwt_simple::prelude::{RS256PublicKey, RSAPublicKeyLike};
use openssl::base64;
use rsa::{PaddingScheme, PublicKey, RsaPrivateKey, RsaPublicKey};
use serde_json::{json, Value};
use sha2::Sha256;
use static_init::dynamic;
use tracing::debug;

use std::{
    fmt::{Debug, Display},
    ops::Deref,
    time::{Duration, Instant, SystemTime},
};

use rand::Rng;
use serde::{
    de::{DeserializeOwned, Visitor},
    Deserialize, Serialize,
};
use std::{collections::HashMap, str::FromStr};
use uuid::Uuid;

use crate::crypto_jwt::JWT_VERIFICATION_OPTIONS;

pub type MsgId = MyUuid;
pub type MsgType = String;
pub type TaskResponse = String;

pub mod crypto;
pub mod crypto_jwt;
pub mod errors;
pub mod logger;
mod traits;

pub mod config;
pub mod config_shared;
// #[cfg(feature = "config-for-broker")]
pub mod config_broker;
// #[cfg(feature = "config-for-proxy")]
pub mod config_proxy;

pub mod beam_id;
pub mod graceful_shutdown;
pub mod http_client;
pub mod middleware;

pub mod examples;

pub mod sse_event;

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash)]
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

#[derive(PartialEq, Eq, Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "lowercase", tag = "status")]
pub enum WorkStatus {
    Claimed,
    TempFailed,
    PermFailed,
    Succeeded,
}

impl Display for WorkStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let str = match self {
            WorkStatus::Claimed => String::from("Claimed"),
            WorkStatus::TempFailed => String::from("Temporary failure"),
            WorkStatus::PermFailed => String::from("Permanent failure"),
            WorkStatus::Succeeded => String::from("Success"),
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

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MsgSigned<M: Msg> {
    #[serde(skip)]
    pub msg: M,
    pub jwt: String,
}

impl<M: Msg + DeserializeOwned> MsgSigned<M> {
    pub async fn verify(token: &str) -> Result<Self, SamplyBeamError> {
        let msg = extract_jwt(token).await?.2.custom;

        debug!("Message has been verified succesfully.");
        Ok(MsgSigned {
            msg,
            jwt: token.to_string(),
        })
    }
}

#[dynamic]
pub static EMPTY_VEC_APPORPROXYID: Vec<AppOrProxyId> = Vec::new();

#[derive(Serialize, Deserialize, Debug)]
#[serde(deny_unknown_fields)]
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

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageType<State>
where
    State: MsgState,
{
    // Maybe add MessageSigned and Encrypted versions
    MsgTaskRequest(MsgTaskRequest<State>),
    MsgTaskResult(MsgTaskResult<State>),
    MsgEmpty(MsgEmpty),
}

pub type PlainMessage = MessageType<Plain>;
pub type EncryptedMessage = MessageType<Encrypted>;

impl EncryptableMsg for PlainMessage {
    type Output = EncryptedMessage;

    fn convert_self(self, body: Encrypted) -> Self::Output {
        match self {
            Self::MsgTaskRequest(m) => Self::Output::MsgTaskRequest(m.convert_self(body)),
            Self::MsgTaskResult(m) => Self::Output::MsgTaskResult(m.convert_self(body)),
            Self::MsgEmpty(m) => Self::Output::MsgEmpty(m),
        }
    }

    fn get_plain(&self) -> &Plain {
        match self {
            Self::MsgTaskRequest(m) => m.get_plain(),
            Self::MsgTaskResult(m) => m.get_plain(),
            Self::MsgEmpty(_) => &Plain { body: None },
        }
    }
}

const MESSAGE_EMPTY_ENCRYPTION: &Encrypted = &Encrypted {
    encrypted: Vec::new(),
    encryption_keys: Vec::new(),
};

impl DecryptableMsg for EncryptedMessage {
    type Output = PlainMessage;

    fn convert_self(self, body: String) -> Self::Output {
        match self {
            Self::MsgTaskRequest(m) => Self::Output::MsgTaskRequest(m.convert_self(body)),
            Self::MsgTaskResult(m) => Self::Output::MsgTaskResult(m.convert_self(body)),
            Self::MsgEmpty(m) => Self::Output::MsgEmpty(m),
        }
    }

    fn get_encryption(&self) -> &Encrypted {
        match self {
            Self::MsgTaskRequest(m) => m.get_encryption(),
            Self::MsgTaskResult(m) => m.get_encryption(),
            Self::MsgEmpty(_) => MESSAGE_EMPTY_ENCRYPTION,
        }
    }
}

impl<T: MsgState> Msg for MessageType<T> {
    fn get_from(&self) -> &AppOrProxyId {
        use MessageType::*;
        match self {
            MsgTaskRequest(m) => m.get_from(),
            MsgTaskResult(m) => m.get_from(),
            MsgEmpty(m) => m.get_from(),
        }
    }

    fn get_to(&self) -> &Vec<AppOrProxyId> {
        use MessageType::*;
        match self {
            MsgTaskRequest(m) => m.get_to(),
            MsgTaskResult(m) => m.get_to(),
            MsgEmpty(m) => m.get_to(),
        }
    }

    fn get_metadata(&self) -> &Value {
        use MessageType::*;
        match self {
            MsgTaskRequest(m) => m.get_metadata(),
            MsgTaskResult(m) => m.get_metadata(),
            MsgEmpty(m) => m.get_metadata(),
        }
    }
}

pub trait DecryptableMsg: Msg + Serialize + Sized {
    type Output: Msg + DeserializeOwned;

    fn get_encryption(&self) -> &Encrypted;
    fn convert_self(self, body: String) -> Self::Output;

    /// Decrypts an encrypted message. Caution: can panic.
    #[allow(clippy::or_fun_call)]
    fn decrypt(
        self,
        my_id: &AppOrProxyId,
        my_priv_key: &RsaPrivateKey,
    ) -> Result<Self::Output, SamplyBeamError> {
        let Encrypted {
            encrypted,
            encryption_keys,
        } = self.get_encryption();
        let to_array_index: usize = self
            .get_to()
            .iter()
            .position(|entry| {
                let entry_str = entry.to_string();

                let mut matched = entry_str.ends_with(&my_id.to_string());
                matched &= match entry_str.find(&my_id.to_string()) {
                    Some(0) => true,                                      // Begins with id
                    Some(i) => entry_str.chars().nth(i - 1) == Some('.'), // Ends with id, but before is a separator (e.g. appId)
                    None => false,
                };
                matched
            }) // TODO remove expect!
            .ok_or(SamplyBeamError::SignEncryptError(
                "Decryption error: This client cannot be found in 'to' list".into(),
            ))?;
        let encrypted_decryption_key = &encryption_keys[to_array_index];

        // Cryptographic Operations
        let cipher_engine = XChaCha20Poly1305::new_from_slice(&my_priv_key.decrypt(
            rsa::PaddingScheme::new_oaep::<sha2::Sha256>(),
            &encrypted_decryption_key,
        )?)
        .map_err(|e| {
            SamplyBeamError::SignEncryptError(format!(
                "Decryption error: Cannot initialize stream cipher because {}",
                e
            ))
        })?;
        let nonce: XNonce = XNonce::clone_from_slice(&encrypted[0..24]);
        let ciphertext = &encrypted[24..];
        let plaintext = String::from_utf8(
            cipher_engine
                .decrypt(&nonce, ciphertext.as_ref())
                .map_err(|e| {
                    SamplyBeamError::SignEncryptError(format!(
                        "Decryption error: Cannot decrypt payload because {}",
                        e
                    ))
                })?,
        )
        .map_err(|e| {
            SamplyBeamError::SignEncryptError(format!(
                "Decryption error: Invalid UTF8 text in decrypted ciphertext {}",
                e
            ))
        })?;

        // self.set_body(plaintext);
        Ok(self.convert_self(plaintext))
    }
}

pub trait EncryptableMsg: Msg + Serialize + Sized {
    type Output: Msg;

    fn convert_self(self, body: Encrypted) -> Self::Output;
    fn get_plain(&self) -> &Plain;

    #[allow(clippy::or_fun_call)]
    fn encrypt(
        self,
        receivers_public_keys: &Vec<RsaPublicKey>,
    ) -> Result<Self::Output, SamplyBeamError> {
        // Generate Symmetric Key and Nonce
        let mut rng = rand::thread_rng();
        let symmetric_key = XChaCha20Poly1305::generate_key(&mut rng);
        let nonce = XChaCha20Poly1305::generate_nonce(&mut rng);

        // Encrypt symmetric key with receivers' public keys
        let (encrypted_keys, err): (Vec<_>, Vec<_>) = receivers_public_keys
            .iter()
            .map(|key| {
                key.encrypt(
                    &mut rng,
                    PaddingScheme::new_oaep::<Sha256>(),
                    symmetric_key.as_slice(),
                )
            })
            .partition_result();
        if !err.is_empty() {
            return Err(SamplyBeamError::SignEncryptError(
                "Encryption error: Cannot encrypt symmetric key".into(),
            ));
        }

        // Encrypt fields content
        let cipher = XChaCha20Poly1305::new(&symmetric_key);

        // I cant belive there is no better way
        let default = String::new();
        let plaintext = self.get_plain().body.as_ref().unwrap_or(&default);

        let mut ciphertext = cipher.encrypt(&nonce, plaintext.as_ref()).or(Err(
            SamplyBeamError::SignEncryptError("Encryption error: Can not encrypt data.".into()),
        ))?;

        // Prepend Nonce to ciphertext
        let mut nonce_and_ciphertext = nonce.to_vec();
        nonce_and_ciphertext.append(&mut ciphertext);

        Ok(self.convert_self(Encrypted {
            encrypted: nonce_and_ciphertext,
            encryption_keys: encrypted_keys,
        }))
    }
}

pub trait Msg: Serialize {
    fn get_from(&self) -> &AppOrProxyId;
    fn get_to(&self) -> &Vec<AppOrProxyId>;
    fn get_metadata(&self) -> &Value;
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

impl<T: MsgState> Msg for MsgTaskRequest<T> {
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

impl<T: MsgState> Msg for MsgTaskResult<T> {
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

mod serialize_time {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use fundu::parse_duration;
    use serde::{self, Deserialize, Deserializer, Serializer};
    use tracing::{debug, error, warn};

    pub fn serialize<S>(time: &SystemTime, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let ttl = match time.duration_since(SystemTime::now()) {
            Ok(v) => v,
            Err(e) => {
                error!("Internal Error: Tried to serialize a task which should have expired and expunged from memory {} seconds ago. Will return TTL=0. Cause: {}", e.duration().as_secs(), e);
                Duration::ZERO
            }
        };
        s.serialize_str(&ttl.as_secs().to_string())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<SystemTime, D::Error>
    where
        D: Deserializer<'de>,
    {
        let duration = &String::deserialize(deserializer)?;
        let ttl = parse_duration(&duration).map_err(serde::de::Error::custom)?;
        let expire = SystemTime::now() + ttl;
        debug!("Deserialized {:?} to time {:?}", duration, expire);
        Ok(expire)
    }
}

pub trait MsgState: Serialize + Eq + PartialEq + Default {}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct Encrypted {
    pub encrypted: Vec<u8>,
    pub encryption_keys: Vec<Vec<u8>>,
}

impl MsgState for Encrypted {}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct Plain {
    pub body: Option<String>,
}

impl MsgState for Plain {}

impl<T: Into<String>> From<T> for Plain {
    fn from(val: T) -> Self {
        Plain {
            body: Some(val.into()),
        }
    }
}

// When const generic enums get stableized this could get beautiful
#[derive(Serialize, Deserialize, Clone)]
pub struct MsgTaskRequest<State = Plain>
where
    State: MsgState,
{
    pub id: MsgId,
    pub from: AppOrProxyId,
    pub to: Vec<AppOrProxyId>,
    #[serde(flatten)]
    pub body: State,
    #[serde(with = "serialize_time", rename = "ttl")]
    pub expire: SystemTime,
    pub failure_strategy: FailureStrategy,
    #[serde(skip)]
    pub results: HashMap<AppOrProxyId, MsgSigned<MsgTaskResult<State>>>,
    pub metadata: Value,
}

//TODO: Implement EncMsg and DecMsg for all message types
impl EncryptableMsg for MsgTaskRequest {
    type Output = MsgTaskRequest<Encrypted>;

    fn convert_self(self, body: Encrypted) -> Self::Output {
        let Self {
            id,
            from,
            to,
            expire,
            failure_strategy,
            metadata,
            ..
        } = self;
        Self::Output {
            body,
            id,
            from,
            to,
            expire,
            failure_strategy,
            metadata,
            results: Default::default(),
        }
    }

    fn get_plain(&self) -> &Plain {
        &self.body
    }
}

impl DecryptableMsg for MsgTaskRequest<Encrypted> {
    type Output = MsgTaskRequest;

    fn convert_self(self, body: String) -> Self::Output {
        let Self {
            id,
            from,
            to,
            expire,
            failure_strategy,
            metadata,
            ..
        } = self;
        Self::Output {
            body: Plain::from(body),
            id,
            from,
            to,
            expire,
            failure_strategy,
            metadata,
            results: Default::default(),
        }
    }

    fn get_encryption(&self) -> &Encrypted {
        &self.body
    }
}

pub type EncryptedMsgTaskRequest = MsgTaskRequest<Encrypted>;
pub type EncryptedMsgTaskResult = MsgTaskResult<Encrypted>;

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct MsgTaskResult<State = Plain>
where
    State: MsgState,
{
    pub from: AppOrProxyId,
    pub to: Vec<AppOrProxyId>,
    pub task: MsgId,
    #[serde(flatten)]
    pub status: WorkStatus,
    #[serde(flatten)]
    pub body: State,
    pub metadata: Value,
}

impl DecryptableMsg for MsgTaskResult<Encrypted> {
    type Output = MsgTaskResult;

    fn convert_self(self, body: String) -> Self::Output {
        let Self {
            from,
            to,
            task,
            status,
            metadata,
            ..
        } = self;
        Self::Output {
            body: Plain::from(body),
            from,
            to,
            task,
            status,
            metadata,
        }
    }

    fn get_encryption(&self) -> &Encrypted {
        &self.body
    }
}

impl EncryptableMsg for MsgTaskResult<Plain> {
    type Output = MsgTaskResult<Encrypted>;

    fn get_plain(&self) -> &Plain {
        &self.body
    }

    fn convert_self(self, body: Encrypted) -> Self::Output {
        let Self {
            from,
            to,
            task,
            status,
            metadata,
            ..
        } = self;
        Self::Output {
            body,
            from,
            to,
            task,
            status,
            metadata,
        }
    }
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

impl HasWaitId<MsgId> for EncryptedMsgTaskRequest {
    fn wait_id(&self) -> MsgId {
        self.id
    }
}

impl HasWaitId<String> for EncryptedMsgTaskResult {
    fn wait_id(&self) -> String {
        format!("{},{}", self.task, self.from)
    }
}

impl<M, I> HasWaitId<I> for MsgSigned<M>
where
    M: HasWaitId<I> + Msg,
    I: PartialEq,
{
    fn wait_id(&self) -> I {
        self.msg.wait_id()
    }
}

impl<T: MsgState> MsgTaskRequest<T> {
    pub fn id(&self) -> &MsgId {
        &self.id
    }
}
impl MsgTaskRequest {
    pub fn new(
        from: AppOrProxyId,
        to: Vec<AppOrProxyId>,
        body: String,
        failure_strategy: FailureStrategy,
        metadata: serde_json::Value,
    ) -> Self {
        MsgTaskRequest {
            id: MsgId::new(),
            from,
            to,
            body: body.into(),
            failure_strategy,
            results: HashMap::new(),
            metadata,
            expire: SystemTime::now() + Duration::from_secs(3600),
        }
    }
}

// Don't compare expire, as it is constantly changing.
// Todo Is the comparison of Results nessecary
impl<T: MsgState> PartialEq for MsgTaskRequest<T> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
            && self.from == other.from
            && self.to == other.to
            && self.body == other.body
            && self.failure_strategy == other.failure_strategy
            && self.results == other.results
            && self.metadata == other.metadata
    }
}
impl<T: MsgState> Eq for MsgTaskRequest<T> {}

#[derive(Debug, Serialize, Deserialize)]
pub struct MsgPing {
    id: MsgId,
    from: AppOrProxyId,
    to: Vec<AppOrProxyId>,
    nonce: [u8; 16],
    metadata: Value,
}

impl MsgPing {
    pub fn new(from: AppOrProxyId, to: AppOrProxyId) -> Self {
        let mut nonce = [0; 16];
        openssl::rand::rand_bytes(&mut nonce)
            .expect("Critical Error: Failed to generate random byte array.");
        MsgPing {
            id: MsgId::new(),
            from,
            to: vec![to],
            nonce,
            metadata: json!(null),
        }
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

#[cfg(test)]
mod tests {
    use crate::beam_id::BrokerId;

    use super::*;

    #[test]
    fn encrypt_decrypt_task() {
        //Create Task
        AppId::set_broker_id("broker.samply.de".to_string());
        let p1_id = AppOrProxyId::AppId(AppId::new("app.proxy1.broker.samply.de").unwrap());
        let p2_id = AppOrProxyId::AppId(AppId::new("app.proxy2.broker.samply.de").unwrap());
        let from = p1_id.clone();
        let to = vec![p1_id.clone(), p2_id.clone()];
        let expiry = SystemTime::now() + Duration::from_secs(60);
        let failure = FailureStrategy::Discard;
        let msg = MsgTaskRequest {
            id: MsgId::new(),
            from,
            to,
            body: "Testbody".into(),
            expire: expiry,
            failure_strategy: failure,
            results: HashMap::new(),
            metadata: "".into(),
        };

        //Setup Keypairs
        let mut rng = rand::thread_rng();
        let rsa_length: usize = 2048;
        let p1_private = RsaPrivateKey::new(&mut rng, rsa_length)
            .expect("Failed to generate private key for proxy 1");
        let p2_private = RsaPrivateKey::new(&mut rng, rsa_length)
            .expect("Failed to generate private key for proxy 2");
        let p1_public = RsaPublicKey::from(&p1_private);
        let p2_public = RsaPublicKey::from(&p2_private);

        // Encrypt Message
        let receivers_public_keys = vec![p1_public, p2_public];
        let msg_encr = msg
            .clone()
            .encrypt(&receivers_public_keys)
            .expect("Could not encrypt message");
        // Decrypt for both proxies
        let msg_p1_decr = msg_encr
            .clone()
            .decrypt(&p1_id, &p1_private)
            .expect("Cannot decrypt message");
        let msg_p2_decr = msg_encr
            .decrypt(&p2_id, &p2_private)
            .expect("Cannot decrypt message");

        assert_eq!(msg_p1_decr, msg_p2_decr);
        assert_eq!(msg, msg_p1_decr);
    }

    #[test]
    fn encrypt_decrypt_result() {
        AppId::set_broker_id("broker.samply.de".to_string());
        let p1_id = AppOrProxyId::AppId(AppId::new("app.proxy1.broker.samply.de").unwrap());
        let p2_id = AppOrProxyId::AppId(AppId::new("app.proxy2.broker.samply.de").unwrap());
        let from = p1_id.clone();
        let to = vec![p1_id.clone(), p2_id.clone()];
        let status = WorkStatus::Succeeded;
        let msg = MsgTaskResult {
            from,
            to,
            task: MsgId::new(),
            status,
            body: "The result is 55!".into(),
            metadata: "".into(),
        };

        //Setup Keypairs
        let mut rng = rand::thread_rng();
        let rsa_length: usize = 2048;
        let p1_private = RsaPrivateKey::new(&mut rng, rsa_length)
            .expect("Failed to generate private key for proxy 1");
        let p2_private = RsaPrivateKey::new(&mut rng, rsa_length)
            .expect("Failed to generate private key for proxy 2");
        let p1_public = RsaPublicKey::from(&p1_private);
        let p2_public = RsaPublicKey::from(&p2_private);

        // Encrypt Message
        let receivers_public_keys = vec![p1_public, p2_public];
        let msg_encr = msg
            .clone()
            .encrypt(&receivers_public_keys)
            .expect("Could not encrypt message");
        // Decrypt for both proxies
        let msg_p1_decr = msg_encr
            .clone()
            .decrypt(&p1_id, &p1_private)
            .expect("Cannot decrypt message");
        let msg_p2_decr = msg_encr
            .clone()
            .decrypt(&p2_id, &p2_private)
            .expect("Cannot decrypt message");

        assert_eq!(msg_p1_decr, msg_p2_decr);
        assert_eq!(msg, msg_p1_decr);
    }
}

impl<T: MsgState + Debug> Debug for MsgTaskRequest<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncryptedMsgTaskRequest")
            .field("id", &self.id)
            .field("from", &self.from)
            .field("to", &self.to)
            .field("body", &self.body)
            .field("expire", &self.expire)
            .field("failure_strategy", &self.failure_strategy)
            .field("metadata", &self.metadata)
            .finish()
    }
}

impl<T: MsgState + Debug> Debug for MsgTaskResult<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncryptedMsgTaskResult")
            .field("from", &self.from)
            .field("to", &self.to)
            .field("task", &self.task)
            .field("status", &self.status)
            .field("body", &self.body)
            .field("metadata", &self.metadata)
            .finish()
    }
}
