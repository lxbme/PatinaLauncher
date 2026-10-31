use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::java::JavaInstall;
use crate::launch::account::LaunchAccount;
use crate::meta::{ArgValue, OneOrMany, RuleEnv, VersionJson, rules_allow};
use crate::paths::DataDirs;
use crate::platform::CLASSPATH_SEPARATOR;
use crate::resolve::AssetLayout;

/// JVM system property carrying the instance id.
pub const INSTANCE_MARKER_PROP: &str = "-Dpatina.instance";
pub const LAUNCHER_NAME: &str = "patina";
pub const LAUNCHER_VERSION: &str = env!("CARGO_PKG_VERSION");

pub struct LaunchSpec<'a> {
    pub version: &'a VersionJson,
    pub data: &'a DataDirs,
    pub game_dir: &'a Path,
    pub natives_dir: &'a Path,
    /// Full path list; the caller puts the client jar last.
    pub classpath: &'a [PathBuf],
    pub asset_layout: &'a AssetLayout,
    pub java: &'a JavaInstall,
    pub account: &'a LaunchAccount,
    pub min_mem_mib: u32,
    pub max_mem_mib: u32,
    pub gc_args: &'a [&'a str],
    pub extra_jvm_args: &'a [String],
    pub logging_config: Option<&'a Path>,
    pub env: &'a RuleEnv,
    /// Instance id; adds `-Dpatina.instance=<id>` so a running game can be
    /// recognised later by its command line.
    pub instance_marker: Option<String>,
    /// Appends `--width <w> --height <h>` after the version's game arguments.
    pub window: Option<crate::instance::WindowSize>,
}

#[derive(Clone)]
pub struct LaunchCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
}

impl std::fmt::Debug for LaunchCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut redacted = Vec::with_capacity(self.args.len());
        let mut prev_token_flag = false;
        for a in &self.args {
            let r = if prev_token_flag {
                "<redacted>".to_string()
            } else if a.starts_with("token:") {
                "token:<redacted>".to_string()
            } else {
                a.clone()
            };
            prev_token_flag = a == "--accessToken";
            redacted.push(r);
        }
        f.debug_struct("LaunchCommand")
            .field("program", &self.program)
            .field("args", &redacted)
            .field("cwd", &self.cwd)
            .finish()
    }
}

/// Replaces `${key}`; unknown keys are kept verbatim.
pub fn substitute(template: &str, vars: &HashMap<&str, String>) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find('}') {
            Some(end) => {
                let key = &after[..end];
                match vars.get(key) {
                    Some(v) => out.push_str(v),
                    None => {
                        out.push_str("${");
                        out.push_str(key);
                        out.push('}');
                    }
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn variables<'a>(spec: &LaunchSpec) -> HashMap<&'a str, String> {
    let assets_root = spec.data.assets();
    let game_assets = match spec.asset_layout {
        AssetLayout::Modern => assets_root.clone(),
        AssetLayout::Virtual(p) | AssetLayout::Resources(p) => p.clone(),
    };
    let uuid = spec.account.uuid.simple().to_string();
    let classpath = spec
        .classpath
        .iter()
        .map(|p| path_str(p))
        .collect::<Vec<_>>()
        .join(CLASSPATH_SEPARATOR);
    let a = spec.account;
    let mut v = HashMap::new();
    v.insert("auth_player_name", a.name.clone());
    v.insert("version_name", spec.version.id.clone());
    v.insert("game_directory", path_str(spec.game_dir));
    v.insert("assets_root", path_str(&assets_root));
    v.insert("game_assets", path_str(&game_assets));
    v.insert("assets_index_name", spec.version.asset_index.id.clone());
    v.insert("auth_session", format!("token:{}:{}", a.access_token, uuid));
    v.insert("auth_uuid", uuid);
    v.insert("auth_access_token", a.access_token.clone());
    v.insert("auth_xuid", a.xuid.clone().unwrap_or_default());
    v.insert("clientid", String::new());
    v.insert("user_type", a.user_type.as_str().to_string());
    v.insert("version_type", spec.version.kind.as_str().to_string());
    v.insert("user_properties", "{}".to_string());
    v.insert("natives_directory", path_str(spec.natives_dir));
    v.insert("launcher_name", LAUNCHER_NAME.to_string());
    v.insert("launcher_version", LAUNCHER_VERSION.to_string());
    v.insert("classpath", classpath);
    v.insert("classpath_separator", CLASSPATH_SEPARATOR.to_string());
    v.insert("library_directory", path_str(&spec.data.libraries()));
    v
}

/// Expands rule-gated arguments into raw (unsubstituted) tokens.
fn expand(list: &[ArgValue], env: &RuleEnv) -> Vec<String> {
    let mut out = Vec::new();
    for arg in list {
        match arg {
            ArgValue::Plain(s) => out.push(s.clone()),
            ArgValue::Conditional { rules, value } => {
                if rules_allow(rules, env) {
                    match value {
                        OneOrMany::One(s) => out.push(s.clone()),
                        OneOrMany::Many(v) => out.extend(v.iter().cloned()),
                    }
                }
            }
        }
    }
    out
}

/// Removes `--width X` / `--height Y` (and `--width=X` forms) from `args[from..]`.
fn strip_size_options(args: &mut Vec<String>, from: usize) {
    let tail = args.split_off(from);
    let mut it = tail.into_iter().peekable();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--width" | "--height" => {
                if it.peek().is_some_and(|next| !next.starts_with('-')) {
                    it.next();
                }
            }
            _ if a.starts_with("--width=") || a.starts_with("--height=") => {}
            _ => args.push(a),
        }
    }
}

pub fn build_command(spec: &LaunchSpec) -> LaunchCommand {
    let vars = variables(spec);
    let v = spec.version;
    let sub = |s: &str| substitute(s, &vars);

    let mut args = vec![
        format!("-Xms{}m", spec.min_mem_mib),
        format!("-Xmx{}m", spec.max_mem_mib),
    ];
    args.extend(spec.gc_args.iter().map(|s| s.to_string()));
    args.extend(spec.extra_jvm_args.iter().cloned());

    if let (Some(cfg), Some(client)) = (
        spec.logging_config,
        v.logging.as_ref().and_then(|l| l.client.as_ref()),
    ) {
        let mut lv = HashMap::new();
        lv.insert("path", path_str(cfg));
        args.push(substitute(&client.argument, &lv));
    }
    args.push("-Dlog4j2.formatMsgNoLookups=true".to_string());

    // Split first, substitute per token: values containing spaces stay one argv entry.
    match v.arguments.as_ref().filter(|a| !a.jvm.is_empty()) {
        Some(a) => args.extend(expand(&a.jvm, spec.env).iter().map(|t| sub(t))),
        None => args.extend(
            [
                "-Djava.library.path=${natives_directory}",
                "-Dminecraft.launcher.brand=${launcher_name}",
                "-Dminecraft.launcher.version=${launcher_version}",
                "-cp",
                "${classpath}",
            ]
            .iter()
            .map(|t| sub(t)),
        ),
    }

    if let Some(id) = &spec.instance_marker {
        args.push(format!("{INSTANCE_MARKER_PROP}={id}"));
    }
    args.push(v.main_class.clone());
    let game_start = args.len();

    match (&v.arguments, &v.minecraft_arguments) {
        (Some(a), _) => args.extend(expand(&a.game, spec.env).iter().map(|t| sub(t))),
        (None, Some(m)) => args.extend(m.split_ascii_whitespace().map(sub)),
        (None, None) => {}
    }
    if let Some(w) = spec.window {
        // The game rejects a duplicated option, so drop the version's own.
        strip_size_options(&mut args, game_start);
        args.extend([
            "--width".to_string(),
            w.width.to_string(),
            "--height".to_string(),
            w.height.to_string(),
        ]);
    }

    LaunchCommand {
        program: spec.java.path.clone(),
        args,
        cwd: spec.game_dir.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launch::account::{UserType, offline_uuid};
    use std::collections::HashSet;

    const MODERN: &str = include_str!("../../tests/fixtures/version_modern.json");
    const LEGACY: &str = include_str!("../../tests/fixtures/version_legacy.json");

    fn env() -> RuleEnv {
        RuleEnv {
            os_name: "linux".into(),
            arch: "x86_64".into(),
            os_version: "6.0".into(),
            features: HashSet::new(),
        }
    }

    fn account() -> LaunchAccount {
        LaunchAccount {
            name: "Dev".into(),
            uuid: offline_uuid("Dev"),
            access_token: "secret-token".into(),
            user_type: UserType::Legacy,
            xuid: None,
        }
    }

    fn java() -> JavaInstall {
        JavaInstall {
            path: PathBuf::from("/j/bin/java"),
            major: 25,
            version: "25".into(),
            is_64bit: true,
        }
    }

    fn build(
        json: &str,
        root: &str,
        cp: &[&str],
        logging: Option<&Path>,
        layout: &AssetLayout,
    ) -> LaunchCommand {
        build_window(json, root, cp, logging, layout, None)
    }

    fn build_window(
        json: &str,
        root: &str,
        cp: &[&str],
        logging: Option<&Path>,
        layout: &AssetLayout,
        window: Option<crate::instance::WindowSize>,
    ) -> LaunchCommand {
        let version: VersionJson = serde_json::from_str(json).unwrap();
        let data = DataDirs::new(root);
        let classpath: Vec<PathBuf> = cp.iter().map(PathBuf::from).collect();
        let (acc, j, e) = (account(), java(), env());
        let extra = vec!["-Dfoo=1".to_string()];
        build_command(&LaunchSpec {
            version: &version,
            data: &data,
            game_dir: Path::new("/g"),
            natives_dir: Path::new("/n"),
            classpath: &classpath,
            asset_layout: layout,
            java: &j,
            account: &acc,
            min_mem_mib: 512,
            max_mem_mib: 4096,
            gc_args: &["-XX:+UseG1GC"],
            extra_jvm_args: &extra,
            logging_config: logging,
            env: &e,
            instance_marker: Some("inst-1".into()),
            window,
        })
    }

    fn after<'a>(c: &'a LaunchCommand, flag: &str) -> &'a str {
        let i = c.args.iter().position(|a| a == flag).unwrap();
        &c.args[i + 1]
    }

    #[test]
    fn window_args_appended() {
        let w = crate::instance::WindowSize {
            width: 1280,
            height: 720,
        };
        let tail = ["--width", "1280", "--height", "720"];
        for (json, layout) in [(MODERN, AssetLayout::Modern), (LEGACY, AssetLayout::Modern)] {
            let c = build_window(json, "/d", &["/d/a.jar"], None, &layout, Some(w));
            assert_eq!(&c.args[c.args.len() - 4..], &tail, "{json:.40}");
            let plain = build(json, "/d", &["/d/a.jar"], None, &layout);
            assert_eq!(&c.args[..c.args.len() - 4], &plain.args[..]);
            assert!(!plain.args.iter().any(|a| a == "--width"));
        }
    }

    #[test]
    fn window_args_replace_existing_size_options() {
        let w = crate::instance::WindowSize {
            width: 1280,
            height: 720,
        };
        let json = LEGACY.replace(
            "--userType ${user_type}",
            "--width 854 --userType ${user_type} --height=480 --height 480",
        );
        assert_ne!(json, LEGACY);
        let c = build_window(
            &json,
            "/d",
            &["/d/a.jar"],
            None,
            &AssetLayout::Modern,
            Some(w),
        );
        assert_eq!(c.args.iter().filter(|a| *a == "--width").count(), 1);
        assert_eq!(c.args.iter().filter(|a| *a == "--height").count(), 1);
        assert!(
            !c.args
                .iter()
                .any(|a| a == "854" || a.starts_with("--height="))
        );
        assert_eq!(
            &c.args[c.args.len() - 4..],
            &["--width", "1280", "--height", "720"]
        );
        assert_eq!(after(&c, "--userType"), "legacy");
    }

    #[test]
    fn modern_command_order_and_rules() {
        let c = build(
            MODERN,
            "/d",
            &["/d/libraries/a.jar", "/d/versions/26.3/26.3.jar"],
            Some(Path::new("/d/log.xml")),
            &AssetLayout::Modern,
        );
        assert_eq!(
            &c.args[0..4],
            &["-Xms512m", "-Xmx4096m", "-XX:+UseG1GC", "-Dfoo=1"]
        );
        assert_eq!(c.args[4], "-Dlog4j.configurationFile=/d/log.xml");
        assert_eq!(c.args[5], "-Dlog4j2.formatMsgNoLookups=true");
        assert!(
            !c.args
                .iter()
                .any(|a| a == "-XstartOnFirstThread" || a == "-Xss1M" || a == "--demo")
        );
        let mc = c
            .args
            .iter()
            .position(|a| a == "net.minecraft.client.main.Main")
            .unwrap();
        let cp = c.args.iter().position(|a| a == "-cp").unwrap();
        assert!(cp < mc);
        let u = c.args.iter().position(|a| a == "--username").unwrap();
        assert!(u > mc && c.args[u + 1] == "Dev");
        assert_eq!(after(&c, "--uuid"), "380df991f603344ca090369bad2a924a");
        assert_eq!(c.cwd, PathBuf::from("/g"));
        assert_eq!(c.program, PathBuf::from("/j/bin/java"));
        assert!(c.args.contains(&"-Djava.library.path=/n".to_string()));
        assert!(
            c.args
                .contains(&"-Dminecraft.launcher.brand=patina".to_string())
        );
    }

    #[test]
    fn marker_is_in_command_line() {
        let c = build(MODERN, "/d", &["/x.jar"], None, &AssetLayout::Modern);
        let m = c
            .args
            .iter()
            .position(|a| a == "-Dpatina.instance=inst-1")
            .unwrap();
        let mc = c
            .args
            .iter()
            .position(|a| a == "net.minecraft.client.main.Main")
            .unwrap();
        assert!(m < mc);
    }

    #[test]
    fn no_logging_arg_without_config() {
        let c = build(MODERN, "/d", &["/x.jar"], None, &AssetLayout::Modern);
        assert!(
            !c.args
                .iter()
                .any(|a| a.starts_with("-Dlog4j.configurationFile"))
        );
        assert_eq!(c.args[4], "-Dlog4j2.formatMsgNoLookups=true");
    }

    #[test]
    fn paths_with_spaces_stay_single_args() {
        let root = "/tmp/数据 目录/root";
        let a = format!("{root}/libraries/a.jar");
        let b = format!("{root}/versions/26.3/26.3.jar");
        let c = build(MODERN, root, &[&a, &b], None, &AssetLayout::Modern);
        assert_eq!(after(&c, "-cp"), format!("{a}:{b}"));
        assert_eq!(after(&c, "--assetsDir"), format!("{root}/assets"));
    }

    #[test]
    fn legacy_defaults_and_session() {
        let c = build(
            LEGACY,
            "/d",
            &["/d/libraries/a.jar", "/d/versions/1.8.9/1.8.9.jar"],
            None,
            &AssetLayout::Virtual(PathBuf::from("/d/assets/virtual/legacy")),
        );
        assert!(c.args.contains(&"-Djava.library.path=/n".to_string()));
        assert_eq!(
            after(&c, "-cp"),
            "/d/libraries/a.jar:/d/versions/1.8.9/1.8.9.jar"
        );
        assert_eq!(after(&c, "--userProperties"), "{}");
        let mc = c
            .args
            .iter()
            .position(|a| a == "net.minecraft.client.main.Main")
            .unwrap();
        assert!(c.args.iter().position(|a| a == "--username").unwrap() > mc);
        assert_eq!(after(&c, "--accessToken"), "secret-token");
        assert_eq!(after(&c, "--assetsDir"), "/d/assets");
    }

    #[test]
    fn debug_output_redacts_token() {
        for json in [MODERN, LEGACY] {
            let c = build(json, "/d", &["/x.jar"], None, &AssetLayout::Modern);
            assert!(c.args.iter().any(|a| a.contains("secret-token")));
            assert!(!format!("{c:?}").contains("secret-token"));
        }
        let mut c = build(LEGACY, "/d", &["/x.jar"], None, &AssetLayout::Modern);
        c.args.push("token:secret-token:abc".into());
        let d = format!("{c:?}");
        assert!(!d.contains("secret-token") && d.contains("token:<redacted>"));
        assert!(!format!("{:?}", account()).contains("secret-token"));
    }

    #[test]
    fn substitute_is_not_recursive() {
        let mut v = HashMap::new();
        v.insert("a", "${auth_access_token}$x".to_string());
        v.insert("auth_access_token", "secret".to_string());
        assert_eq!(substitute("${a}", &v), "${auth_access_token}$x");
    }

    #[test]
    fn substitute_leaves_unknown() {
        let mut v = HashMap::new();
        v.insert("a", "1".to_string());
        assert_eq!(substitute("${a}-${b}", &v), "1-${b}");
    }
    #[test]
    fn malformed_size_does_not_swallow_the_next_option() {
        let mut args = [
            "prefix",
            "--width",
            "--username",
            "Dev",
            "--height",
            "600",
            "--demo",
        ]
        .map(String::from)
        .to_vec();
        strip_size_options(&mut args, 1);
        assert_eq!(args, ["prefix", "--username", "Dev", "--demo"]);
    }
}
