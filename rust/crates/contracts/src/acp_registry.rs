//! ACP registry setup contracts, shared by catalog services and every client.
use crate::{history::object_struct, *};
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct AcpRegistryAgentId(pub BoundedTrimmedString<128>);
impl AcpRegistryAgentId {
    pub fn as_str(&self) -> &str {
        self.0.0.as_str()
    }
}
impl<'de> Deserialize<'de> for AcpRegistryAgentId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = BoundedTrimmedString::<128>::deserialize(d)?;
        let text = value.0.as_str();
        if text
            .as_bytes()
            .first()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
            && text.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)
            })
        {
            Ok(Self(value))
        } else {
            Err(serde::de::Error::custom("invalid ACP registry agent ID"))
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AcpRegistryDistribution {
    Binary,
    Npx,
    Uvx,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AcpRegistryIntegrity {
    Sha256,
    Registry,
}
fn query<'de, D: serde::Deserializer<'de>>(d: D) -> Result<TrimmedString, D::Error> {
    let value = TrimmedString::deserialize(d)?;
    if value.as_str().encode_utf16().count() <= 120 {
        Ok(value)
    } else {
        Err(serde::de::Error::custom(
            "ACP registry query exceeds120 UTF16 units",
        ))
    }
}
fn literal_true<'de, D: serde::Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    if bool::deserialize(d)? {
        Ok(true)
    } else {
        Err(serde::de::Error::custom("expected true"))
    }
}
object_struct! {pub struct AcpRegistrySearchInput {#[serde(deserialize_with="query")] pub query:TrimmedString,}}
object_struct! {pub struct AcpRegistrySearchAgent {
    pub id:AcpRegistryAgentId,pub name:BoundedTrimmedString<160>,pub version:BoundedTrimmedString<128>,pub description:BoundedString<1024>,pub authors:BoundedVec<BoundedString<256>,16>,
    #[serde(deserialize_with="deserialize_required_nullable")] pub license:Option<BoundedString<128>>,
    #[serde(deserialize_with="deserialize_required_nullable")] pub website:Option<BoundedString<2048>>,
    #[serde(deserialize_with="deserialize_required_nullable")] pub repository:Option<BoundedString<2048>>,
    #[serde(deserialize_with="deserialize_required_nullable")] pub icon:Option<BoundedString<2048>>,
    pub distribution:AcpRegistryDistribution,pub integrity:AcpRegistryIntegrity,
}}
object_struct! {pub struct AcpRegistrySearchResult {pub agents:BoundedVec<AcpRegistrySearchAgent,20>,}}
object_struct! {pub struct AcpRegistryPrepareInput {pub agent_id:AcpRegistryAgentId,}}
object_struct! {pub struct AcpRegistryPrepareResult {pub agent_id:AcpRegistryAgentId,pub version:BoundedTrimmedString<128>,pub distribution:AcpRegistryDistribution,#[serde(deserialize_with="literal_true")] pub prepared:bool,}}
object_struct! {pub struct AcpRegistryManagedBinaryUninstallInput {pub agent_id:AcpRegistryAgentId,}}
object_struct! {pub struct AcpRegistryManagedBinaryUninstallResult {pub agent_id:AcpRegistryAgentId,pub removed:bool,}}
