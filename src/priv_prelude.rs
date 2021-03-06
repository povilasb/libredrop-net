//! Common includes.

pub use future_utils::{BoxStream, FutureExt, StreamExt};
pub use futures::{Async, Future, Stream};
pub use peer::PeerInfo;
pub use safe_crypto::{
    gen_encrypt_keypair, Error as EncryptionError, PublicEncryptKey, SecretEncryptKey,
    SharedSecretKey,
};
pub use serde::de::DeserializeOwned;
pub use serde::Serialize;
pub use std::net::SocketAddr;
pub use std::time::Duration;
pub use void::Void;
