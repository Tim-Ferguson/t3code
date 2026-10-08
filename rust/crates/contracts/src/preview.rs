//! Raw preview capture IPC. These are intentionally less constrained than
//! composer context records: captured strings are untrimmed and coordinates
//! use source Schema.Number, including negative and fractional values.
use crate::{deserialize_required_nullable, history::object_struct};
use serde::{Deserialize, Serialize};
use serde_json::Number;

object_struct! {pub struct PickedElementStackFrame {
    #[serde(deserialize_with="deserialize_required_nullable")]
    pub function_name:Option<String>,
    #[serde(deserialize_with="deserialize_required_nullable")]
    pub file_name:Option<String>,
    #[serde(deserialize_with="deserialize_required_nullable")]
    pub line_number:Option<Number>,
    #[serde(deserialize_with="deserialize_required_nullable")]
    pub column_number:Option<Number>,
}}
object_struct! {pub struct PickedElementPayload {
    pub page_url:String,
    #[serde(deserialize_with="deserialize_required_nullable")]
    pub page_title:Option<String>,
    pub tag_name:String,
    #[serde(deserialize_with="deserialize_required_nullable")]
    pub selector:Option<String>,
    pub html_preview:String,
    #[serde(deserialize_with="deserialize_required_nullable")]
    pub component_name:Option<String>,
    #[serde(deserialize_with="deserialize_required_nullable")]
    pub source:Option<PickedElementStackFrame>,
    pub stack:Vec<PickedElementStackFrame>,
    pub styles:String,
    pub picked_at:String,
}}
object_struct! {pub struct PreviewAnnotationRect {pub x:Number,pub y:Number,pub width:Number,pub height:Number,}}
object_struct! {pub struct PreviewAnnotationPoint {pub x:Number,pub y:Number,}}
object_struct! {pub struct PreviewAnnotationElementTarget {pub id:String,pub element:PickedElementPayload,pub rect:PreviewAnnotationRect,}}
object_struct! {pub struct PreviewAnnotationRegionTarget {pub id:String,pub rect:PreviewAnnotationRect,}}
object_struct! {pub struct PreviewAnnotationStrokeTarget {pub id:String,pub color:String,pub width:Number,pub points:Vec<PreviewAnnotationPoint>,pub bounds:PreviewAnnotationRect,}}
// Distinct from the bounded composer PreviewAnnotationStyleChange contract.
object_struct! {pub struct PreviewAnnotationCaptureStyleChange {
    pub target_id:String,
    #[serde(deserialize_with="deserialize_required_nullable")]
    pub selector:Option<String>,
    pub property:String,pub previous_value:String,pub value:String,
}}
object_struct! {pub struct PreviewAnnotationScreenshot {pub data_url:String,pub width:Number,pub height:Number,pub crop_rect:PreviewAnnotationRect,}}
object_struct! {pub struct PreviewAnnotationPayload {
    pub id:String,pub page_url:String,
    #[serde(deserialize_with="deserialize_required_nullable")]
    pub page_title:Option<String>,
    pub comment:String,pub elements:Vec<PreviewAnnotationElementTarget>,pub regions:Vec<PreviewAnnotationRegionTarget>,pub strokes:Vec<PreviewAnnotationStrokeTarget>,pub style_changes:Vec<PreviewAnnotationCaptureStyleChange>,
    #[serde(deserialize_with="deserialize_required_nullable")]
    pub screenshot:Option<PreviewAnnotationScreenshot>,
    pub created_at:String,
}}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PreviewAnnotationSubmission {
    Attach,
    Send,
}
