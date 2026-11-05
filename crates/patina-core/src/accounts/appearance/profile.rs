use super::{AppearanceError, SkinModel};
use crate::accounts::{
    Account,
    error::{AuthError, AuthStage},
};
use serde::Deserialize;
use std::{collections::HashSet, sync::Arc};
use uuid::Uuid;

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct AppearanceIdentity {
    pub(crate) account: Account,
    pub(crate) revision: u64,
}
impl std::fmt::Debug for AppearanceIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppearanceIdentity")
            .field("id", &self.account.id)
            .field("revision", &self.revision)
            .finish_non_exhaustive()
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppearanceTexture {
    pub(crate) identity: Arc<AppearanceIdentity>,
    hash: String,
}
impl AppearanceTexture {
    pub fn hash(&self) -> &str {
        &self.hash
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppearanceSkin {
    texture: AppearanceTexture,
    model: SkinModel,
}
impl AppearanceSkin {
    pub fn texture(&self) -> &AppearanceTexture {
        &self.texture
    }
    pub fn model(&self) -> SkinModel {
        self.model
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CapeId(String);
impl CapeId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cape {
    id: CapeId,
    alias: String,
    texture: AppearanceTexture,
    active: bool,
}
impl Cape {
    pub fn id(&self) -> &CapeId {
        &self.id
    }
    pub fn alias(&self) -> &str {
        &self.alias
    }
    pub fn texture(&self) -> &AppearanceTexture {
        &self.texture
    }
    pub fn is_active(&self) -> bool {
        self.active
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppearanceProfile {
    pub(crate) identity: Arc<AppearanceIdentity>,
    skin: Option<AppearanceSkin>,
    capes: Vec<Cape>,
}
impl AppearanceProfile {
    pub fn account_id(&self) -> &str {
        &self.identity.account.id
    }
    pub fn uuid(&self) -> Uuid {
        self.identity.account.uuid
    }
    pub fn revision(&self) -> u64 {
        self.identity.revision
    }
    pub fn skin(&self) -> Option<&AppearanceSkin> {
        self.skin.as_ref()
    }
    pub fn capes(&self) -> &[Cape] {
        &self.capes
    }
    pub fn active_cape(&self) -> Option<&Cape> {
        self.capes.iter().find(|cape| cape.active)
    }
}
#[derive(Deserialize)]
struct Profile {
    id: Uuid,
    skins: Vec<Skin>,
    capes: Vec<RawCape>,
}
#[derive(Deserialize)]
struct Skin {
    state: String,
    url: String,
    variant: String,
}
#[derive(Deserialize)]
struct RawCape {
    id: String,
    state: String,
    url: String,
    alias: String,
}
fn invalid() -> AppearanceError {
    AuthError::InvalidResponse {
        stage: AuthStage::Profile,
    }
    .into()
}
fn hash(url: &str) -> Result<String, AppearanceError> {
    let hash = url
        .strip_prefix("https://textures.minecraft.net/texture/")
        .or_else(|| url.strip_prefix("http://textures.minecraft.net/texture/"))
        .ok_or_else(invalid)?
        .to_ascii_lowercase();
    if !crate::accounts::skin::valid_texture_hash(&hash) {
        return Err(invalid());
    }
    Ok(hash)
}
fn active(state: &str) -> Result<bool, AppearanceError> {
    match state {
        "ACTIVE" => Ok(true),
        "INACTIVE" => Ok(false),
        _ => Err(invalid()),
    }
}
pub(crate) fn parse_profile(
    bytes: &[u8],
    identity: AppearanceIdentity,
) -> Result<AppearanceProfile, AppearanceError> {
    let profile: Profile = serde_json::from_slice(bytes).map_err(|_| invalid())?;
    if profile.id != identity.account.uuid {
        return Err(AuthError::IdentityMismatch.into());
    }
    if profile.skins.len() > 16 || profile.capes.len() > 128 {
        return Err(invalid());
    }
    let identity = Arc::new(identity);
    let mut skin = None;
    for entry in profile.skins {
        let model = match entry.variant.as_str() {
            "CLASSIC" => SkinModel::Classic,
            "SLIM" => SkinModel::Slim,
            _ => return Err(invalid()),
        };
        let texture = AppearanceTexture {
            identity: identity.clone(),
            hash: hash(&entry.url)?,
        };
        if active(&entry.state)? && skin.replace(AppearanceSkin { texture, model }).is_some() {
            return Err(invalid());
        }
    }
    let mut capes = Vec::new();
    let mut seen = HashSet::new();
    let mut active_count = 0;
    for entry in profile.capes {
        if entry.id.is_empty()
            || entry.id.len() > 128
            || !entry
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || entry.alias.is_empty()
            || entry.alias.len() > 256
            || entry.alias.chars().any(char::is_control)
            || !seen.insert(entry.id.clone())
        {
            return Err(invalid());
        }
        let active = active(&entry.state)?;
        active_count += usize::from(active);
        capes.push(Cape {
            id: CapeId(entry.id),
            alias: entry.alias,
            texture: AppearanceTexture {
                identity: identity.clone(),
                hash: hash(&entry.url)?,
            },
            active,
        });
    }
    if active_count > 1 {
        return Err(invalid());
    }
    Ok(AppearanceProfile {
        identity,
        skin,
        capes,
    })
}

impl AppearanceProfile {
    pub(crate) fn rebind(&mut self, identity: AppearanceIdentity) {
        let identity = Arc::new(identity);
        self.identity = identity.clone();
        if let Some(skin) = &mut self.skin {
            skin.texture.identity = identity.clone();
        }
        for cape in &mut self.capes {
            cape.texture.identity = identity.clone();
        }
    }
}
