#![allow(unused_imports)]

use crypto_jwt::extract_jwt;
use errors::SamplyBrokerError;
use static_init::dynamic;
use tracing::debug;
use aes_gcm::{NewAead, aead::Aead, Aes256Gcm};
use rsa::{RsaPrivateKey, RsaPublicKey, PaddingScheme, PublicKey};

use std::{time::Duration, ops::Deref, fmt::Display};

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
// #[cfg(feature = "config-for-central")]
pub mod config_central;
// #[cfg(feature = "config-for-proxy")]
pub mod config_proxy;

pub mod middleware;
pub mod http_proxy;

#[derive(Serialize,Debug,Clone,Eq,Hash,PartialEq)]
#[serde(transparent)]
pub struct ClientId {
    id: String,
}

impl ClientId {
    pub fn new(id: &str) -> Result<Self, SamplyBrokerError> {
        if Self::is_valid_id_str(id) {
            Ok(Self { id: id.into() })
        } else {
            Err(SamplyBrokerError::InvalidClientIdString(id.into()))
        }
    }

    pub fn random() -> Self {
        const LENGTH: u8 = 8;
        const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
        const SUFFIX: &str = ".randomclientid";
        let mut rng = rand::thread_rng();
        let mut random_id: String = (0..=LENGTH)
            .map(|_| {
                let idx = rng.gen_range(0..CHARSET.len());
                CHARSET[idx] as char
            })
            .collect();
        random_id.push_str(SUFFIX);
        ClientId::new(&random_id)
            .expect("Internal Error: ClientId::random() generated invalid client id. This should not happen")
    }

    fn is_valid_id_str(id: &str) -> bool {
        if ! id.contains('.') {
            return false;
        }
        for char in id.chars() {
            if !(char.is_alphanumeric() || char == '.' || char == '-'){
                return false;
            }
        }
        true
    }
}

impl Display for ClientId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.id)
    }
}

impl Default for ClientId {
    fn default() -> Self {
        Self::random()
    }
}

impl TryFrom<String> for ClientId {
    type Error = SamplyBrokerError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(&value)
    }
}

impl TryFrom<&str> for ClientId {
    type Error = SamplyBrokerError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl<'de> Deserialize<'de> for ClientId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de> {
        deserializer.deserialize_str(ClientIdVisitor)
    }
}

struct ClientIdVisitor;

impl<'de> Visitor<'de> for ClientIdVisitor {
    type Value = ClientId;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(formatter, "string of lower-case letters and/or numbers and at least one '.' separator")
    }

    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        ClientId::new(v)
            .map_err(|_| serde::de::Error::custom("Invalid client ID string"))
    }
}

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
#[serde(rename_all = "lowercase")]
pub enum WorkResult {
    Unclaimed,
    TempFailed(TaskResponse),
    PermFailed(TaskResponse),
    Succeeded(TaskResponse),
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
    pub poll_timeout: Option<Duration>,
    pub poll_count: Option<u16>,
}

#[derive(Clone,Debug,Serialize,Deserialize, PartialEq)]
pub struct MsgSigned<M: Msg> {
    pub msg: M,
    pub sig: String
}

impl<M: Msg> MsgSigned<M> {
    pub async fn verify(&self) -> Result<(), SamplyBrokerError> {
        // Signature valid?
        let (public, _, content) 
            = extract_jwt(&self.sig).await?;

        // Message content matches token?
        let val = serde_json::to_value(&self.msg)
        .expect("Internal error: Unable to interpret already parsed message to JSON Value.");
        if content.custom != val {
            return Err(SamplyBrokerError::RequestValidationFailed);
        }

        // From field matches CN in certificate?
        if public.client != *self.get_from() {
            return Err(SamplyBrokerError::RequestValidationFailed);
        }
        debug!("Message has been verified succesfully.");
        Ok(())
    }
}

#[dynamic]
pub static EMPTY_VEC_CLIENTID: Vec<ClientId> = Vec::new();

#[derive(Serialize,Deserialize,Debug)]
pub struct MsgEmpty {
    pub id: MsgId,
    pub from: ClientId,
}

impl Msg for MsgEmpty {
    fn get_id(&self) -> &MsgId {
        &self.id
    }

    fn get_from(&self) -> &ClientId {
        &self.from
    }

    fn get_to(&self) -> &Vec<ClientId> {
        &EMPTY_VEC_CLIENTID
    }
}

trait EncMsg<M>: Msg + Serialize where M: Msg + DeserializeOwned{
    /// Dectypts an encrypted message. Caution: can panic.
    fn decrypt(&self, my_id: &ClientId, my_priv_key: &RsaPrivateKey) -> Result<M,SamplyBrokerError> {
        // JSON parsing
        let mut encrypted_json = serde_json::to_value(&self).or_else(|_|Err(SamplyBrokerError::SignEncryptError("Decryption error: Cannot deserialize message.")))?
            .as_object().ok_or(SamplyBrokerError::SignEncryptError("Decryption error: Cannot deserialize message."))?;
        let encrypted = encrypted_json.remove("encrypted").ok_or(SamplyBrokerError::SignEncryptError("Decryption error: No encrypted payload found."))?
            .as_str().ok_or(SamplyBrokerError::SignEncryptError("Decryption error: Encrypted payload not readable."))?
            .as_bytes();
        let to_array_index: usize = encrypted_json.get("to").ok_or(SamplyBrokerError::SignEncryptError("Decryption error: 'to' field not readable."))?
            .as_array().ok_or(SamplyBrokerError::SignEncryptError("Decryption error: Cannot get adressee array."))?
            .iter()
            .position(|&entry| entry.as_str().expect("Decryption error: Cannot parse 'to' entries") == my_id.to_string()).ok_or(SamplyBrokerError::SignEncryptError("Decryption error: This client cannot be found in 'to' list."))?;
        let encrypted_decryption_key = encrypted_json.remove("encryption_keys").ok_or(SamplyBrokerError::SignEncryptError("Decryption error: Cannot read 'encryption_keys' field."))?
            .as_array().ok_or(SamplyBrokerError::SignEncryptError("Decryption error: Cannot read 'encrypted_keys' array."))?
            [to_array_index].as_str().ok_or(SamplyBrokerError::SignEncryptError("Decryption error: Encryption key is not readable."))?;
        // Cryptographic Operations
        let decryption_key = aes_gcm::Key::from_slice(&my_priv_key.decrypt(rsa::PaddingScheme::new_oaep::<sha2::Sha256>(), &encrypted_decryption_key.as_bytes())?);
        let nonce = aes_gcm::Nonce::from_slice(&encrypted[0..12]);
        let ciphertext = &encrypted[12..];
        let cipher = aes_gcm::Aes256Gcm::new(decryption_key);
        let plaintext = cipher.decrypt(nonce, ciphertext).map_err(|_|SamplyBrokerError::SignEncryptError("Decryption error: Cannot decrypt payload."))?;
        //JSON Reassembling
        let mut decrypted_json = encrypted_json;
        let decrypted_elements = serde_json::to_value(plaintext).map_err(|_|SamplyBrokerError::SignEncryptError("Decryption error: Decrypted plaintext invalid."))?
            .as_object().ok_or(SamplyBrokerError::SignEncryptError("Decryption error: Decrypted plaintext invalid."))?;
        for (key, value) in decrypted_elements {
            _ = decrypted_json.insert(*key, *value).ok_or(SamplyBrokerError::SignEncryptError("Decryption error: Cannot reassemble decrypted task."))?;
        }
        let result: M = serde_json::from_value(serde_json::Value::from(*decrypted_json)).or(Err(SamplyBrokerError::SignEncryptError("Decryption error: Cannot deserialize message")))?;
        Ok(result)
    }
}

trait DecMsg<M>: Msg + Serialize where M: Msg + DeserializeOwned {
    fn encrypt(&self, fields_to_encrypt: &Vec<&str>, reciever_public_keys: &Vec<RsaPublicKey>) -> Result<M, SamplyBrokerError> {
        let mut symmetric_key = [0;256];
        let mut nonce = [0;12];
        openssl::rand::rand_bytes(&mut symmetric_key).or_else(|_| Err(SamplyBrokerError::SignEncryptError("Encryption error: Cannot create symmetric key.")))?;
        openssl::rand::rand_bytes(&mut nonce).or_else(|_| Err(SamplyBrokerError::SignEncryptError("Encryption error: Cannot create nonce.")))?;

        let mut cleartext_json = serde_json::to_value(&self).or_else(|_|Err(SamplyBrokerError::SignEncryptError("Cannot deserialize message")))?
            .as_object().ok_or(SamplyBrokerError::SignEncryptError("Cannot deserialize message."))?;
        
        let mut rng = rand::thread_rng();
        let mut encrypted_keys = Vec::new();
        let encrypted_keys: Vec<String> = reciever_public_keys.iter()
            .encrypt(&mut rng, PaddingScheme::new_oaep(), &symmetric_key).or_else(|_| Err(SamplyBrokerError::SignEncryptError("Encryption error: Cannot encrypt symmetric key")))
            .collect();
        
        let mut json_to_encrypt = cleartext_json.clone();
        json_to_encrypt.retain(|k,_| fields_to_encrypt.contains(&k.as_str()));
        let mut encrypted_json = *cleartext_json;
        for f in fields_to_encrypt {
            _ = encrypted_json.remove(f);
        }

        encrypted_json.insert(String::from("encryption_keys"), serde_json::Value::from(encrypted_keys));

        let cipher = Aes256Gcm::new(aes_gcm::Key::from_slice(&symmetric_key));
        let plaintext = serde_json::Value::from(json_to_encrypt).as_str().ok_or(SamplyBrokerError::SignEncryptError("Encryption error: Cannot encrypt data."))?.as_bytes();
        let ciphertext = cipher.encrypt(aes_gcm::Nonce::from_slice(&nonce), plaintext).or(Err(SamplyBrokerError::SignEncryptError("Encryption error: Can not encrypt data.")))?;
        
        encrypted_json.insert(String::from("encrypted"), serde_json::Value::from(ciphertext));

        let result: M = serde_json::from_value(serde_json::Value::from(encrypted_json)).or(Err(SamplyBrokerError::SignEncryptError("Encryption error: Cannot deserialize message")))?;


        Ok(result)
    }

}

pub trait Msg: Serialize {
    fn get_id(&self) -> &MsgId;
    fn get_from(&self) -> &ClientId;
    fn get_to(&self) -> &Vec<ClientId>;
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
    fn get_id(&self) -> &MsgId {
        self.msg.get_id()
    }

    fn get_from(&self) -> &ClientId {
        self.msg.get_from()
    }

    fn get_to(&self) -> &Vec<ClientId> {
        self.msg.get_to()
    }
}

impl Msg for MsgTaskRequest {
    fn get_id(&self) -> &MsgId {
        &self.id
    }

    fn get_from(&self) -> &ClientId {
        &self.from
    }

    fn get_to(&self) -> &Vec<ClientId> {
        &self.to
    }
}

impl Msg for MsgTaskResult {
    fn get_id(&self) -> &MsgId {
        &self.id
    }

    fn get_from(&self) -> &ClientId {
        &self.from
    }

    fn get_to(&self) -> &Vec<ClientId> {
        &self.to
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

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct MsgTaskRequest {
    pub id: MsgId,
    pub from: ClientId,
    pub to: Vec<ClientId>,
    pub task_type: MsgType,
    pub body: String,
    // pub expire: SystemTime,
    pub failure_strategy: FailureStrategy,
    #[serde(skip)]
    pub results: HashMap<ClientId,MsgSigned<MsgTaskResult>>,
}
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct EncryptedMsgTaskRequest {
    pub id: MsgId,
    pub from: ClientId,
    pub to: Vec<ClientId>,
    //auth
    pub task_type: Option<MsgType>,
    pub body: Option<String>,
    // pub expire: SystemTime,
    pub failure_strategy: Option<FailureStrategy>,
    pub encrypted: String,
    pub encryption_keys: Vec<Option<String>>,
    #[serde(skip)]
    pub results: HashMap<ClientId,MsgTaskResult>,
}

//TODO: Implement EncMsg and DecMsg for all message types
//impl<MsgTaskRequest> EncMsg<MsgTaskRequest> for EncryptedMsgTaskRequest{}
//impl<EncryptedMsgTaskRequest> DecMsg<EncryptedMsgTaskRequest> for MsgTaskRequest{}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct MsgTaskResult {
    pub id: MsgId,
    pub from: ClientId, // was: worker_id
    pub to: Vec<ClientId>,
    pub task: MsgId,
    pub result: WorkResult,
}

pub trait HasWaitId<T> {
    fn get_wait_id(&self) -> T;
}

impl HasWaitId<MsgId> for MsgTaskRequest {
    fn get_wait_id(&self) -> MsgId {
        self.id
    }
}

impl HasWaitId<MsgId> for MsgTaskResult {
    fn get_wait_id(&self) -> MsgId {
        self.task
    }
}

impl<M> HasWaitId<MsgId> for MsgSigned<M> where M: HasWaitId<MsgId> + Msg {
    fn get_wait_id(&self) -> MsgId {
        self.msg.get_wait_id()
    }
}

impl MsgTaskRequest {
    fn new(
        from: ClientId,
        to: Vec<ClientId>,
        task_type: MsgType,
        body: String,
        failure_strategy: FailureStrategy,
    ) -> Self {
        MsgTaskRequest {
            id: MsgId::new(),
            from,
            to,
            task_type,
            body,
            failure_strategy,
            results: HashMap::new(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MsgPing {
    id: MsgId,
    from: ClientId,
    to: Vec<ClientId>,
    nonce: [u8; 16]
}

impl MsgPing {
    pub fn new(from: ClientId, to: ClientId) -> Self {
        let mut nonce = [0;16];
        openssl::rand::rand_bytes(&mut nonce)
            .expect("Critical Error: Failed to generate random byte array.");
        MsgPing { id: MsgId::new(), from, to: vec![to], nonce }
    }
}

impl Msg for MsgPing {
    fn get_id(&self) -> &MsgId {
        &self.id
    }

    fn get_from(&self) -> &ClientId {
        &self.from
    }

    fn get_to(&self) -> &Vec<ClientId> {
        &self.to
    }
}

pub fn generate_example_tasks(client1_id: Option<ClientId>) -> HashMap<MsgId, MsgTaskRequest> {
    let mut tasks: HashMap<MsgId, MsgTaskRequest> = HashMap::new();
    let client1 = client1_id.unwrap_or_default();
    let client2 = ClientId::random();

    let task_for_clients_1_2 = MsgTaskRequest::new(
        client1.clone(),
        vec![client1.clone(), client2.clone()],
        "My important task".to_string(),
        "This task is for client1 and client2".to_string(),
        FailureStrategy::Retry { backoff_millisecs: 1000, max_tries: 5 },
    );

    let response_by_client1 = MsgTaskResult {
        id: MsgId::new(),
        from: client1.clone(),
        to: vec![client1.clone()],
        task: task_for_clients_1_2.id,
        result: crate::WorkResult::Succeeded("All done!".to_string()),
    };
    let response_by_client2 = MsgTaskResult {
        id: MsgId::new(),
        from: client2,
        to: vec![client1],
        task: task_for_clients_1_2.id,
        result: crate::WorkResult::PermFailed("Unable to complete".to_string()),
    };
    tasks.insert(task_for_clients_1_2.id, task_for_clients_1_2);
    let task_in_map = tasks.values_mut().next().unwrap(); // only used in testing
    for result in [response_by_client1, response_by_client2] {
        let result = MsgSigned{
            msg: result,
            sig: String::from("just_an_example"),
        };
        task_in_map.results.insert(result.msg.from.clone(), result);
    }
    tasks
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
