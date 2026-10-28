//! Static fake data for the prototype: instances, recent mods, a launch log.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Loader {
    Vanilla,
    Fabric,
    Forge,
    NeoForge,
    Quilt,
}

impl Loader {
    pub fn label(self) -> &'static str {
        match self {
            Loader::Vanilla => "原版",
            Loader::Fabric => "Fabric",
            Loader::Forge => "Forge",
            Loader::NeoForge => "NeoForge",
            Loader::Quilt => "Quilt",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Instance {
    pub name: &'static str,
    /// Letter on the icon tile.
    pub initial: &'static str,
    /// Index into the palette's tile tints.
    pub tint: usize,
    pub loader: Loader,
    /// Loader version, empty for vanilla.
    pub loader_version: &'static str,
    pub mc: &'static str,
    pub mods: u32,
    pub java: u8,
    pub memory: &'static str,
    pub playtime: &'static str,
    pub saves: u32,
    pub size: &'static str,
    pub last_played: &'static str,
}

impl Instance {
    /// Second line of a list row: `1.21.4 · 84 mods`.
    pub fn summary(&self) -> String {
        if self.mods == 0 {
            self.mc.to_string()
        } else {
            format!("{} · {} mods", self.mc, self.mods)
        }
    }

    /// `Fabric 0.16.10`, or `原版`.
    pub fn loader_full(&self) -> String {
        if self.loader_version.is_empty() {
            self.loader.label().to_string()
        } else {
            format!("{} {}", self.loader.label(), self.loader_version)
        }
    }
}

pub fn instances() -> Vec<Instance> {
    use Loader::*;
    let i = |name, initial, tint, loader, loader_version, mc, mods, java, memory, playtime, saves, size, last_played| Instance {
        name,
        initial,
        tint,
        loader,
        loader_version,
        mc,
        mods,
        java,
        memory,
        playtime,
        saves,
        size,
        last_played,
    };
    vec![
        i("生存 1.21.4", "生", 0, Fabric, "0.16.10", "1.21.4", 84, 21, "6 GB", "126 小时", 4, "2.4 GB", "2 小时前"),
        i("All the Mods 10", "A", 1, NeoForge, "21.1.77", "1.21.1", 412, 21, "10 GB", "58 小时", 2, "7.9 GB", "昨天"),
        i("GTNH", "G", 2, Forge, "10.13.4.1614", "1.7.10", 287, 21, "8 GB", "311 小时", 1, "5.2 GB", "3 天前"),
        i("Create 机械动力", "C", 4, Forge, "47.3.12", "1.20.1", 126, 17, "6 GB", "44 小时", 3, "3.1 GB", "5 天前"),
        i("原版 1.20.1", "原", 3, Vanilla, "", "1.20.1", 0, 17, "4 GB", "92 小时", 6, "860 MB", "1 周前"),
        i("空岛生存", "空", 5, Fabric, "0.15.11", "1.20.4", 37, 17, "4 GB", "17 小时", 1, "1.1 GB", "2 周前"),
        i("Vault Hunters 3", "V", 7, Forge, "40.2.21", "1.18.2", 231, 17, "8 GB", "73 小时", 2, "4.6 GB", "3 周前"),
        i("测试服 Quilt", "测", 6, Quilt, "0.27.1", "1.21.1", 12, 21, "4 GB", "3 小时", 1, "420 MB", "1 个月前"),
    ]
}

#[derive(Debug, Clone)]
pub struct ModEntry {
    pub name: &'static str,
    pub version: &'static str,
    pub date: &'static str,
    pub tint: usize,
}

/// "Recently updated" mods for an instance's loader.
pub fn recent_mods(loader: Loader) -> Vec<ModEntry> {
    let m = |name, version, date, tint| ModEntry {
        name,
        version,
        date,
        tint,
    };
    match loader {
        Loader::Vanilla => vec![],
        Loader::Fabric | Loader::Quilt => vec![
            m("Sodium", "0.6.5", "10-27", 3),
            m("Fabric API", "0.110.5", "10-26", 4),
            m("Lithium", "0.14.3", "10-24", 5),
            m("Iris Shaders", "1.8.1", "10-21", 6),
            m("Mod Menu", "13.0.0", "10-18", 0),
            m("Xaero's Minimap", "24.6.1", "10-12", 1),
        ],
        Loader::Forge | Loader::NeoForge => vec![
            m("Create", "6.0.4", "10-27", 4),
            m("JEI", "19.21.0", "10-25", 3),
            m("Embeddium", "1.0.11", "10-22", 5),
            m("FTB Quests", "2101.1.3", "10-19", 1),
            m("Applied Energistics 2", "19.1.1", "10-15", 6),
            m("Mekanism", "10.7.8", "10-09", 2),
        ],
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

impl Level {
    pub fn label(self) -> &'static str {
        match self {
            Level::Info => "INFO",
            Level::Warn => "WARN",
            Level::Error => "ERROR",
        }
    }
}

#[derive(Debug, Clone)]
pub struct LogLine {
    pub time: &'static str,
    pub level: Level,
    pub thread: &'static str,
    pub msg: &'static str,
}

pub fn launch_log() -> Vec<LogLine> {
    use Level::*;
    let l = |time, level, thread, msg| LogLine {
        time,
        level,
        thread,
        msg,
    };
    vec![
        l("14:02:11.204", Info, "main", "Patina 0.1.0 正在启动 生存 1.21.4（Fabric 0.16.10）"),
        l("14:02:11.219", Info, "main", "Java 21.0.5+11 (Eclipse Adoptium) · -Xmx6G -XX:+UseG1GC"),
        l("14:02:11.388", Info, "main", "Loading Minecraft 1.21.4 with Fabric Loader 0.16.10"),
        l("14:02:11.902", Info, "main", "Loading 84 mods:"),
        l("14:02:11.903", Info, "main", "  - fabric-api 0.110.5+1.21.4"),
        l("14:02:11.903", Info, "main", "  - sodium 0.6.5+mc1.21.4"),
        l("14:02:11.904", Info, "main", "  - lithium 0.14.3+mc1.21.4"),
        l("14:02:11.904", Info, "main", "  - iris 1.8.1+mc1.21.4"),
        l("14:02:12.611", Warn, "main", "Mod 'xaerominimap' 声明了可选依赖 'waystones'，但未安装"),
        l("14:02:13.077", Info, "main", "SpongePowered MIXIN Subsystem Version=0.8.7 Source=union:/mixin"),
        l("14:02:13.540", Info, "Datafixer Bootstrap", "188 Datafixer optimizations took 214 milliseconds"),
        l("14:02:15.802", Info, "Render thread", "Environment: Environment[sessionHost=https://sessionserver.mojang.com]"),
        l("14:02:15.966", Info, "Render thread", "Setting user: Steve"),
        l("14:02:16.430", Info, "Render thread", "Backend library: LWJGL version 3.3.3-snapshot"),
        l("14:02:16.871", Info, "Render thread", "OpenGL: AMD Radeon 780M (radeonsi, gfx1103) GL version 4.6"),
        l("14:02:17.120", Warn, "Render thread", "Sodium: 检测到不受支持的驱动配置，已回退到兼容模式"),
        l("14:02:18.336", Info, "Render thread", "Reloading ResourceManager: vanilla, fabric, sodium, iris, lithium"),
        l("14:02:19.704", Info, "Worker-Main-4", "Found 214 shaders in pack directory"),
        l("14:02:20.019", Error, "Worker-Main-6", "Failed to load model minecraft:block/oak_shelf: missing texture"),
        l("14:02:20.021", Warn, "Worker-Main-6", "Using missing texture, unable to load minecraft:textures/block/oak_shelf.png"),
        l("14:02:21.455", Info, "Render thread", "Created: 2048x1024x4 minecraft:textures/atlas/blocks.png-atlas"),
        l("14:02:21.890", Info, "Render thread", "Sound engine started"),
        l("14:02:22.301", Info, "Render thread", "Iris: 光影包 Complementary Reimagined r5.3 已加载"),
        l("14:02:24.612", Info, "Server thread", "Starting integrated minecraft server version 1.21.4"),
        l("14:02:25.118", Info, "Server thread", "Preparing level \"新的世界\""),
        l("14:02:27.470", Info, "Server thread", "Preparing spawn area: 100%"),
        l("14:02:27.503", Info, "Server thread", "Time elapsed: 2385 ms"),
        l("14:02:28.044", Warn, "Server thread", "Can't keep up! Is the server overloaded? Running 2104ms behind"),
        l("14:02:28.900", Info, "Render thread", "Loaded 1351 advancements"),
        l("14:03:02.776", Error, "Render thread", "Unable to play unknown soundEvent: create:mechanical_saw"),
        l("14:03:15.081", Info, "Render thread", "[CHAT] 已保存截图 2026-10-29_14.03.15.png"),
    ]
}
