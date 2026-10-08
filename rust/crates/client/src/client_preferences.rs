//! Browser-local ClientSettings persistence. Patches retain unrelated raw keys
//! while the source typed schemas validate every published snapshot.
use serde_json::{Map, Value};
use t3_contracts::{ClientSettings, ClientSettingsPatch};
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteReceipt {
    pub revision: u64,
    pub bytes: String,
}
#[derive(Debug, Clone, Default)]
pub struct Preferences {
    raw: Option<Map<String, Value>>,
    pending: Vec<(u64, Map<String, Value>)>,
    snapshot: ClientSettings,
    revision: u64,
    persisted_revision: u64,
    hydration_generation: u64,
    pub read_error: Option<String>,
    pub write_error: Option<String>,
}
impl Preferences {
    pub fn snapshot(&self) -> &ClientSettings {
        &self.snapshot
    }
    pub fn hydrated(&self) -> bool {
        self.raw.is_some() && self.read_error.is_none()
    }
    pub fn begin_hydration(&mut self) -> u64 {
        self.hydration_generation += 1;
        self.hydration_generation
    }
    pub fn hydrate(
        &mut self,
        generation: u64,
        raw: Result<Option<&str>, String>,
    ) -> Result<bool, String> {
        if generation != self.hydration_generation {
            return Ok(false);
        }
        let recovered = (|| {
            let raw = raw?;
            let value: Value = match raw {
                None => Value::Object(Map::new()),
                Some(raw) => serde_json::from_str(raw).map_err(|_| "Saved client preferences are invalid. Saved preferences have been preserved.".to_owned())?,
            };
            let mut object = value.as_object().cloned().ok_or_else(|| "Saved client preferences must be an object. Saved preferences have been preserved.".to_owned())?;
            // Reject the source before overlays: a valid edit cannot repair and
            // overwrite a malformed document with unsupported saved fields.
            serde_json::from_value::<ClientSettings>(Value::Object(object.clone())).map_err(
                |_| {
                    "Saved client preferences are invalid. Saved preferences have been preserved."
                        .to_owned()
                },
            )?;
            for (_, patch) in &self.pending {
                object.extend(patch.clone());
            }
            let snapshot = serde_json::from_value::<ClientSettings>(Value::Object(object.clone()))
                .map_err(|cause| cause.to_string())?;
            Ok::<_, String>((object, snapshot))
        })();
        match recovered {
            Ok((object, snapshot)) => {
                self.raw = Some(object);
                self.snapshot = snapshot;
                self.read_error = None;
                Ok(true)
            }
            Err(error) => {
                self.read_error = Some(error.clone());
                Err(error)
            }
        }
    }
    pub fn patch(&mut self, patch: Value) -> Result<(), String> {
        let patch: ClientSettingsPatch =
            serde_json::from_value(patch).map_err(|cause| cause.to_string())?;
        let patch = serde_json::to_value(patch)
            .map_err(|cause| cause.to_string())?
            .as_object()
            .cloned()
            .expect("typed settings patch object");
        if patch.is_empty() {
            return Ok(());
        }
        if let Some(raw) = self.raw.as_ref().filter(|_| self.read_error.is_none()) {
            let mut next = raw.clone();
            next.extend(patch.clone());
            let snapshot = serde_json::from_value::<ClientSettings>(Value::Object(next.clone()))
                .map_err(|cause| cause.to_string())?;
            self.raw = Some(next);
            self.snapshot = snapshot;
        } else {
            // Publication waits for the valid source document.
        }
        self.revision += 1;
        self.pending.push((self.revision, patch));
        Ok(())
    }
    pub fn needs_flush(&self) -> bool {
        self.revision > self.persisted_revision
    }
    pub fn write_receipt(&self) -> Option<WriteReceipt> {
        if self.read_error.is_some() {
            return None;
        }
        let raw = self.raw.as_ref()?;
        (self.revision > self.persisted_revision).then(|| WriteReceipt {
            revision: self.revision,
            bytes: serde_json::to_string(raw).expect("settings JSON"),
        })
    }
    pub fn acknowledge(&mut self, receipt: &WriteReceipt) {
        if self.write_receipt().as_ref() != Some(receipt) {
            return;
        }
        self.persisted_revision = self
            .persisted_revision
            .max(receipt.revision.min(self.revision));
        self.pending
            .retain(|(revision, _)| *revision > self.persisted_revision);
        self.write_error = None;
    }
    pub fn failed_write(&mut self, error: String) {
        self.write_error = Some(error);
    }
}
