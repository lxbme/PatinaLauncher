use super::*;
use crate::accounts::{Account, AccountKind};
use serde_json::{Value, json};
use uuid::Uuid;

pub(super) fn profile_json() -> Value {
    json!({"id":"0123456789abcdef0123456789abcdef","name":"Player_123",
    "skins":[{"id":"skin-id","state":"ACTIVE","url":"http://textures.minecraft.net/texture/ABCDEF","variant":"SLIM"}],
    "capes":[{"id":"cape-one","state":"ACTIVE","url":"https://textures.minecraft.net/texture/cdefab","alias":"Migrator"}], "future":true})
}
fn parse(value: Value) -> Result<AppearanceProfile, AppearanceError> {
    let uuid = Uuid::parse_str("0123456789abcdef0123456789abcdef").unwrap();
    let identity = AppearanceIdentity {
        account: Account {
            id: uuid.simple().to_string(),
            kind: AccountKind::Msa,
            username: "Player_123".into(),
            uuid,
            token_ref: Some("opaque-reference".into()),
            verification: None,
            skin_hash: None,
            offline_origin: None,
        },
        revision: 1,
    };
    parse_profile(&serde_json::to_vec(&value).unwrap(), identity)
}
#[test]
fn appearance_profile_parses_variants_and_owned_capes() {
    let profile = parse(profile_json()).unwrap();
    assert_eq!(profile.account_id(), "0123456789abcdef0123456789abcdef");
    assert_eq!(profile.skin().unwrap().model(), SkinModel::Slim);
    assert_eq!(profile.skin().unwrap().texture().hash(), "abcdef");
    assert_eq!(profile.capes().len(), 1);
    assert_eq!(profile.active_cape().unwrap().id().as_str(), "cape-one");
    assert_eq!(profile.capes()[0].alias(), "Migrator");
    assert!(!format!("{profile:?}").contains("opaque-reference"));
    let mut classic = profile_json();
    classic["skins"][0]["variant"] = json!("CLASSIC");
    assert_eq!(
        parse(classic).unwrap().skin().unwrap().model(),
        SkinModel::Classic
    );
    let mut empty = profile_json();
    empty["skins"] = json!([]);
    empty["capes"] = json!([]);
    let p = parse(empty).unwrap();
    assert!(p.skin().is_none());
    assert!(p.capes().is_empty());
    assert!(p.active_cape().is_none());
}
#[test]
fn appearance_profile_missing_malformed_and_unsafe_fail_closed() {
    for field in ["capes", "skins"] {
        let mut missing = profile_json();
        missing.as_object_mut().unwrap().remove(field);
        assert!(parse(missing).is_err());
        for bad in [Value::Null, json!({}), json!("bad")] {
            let mut v = profile_json();
            v[field] = bad;
            assert!(parse(v).is_err());
        }
    }
    for url in [
        "https://evil.example/texture/abcdef",
        "https://textures.minecraft.net.evil/texture/abcdef",
        "https://textures.minecraft.net/texture/abc?token=secret",
        "https://user:secret@textures.minecraft.net/texture/abc",
        "https://textures.minecraft.net:444/texture/abc",
        "https://textures.minecraft.net/texture/../abc",
    ] {
        let mut v = profile_json();
        v["skins"][0]["url"] = json!(url);
        assert!(parse(v).is_err(), "{url}");
    }
    for field in ["id", "state", "url", "alias"] {
        let mut v = profile_json();
        v["capes"][0].as_object_mut().unwrap().remove(field);
        assert!(parse(v).is_err());
    }
    let mut mismatch = profile_json();
    mismatch["id"] = json!(Uuid::from_u128(9).simple().to_string());
    assert!(matches!(
        parse(mismatch),
        Err(AppearanceError::Auth(
            crate::accounts::error::AuthError::IdentityMismatch
        ))
    ));
    let mut duplicate = profile_json();
    let cape = duplicate["capes"][0].clone();
    duplicate["capes"].as_array_mut().unwrap().push(cape);
    assert!(parse(duplicate).is_err());
    let mut unknown = profile_json();
    unknown["skins"][0]["variant"] = json!("UNKNOWN");
    assert!(parse(unknown).is_err());
}
