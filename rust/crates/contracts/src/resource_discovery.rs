//! Source preview discovery stream boundary. URLs are bounded, trimmed strings;
//! protocol and loopback filtering belong to PortScanner, not this wire schema.
use crate::{history::object_struct, *};
use serde::{Deserialize, Serialize};
pub type ConfiguredLocalServerUrls = BoundedVec<BoundedTrimmedString<2048>, 32>;
object_struct! {pub struct DiscoveredLocalServerTerminal{pub thread_id:ThreadId,pub terminal_id:TrimmedNonEmptyString,}}
object_struct! {pub struct DiscoveredLocalServer{
    pub host:TrimmedNonEmptyString,pub port:RangeInt<1,65535>,pub url:BoundedTrimmedString<2048>,
    #[serde(deserialize_with="deserialize_required_nullable")]
    pub process_name:Option<TrimmedNonEmptyString>,
    #[serde(deserialize_with="deserialize_required_nullable")]
    pub pid:Option<PositiveInt>,
    #[serde(deserialize_with="deserialize_required_nullable")]
    pub terminal:Option<DiscoveredLocalServerTerminal>,
}}
object_struct! {pub struct DiscoveredLocalServerList{
    pub servers:Vec<DiscoveredLocalServer>,pub scanned_at:String,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub configured_url_probing:Option<Option<LiteralBool<true>>>,
}}
object_struct! {pub struct SubscribeDiscoveredLocalServersInput{
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub configured_urls:Option<Option<ConfiguredLocalServerUrls>>,
}}
