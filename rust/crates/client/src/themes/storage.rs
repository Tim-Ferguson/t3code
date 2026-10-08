//! Source useTheme storage transactions. The legacy follow flag is migration input only.
use super::*;
#[allow(async_fn_in_trait)]
pub trait Storage {
    async fn get(&mut self, key: &str) -> Result<Option<String>, String>;
    async fn set(&mut self, key: &str, value: &str) -> Result<(), String>;
    async fn remove(&mut self, key: &str) -> Result<(), String>;
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub theme: String,
    pub resolved_theme: Appearance,
    pub system_dark: bool,
    pub appearance_mode: Mode,
    pub theme_halves: Option<Halves>,
}
impl Default for Snapshot {
    fn default() -> Self {
        Self {
            theme: "system".into(),
            resolved_theme: Appearance::Light,
            system_dark: false,
            appearance_mode: Mode::System,
            theme_halves: None,
        }
    }
}
#[derive(Default)]
pub struct ReadState {
    pub failure: Option<String>,
}
impl ReadState {
    pub fn storage_changed(&mut self, key: Option<&str>) {
        if key.is_none() || key == Some(THEME_KEY) {
            self.failure = None;
        }
    }
    pub async fn theme(&mut self, storage: &mut impl Storage, catalog: &Catalog) -> String {
        if self.failure.is_some() {
            return "system".into();
        }
        match storage.get(THEME_KEY).await {
            Ok(raw) => catalog.stored_preference(raw.as_deref()),
            Err(error) => {
                self.failure = Some(error);
                "system".into()
            }
        }
    }
    pub async fn snapshot(
        &mut self,
        storage: &mut impl Storage,
        catalog: &Catalog,
        system_dark: bool,
    ) -> Snapshot {
        let theme = self.theme(storage, catalog).await;
        let mode = read_mode(storage, catalog, &theme).await;
        let halves = storage
            .get(HALVES_KEY)
            .await
            .ok()
            .flatten()
            .and_then(|raw| catalog.parse_halves(Some(&raw)));
        let system_dark = mode == Mode::System && system_dark;
        Snapshot {
            resolved_theme: catalog.resolve(
                &theme,
                system_dark,
                Some(mode == Mode::System),
                Some(mode),
                halves.as_ref(),
            ),
            theme,
            system_dark,
            appearance_mode: mode,
            theme_halves: halves,
        }
    }
}
pub async fn read_mode(storage: &mut impl Storage, catalog: &Catalog, theme: &str) -> Mode {
    let mode = storage.get(MODE_KEY).await.ok().flatten();
    if let Some(mode) = mode.as_deref().and_then(Mode::parse) {
        return mode;
    }
    let follow = storage.get(FOLLOW_KEY).await.ok().flatten();
    catalog.stored_mode(theme, None, follow.as_deref())
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Theme(String),
    Mode(Mode),
    Half(Appearance, Option<String>),
    ClearHalves,
}
/// Writes settle before the new choice is applied. Failed theme writes restore the raw mix.
pub async fn apply(
    storage: &mut impl Storage,
    catalog: &Catalog,
    reads: &mut ReadState,
    action: Action,
) -> Result<(), String> {
    match action {
        Action::Theme(theme) => {
            let current = reads.theme(storage, catalog).await;
            let mode = read_mode(storage, catalog, &current).await;
            storage.set(MODE_KEY, mode.key()).await?;
            let previous = storage.get(HALVES_KEY).await?;
            storage.remove(HALVES_KEY).await?;
            if let Err(error) = storage.set(THEME_KEY, &theme).await {
                if let Some(previous) = previous {
                    let _ = storage.set(HALVES_KEY, &previous).await;
                }
                return Err(error);
            }
            reads.failure = None;
        }
        Action::Mode(mode) => {
            storage.set(MODE_KEY, mode.key()).await?;
            reads.failure = None;
        }
        Action::Half(appearance, id) => {
            let raw = storage.get(HALVES_KEY).await.ok().flatten();
            let mut halves = raw_halves(raw.as_deref());
            match appearance {
                Appearance::Light => halves.light = id,
                Appearance::Dark => halves.dark = id,
            }
            if halves.empty() {
                storage.remove(HALVES_KEY).await?;
            } else {
                storage
                    .set(
                        HALVES_KEY,
                        &serde_json::to_string(&halves).expect("string halves"),
                    )
                    .await?;
            }
        }
        Action::ClearHalves => storage.remove(HALVES_KEY).await?,
    }
    Ok(())
}
